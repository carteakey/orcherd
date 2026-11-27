//! Supervises recorded reverse SSH tunnels (`<state_dir>/tunnels/*.json`) from
//! `orcherd daemon`: restarts dropped tunnels with exponential backoff and
//! escalates once to supervisors of affected teams after repeated failures.

use crate::{
    daemon::{list_all_team_ids, process_is_alive},
    tunnel::{self, TunnelRecord},
};
use orcherd_core::{Actor, Address, Op, RequestEnvelope, Seat, SeatRole, Store};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Child,
    time::{Duration, Instant},
};

/// How often the daemon checks recorded SSH tunnels for liveness.
pub const CHECK_EVERY: Duration = Duration::from_secs(30);

/// Initial backoff after a failed tunnel restart (doubles per failure).
pub const INITIAL_BACKOFF: Duration = Duration::from_secs(30);

/// Maximum backoff between restart attempts for an unreachable machine.
pub const MAX_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// Consecutive failed restarts before escalating to supervisors of teams on that machine.
pub const ESCALATE_AFTER_FAILURES: u32 = 3;

/// Exponential backoff delay for the `failures`-th consecutive failure (1-indexed):
/// 30 s, 60 s, 120 s, 240 s, 480 s, capped at 10 min.
pub fn backoff_delay(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(5);
    INITIAL_BACKOFF
        .saturating_mul(1u32 << shift)
        .min(MAX_BACKOFF)
}

/// Per-machine tunnel restart and escalation state kept in memory by the daemon.
#[derive(Debug, Default)]
pub struct TunnelWatch {
    last_check: Option<Instant>,
    machines: BTreeMap<String, MachineState>,
    /// Daemon-spawned SSH children kept so `try_wait()` reaps them instead of
    /// leaving zombies (`Z`) that fool `process_is_alive`.
    children: BTreeMap<String, Child>,
}

#[derive(Debug, Default)]
struct MachineState {
    failures: u32,
    next_retry_at: Option<Instant>,
    escalated: bool,
}

impl TunnelWatch {
    /// Reaps any daemon-spawned SSH children that have already exited.
    fn reap_children(&mut self) {
        self.children
            .retain(|_, child| matches!(child.try_wait(), Ok(None)));
    }

    /// Checks recorded tunnels using real process checks and SSH restarts.
    /// Returns the number of escalations written (so the daemon can ring supervisors).
    pub fn tick(&mut self, store: &mut Store, state_dir: &Path) -> usize {
        self.tick_with(store, state_dir, Instant::now(), process_is_alive, |rec| {
            tunnel::spawn_background_tunnel_child(
                state_dir,
                &rec.machine,
                rec.remote_socket_override.as_deref(),
                &rec.ssh_args,
            )
            .and_then(|(st, child)| {
                let pid = st
                    .pid
                    .ok_or_else(|| "spawned tunnel had no pid".to_owned())?;
                Ok((pid, Some(child)))
            })
        })
    }

    /// Checks every recorded tunnel in `<state_dir>/tunnels/*.json` at most once
    /// every `CHECK_EVERY`. Clock, liveness, and restart are injected for tests.
    pub fn tick_with<A, S>(
        &mut self,
        store: &mut Store,
        state_dir: &Path,
        now: Instant,
        mut is_alive: A,
        mut start: S,
    ) -> usize
    where
        A: FnMut(u32) -> bool,
        S: FnMut(&TunnelRecord) -> Result<(u32, Option<Child>), String>,
    {
        if let Some(last) = self.last_check
            && now.saturating_duration_since(last) < CHECK_EVERY
        {
            return 0;
        }
        self.last_check = Some(now);
        self.reap_children();

        let records = tunnel::list_tunnel_records(state_dir);
        let active: BTreeSet<String> = records.iter().map(|r| r.machine.clone()).collect();
        self.machines.retain(|m, _| active.contains(m));

        let mut escalations = 0usize;
        for mut record in records {
            if record.pid.is_some_and(&mut is_alive) {
                // Live (or recovered externally): clear backoff and escalation state.
                self.machines.remove(&record.machine);
                continue;
            }

            let state = self.machines.entry(record.machine.clone()).or_default();
            if state.next_retry_at.is_some_and(|at| now < at) {
                continue;
            }

            match start(&record) {
                Ok((new_pid, child)) => {
                    if let Some(child) = child {
                        self.children.insert(record.machine.clone(), child);
                    }
                    record.pid = Some(new_pid);
                    let _ = tunnel::write_tunnel_record(state_dir, &record);
                    self.machines.remove(&record.machine);
                }
                Err(err) => {
                    state.failures = state.failures.saturating_add(1);
                    state.next_retry_at = Some(now + backoff_delay(state.failures));
                    if state.failures >= ESCALATE_AFTER_FAILURES && !state.escalated {
                        state.escalated = true;
                        escalations +=
                            escalate_tunnel_down(store, &record.machine, state.failures, &err);
                    }
                }
            }
        }
        escalations
    }
}

