//! Event-driven doorbell delivery, auto-escalation, `doc.set` projection,
//! and remote socket server (`orcherd daemon`, `SPEC.md` §§5–7).

use crate::{
    identity::{resolve_db_path, resolve_state_dir},
    tunnel::remove_stale_socket,
};
use orcherd_core::{
    Actor, Address, DispatchState, DoorbellOutcome, Op, RequestEnvelope, Store, TeamProjection,
    format_doorbell,
};
use orcherd_hc::{
    Core, Endpoint, Error as HcError,
    event::{Notice, TerminalChange},
    new_command,
    wire::{BLOCKED, STATUS_MISMATCH, SendText, StatusState},
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaemonStatus {
    pub lock_path: PathBuf,
    pub pid: Option<u32>,
    pub running: bool,
    pub state_dir: PathBuf,
}

pub fn lock_file_path(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon.lock")
}

pub fn status(state_dir: &Path) -> DaemonStatus {
    let lock_path = lock_file_path(state_dir);
    let pid = fs::read_to_string(&lock_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&pid| process_is_alive(pid));
    DaemonStatus {
        state_dir: state_dir.to_path_buf(),
        lock_path,
        running: pid.is_some(),
        pid,
    }
}

pub fn start_detached(state_dir: &Path) -> Result<DaemonStatus, String> {
    let current = status(state_dir);
    if current.running {
        return Ok(current);
    }
    fs::create_dir_all(state_dir)
        .map_err(|e| format!("failed to create `{}`: {e}", state_dir.display()))?;
    let exe = env::current_exe().map_err(|e| format!("failed to resolve current exe: {e}"))?;
    let mut cmd = new_command(&exe);
    cmd.args(["daemon", "run"])
        .env("ORCHERD_STATE_DIR", state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn detached daemon: {e}"))?;
    let pid = child.id();
    let lock_path = lock_file_path(state_dir);
    let _ = fs::write(&lock_path, format!("{pid}\n"));
    Ok(DaemonStatus {
        state_dir: state_dir.to_path_buf(),
        lock_path,
        running: true,
        pid: Some(pid),
    })
}

pub fn stop(state_dir: &Path) -> Result<DaemonStatus, String> {
    let current = status(state_dir);
    if let Some(pid) = current.pid {
        terminate_process(pid);
    }
    let _ = fs::remove_file(&current.lock_path);
    Ok(DaemonStatus {
        state_dir: state_dir.to_path_buf(),
        lock_path: current.lock_path,
        running: false,
        pid: None,
    })
}

/// Runs one delivery and escalation pass across all teams in `store`, using `core` when available.
pub fn tick_once(store: &mut Store, core: Option<&Core>) -> Result<usize, String> {
    let team_ids = list_all_team_ids(store)?;
    let mut deliveries = 0usize;
    let actor = Actor::Human {
        terminal: Some("orcherd-daemon".to_owned()),
    };

    for team_id in team_ids {
        let proj = store
            .team_projection(&team_id)
            .map_err(|e| format!("failed to load projection for `{team_id}`: {e}"))?;

        if let Some(core) = core {
            publish_team_doc(core, &proj);
        }

        if proj.team.paused_at.is_some() {
            continue;
        }

        let listed_terminals = core.and_then(|c| c.list(false).ok()).map(|l| l.terminals);

        for seat in &proj.seats {
            if seat.paused_at.is_some() {
                continue;
            }
            let Address::Core {
                ref machine,
                ref terminal,
            } = seat.address
            else {
                continue;
            };
            let target_address = match machine {
                Some(m) if !terminal.contains('/') => format!("{m}/{terminal}"),
                _ => terminal.clone(),
            };

            let undelivered = store
                .list_undelivered_inbox(&team_id, &seat.name)
                .map_err(|e| format!("failed listing undelivered inbox: {e}"))?;
            if undelivered.is_empty() {
                continue;
            }
            let Some(latest) = undelivered.last() else {
                continue;
            };
            let max_seq = latest.seq;
            let doorbell_line = format_doorbell(undelivered.len(), latest.kind, &latest.ref_id);

            let Some(core) = core else {
                continue;
            };

            // Verify via herdr-core that the terminal exists and the agent is still in front (SPEC.md §5.1).
            if let Some(ref terms) = listed_terminals
                && let Some(record) = terms
                    .iter()
                    .find(|r| r.address == target_address || r.address == *terminal)
                && record.foreground.is_some()
                && !orcherd_hc::reply::agent_in_front(record, &seat.agent)
            {
                let _ = store.apply(
                    &actor,
                    RequestEnvelope::new(Op::DaemonRecordDoorbell {
                        seat: seat.name.clone(),
                        rung_through_seq: max_seq,
                        outcome: DoorbellOutcome::Unreachable,
                    })
                    .with_team(&team_id),
                );
                if let Some(dispatch_id) = active_dispatch_for_seat(&proj, &seat.seat_id) {
                    let _ = store.apply(
                        &actor,
                        RequestEnvelope::new(Op::DaemonEscalate {
                            seat: seat.name.clone(),
                            dispatch_id: Some(dispatch_id),
                            stop_reason: "exited".to_owned(),
                            detail: format!(
                                "agent `{}` exited back to shell in terminal `{target_address}`",
                                seat.agent
                            ),
                        })
                        .with_team(&team_id),
                    );
                }
                continue;
            }

            let send_res = core.send(&SendText {
                force: false,
                only_if: Some("idle"),
                terminal: &target_address,
                text: &doorbell_line,
            });

            match send_res {
                Ok(sent) => {
                    let outcome = match sent.started {
                        Some(true) => DoorbellOutcome::Confirmed,
                        None => DoorbellOutcome::Unconfirmed,
                        Some(false) => DoorbellOutcome::StatusMismatch,
                    };
                    let _ = store.apply(
                        &actor,
                        RequestEnvelope::new(Op::DaemonRecordDoorbell {
                            seat: seat.name.clone(),
                            rung_through_seq: max_seq,
                            outcome,
                        })
                        .with_team(&team_id),
                    );
                    if matches!(
                        outcome,
                        DoorbellOutcome::Confirmed | DoorbellOutcome::Unconfirmed
                    ) {
                        deliveries += 1;
                    }
                }
                Err(HcError::Refused {
                    kind: Some(ref kind),
                    ref message,
                    ..
                }) if kind == STATUS_MISMATCH => {
                    let _ = message;
                    let _ = store.apply(
                        &actor,
                        RequestEnvelope::new(Op::DaemonRecordDoorbell {
                            seat: seat.name.clone(),
                            rung_through_seq: max_seq,
                            outcome: DoorbellOutcome::StatusMismatch,
                        })
                        .with_team(&team_id),
                    );
                }
                Err(HcError::Refused {
                    kind: Some(ref kind),
                    ref message,
                    ..
                }) if kind == BLOCKED => {
                    let _ = store.apply(
                        &actor,
                        RequestEnvelope::new(Op::DaemonRecordDoorbell {
                            seat: seat.name.clone(),
                            rung_through_seq: max_seq,
                            outcome: DoorbellOutcome::Blocked,
                        })
                        .with_team(&team_id),
                    );
                    let _ = store.apply(
                        &actor,
                        RequestEnvelope::new(Op::DaemonEscalate {
                            seat: seat.name.clone(),
                            dispatch_id: active_dispatch_for_seat(&proj, &seat.seat_id),
                            stop_reason: "blocked".to_owned(),
                            detail: format!(
                                "terminal `{target_address}` is blocked on a dialog: {message}"
                            ),
                        })
                        .with_team(&team_id),
                    );
                }
                Err(err) => {
                    let _ = store.apply(
                        &actor,
                        RequestEnvelope::new(Op::DaemonRecordDoorbell {
                            seat: seat.name.clone(),
                            rung_through_seq: max_seq,
                            outcome: DoorbellOutcome::Unreachable,
                        })
                        .with_team(&team_id),
                    );
                    let _ = err;
                }
            }
        }
    }

    Ok(deliveries)
}

/// Processes a `herdr-core` event notice and triggers auto-escalations when a
/// managed seat terminal blocks on an auth/permission dialog or exits unexpectedly.
pub fn handle_core_notice(store: &mut Store, notice: Notice) {
    let Notice::Event(event) = notice else {
        return;
    };
    let Ok(team_ids) = list_all_team_ids(store) else {
        return;
    };
    let actor = Actor::Human {
        terminal: Some("orcherd-daemon".to_owned()),
    };

    for team_id in team_ids {
        let Ok(proj) = store.team_projection(&team_id) else {
            continue;
        };
        for seat in &proj.seats {
            let matches_terminal = match &seat.address {
                Address::Core {
                    machine: Some(m),
                    terminal: t,
                } => event.terminal == *t || event.terminal == format!("{m}/{t}"),
                Address::Core {
                    machine: None,
                    terminal: t,
                } => event.terminal == *t,
                _ => false,
            };
            if !matches_terminal {
                continue;
            }

            match &event.change {
                TerminalChange::Status {
                    status: Some(status),
                    ..
                } if status.state == StatusState::Blocked => {
                    let kind = status.kind.as_deref().unwrap_or("dialog");
                    let _ = store.apply(
                        &actor,
                        RequestEnvelope::new(Op::DaemonEscalate {
                            seat: seat.name.clone(),
                            dispatch_id: active_dispatch_for_seat(&proj, &seat.seat_id),
                            stop_reason: "blocked".to_owned(),
                            detail: format!(
                                "seat `{}` blocked on {kind}: {}",
                                seat.name, status.msg
                            ),
                        })
                        .with_team(&team_id),
                    );
                }
                TerminalChange::Finished(ended) | TerminalChange::Lost(ended) => {
                    if let Some(dispatch_id) = active_dispatch_for_seat(&proj, &seat.seat_id) {
                        let stop_reason = if matches!(event.change, TerminalChange::Lost(_)) {
                            "lost"
                        } else {
                            "exited"
                        };
                        let _ = store.apply(
                            &actor,
                            RequestEnvelope::new(Op::DaemonEscalate {
                                seat: seat.name.clone(),
                                dispatch_id: Some(dispatch_id),
                                stop_reason: stop_reason.to_owned(),
                                detail: format!(
                                    "terminal `{}` {} (exit_code: {:?}, reason: {:?})",
                                    event.terminal, stop_reason, ended.exit_code, ended.reason
                                ),
                            })
                            .with_team(&team_id),
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

pub fn run_foreground(once: bool) -> Result<(), String> {
    let state_dir = resolve_state_dir();
    fs::create_dir_all(&state_dir)
        .map_err(|e| format!("failed to create `{}`: {e}", state_dir.display()))?;
    // One daemon per store: two concurrent `spawn`s can both start one.
    if let Some(pid) = status(&state_dir).pid.filter(|&pid| pid != std::process::id()) {
        if once {
            return Err(format!("orcherd daemon already running (pid {pid}); stop it before `daemon run --once`"));
        }
        return Ok(());
    }
    let lock_path = lock_file_path(&state_dir);
    let _ = fs::write(&lock_path, format!("{}\n", std::process::id()));

    let db_path = resolve_db_path(&state_dir);
    let mut store =
        Store::open(&db_path).map_err(|e| format!("failed to open `{}`: {e}", db_path.display()))?;

    let core = Endpoint::resolve_env()
        .ok()
        .and_then(|ep| Core::connect(&ep).ok());
    let mut subscription = core.as_ref().and_then(|c| c.subscribe().ok());

    if once {
        let _ = tick_once(&mut store, core.as_ref())?;
        return Ok(());
    }

    #[cfg(unix)]
    let mut listeners = bind_remote_listeners(&state_dir);
    let mut last_data_version = -1i64;
    let mut last_tick = Instant::now() - Duration::from_secs(10);

    loop {
        #[cfg(unix)]
        {
            refresh_remote_listeners(&state_dir, &mut listeners);
            for (machine, listener) in &listeners {
                while let Ok((stream, _)) = listener.accept() {
                    let m = if machine == "default" {
                        None
                    } else {
                        Some(machine.as_str())
                    };
                    crate::remote::handle_client_stream(&mut store, stream, m);
                }
            }
        }

        let mut woke_by_event = false;
        if let Some(ref mut sub) = subscription
            && let Ok(notices) = sub.next(Duration::from_millis(20))
        {
            for notice in notices {
                if matches!(
                    notice,
                    Notice::Event(ref ev)
                        if matches!(
                            ev.change,
                            TerminalChange::Status {
                                status: Some(ref s),
                                ..
                            } if s.state == StatusState::Idle
                        )
                ) {
                    woke_by_event = true;
                }
                handle_core_notice(&mut store, notice);
            }
        }

        let current_dv = store.data_version().unwrap_or(last_data_version);
        if woke_by_event
            || current_dv != last_data_version
            || last_tick.elapsed() >= Duration::from_secs(2)
        {
            let _ = tick_once(&mut store, core.as_ref());
            last_data_version = store.data_version().unwrap_or(current_dv);
            last_tick = Instant::now();
        }

        std::thread::sleep(Duration::from_millis(80));
    }
}

fn publish_team_doc(core: &Core, proj: &TeamProjection) {
    let doc_name = format!("orcherd/team/{}", proj.team.team_id);
    let Ok(value) = serde_json::to_value(proj) else {
        return;
    };
    let version = core.doc_get(&doc_name).map(|d| d.version).unwrap_or(0);
    let _ = core.doc_set(&doc_name, &value, version);
}

fn active_dispatch_for_seat(proj: &TeamProjection, seat_id: &str) -> Option<String> {
    proj.active_dispatches
        .iter()
        .find(|d| {
            d.seat_id == seat_id
                && matches!(
                    d.state,
                    DispatchState::Starting | DispatchState::Ready | DispatchState::Working
                )
        })
        .map(|d| d.dispatch_id.clone())
}

fn list_all_team_ids(store: &Store) -> Result<Vec<String>, String> {
    let mut stmt = store
        .connection()
        .prepare("SELECT team_id FROM teams ORDER BY created_at ASC")
        .map_err(|e| format!("failed preparing teams query: {e}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("failed querying teams: {e}"))?;
    let mut ids = Vec::new();
    for id in rows.flatten() {
        ids.push(id);
    }
    Ok(ids)
}

#[cfg(unix)]
fn bind_remote_listeners(
    state_dir: &Path,
) -> BTreeMap<String, std::os::unix::net::UnixListener> {
    let mut map = BTreeMap::new();
    refresh_remote_listeners(state_dir, &mut map);
    map
}

#[cfg(unix)]
fn refresh_remote_listeners(
    state_dir: &Path,
    map: &mut BTreeMap<String, std::os::unix::net::UnixListener>,
) {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    let remote_dir = state_dir.join("remote");
    let _ = fs::create_dir_all(&remote_dir);
    let default_sock = remote_dir.join("default.sock");
    if !map.contains_key("default") {
        let _ = remove_stale_socket(&default_sock);
        if let Ok(listener) = UnixListener::bind(&default_sock) {
            let _ = fs::set_permissions(&default_sock, fs::Permissions::from_mode(0o600));
            let _ = listener.set_nonblocking(true);
            map.insert("default".to_owned(), listener);
        }
    }
    if let Ok(entries) = fs::read_dir(&remote_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("sock")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && !map.contains_key(stem)
            {
                let _ = remove_stale_socket(&path);
                if let Ok(listener) = UnixListener::bind(&path) {
                    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
                    let _ = listener.set_nonblocking(true);
                    map.insert(stem.to_owned(), listener);
                }
            }
        }
    }
}

/// Linux answers from `/proc`; macOS and the BSDs have no `/proc`, so ask `kill -0`,
/// which checks the pid without signalling it. Without this, macOS never saw a running
/// daemon: `daemon stop` was a no-op and every `init`/`spawn` started another one.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    if Path::new("/proc/self").exists() {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    new_command("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn terminate_process(pid: u32) {
    let _ = new_command("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
}

#[cfg(not(unix))]
fn terminate_process(_pid: u32) {}
