//! `orcherd preflight --agent <id>`: compares the binary path and `--version` that
//! `herdr-core` (`hc`) terminals resolve against the current user shell's `PATH`.

use crate::{
    agent_argv,
    cli::{GlobalFlags, req_val},
    output::print_value,
    worktree,
};
use orcherd_hc::new_command;
use serde::Serialize;
use std::{
    env,
    path::{Path, PathBuf},
};

const HC_PROBE_SCRIPT: &str = "\
p=$(command -v \"$1\" 2>/dev/null || true); \
printf '%s\\n' \"$p\"; \
if [ -n \"$p\" ]; then \"$p\" --version 2>&1 | head -n 1; fi";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub path: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreflightReport {
    pub agent: String,
    pub binary: String,
    pub hc: ProbeResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    pub path_drift: bool,
    pub shell: ProbeResult,
    pub version_drift: bool,
}

pub fn handle_preflight(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut agent = None;
    let mut machine = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--agent" => {
                i += 1;
                agent = Some(req_val(args, i, "--agent")?.to_owned());
            }
            "--on" | "--machine" => {
                i += 1;
                machine = Some(req_val(args, i, "--on")?.to_owned());
            }
            val if !val.starts_with('-') && agent.is_none() => {
                agent = Some(val.to_owned());
            }
            other => return Err(format!("unknown `preflight` option `{other}`")),
        }
        i += 1;
    }

    let agent = agent.ok_or_else(|| "`orcherd preflight` requires `--agent <id>`".to_owned())?;
    let project_cfg = env::current_dir()
        .ok()
        .and_then(|cwd| worktree::load_project_config(&cwd).ok())
        .unwrap_or_default();
    let argv = agent_argv::build_agent_argv(&agent, None, None, &project_cfg.agents);
    let binary = argv.into_iter().next().unwrap_or_else(|| agent.clone());

    let hc = probe_hc(&binary, machine.as_deref());
    let shell = probe_shell(&binary);
    let report = build_report(agent, binary, machine, hc, shell);
    print_value(&report, global.json, format_report);
    Ok(0)
}

pub(crate) fn build_report(
    agent: String,
    binary: String,
    machine: Option<String>,
    hc: ProbeResult,
    shell: ProbeResult,
) -> PreflightReport {
    let both_ok = hc.error.is_none() && shell.error.is_none();
    let path_drift = both_ok && hc.path != shell.path;
    let version_drift = both_ok && hc.version != shell.version;
    PreflightReport {
        agent,
        binary,
        hc,
        machine,
        path_drift,
        shell,
        version_drift,
    }
}

pub(crate) fn parse_probe_output(stdout: &str) -> ProbeResult {
    let mut lines = stdout.lines();
    let path = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let version = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    ProbeResult {
        error: None,
        path,
        version,
    }
}

/// Runs the probe through `hc exec` so `hc` handles framing, base64 decoding, and `--on`.
fn probe_hc(binary: &str, machine: Option<&str>) -> ProbeResult {
    let mut cmd = new_command("hc");
    cmd.arg("exec");
    if let Some(m) = machine {
        cmd.args(["--on", m]);
    }
    cmd.args(["--timeout", "10", "--", "sh", "-c", HC_PROBE_SCRIPT, "_", binary]);
    match cmd.output() {
        Ok(out) if out.status.success() => {
            parse_probe_output(&String::from_utf8_lossy(&out.stdout))
        }
        Ok(out) => {
            let detail = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            let err = if detail.is_empty() {
                format!("`hc exec` exited with {}", out.status)
            } else {
                format!("`hc exec` failed: {detail}")
            };
            ProbeResult {
                error: Some(err),
                path: None,
                version: None,
            }
        }
        Err(err) => ProbeResult {
            error: Some(format!("failed to run `hc exec`: {err}")),
            path: None,
            version: None,
        },
    }
}

fn probe_shell(binary: &str) -> ProbeResult {
    let Some(resolved) = find_on_path(binary) else {
        return ProbeResult {
            error: None,
            path: None,
            version: None,
        };
    };
    let version = new_command(&resolved)
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| {
            let text = if out.stdout.is_empty() {
                String::from_utf8_lossy(&out.stderr).into_owned()
            } else {
                String::from_utf8_lossy(&out.stdout).into_owned()
            };
            text.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_owned)
        });
    ProbeResult {
        error: None,
        path: Some(resolved.display().to_string()),
        version,
    }
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let as_path = Path::new(binary);
    if as_path.components().count() > 1 {
        return as_path.is_file().then(|| as_path.to_path_buf());
    }
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn format_probe(probe: &ProbeResult) -> String {
    if let Some(ref err) = probe.error {
        return format!("(error: {err})");
    }
    match (&probe.path, &probe.version) {
        (Some(path), Some(ver)) => format!("{path}  ({ver})"),
        (Some(path), None) => format!("{path}  (version unavailable)"),
        (None, _) => "(not found)".to_owned(),
    }
}

fn format_report(report: &PreflightReport) -> String {
    let hc_label = match report.machine.as_deref() {
        Some(m) => format!("hc ({m}):"),
        None => "hc:   ".to_owned(),
    };
    let mut out = format!(
        "Preflight: {} (binary: {})\n  {hc_label} {}\n  shell: {}",
        report.agent,
        report.binary,
        format_probe(&report.hc),
        format_probe(&report.shell),
    );
    if report.hc.error.is_none() && report.hc.path.is_none() {
        out.push_str("\n  WARNING: binary not found in hc environment");
    } else if report.version_drift && report.path_drift {
        out.push_str("\n  WARNING: PATH and version drift between hc and current shell");
    } else if report.version_drift {
        out.push_str("\n  WARNING: version drift between hc and current shell");
    } else if report.path_drift {
        out.push_str("\n  note: PATH differs between hc and current shell (same version)");
    } else if report.hc.error.is_none() && report.shell.error.is_none() {
        out.push_str("\n  status: match");
    }
    out
}