fn seat_machine(seat: &Seat) -> Option<&str> {
    match &seat.address {
        Address::Core {
            machine: Some(m), ..
        } => Some(m.as_str()),
        Address::Core {
            machine: None,
            terminal,
        } => terminal.split_once('/').map(|(m, _)| m),
        _ => None,
    }
}

fn escalate_tunnel_down(store: &mut Store, machine: &str, failures: u32, last_err: &str) -> usize {
    let Ok(team_ids) = list_all_team_ids(store) else {
        return 0;
    };
    let actor = Actor::Human {
        terminal: Some("orcherd-daemon".to_owned()),
    };
    let detail = format!(
        "SSH tunnel to `{machine}` failed to restart {failures} time(s) ({last_err}). \
         Remote seats on `{machine}` cannot reach the store until connectivity returns or \
         `orcherd tunnel {machine}` succeeds."
    );
    let mut escalated = 0usize;
    for team_id in team_ids {
        let Ok(proj) = store.team_projection(&team_id) else {
            continue;
        };
        let Some(seat) = proj
            .seats
            .iter()
            .find(|s| s.role != SeatRole::Supervisor && seat_machine(s) == Some(machine))
        else {
            continue;
        };
        if store
            .apply(
                &actor,
                RequestEnvelope::new(Op::DaemonEscalate {
                    seat: seat.name.clone(),
                    dispatch_id: None,
                    stop_reason: "tunnel_down".to_owned(),
                    detail: detail.clone(),
                })
                .with_team(&team_id),
            )
            .is_ok()
        {
            escalated += 1;
        }
    }
    escalated
}

#[cfg(test)]
mod tests {
    use super::{CHECK_EVERY, MAX_BACKOFF, TunnelWatch, backoff_delay};
    use crate::tunnel::{self, TunnelRecord};
    use orcherd_core::{
        Actor, Address, InboxKind, Op, ReceiptData, RequestEnvelope, SeatRole, Store, TemplateKind,
    };
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn setup_store_with_m2_seat() -> (TempDir, Store, String) {
        let dir = TempDir::new().unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let human = Actor::Human { terminal: None };
        let team_id = store
            .apply(
                &human,
                RequestEnvelope::new(Op::Init {
                    name: "fleet".to_owned(),
                    template: Some(TemplateKind::Small),
                    goal: Some("Remote tunnel test".to_owned()),
                    workspace_root: dir.path().display().to_string(),
                    bootstrap_symlinks: Vec::new(),
                    bootstrap_copies: Vec::new(),
                }),
            )
            .unwrap()
            .team_id;
        // Bind two seats on m2 to verify one escalation per team/machine.
        for (seat, term) in [("w1", "t7"), ("w2", "t8")] {
            store
                .apply(
                    &human,
                    RequestEnvelope::new(Op::SeatRebind {
                        seat: seat.to_owned(),
                        address: Address::Core {
                            machine: Some("m2".to_owned()),
                            terminal: term.to_owned(),
                        },
                    })
                    .with_team(&team_id),
                )
                .unwrap();
        }
        (dir, store, team_id)
    }

    fn read_super_escalations(store: &mut Store, team_id: &str) -> Vec<orcherd_core::InboxItem> {
        let sup = Actor::Seat {
            team_id: Some(team_id.to_owned()),
            seat_name: "super".to_owned(),
            terminal: None,
        };
        let ReceiptData::Inbox(items) = store
            .apply(
                &sup,
                RequestEnvelope::new(Op::InboxRead {
                    peek: false,
                    seat: None,
                    limit: None,
                })
                .with_team(team_id),
            )
            .unwrap()
            .data
        else {
            panic!("expected inbox");
        };
        items
            .into_iter()
            .filter(|i| i.kind == InboxKind::Escalation)
            .collect()
    }

