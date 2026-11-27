//! Replay and live-binding guards for `orcherd spawn`.

use orcherd_core::Store;

/// Check whether `seat_name` currently holds a live terminal binding in `store`.
pub(crate) fn seat_is_bound(store: &Store, team_id: &str, seat_name: &str) -> bool {
    store
        .team_projection(team_id)
        .ok()
        .and_then(|proj| proj.seats.into_iter().find(|s| s.name == seat_name))
        .is_some_and(|s| s.address.is_bound())
}

/// Decide whether `orcherd spawn` should cold-launch a new terminal.
/// Skips launch on `--no-launch`, on replayed `SpawnPrepare` receipts, on warm reuse,
/// or when the seat is already bound to a live terminal without `--resume`.
pub(crate) fn should_cold_launch(
    replayed: bool,
    warm_reuse: bool,
    seat_bound_now: bool,
    has_resume_session: bool,
    no_launch: bool,
) -> bool {
    !no_launch && !replayed && !warm_reuse && !(seat_bound_now && !has_resume_session)
}
