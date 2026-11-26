//! State path and caller identity resolution (`SPEC.md` §6.1).

use orcherd_core::{Actor, Address, Store};
use std::{
    env,
    path::{Path, PathBuf},
};

pub fn resolve_state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("ORCHERD_STATE_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("orcherd");
    }
    #[cfg(windows)]
    if let Some(local) = env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        return PathBuf::from(local).join("orcherd");
    }
    if let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join(".local").join("state").join("orcherd");
    }
    PathBuf::from(".orcherd-state")
}

pub fn resolve_db_path(state_dir: &Path) -> PathBuf {
    if let Some(path) = env::var_os("ORCHERD_DB_PATH").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    state_dir.join("store.sqlite3")
}

pub fn resolve_team_arg(explicit: Option<&str>) -> Option<String> {
    if let Some(t) = explicit.filter(|s| !s.trim().is_empty()) {
        return Some(t.trim().to_owned());
    }
    env::var("ORCHERD_TEAM")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

pub fn current_terminal_env() -> Option<String> {
    let machine = env::var("ORCHERD_MACHINE")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if let Ok(hc) = env::var("HC_TERMINAL_ID") {
        let trimmed = hc.trim();
        if !trimmed.is_empty() {
            return Some(match machine {
                Some(m) if !trimmed.contains('/') => format!("{m}/{trimmed}"),
                _ => trimmed.to_owned(),
            });
        }
    }
    env::var("HERDR_PANE_ID")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Resolves the caller's `Actor` following `SPEC.md` §6.1:
/// 1. Explicit `--as <seat>`
/// 2. `$ORCHERD_SEAT`
/// 3. Matching `address_terminal` in the resolved team for `$HC_TERMINAL_ID` / `$HERDR_PANE_ID`
/// 4. `Actor::Human`
pub fn resolve_actor(
    store: Option<&Store>,
    team_ref: Option<&str>,
    explicit_as: Option<&str>,
) -> Actor {
    let terminal = current_terminal_env();
    let team_env = resolve_team_arg(team_ref);

    if let Some(seat) = explicit_as.map(str::trim).filter(|s| !s.is_empty()) {
        return Actor::Seat {
            team_id: team_env,
            seat_name: seat.to_owned(),
            terminal,
        };
    }

    if let Ok(seat) = env::var("ORCHERD_SEAT") {
        let trimmed = seat.trim();
        if !trimmed.is_empty() {
            return Actor::Seat {
                team_id: team_env,
                seat_name: trimmed.to_owned(),
                terminal,
            };
        }
    }

    if let (Some(store), Some(term)) = (store, terminal.as_deref())
        && let Ok(team) = store.resolve_team(team_env.as_deref())
        && let Ok(proj) = store.team_projection(&team.team_id)
    {
        for seat in proj.seats {
            let matches = match &seat.address {
                Address::Core {
                    machine: Some(m),
                    terminal: t,
                } => term == t || term == format!("{m}/{t}"),
                Address::Core {
                    machine: None,
                    terminal: t,
                } => term == t,
                Address::Herdr1Pane { pane } => term == pane,
                Address::None => false,
            };
            if matches {
                return Actor::Seat {
                    team_id: Some(team.team_id),
                    seat_name: seat.name,
                    terminal: Some(term.to_owned()),
                };
            }
        }
    }

    Actor::Human { terminal }
}
