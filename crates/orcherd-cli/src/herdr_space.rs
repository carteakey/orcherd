//! Shows spawned seats in the Herdr desktop app (#9), one space per team.
//!
//! The app adopts an `hc` terminal only when it carries `herdr.space=<id>`; without
//! `herdr.tab` it opens the terminal as a new tab in that space, and a space id it doesn't
//! know becomes a new space. Giving every seat of a team the same id puts the team in its
//! own space, so Herdr's Agents list, grouped by space, groups agents by team. The orcherd
//! board mod names each such space after its team.

pub const SPACE_LABEL: &str = "herdr.space";

/// Prefix of the space ids orcherd assigns; the board mod renames these spaces.
pub const TEAM_SPACE_PREFIX: &str = "orcherd-";

/// The `herdr.space` for a new seat of `team_id`, or `None` to leave it headless.
///
/// Space ids are local to a machine and the app only adopts remote terminals in spaces
/// it started there, so seats placed `--on` another machine stay headless.
pub fn team_space(team_id: &str, machine: Option<&str>) -> Option<String> {
    machine.is_none().then(|| format!("{TEAM_SPACE_PREFIX}{team_id}"))
}

#[cfg(test)]
mod tests {
    use super::team_space;

    #[test]
    fn local_seats_share_their_team_space_and_remote_seats_stay_headless() {
        assert_eq!(team_space("team-c6cbab4f0a2f", None).as_deref(), Some("orcherd-team-c6cbab4f0a2f"));
        assert_eq!(team_space("team-c6cbab4f0a2f", Some("m1")), None);
    }
}
