//! Launch `argv` resolution for seat agent harnesses, with `.orcherd.toml` overrides.

use crate::worktree::AgentConfigSection;
use orcherd_hc::AgentKind;
use std::collections::BTreeMap;

/// Resolves the launch argv for a seat's agent harness.
///
/// When `.orcherd.toml` sets `[agents.<id>].argv`, it replaces the built-in base argv.
/// `--model` and resume flags are still appended on top of the resolved base.
pub(crate) fn build_agent_argv(
    agent: &str,
    model: Option<&str>,
    resume: Option<&str>,
    overrides: &BTreeMap<String, AgentConfigSection>,
) -> Vec<String> {
    let resume = resume.filter(|s| !s.is_empty());
    let mut argv = override_argv(agent, overrides).unwrap_or_else(|| builtin_base_argv(agent));

    if let Some(m) = model.filter(|s| !s.is_empty()) {
        argv.push("--model".to_owned());
        argv.push(m.to_owned());
    }
    if let Some(r) = resume {
        let is_agy = argv
            .first()
            .is_some_and(|bin| AgentKind::lookup(bin) == Some(AgentKind::Antigravity));
        if is_agy && r == "latest" {
            argv.push("--continue".to_owned());
        } else {
            argv.push("--resume".to_owned());
            if r != "latest" {
                argv.push(r.to_owned());
            }
        }
    }
    argv
}

fn override_argv(
    agent: &str,
    overrides: &BTreeMap<String, AgentConfigSection>,
) -> Option<Vec<String>> {
    let section = overrides.get(agent).or_else(|| {
        AgentKind::lookup(agent).and_then(|kind| overrides.get(kind.name()))
    })?;
    section
        .argv
        .as_ref()
        .filter(|argv| argv.first().is_some_and(|bin| !bin.trim().is_empty()))
        .cloned()
}

/// Built-in base argv when `.orcherd.toml` has no `[agents.<id>]` override.
///
/// The seat's agent id is a harness name, not always a binary: `antigravity` ships as `agy`.
/// Unattended seats run with the harness's no-approval flag, because a permission prompt in a
/// worker terminal stalls the task until a human answers it.
fn builtin_base_argv(agent: &str) -> Vec<String> {
    match agent {
        "antigravity" | "agy" | "antigravity-cli" | "antigravity_cli" => {
            vec!["agy".to_owned(), "--dangerously-skip-permissions".to_owned()]
        }
        other => vec![other.to_owned()],
    }
}
