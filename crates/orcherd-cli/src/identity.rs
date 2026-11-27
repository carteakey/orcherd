//! State path and caller identity resolution (`SPEC.md` §6.1).

use crate::worktree;
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

/// Resolves the target team reference for every CLI command in fixed precedence order:
/// 1. `--team`
/// 2. `$ORCHERD_TEAM`
/// 3. `[team] name` in `.orcherd.toml`, walking up from `cwd` to the git root
/// 4. Deepest `workspace_root` in `store` containing `cwd` (newest team on a tie)
///
/// Returns `None` so `Store::resolve_team(None)` falls back to the newest team in the store.
pub fn resolve_team_ref(explicit: Option<&str>, store: Option<&Store>) -> Result<Option<String>, String> {
    let env_team = env::var("ORCHERD_TEAM").ok();
    let cwd = env::current_dir().ok();
    resolve_team_ref_in(explicit, env_team.as_deref(), cwd.as_deref(), store)
}

pub(crate) fn resolve_team_ref_in(
    explicit: Option<&str>,
    env_team: Option<&str>,
    cwd: Option<&Path>,
    store: Option<&Store>,
) -> Result<Option<String>, String> {
    if let Some(t) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(Some(t.to_owned()));
    }
    if let Some(t) = env_team.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(Some(t.to_owned()));
    }
    if let Some(dir) = cwd {
        if let Some(name) = team_name_from_project_config(dir)? {
            return Ok(Some(name));
        }
        if let Some(store) = store
            && let Ok(Some(team_id)) = store.team_for_path(dir)
        {
            return Ok(Some(team_id));
        }
    }
    Ok(None)
}

/// Walks up from `cwd` to the enclosing git root (inclusive) looking for `.orcherd.toml`'s `[team] name`.
fn team_name_from_project_config(cwd: &Path) -> Result<Option<String>, String> {
    for dir in cwd.ancestors() {
        if dir.join(".orcherd.toml").is_file() {
            let cfg = worktree::load_project_config(dir)?;
            if let Some(name) = cfg
                .team
                .name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Ok(Some(name.to_owned()));
            }
        }
        // Stop at the repo or worktree root so a parent directory's config cannot leak in.
        if dir.join(".git").exists() {
            break;
        }
    }
    Ok(None)
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
