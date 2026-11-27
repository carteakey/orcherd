//! Git worktree placement and `.orcherd.toml` bootstrap symlinks/copies.
//!
//! Creates child or top-level worktrees and links git-ignored dependencies
//! (`.env`, `.venv`, `node_modules`, media caches) from the workspace root.

use orcherd_hc::new_command;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeSpec {
    Current,
    NewChild { name: Option<String> },
    NewTopLevel { name: Option<String> },
    Path(PathBuf),
}

impl WorktreeSpec {
    pub fn parse(raw: &str, default_name: Option<&str>) -> Result<Self, String> {
        let trimmed = raw.trim();
        match trimmed {
            "" | "current" | "active" => Ok(Self::Current),
            "new-child" => Ok(Self::NewChild {
                name: default_name.map(str::to_owned),
            }),
            "new-top-level" => Ok(Self::NewTopLevel {
                name: default_name.map(str::to_owned),
            }),
            other => {
                if let Some(rest) = other.strip_prefix("path:") {
                    if rest.trim().is_empty() {
                        return Err("worktree `path:` selector cannot be empty".to_owned());
                    }
                    Ok(Self::Path(PathBuf::from(rest.trim())))
                } else {
                    Err(format!(
                        "invalid `--worktree` value `{other}`; expected `current`, `new-child`, `new-top-level`, or `path:<dir>`"
                    ))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProjectConfigFile {
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfigSection>,
    #[serde(default)]
    pub team: TeamConfigSection,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentConfigSection {
    #[serde(default)]
    pub argv: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TeamConfigSection {
    #[serde(default)]
    pub bootstrap_copies: Vec<String>,
    #[serde(default)]
    pub bootstrap_symlinks: Vec<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreparedWorktree {
    pub branch: Option<String>,
    pub copied: Vec<String>,
    pub created: bool,
    pub path: PathBuf,
    pub symlinked: Vec<String>,
}

/// Reads `.orcherd.toml` from `workspace_root` when present.
pub fn load_project_config(workspace_root: &Path) -> Result<ProjectConfigFile, String> {
    let path = workspace_root.join(".orcherd.toml");
    if !path.is_file() {
        return Ok(ProjectConfigFile::default());
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read `{}`: {e}", path.display()))?;
    let cfg: ProjectConfigFile =
        toml::from_str(&text).map_err(|e| format!("failed to parse `{}`: {e}", path.display()))?;
    for (agent, section) in &cfg.agents {
        if let Some(argv) = &section.argv
            && argv.first().is_none_or(|bin| bin.trim().is_empty())
        {
            return Err(format!(
                "invalid `{}`: `[agents.{agent}].argv` must start with a non-empty executable",
                path.display()
            ));
        }
    }
    Ok(cfg)
}

/// Resolves or creates the worktree for a seat and applies `bootstrap_symlinks` and `bootstrap_copies`.
pub fn prepare(
    workspace_root: &Path,
    spec: &WorktreeSpec,
    seat_name: &str,
    base_branch: Option<&str>,
    symlinks: &[String],
    copies: &[String],
) -> Result<PreparedWorktree, String> {
    match spec {
        WorktreeSpec::Current => Ok(PreparedWorktree {
            path: workspace_root.to_path_buf(),
            branch: None,
            created: false,
            symlinked: Vec::new(),
            copied: Vec::new(),
        }),
        WorktreeSpec::Path(path) => {
            let resolved = if path.is_absolute() {
                path.clone()
            } else {
                workspace_root.join(path)
            };
            if !resolved.is_dir() {
                return Err(format!(
                    "worktree directory `{}` does not exist",
                    resolved.display()
                ));
            }
            Ok(PreparedWorktree {
                path: resolved,
                branch: None,
                created: false,
                symlinked: Vec::new(),
                copied: Vec::new(),
            })
        }
        WorktreeSpec::NewChild { name } | WorktreeSpec::NewTopLevel { name } => {
            let slug = sanitize_slug(name.as_deref().unwrap_or(seat_name));
            let is_child = matches!(spec, WorktreeSpec::NewChild { .. });
            let target_dir = worktree_target_dir(workspace_root, &slug, is_child)?;
            let branch = format!("orcherd/{slug}");
            let created = if target_dir.join(".git").exists() {
                false
            } else {
                create_git_worktree(workspace_root, &target_dir, &branch, base_branch)?;
                true
            };
            let symlinked = apply_bootstrap_symlinks(workspace_root, &target_dir, symlinks)?;
            let copied = apply_bootstrap_copies(workspace_root, &target_dir, copies)?;
            Ok(PreparedWorktree {
                path: target_dir,
                branch: Some(branch),
                created,
                symlinked,
                copied,
            })
        }
    }
}

fn worktree_target_dir(workspace_root: &Path, slug: &str, is_child: bool) -> Result<PathBuf, String> {
    let parent = workspace_root
        .parent()
        .ok_or_else(|| format!("workspace root `{}` has no parent directory", workspace_root.display()))?;
    let repo_name = workspace_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    if is_child {
        Ok(parent.join(format!("{repo_name}-wt")).join(slug))
    } else {
        Ok(parent.join(format!("{repo_name}-{slug}")))
    }
}

fn create_git_worktree(
    workspace_root: &Path,
    target_dir: &Path,
    branch: &str,
    base_branch: Option<&str>,
) -> Result<(), String> {
    if let Some(parent) = target_dir.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create worktree parent `{}`: {e}", parent.display()))?;
    }
    let mut cmd = new_command("git");
    cmd.current_dir(workspace_root)
        .args(["worktree", "add", "-B", branch])
        .arg(target_dir);
    if let Some(base) = base_branch.filter(|b| !b.trim().is_empty()) {
        cmd.arg(base);
    }
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run `git worktree add`: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`git worktree add` failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(())
}

pub fn apply_bootstrap_symlinks(
    source_root: &Path,
    target_root: &Path,
    rel_paths: &[String],
) -> Result<Vec<String>, String> {
    let mut applied = Vec::new();
    for rel in rel_paths {
        let clean = validate_relative_path(rel)?;
        let src = source_root.join(&clean);
        if !src.exists() {
            continue;
        }
        let dst = target_root.join(&clean);
        if dst.exists() || dst.symlink_metadata().is_ok() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create parent `{}`: {e}", parent.display()))?;
        }
        create_symlink(&src, &dst)
            .map_err(|e| format!("failed to symlink `{}` -> `{}`: {e}", dst.display(), src.display()))?;
        applied.push(rel.clone());
    }
    Ok(applied)
}

pub fn apply_bootstrap_copies(
    source_root: &Path,
    target_root: &Path,
    rel_paths: &[String],
) -> Result<Vec<String>, String> {
    let mut applied = Vec::new();
    for rel in rel_paths {
        let clean = validate_relative_path(rel)?;
        let src = source_root.join(&clean);
        if !src.exists() {
            continue;
        }
        let dst = target_root.join(&clean);
        if dst.exists() {
            continue;
        }
        copy_recursive(&src, &dst)
            .map_err(|e| format!("failed to copy `{}` -> `{}`: {e}", src.display(), dst.display()))?;
        applied.push(rel.clone());
    }
    Ok(applied)
}

fn validate_relative_path(rel: &str) -> Result<PathBuf, String> {
    let path = Path::new(rel.trim());
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(format!("bootstrap path `{rel}` must be a non-empty relative path"));
    }
    for comp in path.components() {
        if matches!(
            comp,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        ) {
            return Err(format!("bootstrap path `{rel}` cannot traverse outside root"));
        }
    }
    Ok(path.to_path_buf())
}

#[cfg(unix)]
fn create_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn create_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    if src.is_dir() {
        std::os::windows::fs::symlink_dir(src, dst)
    } else {
        std::os::windows::fs::symlink_file(src, dst)
    }
}

fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst).map(|_| ())
    }
}

fn sanitize_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch == ' ' || ch == '/' {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "worker".to_owned()
    } else {
        trimmed.to_owned()
    }
}