    #[test]
    fn dead_tunnel_restarts_and_stopped_record_stays_stopped() {
        let (dir, mut store, _team_id) = setup_store_with_m2_seat();
        tunnel::write_tunnel_record(
            dir.path(),
            &TunnelRecord {
                machine: "m2".to_owned(),
                remote_socket_override: Some("/remote/control.sock".to_owned()),
                ssh_args: vec!["-p".to_owned(), "2222".to_owned()],
                pid: Some(100),
            },
        )
        .unwrap();

        let mut watch = TunnelWatch::default();
        let t0 = Instant::now();
        let mut starts = 0usize;

        // Dead pid 100 triggers a restart, preserving custom ssh_args and remote_socket_override.
        watch.tick_with(
            &mut store,
            dir.path(),
            t0,
            |pid| pid == 200,
            |rec| {
                starts += 1;
                assert_eq!(rec.remote_socket_override.as_deref(), Some("/remote/control.sock"));
                assert_eq!(rec.ssh_args, ["-p", "2222"]);
                Ok((200, None))
            },
        );
        assert_eq!(starts, 1);
        assert_eq!(
            tunnel::read_tunnel_record(dir.path(), "m2").unwrap().unwrap().pid,
            Some(200)
        );

        // Next check sees live pid 200 and does not restart.
        watch.tick_with(&mut store, dir.path(), t0 + CHECK_EVERY, |pid| pid == 200, |_| {
            starts += 1;
            Ok((201, None))
        });
        assert_eq!(starts, 1);

        // Removing the record via stop_tunnel ensures it is never restarted again.
        tunnel::stop_tunnel(dir.path(), "m2").unwrap();
        assert!(tunnel::read_tunnel_record(dir.path(), "m2").unwrap().is_none());
        watch.tick_with(&mut store, dir.path(), t0 + CHECK_EVERY * 2, |_| false, |_| {
            starts += 1;
            Ok((202, None))
        });
        assert_eq!(starts, 1, "a stopped tunnel must never be restarted");
    }

