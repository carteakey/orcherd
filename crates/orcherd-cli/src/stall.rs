//! Escalates seats stuck reporting `working` with an unchanged screen while
//! unread inbox items wait (#8). Never types into the seat; only observes a
//! bounded preview and notifies the supervisor once.

use orcherd_core::{Actor, Op, RequestEnvelope, Seat, SeatRole, Store};
use orcherd_hc::Core;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

/// How long a `working` seat with unread inbox items may show an unchanged
/// screen before the daemon escalates it as stalled.
pub const STALL_AFTER: Duration = Duration::from_secs(5 * 60);

/// Minimum gap between `hc read` previews for the same seat so the 2 s daemon
/// tick does not attach to every working terminal on every loop.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(30);

const PREVIEW_LINES: u32 = 40;
const PREVIEW_TIMEOUT: Duration = Duration::from_millis(500);

/// Per-seat screen-hash memory kept in the daemon alongside `RingBackoff`.
#[derive(Debug, Default)]
pub struct StallWatch {
    entries: BTreeMap<(String, String), Entry>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    hash: u64,
    unchanged_since: Instant,
    last_sampled: Instant,
    escalated: bool,
}

impl StallWatch {
    pub fn clear(&mut self, key: &(String, String)) {
        self.entries.remove(key);
    }

