//! Inherits `agy`'s `trustedWorkspaces` allowlist from the parent repo into new worktrees.
//!
//! Antigravity CLI keeps an exact-path `trustedWorkspaces` array in
//! `~/.gemini/antigravity-cli/settings.json`, so trusting `~/repos/orcherd` does not cover
//! `~/repos/orcherd-wt/<seat>`. When `orcherd spawn` launches a local `agy` seat in a
//! worktree it created (`new-child` / `new-top-level`) and `workspace_root` is already
//! trusted, we append the worktree path atomically before launch. Untrusted roots and
//! unparseable settings files are never modified.

use crate::worktree::WorktreeSpec;
use orcherd_core::Receipt;
use orcherd_hc::AgentKind;
use serde_json::Value;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
};

/// Outcome of attempting to inherit `agy` workspace trust for a newly created worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrustOutcome {
    Added,
    AlreadyTrusted,
    Skipped(String),
}

/// Resolves `agy`'s `settings.json` path from `$GEMINI_CLI_HOME`, else `$HOME` / `$USERPROFILE`.
pub(crate) fn settings_path() -> Option<PathBuf> {
    let base = env::var_os("GEMINI_CLI_HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| env::var_os("HOME").filter(|v| !v.is_empty()))
        .or_else(|| env::var_os("USERPROFILE").filter(|v| !v.is_empty()))?;
    Some(settings_path_in(Path::new(&base)))
}

pub(crate) fn settings_path_in(home: &Path) -> PathBuf {
    home.join(".gemini")
        .join("antigravity-cli")
        .join("settings.json")
}

/// For local Antigravity seats launched in an `orcherd`-created worktree, inherits
/// `trustedWorkspaces` from `workspace_root` when already trusted. Returns an optional
/// one-line note for `receipt.next` whenever trust could not be inherited.
pub(crate) fn maybe_inherit_worktree_trust(
    agent: &str,
    wt_spec: &WorktreeSpec,
    workspace_root: &Path,
    worktree_path: &Path,
    machine: Option<&str>,
) -> Option<String> {
    if AgentKind::lookup(agent) != Some(AgentKind::Antigravity) {
        return None;
    }
    if !matches!(
        wt_spec,
        WorktreeSpec::NewChild { .. } | WorktreeSpec::NewTopLevel { .. }
    ) {
        return None;
    }
    if let Some(m) = machine {
        return Some(format!(
            "remote seat on `{m}`: skipped local agy trustedWorkspaces update; accept agy's trust prompt on `{m}` if shown"
        ));
    }
    let Some(path) = settings_path() else {
        return Some(
            "HOME is unset: skipped agy trustedWorkspaces update; accept agy's trust prompt if shown"
                .to_owned(),
        );
    };
    match inherit_worktree_trust_in(&path, workspace_root, worktree_path) {
        TrustOutcome::Added | TrustOutcome::AlreadyTrusted => None,
        TrustOutcome::Skipped(note) => Some(note),
    }
}

/// Appends an optional advisory note to `receipt.next` without clobbering existing guidance.
pub(crate) fn append_note(receipt: &mut Receipt, note: Option<String>) {
    let Some(note) = note.filter(|s| !s.trim().is_empty()) else {
        return;
    };
    receipt.next = Some(match receipt.next.take() {
        Some(existing) if !existing.trim().is_empty() => format!("{note}. {existing}"),
        _ => note,
    });
}

/// Reads `settings_path`, appends `worktree_path` to `trustedWorkspaces` only when
/// `workspace_root` is already present and `worktree_path` is absent, and replaces the file
/// atomically while preserving its mode.
pub(crate) fn inherit_worktree_trust_in(
    settings_path: &Path,
    workspace_root: &Path,
    worktree_path: &Path,
) -> TrustOutcome {
    if !settings_path.is_file() {
        return TrustOutcome::Skipped(format!(
            "agy settings not found at `{}`; accept agy's trust prompt if shown",
            settings_path.display()
        ));
    }
    let orig_meta = match fs::metadata(settings_path) {
        Ok(m) => m,
        Err(err) => {
            return TrustOutcome::Skipped(format!(
                "could not stat agy settings `{}` ({err}); left untouched",
                settings_path.display()
            ));
        }
    };
    let raw = match fs::read_to_string(settings_path) {
        Ok(s) => s,
        Err(err) => {
            return TrustOutcome::Skipped(format!(
                "could not read agy settings `{}` ({err}); left untouched",
                settings_path.display()
            ));
        }
    };
    let mut doc: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            return TrustOutcome::Skipped(format!(
                "could not parse agy settings `{}` ({err}); left untouched",
                settings_path.display()
            ));
        }
    };
    let Some(list) = doc
        .as_object_mut()
        .and_then(|obj| obj.get_mut("trustedWorkspaces"))
        .and_then(Value::as_array_mut)
    else {
        return TrustOutcome::Skipped(format!(
            "workspace root `{}` is not in agy trustedWorkspaces; accept agy's trust prompt if shown",
            workspace_root.display()
        ));
    };

    let root_trusted = list
        .iter()
        .filter_map(Value::as_str)
        .any(|entry| paths_match(entry, workspace_root));
    if !root_trusted {
        return TrustOutcome::Skipped(format!(
            "workspace root `{}` is not in agy trustedWorkspaces; accept agy's trust prompt if shown",
            workspace_root.display()
        ));
    }

    let already_trusted = list
        .iter()
        .filter_map(Value::as_str)
        .any(|entry| paths_match(entry, worktree_path));
    if already_trusted {
        return TrustOutcome::AlreadyTrusted;
    }

    list.push(Value::String(worktree_path.display().to_string()));
    let mut serialized = match serde_json::to_string_pretty(&doc) {
        Ok(s) => s,
        Err(err) => {
            return TrustOutcome::Skipped(format!(
                "could not serialize agy settings `{}` ({err}); left untouched",
                settings_path.display()
            ));
        }
    };
    serialized.push('\n');

    match atomic_write_preserving_mode(settings_path, serialized.as_bytes(), orig_meta.permissions()) {
        Ok(()) => TrustOutcome::Added,
        Err(err) => TrustOutcome::Skipped(format!(
            "could not update agy settings `{}` ({err}); accept agy's trust prompt if shown",
            settings_path.display()
        )),
    }
}

