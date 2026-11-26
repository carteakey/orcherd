//! Shows spawned seats in the Herdr desktop app (#9).
//!
//! The app adopts an `hc` terminal only when it carries `herdr.space=<id>`; without
//! `herdr.tab` it opens the terminal as a new tab in that space. A seat inherits the
//! space of whoever spawned it, else of the team's supervisor, so seats appear next to
//! the agent directing them.

use crate::identity::current_terminal_env;
use orcherd_core::{Address, SeatRole, Store};
use orcherd_hc::Core;

pub const SPACE_LABEL: &str = "herdr.space";

/// The `herdr.space` for a new seat, or `None` to leave it headless.
///
/// Space ids are local to a machine and the app only adopts remote terminals in spaces
/// it started there, so seats placed `--on` another machine stay headless.
pub fn inherited_space(core: &Core, store: &Store, team_id: &str, machine: Option<&str>) -> Option<String> {
    if machine.is_some() {
        return None;
    }
    let supervisor = store.team_projection(team_id).ok().and_then(|proj| {
        proj.seats.into_iter().find_map(|s| match (s.role, s.address) {
            (SeatRole::Supervisor, Address::Core { machine: None, terminal }) => Some(terminal),
            _ => None,
        })
    });
    let candidates: Vec<String> = current_terminal_env().into_iter().chain(supervisor).collect();
    let terminals = core.list(false).ok()?.terminals;
    candidates.iter().find_map(|addr| {
        terminals
            .iter()
            .find(|t| t.address == *addr)
            .and_then(|t| t.labels.get(SPACE_LABEL).cloned())
    })
}
