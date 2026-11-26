//! Bundled progressive skill guides (`orcherd skills list | get | install`).
//!
//! `SKILL.md` is the static discovery stub installed to `~/.agents/skills/orcherd/SKILL.md`
//! and `~/.claude/skills/orcherd/SKILL.md`. `GUIDE.md` and `references/*.md` are compiled
//! into the binary so skill instructions never drift from the executable version.

use serde::Serialize;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub const SKILL_NAME: &str = "orcherd";
pub const SKILL_DESCRIPTION: &str = "Orchestrate multi-agent teams, persistent seats, task boards, bulletins, and supervised workers on herdr-core (hc) and get their results back.";

pub const DISCOVERY_STUB: &str = include_str!("../../../skills/orcherd/SKILL.md");
pub const BASE_GUIDE: &str = include_str!("../../../skills/orcherd/GUIDE.md");

pub const REFERENCES: &[(&str, &str)] = &[
    // keep-sorted start
    (
        "references/messaging-and-gates.md",
        include_str!("../../../skills/orcherd/references/messaging-and-gates.md"),
    ),
    (
        "references/parallel-start.md",
        include_str!("../../../skills/orcherd/references/parallel-start.md"),
    ),
    (
        "references/placement-and-remote.md",
        include_str!("../../../skills/orcherd/references/placement-and-remote.md"),
    ),
    (
        "references/recovery-and-cleanup.md",
        include_str!("../../../skills/orcherd/references/recovery-and-cleanup.md"),
    ),
    (
        "references/seats-and-templates.md",
        include_str!("../../../skills/orcherd/references/seats-and-templates.md"),
    ),
    (
        "references/supervisor-loop.md",
        include_str!("../../../skills/orcherd/references/supervisor-loop.md"),
    ),
    // keep-sorted end
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillSummary {
    pub description: &'static str,
    pub name: &'static str,
    pub references: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillDocument {
    pub content: String,
    pub name: &'static str,
    pub reference: Option<String>,
    pub references: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledSkills {
    pub installed_paths: Vec<PathBuf>,
    pub skill: &'static str,
}

pub fn list() -> Vec<SkillSummary> {
    vec![SkillSummary {
        name: SKILL_NAME,
        description: SKILL_DESCRIPTION,
        references: reference_names(),
    }]
}

pub fn reference_names() -> Vec<&'static str> {
    REFERENCES.iter().map(|(name, _)| *name).collect()
}

pub fn get(skill: Option<&str>, reference: Option<&str>, full: bool) -> Result<SkillDocument, String> {
    let target = skill.unwrap_or(SKILL_NAME);
    if target != SKILL_NAME && target != "orchestration" {
        return Err(format!(
            "unknown skill `{target}`; available skills: `{SKILL_NAME}`"
        ));
    }

    if let Some(ref_name) = reference {
        let normalized = if ref_name.starts_with("references/") {
            ref_name.to_owned()
        } else {
            format!("references/{ref_name}")
        };
        for (name, body) in REFERENCES {
            if *name == normalized {
                return Ok(SkillDocument {
                    name: SKILL_NAME,
                    reference: Some((*name).to_owned()),
                    content: (*body).to_owned(),
                    references: reference_names(),
                });
            }
        }
        return Err(format!(
            "unknown reference `{ref_name}`; available references: {}",
            reference_names().join(", ")
        ));
    }

    let mut content = BASE_GUIDE.to_owned();
    if full {
        for (name, body) in REFERENCES {
            content.push_str("\n\n---\n<!-- ");
            content.push_str(name);
            content.push_str(" -->\n\n");
            content.push_str(body);
        }
    }

    Ok(SkillDocument {
        name: SKILL_NAME,
        reference: None,
        content,
        references: reference_names(),
    })
}

/// Installs the static discovery stub (`SKILL.md`) into the user's `~/.agents/skills/orcherd/`
/// and `~/.claude/skills/orcherd/` directories (or a custom root when specified).
pub fn install(custom_root: Option<&Path>) -> io::Result<InstalledSkills> {
    let roots = match custom_root {
        Some(root) => vec![root.to_path_buf()],
        None => default_skill_roots()?,
    };
    let mut installed_paths = Vec::with_capacity(roots.len());
    for root in roots {
        let dir = root.join(SKILL_NAME);
        fs::create_dir_all(&dir)?;
        let file = dir.join("SKILL.md");
        fs::write(&file, DISCOVERY_STUB)?;
        installed_paths.push(file);
    }
    Ok(InstalledSkills {
        skill: SKILL_NAME,
        installed_paths,
    })
}

fn default_skill_roots() -> io::Result<Vec<PathBuf>> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(vec![
        home.join(".agents").join("skills"),
        home.join(".claude").join("skills"),
    ])
}
