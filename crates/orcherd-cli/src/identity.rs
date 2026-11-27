//! State path and caller identity resolution (`SPEC.md` §6.1).

use crate::{tunnel, worktree};
use orcherd_core::{Actor, Address, Store};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

/// Upper bound on sibling `orcherd*` state directories scanned on a `team not found` error.
const MAX_SIBLING_STORES: usize = 10;

/// Parent directory holding default and sibling `orcherd*` state directories
/// (`$XDG_STATE_HOME`, `%LOCALAPPDATA%`, or `~/.local/state`).
pub fn state_base_dir() -> Option<PathBuf> {
    if let Some(xdg) = env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg));
    }
    #[cfg(windows)]
    if let Some(local) = env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(local));
    }
    env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(|home| PathBuf::from(home).join(".local").join("state"))
}

/// Default store directory when `ORCHERD_STATE_DIR` is not overridden.
pub fn default_state_dir() -> PathBuf {
    state_base_dir()
        .map(|base| base.join("orcherd"))
        .unwrap_or_else(|| PathBuf::from(".orcherd-state"))
}

pub fn resolve_state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("ORCHERD_STATE_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    default_state_dir()
}

pub fn resolve_db_path(state_dir: &Path) -> PathBuf {
    if let Some(path) = env::var_os("ORCHERD_DB_PATH").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    state_dir.join("store.sqlite3")
}

/// Builds the environment exported to a spawned seat terminal.
///
/// Local seats inherit a non-default `ORCHERD_STATE_DIR` (resolved to an absolute path so
/// child worktrees hit the same store); remote seats keep the SSH tunnel socket route.
pub(crate) fn build_seat_env(
    exe: &str,
    team_id: &str,
    seat_name: &str,
    dispatch_id: Option<&str>,
    machine: Option<&str>,
    state_dir: &Path,
    default_dir: &Path,
) -> BTreeMap<&'static str, String> {
    let mut env_map = BTreeMap::new();
    env_map.insert("ORCHERD_CLI_COMMAND", exe.to_owned());
    env_map.insert("ORCHERD_TEAM", team_id.to_owned());
    env_map.insert("ORCHERD_SEAT", seat_name.to_owned());
    if let Some(d_id) = dispatch_id {
        env_map.insert("ORCHERD_DISPATCH", d_id.to_owned());
    }
    if let Some(m) = machine {
        env_map.insert("ORCHERD_MACHINE", m.to_owned());
        env_map.insert(
            "ORCHERD_REMOTE_SOCKET",
            tunnel::default_remote_socket_expr().to_owned(),
        );
    } else if state_dir != default_dir {
        let abs = if state_dir.is_relative() {
            env::current_dir()
                .map(|cwd| cwd.join(state_dir))
                .unwrap_or_else(|_| state_dir.to_path_buf())
        } else {
            state_dir.to_path_buf()
        };
        env_map.insert("ORCHERD_STATE_DIR", abs.display().to_string());
    }
    env_map
}

/// Scans up to `MAX_SIBLING_STORES` sibling `orcherd*` state directories (read-only, no
/// migrations) for a team matching `team_ref`, returning the first directory that holds it.
pub(crate) fn find_team_in_sibling_stores(
    team_ref: &str,
    skip_dir: Option<&Path>,
) -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Some(base) = state_base_dir() {
        roots.push(base);
    }
    if let Some(parent) = skip_dir.and_then(Path::parent)
        && !parent.as_os_str().is_empty()
        && !roots.iter().any(|r| r == parent)
    {
        roots.push(parent.to_path_buf());
    }
    find_team_in_siblings_under(team_ref, &roots, skip_dir)
}