fn paths_match(entry: &str, target: &Path) -> bool {
    let entry_path = Path::new(entry);
    if entry_path == target {
        return true;
    }
    if let (Ok(a), Ok(b)) = (fs::canonicalize(entry_path), fs::canonicalize(target)) {
        return a == b;
    }
    false
}

fn atomic_write_preserving_mode(
    target: &Path,
    bytes: &[u8],
    perms: fs::Permissions,
) -> std::io::Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("settings.json");
    let tmp = parent.join(format!(".{file_name}.{}.{nanos}.tmp", process::id()));

    let write_res = (|| -> std::io::Result<()> {
        fs::write(&tmp, bytes)?;
        fs::set_permissions(&tmp, perms)?;
        fs::rename(&tmp, target)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_res
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn write_settings(home: &Path, content: &str) -> PathBuf {
        let path = settings_path_in(home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    #[test]
    fn trusted_root_adds_worktree_preserves_other_keys_and_mode_and_deduplicates() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let root = tmp.path().join("repos").join("orcherd");
        let wt = tmp.path().join("repos").join("orcherd-wt").join("neon1");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&wt).unwrap();

        let initial = json!({
            "artifactReviewPolicy": "always",
            "model": "gemini-4-neon-high",
            "toolPermission": "auto",
            "nested": { "retries": 3, "enabled": true },
            "trustedWorkspaces": [root.display().to_string(), "/other/repo"]
        });
        let settings = write_settings(&home, &serde_json::to_string_pretty(&initial).unwrap());

        assert_eq!(
            inherit_worktree_trust_in(&settings, &root, &wt),
            TrustOutcome::Added
        );

        let updated: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(updated["artifactReviewPolicy"], "always");
        assert_eq!(updated["model"], "gemini-4-neon-high");
        assert_eq!(updated["toolPermission"], "auto");
        assert_eq!(updated["nested"], json!({ "retries": 3, "enabled": true }));
        assert_eq!(
            updated["trustedWorkspaces"],
            json!([
                root.display().to_string(),
                "/other/repo",
                wt.display().to_string()
            ])
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&settings).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Calling again does not duplicate the already-present path.
        assert_eq!(
            inherit_worktree_trust_in(&settings, &root, &wt),
            TrustOutcome::AlreadyTrusted
        );
        let after_second: Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(after_second["trustedWorkspaces"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn untrusted_root_missing_file_and_malformed_json_leave_settings_untouched() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let root = tmp.path().join("repos").join("orcherd");
        let wt = tmp.path().join("repos").join("orcherd-wt").join("neon2");

        // 1. Missing file: skipped, file is not created.
        let missing = settings_path_in(&home);
        assert!(matches!(
            inherit_worktree_trust_in(&missing, &root, &wt),
            TrustOutcome::Skipped(msg) if msg.contains("not found")
        ));
        assert!(!missing.exists());

        // 2. Untrusted root: skipped, file bytes untouched.
        let untrusted_raw = "{\n  \"model\": \"default\",\n  \"trustedWorkspaces\": [\"/somewhere/else\"]\n}\n";
        let settings = write_settings(&home, untrusted_raw);
        assert!(matches!(
            inherit_worktree_trust_in(&settings, &root, &wt),
            TrustOutcome::Skipped(msg) if msg.contains("not in agy trustedWorkspaces")
        ));
        assert_eq!(fs::read_to_string(&settings).unwrap(), untrusted_raw);

        // 3. Malformed JSON: skipped, file bytes untouched.
        let broken_raw = "{ \"trustedWorkspaces\": [unterminated";
        let settings = write_settings(&home, broken_raw);
        assert!(matches!(
            inherit_worktree_trust_in(&settings, &root, &wt),
            TrustOutcome::Skipped(msg) if msg.contains("could not parse")
        ));
        assert_eq!(fs::read_to_string(&settings).unwrap(), broken_raw);
    }

    #[test]
    fn remote_non_agy_and_non_created_worktree_rules() {
        let root = Path::new("/repos/orcherd");
        let wt = Path::new("/repos/orcherd-wt/neon3");
        let new_child = WorktreeSpec::NewChild {
            name: Some("neon3".to_owned()),
        };

        // Remote seat skips local settings edit and returns a `next` note.
        let remote_note = maybe_inherit_worktree_trust("antigravity", &new_child, root, wt, Some("m1"));
        assert!(remote_note.as_deref().is_some_and(|s| s.contains("remote seat on `m1`")));

        // Non-antigravity seat or `current` / `path:` worktree does nothing.
        assert_eq!(
            maybe_inherit_worktree_trust("claude", &new_child, root, wt, None),
            None
        );
        assert_eq!(
            maybe_inherit_worktree_trust("antigravity", &WorktreeSpec::Current, root, root, None),
            None
        );
        assert_eq!(
            maybe_inherit_worktree_trust(
                "antigravity",
                &WorktreeSpec::Path(wt.to_path_buf()),
                root,
                wt,
                None,
            ),
            None
        );
    }
}