    #[test]
    fn failed_restarts_back_off_escalate_once_and_clear_on_recovery() {
        assert_eq!(backoff_delay(1), Duration::from_secs(30));
        assert_eq!(backoff_delay(2), Duration::from_secs(60));
        assert_eq!(backoff_delay(3), Duration::from_secs(120));
        assert_eq!(backoff_delay(6), MAX_BACKOFF);

        let (dir, mut store, team_id) = setup_store_with_m2_seat();
        // Add a second team with no seats on m2; it must not receive an escalation.
        let human = Actor::Human { terminal: None };
        let other_team = store
            .apply(
                &human,
                RequestEnvelope::new(Op::Init {
                    name: "local-only".to_owned(),
                    template: Some(TemplateKind::Small),
                    goal: None,
                    workspace_root: "/tmp/local-only".to_owned(),
                    bootstrap_symlinks: Vec::new(),
                    bootstrap_copies: Vec::new(),
                }),
            )
            .unwrap()
            .team_id;

        tunnel::write_tunnel_record(
            dir.path(),
            &TunnelRecord {
                machine: "m2".to_owned(),
                remote_socket_override: None,
                ssh_args: Vec::new(),
                pid: Some(100),
            },
        )
        .unwrap();

        let mut watch = TunnelWatch::default();
        let t0 = Instant::now();
        let mut attempts = 0usize;
        let fail = |attempts: &mut usize| {
            *attempts += 1;
            Err::<(u32, Option<std::process::Child>), String>(
                "ssh: Connection timed out".to_owned(),
            )
        };

        // Failure 1 at t=0 -> next retry at t+30s.
        assert_eq!(watch.tick_with(&mut store, dir.path(), t0, |_| false, |_| fail(&mut attempts)), 0);
        assert_eq!(attempts, 1);

        // Failure 2 at t+30s -> next retry at t+90s (+60s backoff).
        assert_eq!(
            watch.tick_with(&mut store, dir.path(), t0 + Duration::from_secs(30), |_| false, |_| fail(&mut attempts)),
            0
        );
        assert_eq!(attempts, 2);

        // At t+60s: still inside 60s backoff, so no retry happens.
        assert_eq!(
            watch.tick_with(&mut store, dir.path(), t0 + Duration::from_secs(60), |_| false, |_| fail(&mut attempts)),
            0
        );
        assert_eq!(attempts, 2);

        // Failure 3 at t+90s -> escalates once to fleet's supervisor, next retry at t+210s (+120s).
        assert_eq!(
            watch.tick_with(&mut store, dir.path(), t0 + Duration::from_secs(90), |_| false, |_| fail(&mut attempts)),
            1
        );
        assert_eq!(attempts, 3);
        let items = read_super_escalations(&mut store, &team_id);
        assert_eq!(items.len(), 1, "expected one escalation for m2: {items:?}");
        assert_eq!(items[0].ref_id, "tunnel:m2");
        assert!(items[0].subject.contains("m2"));
        assert!(read_super_escalations(&mut store, &other_team).is_empty());

        // Failure 4 at t+210s -> does not escalate again while still down.
        assert_eq!(
            watch.tick_with(&mut store, dir.path(), t0 + Duration::from_secs(210), |_| false, |_| fail(&mut attempts)),
            0
        );
        assert_eq!(attempts, 4);
        assert!(read_super_escalations(&mut store, &team_id).is_empty());

        // Recovery at t+450s clears backoff and escalation state.
        assert_eq!(
            watch.tick_with(&mut store, dir.path(), t0 + Duration::from_secs(450), |_| false, |_| Ok((300, None))),
            0
        );

        // A later drop starts fresh at 30s backoff and can escalate again after 3 failures.
        let mut later_attempts = 0usize;
        assert_eq!(
            watch.tick_with(&mut store, dir.path(), t0 + Duration::from_secs(480), |_| false, |_| fail(&mut later_attempts)),
            0
        );
        assert_eq!(
            watch.tick_with(&mut store, dir.path(), t0 + Duration::from_secs(510), |_| false, |_| fail(&mut later_attempts)),
            0
        );
        assert_eq!(
            watch.tick_with(&mut store, dir.path(), t0 + Duration::from_secs(570), |_| false, |_| fail(&mut later_attempts)),
            1
        );
        assert_eq!(later_attempts, 3);
        assert_eq!(read_super_escalations(&mut store, &team_id).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn reaps_exited_daemon_child_so_second_drop_restarts() {
        let (dir, mut store, _team_id) = setup_store_with_m2_seat();
        tunnel::write_tunnel_record(
            dir.path(),
            &TunnelRecord {
                machine: "m2".to_owned(),
                remote_socket_override: None,
                ssh_args: Vec::new(),
                pid: None,
            },
        )
        .unwrap();

        let mut watch = TunnelWatch::default();
        let t0 = Instant::now();
        let mut starts = 0usize;

        // First restart spawns a real child that exits immediately; without reaping,
        // it stays a zombie and `process_is_alive(pid)` would keep returning true forever.
        watch.tick_with(
            &mut store,
            dir.path(),
            t0,
            crate::daemon::process_is_alive,
            |_| {
                starts += 1;
                let child = orcherd_hc::new_command("sh")
                    .args(["-c", "exit 0"])
                    .spawn()
                    .unwrap();
                Ok((child.id(), Some(child)))
            },
        );
        assert_eq!(starts, 1);
        let first_pid = tunnel::read_tunnel_record(dir.path(), "m2")
            .unwrap()
            .unwrap()
            .pid
            .unwrap();

        // Wait until the first child has exited (becoming a zombie on Linux until reaped).
        if std::path::Path::new("/proc/self").exists() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok(stat) = std::fs::read_to_string(format!("/proc/{first_pid}/stat"))
                    && stat.contains(") Z ")
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }

        // Next check must reap the exited child so process_is_alive sees it as dead and restarts.
        watch.tick_with(
            &mut store,
            dir.path(),
            t0 + CHECK_EVERY,
            crate::daemon::process_is_alive,
            |_| {
                starts += 1;
                Ok((999_999, None))
            },
        );
        assert_eq!(starts, 2, "exited daemon child must be reaped and restarted");
        assert!(!crate::daemon::process_is_alive(first_pid));
        assert_eq!(
            tunnel::read_tunnel_record(dir.path(), "m2").unwrap().unwrap().pid,
            Some(999_999)
        );
    }

    #[test]
    fn non_worker_supervisor_seat_on_remote_never_self_escalates() {
        let dir = TempDir::new().unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let human = Actor::Human { terminal: None };
        let team_id = store
            .apply(
                &human,
                RequestEnvelope::new(Op::Init {
                    name: "solo".to_owned(),
                    template: Some(TemplateKind::Small),
                    goal: None,
                    workspace_root: dir.path().display().to_string(),
                    bootstrap_symlinks: Vec::new(),
                    bootstrap_copies: Vec::new(),
                }),
            )
            .unwrap()
            .team_id;
        store
            .apply(
                &human,
                RequestEnvelope::new(Op::SeatRebind {
                    seat: "super".to_owned(),
                    address: Address::Core {
                        machine: Some("m2".to_owned()),
                        terminal: "t1".to_owned(),
                    },
                })
                .with_team(&team_id),
            )
            .unwrap();
        assert_eq!(
            store.team_projection(&team_id).unwrap().seats[0].role,
            SeatRole::Supervisor
        );
        tunnel::write_tunnel_record(
            dir.path(),
            &TunnelRecord {
                machine: "m2".to_owned(),
                remote_socket_override: None,
                ssh_args: Vec::new(),
                pid: Some(100),
            },
        )
        .unwrap();
        let mut watch = TunnelWatch::default();
        let t0 = Instant::now();
        for secs in [0, 30, 90] {
            let _ = watch.tick_with(
                &mut store,
                dir.path(),
                t0 + Duration::from_secs(secs),
                |_| false,
                |_| Err("down".to_owned()),
            );
        }
        assert!(read_super_escalations(&mut store, &team_id).is_empty());
    }
}
