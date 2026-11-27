//! Event-driven doorbell delivery, auto-escalation, `doc.set` projection,
//! and remote socket server (`orcherd daemon`, `SPEC.md` §§5–7).

use crate::{
    identity::{resolve_db_path, resolve_state_dir},
    tunnel::remove_stale_socket,
};
use orcherd_core::{
    Actor, Address, DispatchState, DoorbellOutcome, Op, RequestEnvelope, Seat, SeatRole, Store,
    TeamProjection, format_doorbell,
};
use orcherd_hc::{
    Core, Endpoint, Error as HcError,
    event::{Notice, TerminalChange},
    new_command,
    wire::{BLOCKED, STATUS_MISMATCH, SendText, StatusState},
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
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

/// How long a doorbell may be refused before the supervisor hears about it once.
const ESCALATE_AFTER: Duration = Duration::from_secs(60);

/// How long a newly seen terminal may report `blocked` during startup (e.g. while
/// `agy` loads its token source) before the daemon escalates it.
pub const STARTUP_GRACE: Duration = Duration::from_secs(15);

/// When to ring a seat, from the status its harness reports to herdr-core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ring {
    /// Mid-turn: wait. The doorbell goes out when the turn ends.
    Wait,
    /// `idle`: ring, with herdr-core re-checking `idle` as it types.
    IfIdle,
    /// Turn over but not `idle` (`done`, `error`, no status reported): ring now.
    /// herdr-core still refuses a terminal blocked on a dialog.
    Now,
}

/// `hc send --if` only takes `idle` or `working`, and some harnesses (agy) end a turn as
/// `done` and stay there, so gating on `idle` alone never rang them (#12).
pub fn ring_condition(state: Option<StatusState>) -> Ring {
    match state {
        Some(StatusState::Working) => Ring::Wait,
        Some(StatusState::Idle) => Ring::IfIdle,
        Some(StatusState::Blocked | StatusState::Done | StatusState::Error | StatusState::Other) | None => Ring::Now,
    }
}

/// Per-seat memory of refused rings and startup-grace blocks, kept by the daemon
/// instead of the store so transient blips don't write audit rows or false escalations.
#[derive(Debug, Default)]
pub struct RingBackoff {
    refused_since: BTreeMap<(String, String), Instant>,
    escalated: BTreeSet<(String, String)>,
    /// Terminals already seen `working`, `idle`, or `done` (past startup grace).
    ready_terminals: BTreeSet<String>,
    /// Startup `blocked` notices waiting out `STARTUP_GRACE`, keyed by terminal address.
    pending_blocks: BTreeMap<String, (Instant, String)>,
    /// Tracks `working` seats with frozen screens while unread inbox items wait (#8).
    stall: crate::stall::StallWatch,
}

impl RingBackoff {
    /// True once this seat's rings have been refused for longer than `ESCALATE_AFTER`.
    fn stalled(&mut self, key: &(String, String)) -> bool {
        self.refused_since.entry(key.clone()).or_insert_with(Instant::now).elapsed() >= ESCALATE_AFTER
    }

    /// True the first time it's asked for this seat since the last delivery.
    fn escalate_once(&mut self, key: &(String, String)) -> bool {
        self.escalated.insert(key.clone())
    }

    fn clear(&mut self, key: &(String, String)) {
        self.refused_since.remove(key);
        self.escalated.remove(key);
        self.stall.clear(key);
    }

    fn mark_ready(&mut self, terminal: &str) {
        self.pending_blocks.remove(terminal);
        self.ready_terminals.insert(terminal.to_owned());
    }

    /// True if `terminal` is past startup grace (already seen ready, or first seen
    /// blocked at least `STARTUP_GRACE` ago); otherwise records/keeps its start time.
    fn block_grace_elapsed(&mut self, terminal: &str, detail: String, now: Instant) -> bool {
        if self.ready_terminals.contains(terminal) {
            return true;
        }
        let since = self.pending_blocks.entry(terminal.to_owned()).or_insert((now, detail)).0;
        if now.saturating_duration_since(since) >= STARTUP_GRACE {
            self.pending_blocks.remove(terminal);
            self.ready_terminals.insert(terminal.to_owned());
            true
        } else {
            false
        }
    }

