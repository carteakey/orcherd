//! Integration tests for unified CLI team resolution (`#13`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

fn run_cli(state_dir: &Path, cwd: &Path, args: &[&str]) -> Output {
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
    cmd.output().unwrap()
}

fn run_json(state_dir: &Path, cwd: &Path, args: &[&str]) -> serde_json::Value {
    let out = run_cli(state_dir, cwd, args);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn run_err(state_dir: &Path, cwd: &Path, args: &[&str]) -> String {
    let out = run_cli(state_dir, cwd, args);
    assert!(!out.status.success(), "expected failure for {args:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn spawn_resolves_team_like_task_show_and_names_team_on_not_found() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let repo_a = tmp.path().join("k-squared");
    let repo_a_sub = repo_a.join("crates").join("app");
    let repo_b = tmp.path().join("l3ms");
    fs::create_dir_all(repo_a.join(".git")).unwrap();
    fs::create_dir_all(&repo_a_sub).unwrap();
    fs::create_dir_all(repo_b.join(".git")).unwrap();

    let init_a = run_json(&state_dir, &repo_a, &["init", "--name", "lvl2", "--template", "studio-crew", "--json"]);
    let lvl2_id = init_a["team_id"].as_str().unwrap().to_owned();
    let created = run_json(&state_dir, &repo_a, &["task", "create", "--spec", "Build feature", "--assign", "builder-a", "--json"]);
    let task_id = created["data"]["value"]["task_id"].as_str().unwrap().to_owned();

    // Initialize a newer team in a different repo so `l3ms` is the newest team in the store.
    run_json(&state_dir, &repo_b, &["init", "--name", "l3ms", "--template", "small", "--json"]);

    // Without `--team`, both `task show` and `spawn --task` in `repo_a` resolve `lvl2` (not `l3ms`).
    let show = run_json(&state_dir, &repo_a, &["task", "show", &task_id, "--json"]);
    let spawn = run_json(&state_dir, &repo_a, &["spawn", "--seat", "builder-a", "--task", &task_id, "--no-launch", "--json"]);
    assert_eq!(show["team_id"].as_str(), Some(lvl2_id.as_str()));
    assert_eq!(spawn["team_id"].as_str(), Some(lvl2_id.as_str()));

    // Register a newer team at the same `repo_a` root, then put `.orcherd.toml` naming `lvl2`.
    // Both `task show` and `spawn` from a subdirectory must prefer `.orcherd.toml` over `workspace_root`.
    run_json(&state_dir, &repo_a, &["init", "--name", "lvl2-shadow", "--template", "small", "--json"]);
    fs::write(repo_a.join(".orcherd.toml"), "[team]\nname = \"lvl2\"\n").unwrap();

    let show_sub = run_json(&state_dir, &repo_a_sub, &["task", "show", &task_id, "--json"]);
    let spawn_sub = run_json(&state_dir, &repo_a_sub, &["spawn", "--seat", "builder-a", "--task", &task_id, "--no-launch", "--json"]);
    assert_eq!(show_sub["team_id"].as_str(), Some(lvl2_id.as_str()));
    assert_eq!(spawn_sub["team_id"].as_str(), Some(lvl2_id.as_str()));

    // Missing task, seat, and gate lookups from `repo_b` (`l3ms`) name `l3ms`.
    let miss_spawn = run_err(&state_dir, &repo_b, &["spawn", "--seat", "w1", "--task", &task_id, "--no-launch"]);
    assert!(miss_spawn.contains(&format!("task not found in team l3ms: {task_id}")), "{miss_spawn}");
    let miss_seat = run_err(&state_dir, &repo_b, &["seat", "show", "ghost"]);
    assert!(miss_seat.contains("seat not found in team l3ms: ghost"), "{miss_seat}");
    let miss_gate = run_err(&state_dir, &repo_b, &["approve", "gate-missing"]);
    assert!(miss_gate.contains("gate not found in team l3ms: gate-missing"), "{miss_gate}");
}