    /// Observes a `working` seat at `now`. Returns `true` once when `has_unread`
    /// is true and the screen hash has stayed unchanged for at least `STALL_AFTER`.
    pub fn observe<F>(
        &mut self,
        key: &(String, String),
        has_unread: bool,
        now: Instant,
        mut read_hash: F,
    ) -> bool
    where
        F: FnMut() -> Option<u64>,
    {
        if !has_unread {
            self.entries.remove(key);
            return false;
        }
        match self.entries.get_mut(key) {
            None => {
                let Some(hash) = read_hash() else {
                    return false;
                };
                self.entries.insert(
                    key.clone(),
                    Entry {
                        hash,
                        unchanged_since: now,
                        last_sampled: now,
                        escalated: false,
                    },
                );
                false
            }
            Some(entry) => {
                if now.saturating_duration_since(entry.last_sampled) < SAMPLE_EVERY {
                    return false;
                }
                entry.last_sampled = now;
                let Some(hash) = read_hash() else {
                    return false;
                };
                if hash != entry.hash {
                    entry.hash = hash;
                    entry.unchanged_since = now;
                    entry.escalated = false;
                    return false;
                }
                if !entry.escalated
                    && now.saturating_duration_since(entry.unchanged_since) >= STALL_AFTER
                {
                    entry.escalated = true;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Checks a `working` seat and escalates once with `stop_reason: "stalled"`
    /// if its screen stayed frozen past `STALL_AFTER` while unread items wait.
    pub fn check_working<F>(
        &mut self,
        store: &mut Store,
        seat: &Seat,
        dispatch_id: Option<String>,
        unread_count: usize,
        now: Instant,
        read_hash: F,
    ) -> bool
    where
        F: FnMut() -> Option<u64>,
    {
        let key = (seat.team_id.clone(), seat.seat_id.clone());
        if seat.role == SeatRole::Supervisor {
            self.clear(&key);
            return false;
        }
        if !self.observe(&key, unread_count > 0, now, read_hash) {
            return false;
        }
        let actor = Actor::Human {
            terminal: Some("orcherd-daemon".to_owned()),
        };
        let _ = store.apply(
            &actor,
            RequestEnvelope::new(Op::DaemonEscalate {
                seat: seat.name.clone(),
                dispatch_id,
                stop_reason: "stalled".to_owned(),
                detail: format!(
                    "seat `{}` reported `working` with an unchanged screen for over {} min while {unread_count} unread inbox item(s) wait; inspect with `orcherd seat read {}`",
                    seat.name,
                    STALL_AFTER.as_secs() / 60,
                    seat.name,
                ),
            })
            .with_team(&seat.team_id),
        );
        true
    }
}

/// Hashes a bounded observe-only `herdr-core` preview of `terminal`.
pub fn hash_screen(core: &Core, terminal: &str) -> Option<u64> {
    let preview = core
        .read_preview(terminal, PREVIEW_LINES, PREVIEW_TIMEOUT)
        .ok()?;
    Some(hash_bytes(preview.as_bytes()))
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    // FNV-1a 64-bit: deterministic and dependency-free.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{STALL_AFTER, StallWatch, hash_bytes};
    use orcherd_core::{
        Actor, InboxKind, Op, ReceiptData, RequestEnvelope, Seat, SeatRole, Store, TemplateKind,
    };
    use std::time::{Duration, Instant};

    fn setup_team() -> (Store, String, Seat, String) {
        let mut store = Store::open_in_memory().unwrap();
        let human = Actor::Human { terminal: None };
        let team_id = store
            .apply(
                &human,
                RequestEnvelope::new(Op::Init {
                    name: "crew".to_owned(),
                    template: Some(TemplateKind::ReviewCrew),
                    goal: Some("Stall test".to_owned()),
                    workspace_root: "/tmp/crew".to_owned(),
                    bootstrap_symlinks: Vec::new(),
                    bootstrap_copies: Vec::new(),
                }),
            )
            .unwrap()
            .team_id;
        let ReceiptData::SpawnPrepared {
            seat,
            dispatch: Some(disp),
            ..
        } = store
            .apply(
                &human,
                RequestEnvelope::new(Op::SpawnPrepare {
                    seat: "zcode1".to_owned(),
                    role: Some(SeatRole::Worker),
                    agent: Some("zcode".to_owned()),
                    model: None,
                    account: None,
                    effort: None,
                    worktree_path: None,
                    branch: None,
                    machine: None,
                    task_id: None,
                    spec: Some("Do work".to_owned()),
                    title: Some("Do work".to_owned()),
                    keep_open: None,
                    resume_session: None,
                    retry_of: None,
                })
                .with_team(&team_id),
            )
            .unwrap()
            .data
        else {
            panic!("expected SpawnPrepared with dispatch");
        };
        (store, team_id, *seat, disp.dispatch_id)
    }

    fn supervisor_escalations(store: &Store, team_id: &str) -> Vec<orcherd_core::InboxItem> {
        store
            .list_undelivered_inbox(team_id, "super")
            .unwrap()
            .into_iter()
            .filter(|i| i.kind == InboxKind::Escalation)
            .collect()
    }

    #[test]
    fn unchanged_screen_past_threshold_escalates_exactly_once() {
        let (mut store, team_id, seat, disp_id) = setup_team();
        let mut watch = StallWatch::default();
        let t0 = Instant::now();
        let frozen = hash_bytes(b"zcode stuck frame");

        assert!(!watch.check_working(
            &mut store, &seat, Some(disp_id.clone()), 1, t0,
            || Some(frozen),
        ));
        assert!(!watch.check_working(
            &mut store, &seat, Some(disp_id.clone()), 1,
            t0 + STALL_AFTER - Duration::from_secs(30),
            || Some(frozen),
        ));
        assert!(supervisor_escalations(&store, &team_id).is_empty());

        // Past threshold: escalates once with stop_reason = stalled.
        assert!(watch.check_working(
            &mut store, &seat, Some(disp_id.clone()), 1,
            t0 + STALL_AFTER,
            || Some(frozen),
        ));
        // Later ticks with the same frozen screen do not escalate again.
        assert!(!watch.check_working(
            &mut store, &seat, Some(disp_id.clone()), 1,
            t0 + STALL_AFTER + Duration::from_secs(120),
            || Some(frozen),
        ));

        let items = supervisor_escalations(&store, &team_id);
        assert_eq!(items.len(), 1, "expected exactly one stall escalation: {items:?}");
        assert_eq!(items[0].ref_id, disp_id);
        assert!(items[0].body.contains("unchanged screen"));
        let proj = store.team_projection(&team_id).unwrap();
        let disp = proj.active_dispatches.iter().find(|d| d.dispatch_id == disp_id).unwrap();
        assert_eq!(disp.stop_reason.as_deref(), Some("stalled"));
    }

    #[test]
    fn changing_screen_resets_clock_and_never_escalates() {
        let (mut store, team_id, seat, disp_id) = setup_team();
        let mut watch = StallWatch::default();
        let t0 = Instant::now();

        for step in 0..5u64 {
            let hash = hash_bytes(format!("frame {step}").as_bytes());
            assert!(!watch.check_working(
                &mut store, &seat, Some(disp_id.clone()), 1,
                t0 + Duration::from_secs(step * 180),
                || Some(hash),
            ));
        }
        assert!(supervisor_escalations(&store, &team_id).is_empty());
    }

    #[test]
    fn seat_with_no_unread_items_is_never_flagged() {
        let (mut store, team_id, seat, disp_id) = setup_team();
        let mut watch = StallWatch::default();
        let t0 = Instant::now();
        let mut samples = 0usize;

        for offset in [0, 60, STALL_AFTER.as_secs(), STALL_AFTER.as_secs() + 120] {
            assert!(!watch.check_working(
                &mut store, &seat, Some(disp_id.clone()), 0,
                t0 + Duration::from_secs(offset),
                || {
                    samples += 1;
                    Some(42)
                },
            ));
        }
        assert_eq!(samples, 0, "idle-inbox seats must not even be preview-sampled");
        assert!(supervisor_escalations(&store, &team_id).is_empty());
    }
}