    /// Escalates any deferred startup `blocked` notices whose grace period has elapsed.
    pub fn flush_pending_blocks(&mut self, store: &mut Store, now: Instant) -> usize {
        let due: Vec<String> = self
            .pending_blocks
            .iter()
            .filter(|(_, (since, _))| now.saturating_duration_since(*since) >= STARTUP_GRACE)
            .map(|(terminal, _)| terminal.clone())
            .collect();
        if due.is_empty() {
            return 0;
        }
        let Ok(team_ids) = list_all_team_ids(store) else {
            return 0;
        };
        let actor = Actor::Human {
            terminal: Some("orcherd-daemon".to_owned()),
        };
        let mut escalated = 0usize;
        for terminal in due {
            let Some((_, detail)) = self.pending_blocks.remove(&terminal) else {
                continue;
            };
            self.ready_terminals.insert(terminal.clone());
            for team_id in &team_ids {
                let Ok(proj) = store.team_projection(team_id) else {
                    continue;
                };
                for seat in &proj.seats {
                    if seat.role != SeatRole::Supervisor && seat_matches_terminal(seat, &terminal) {
                        let _ = store.apply(
                            &actor,
                            RequestEnvelope::new(Op::DaemonEscalate {
                                seat: seat.name.clone(),
                                dispatch_id: active_dispatch_for_seat(&proj, &seat.seat_id),
                                stop_reason: "blocked".to_owned(),
                                detail: detail.clone(),
                            })
                            .with_team(team_id),
                        );
                        escalated += 1;
                    }
                }
            }
        }
        escalated
    }
}

fn record_doorbell(store: &mut Store, actor: &Actor, team_id: &str, seat: &str, seq: i64, outcome: DoorbellOutcome) {
    let _ = store.apply(
        actor,
        RequestEnvelope::new(Op::DaemonRecordDoorbell {
            seat: seat.to_owned(),
            rung_through_seq: seq,
            outcome,
        })
        .with_team(team_id),
    );
}

/// Runs one delivery and escalation pass across all teams in `store`, using `core` when available.
pub fn tick_once(
    store: &mut Store,
    core: Option<&Core>,
    backoff: &mut RingBackoff,
) -> Result<usize, String> {
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
                backoff.clear(&(team_id.clone(), seat.seat_id.clone()));
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

            let key = (team_id.clone(), seat.seat_id.clone());
            let record = listed_terminals.as_ref().and_then(|terms| {
                terms
                    .iter()
                    .find(|r| r.address == target_address || r.address == *terminal)
            });

            // Verify via herdr-core that the agent is still in front (SPEC.md §5.1).
            // Escalate once per stall: each escalation settles the dispatch as failed.
            if let Some(record) = record
                && record.foreground.is_some()
                && !orcherd_hc::reply::agent_in_front(record, &seat.agent)
            {
                if backoff.escalate_once(&key)
                    && let Some(dispatch_id) = active_dispatch_for_seat(&proj, &seat.seat_id)
                {
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

            let state = record.and_then(|r| r.status.as_ref()).map(|st| st.state);
            if matches!(state, Some(StatusState::Working | StatusState::Idle | StatusState::Done)) {
                backoff.mark_ready(&target_address);
            }
            let only_if = match ring_condition(state) {
                Ring::Wait => {
                    backoff.stall.check_working(
                        store,
                        seat,
                        active_dispatch_for_seat(&proj, &seat.seat_id),
                        undelivered.len(),
                        Instant::now(),
                        || crate::stall::hash_screen(core, &target_address),
                    );
                    continue;
                }
                Ring::IfIdle => {
                    backoff.stall.clear(&key);
                    Some("idle")
                }
                Ring::Now => {
                    backoff.stall.clear(&key);
                    None
                }
            };
            let send_res = core.send(&SendText {
                force: false,
                only_if,
                terminal: &target_address,
                text: &doorbell_line,
            });

            // Only real deliveries and one-time escalations reach the store (and its audit log).
            // A refused ring retries on the next tick and escalates once after ESCALATE_AFTER.
            match send_res {
                Ok(sent) if sent.started != Some(false) => {
                    let outcome = if sent.started == Some(true) {
                        DoorbellOutcome::Confirmed
                    } else {
                        DoorbellOutcome::Unconfirmed
                    };
                    record_doorbell(store, &actor, &team_id, &seat.name, max_seq, outcome);
                    backoff.clear(&key);
                    backoff.mark_ready(&target_address);
                    deliveries += 1;
                }
                Err(HcError::Refused {
                    kind: Some(ref kind),
                    ref message,
                    ..
                }) if kind == BLOCKED => {
                    // Supervisor dialogs are for the human operator; never self-escalate.
                    // Check startup grace before consuming the one-shot escalation bit,
                    // and escalate via DaemonEscalate alone so one block writes one inbox item.
                    if seat.role != SeatRole::Supervisor {
                        let detail =
                            format!("terminal `{target_address}` is blocked on a dialog: {message}");
                        if backoff.block_grace_elapsed(&target_address, detail.clone(), Instant::now())
                            && backoff.escalate_once(&key)
                        {
                            let _ = store.apply(
                                &actor,
                                RequestEnvelope::new(Op::DaemonEscalate {
                                    seat: seat.name.clone(),
                                    dispatch_id: active_dispatch_for_seat(&proj, &seat.seat_id),
                                    stop_reason: "blocked".to_owned(),
                                    detail,
                                })
                                .with_team(&team_id),
                            );
                        }
                    }
                }
                refused => {
                    let outcome = match refused {
                        Err(HcError::Refused { kind: Some(ref kind), .. }) if kind == STATUS_MISMATCH => {
                            DoorbellOutcome::StatusMismatch
                        }
                        Ok(_) => DoorbellOutcome::StatusMismatch,
                        Err(_) => DoorbellOutcome::Unreachable,
                    };
                    if seat.role != SeatRole::Supervisor
                        && backoff.stalled(&key)
                        && backoff.escalate_once(&key)
                    {
                        // Recording a failed outcome writes one escalation to the supervisor.
                        record_doorbell(store, &actor, &team_id, &seat.name, max_seq, outcome);
                    }
                }
            }
        }
    }

    Ok(deliveries)
}