pub(crate) fn find_team_in_siblings_under(
    team_ref: &str,
    roots: &[PathBuf],
    skip_dir: Option<&Path>,
) -> Option<PathBuf> {
    let query = team_ref.trim();
    if query.is_empty() || query == "<default>" {
        return None;
    }
    let skip_canon = skip_dir.and_then(|d| fs::canonicalize(d).ok());
    let mut scanned = 0usize;
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("orcherd"))
                    && p.join("store.sqlite3").is_file()
            })
            .collect();
        dirs.sort();
        for dir in dirs {
            if skip_dir.is_some_and(|s| s == dir) {
                continue;
            }
            if let Some(ref sc) = skip_canon
                && fs::canonicalize(&dir).ok().as_ref() == Some(sc)
            {
                continue;
            }
            if scanned >= MAX_SIBLING_STORES {
                return None;
            }
            scanned += 1;
            let Ok(store) = Store::open_read_only(dir.join("store.sqlite3")) else {
                continue;
            };
            if store.resolve_team(Some(query)).is_ok() {
                return Some(dir);
            }
        }
    }
    None
}

/// If `err` is a local `team not found: <ref>` error and a sibling `orcherd*` store holds
/// that team, appends a hint pointing directly at the store and `ORCHERD_STATE_DIR=...`.
pub(crate) fn annotate_team_not_found(err: String, current_state_dir: &Path) -> String {
    let Some(team_ref) = err.strip_prefix("team not found: ") else {
        return err;
    };
    let Some(found_dir) = find_team_in_sibling_stores(team_ref, Some(current_state_dir)) else {
        return err;
    };
    let dir = found_dir.display();
    format!("{err}; {team_ref} is in {dir}; run with ORCHERD_STATE_DIR={dir}")
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

#[cfg(test)]
mod tests {
    use super::*;
    use orcherd_core::{Op, RequestEnvelope, TemplateKind};
    use tempfile::TempDir;

    fn seed_team(dir: &Path, name: &str) -> String {
        let mut store = Store::open(dir.join("store.sqlite3")).unwrap();
        let human = Actor::Human { terminal: None };
        store
            .apply(
                &human,
                RequestEnvelope::new(Op::Init {
                    name: name.to_owned(),
                    template: Some(TemplateKind::Small),
                    goal: None,
                    workspace_root: dir.display().to_string(),
                    bootstrap_symlinks: Vec::new(),
                    bootstrap_copies: Vec::new(),
                }),
            )
            .unwrap()
            .team_id
    }

    #[test]
    fn sibling_scan_finds_team_skips_corrupt_and_respects_bound() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let default_dir = base.join("orcherd");
        let corrupt_dir = base.join("orcherd-bad");
        let somalog_dir = base.join("orcherd-somalog");
        fs::create_dir_all(&default_dir).unwrap();
        fs::create_dir_all(&corrupt_dir).unwrap();
        fs::write(corrupt_dir.join("store.sqlite3"), b"corrupt").unwrap();
        let _ = Store::open(default_dir.join("store.sqlite3")).unwrap();
        let team_id = seed_team(&somalog_dir, "somalog");

        let roots = vec![base.to_path_buf()];
        assert_eq!(
            find_team_in_siblings_under(&team_id, &roots, Some(&default_dir)),
            Some(somalog_dir.clone())
        );
        assert_eq!(
            find_team_in_siblings_under("somalog", &roots, Some(&default_dir)),
            Some(somalog_dir)
        );
        assert_eq!(
            find_team_in_siblings_under("missing-team", &roots, Some(&default_dir)),
            None
        );
        assert_eq!(
            find_team_in_siblings_under("<default>", &roots, Some(&default_dir)),
            None
        );

        // Bound check: more than MAX_SIBLING_STORES empty dirs sorted before `orcherd-zzz`
        // stops after MAX_SIBLING_STORES without opening `orcherd-zzz`.
        let bounded = TempDir::new().unwrap();
        for idx in 0..MAX_SIBLING_STORES {
            let d = bounded.path().join(format!("orcherd-{idx:02}"));
            let _ = Store::open(d.join("store.sqlite3")).unwrap();
        }
        let beyond = bounded.path().join("orcherd-zzz");
        let beyond_id = seed_team(&beyond, "beyond");
        assert_eq!(
            find_team_in_siblings_under(&beyond_id, &[bounded.path().to_path_buf()], None),
            None
        );
    }
}
