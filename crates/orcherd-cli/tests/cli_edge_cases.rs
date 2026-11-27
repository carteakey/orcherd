//! Integration tests for CLI edge cases: JSON errors, preflight exit code/config walk,
//! and global flags not hijacking option values.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

fn run_cli(state_dir: &Path, cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_orcherd"));
    cmd.current_dir(cwd)
        .args(args)
        .env("ORCHERD_STATE_DIR", state_dir)
        .env("ORCHERD_NO_AUTO_DAEMON", "1");
    for var in [
        "ORCHERD_TEAM",
        "ORCHERD_SEAT",
        "ORCHERD_DISPATCH",
        "ORCHERD_REMOTE_SOCKET",
        "HC_TERMINAL_ID",
        "HERDR_PANE_ID",
    ] {
        cmd.env_remove(var);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

#[test]
fn json_errors_emit_structured_json_on_stdout() {
    let tmp = TempDir::new().unwrap();
    let (state_dir, repo) = (tmp.path().join("state"), tmp.path().join("repo"));
    fs::create_dir_all(repo.join(".git")).unwrap();

    let init = run_cli(&state_dir, &repo, &["init", "--name", "t1", "--template", "small", "--json"], &[]);
    assert!(init.status.success());

    // `--json` failure writes structured JSON to stdout, nothing to stderr, and exits non-zero.
    let err_json = run_cli(&state_dir, &repo, &["task", "show", "nonexistent", "--team", "t1", "--json"], &[]);
    assert!(!err_json.status.success());
    assert!(err_json.stderr.is_empty(), "expected empty stderr, got: {}", String::from_utf8_lossy(&err_json.stderr));
    let parsed: serde_json::Value = serde_json::from_slice(&err_json.stdout).unwrap();
    assert_eq!(parsed["ok"].as_bool(), Some(false));
    assert!(
        parsed["error"].as_str().is_some_and(|e| e.contains("task not found in team t1: nonexistent")),
        "unexpected error json: {parsed}"
    );

    // Flag-parsing failure after `--json` also emits structured JSON.
    let flag_err = run_cli(&state_dir, &repo, &["--json", "--team"], &[]);
    assert!(!flag_err.status.success());
    let flag_json: serde_json::Value = serde_json::from_slice(&flag_err.stdout).unwrap();
    assert_eq!(flag_json["ok"].as_bool(), Some(false));
    assert!(flag_json["error"].as_str().is_some_and(|e| e.contains("--team")));

    // Without `--json`, errors stay plain text on stderr.
    let err_plain = run_cli(&state_dir, &repo, &["task", "show", "nonexistent", "--team", "t1"], &[]);
    assert!(!err_plain.status.success());
    assert!(err_plain.stdout.is_empty());
    assert!(String::from_utf8_lossy(&err_plain.stderr).contains("orcherd error: task not found in team t1: nonexistent"));
}

#[test]
fn preflight_exits_nonzero_on_failure_and_loads_git_root_config_from_subdir() {
    let tmp = TempDir::new().unwrap();
    let (state_dir, repo) = (tmp.path().join("state"), tmp.path().join("repo"));
    let sub = repo.join("crates").join("sub");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(&sub).unwrap();
    fs::write(
        repo.join(".orcherd.toml"),
        "[agents.myagent]\nargv = [\"custom-agent-xyz-999\"]\n",
    )
    .unwrap();

    // Clear PATH so `hc exec` fails cleanly without touching any live daemon.
    let out = run_cli(&state_dir, &sub, &["preflight", "--agent", "myagent", "--json"], &[("PATH", "")]);
    assert!(!out.status.success(), "preflight must exit non-zero on error/missing binary");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["agent"].as_str(), Some("myagent"));
    assert_eq!(report["binary"].as_str(), Some("custom-agent-xyz-999"));
    assert!(report["hc"]["error"].is_string());
}

#[test]
fn global_flags_do_not_hijack_option_values_end_to_end() {
    let tmp = TempDir::new().unwrap();
    let (state_dir, repo) = (tmp.path().join("state"), tmp.path().join("repo"));
    fs::create_dir_all(repo.join(".git")).unwrap();

    let init = run_cli(&state_dir, &repo, &["init", "--name", "t1", "--template", "small", "--json"], &[]);
    assert!(init.status.success());

    // `ask --question -V --json` must create a gate with question "-V", not print version.
    let ask = run_cli(&state_dir, &repo, &["ask", "--as", "w1", "--question", "-V", "--json"], &[]);
    assert!(ask.status.success(), "stderr={}", String::from_utf8_lossy(&ask.stderr));
    let ask_json: serde_json::Value = serde_json::from_slice(&ask.stdout).unwrap();
    assert_eq!(ask_json["data"]["kind"].as_str(), Some("gate"));
    assert_eq!(ask_json["data"]["value"]["question"].as_str(), Some("-V"));

    // `task create --title -V --spec -h --json` must preserve both `-V` and `-h` as values.
    let task = run_cli(&state_dir, &repo, &["task", "create", "--title", "-V", "--spec", "-h", "--json"], &[]);
    assert!(task.status.success(), "stderr={}", String::from_utf8_lossy(&task.stderr));
    let task_json: serde_json::Value = serde_json::from_slice(&task.stdout).unwrap();
    assert_eq!(task_json["data"]["value"]["title"].as_str(), Some("-V"));
    assert_eq!(task_json["data"]["value"]["spec"].as_str(), Some("-h"));
}