/// Processes a `herdr-core` event notice and triggers auto-escalations when a
/// managed seat terminal blocks on an auth/permission dialog or exits unexpectedly.
pub fn handle_core_notice(
    store: &mut Store,
    backoff: &mut RingBackoff,
    notice: Notice,
    now: Instant,
) {
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
            if !seat_matches_terminal(seat, &event.terminal) {
                continue;
            }

            match &event.change {
                TerminalChange::Status {
                    status: Some(status),
                    ..
                } if status.state == StatusState::Blocked => {
                    if seat.role == SeatRole::Supervisor {
                        continue;
                    }
                    let kind = status.kind.as_deref().unwrap_or("dialog");
                    let detail = format!("seat `{}` blocked on {kind}: {}", seat.name, status.msg);
                    if backoff.block_grace_elapsed(&event.terminal, detail.clone(), now) {
                        let _ = store.apply(
                            &actor,
                            RequestEnvelope::new(Op::DaemonEscalate {
                                seat: seat.name.clone(),
                                dispatch_id: active_dispatch_for_seat(&proj, &seat.seat_id),
                                stop_reason: "blocked".to_owned(),
                                detail,
                            })
                            .with_team(&team_id),
                        );
                    }
                }
                TerminalChange::Status {
                    status: Some(status),
                    ..
                } if matches!(
                    status.state,
                    StatusState::Working | StatusState::Idle | StatusState::Done
                ) =>
                {
                    backoff.mark_ready(&event.terminal);
                }
                TerminalChange::Status { .. } => {
                    backoff.pending_blocks.remove(&event.terminal);
                }
                TerminalChange::Finished(ended) | TerminalChange::Lost(ended) => {
                    backoff.pending_blocks.remove(&event.terminal);
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
        let _ = tick_once(&mut store, core.as_ref(), &mut RingBackoff::default())?;
        return Ok(());
    }

    #[cfg(unix)]
    let mut listeners = bind_remote_listeners(&state_dir);
    let mut last_data_version = -1i64;
    let mut last_tick = Instant::now() - Duration::from_secs(10);
    let mut backoff = RingBackoff::default();
    let mut binary = crate::self_update::BinaryWatch::start();

    loop {
        if let Some(new_binary) = binary.as_mut().and_then(crate::self_update::BinaryWatch::replaced) {
            // Keeps the pid, so `daemon.lock` stays valid; the new code rebinds the sockets.
            let err = crate::self_update::reexec(&new_binary);
            eprintln!("orcherd daemon: couldn't re-exec `{}`: {err}", new_binary.display());
            binary = None;
        }
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
                            } if matches!(s.state, StatusState::Idle | StatusState::Done)
                        )
                ) {
                    woke_by_event = true;
                }
                handle_core_notice(&mut store, &mut backoff, notice, Instant::now());
            }
        }
        if backoff.flush_pending_blocks(&mut store, Instant::now()) > 0 {
            woke_by_event = true;
        }

        let current_dv = store.data_version().unwrap_or(last_data_version);
        if woke_by_event
            || current_dv != last_data_version
            || last_tick.elapsed() >= Duration::from_secs(2)
        {
            let _ = tick_once(&mut store, core.as_ref(), &mut backoff);
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

fn seat_matches_terminal(seat: &Seat, terminal: &str) -> bool {
    match &seat.address {
        Address::Core {
            machine: Some(m),
            terminal: t,
        } => terminal == *t || terminal == format!("{m}/{t}"),
        Address::Core {
            machine: None,
            terminal: t,
        } => terminal == *t,
        _ => false,
    }
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
