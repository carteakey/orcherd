//! Integration tests for per-team `--request-id` isolation and `spawn` replay safety.
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
        "HC_BINARY",
    ] {
        cmd.env_remove(var);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn run_json(state_dir: &Path, cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> serde_json::Value {
    let out = run_cli(state_dir, cwd, args, extra_env);
    assert!(
        out.status.success(),
        "args={args:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn init_team(state_dir: &Path, repo: &Path, name: &str) -> String {
    fs::create_dir_all(repo.join(".git")).unwrap();
    let v = run_json(state_dir, repo, &["init", "--name", name, "--template", "small", "--json"], &[]);
    v["team_id"].as_str().unwrap().to_owned()
}

#[test]
fn cross_team_request_id_replays_do_not_collide() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let (repo_a, repo_b) = (tmp.path().join("alpha"), tmp.path().join("beta"));
    let team_a = init_team(&state_dir, &repo_a, "alpha");
    let team_b = init_team(&state_dir, &repo_b, "beta");

    let created_a = run_json(
        &state_dir, &repo_a,
        &["task", "create", "--team", "alpha", "--spec", "Ship fix", "--request-id", "req-cross-1", "--json"], &[],
    );
    let created_b = run_json(
        &state_dir, &repo_b,
        &["task", "create", "--team", "beta", "--spec", "Ship fix", "--request-id", "req-cross-1", "--json"], &[],
    );

    assert_eq!(created_a["team_id"].as_str(), Some(team_a.as_str()));
    assert_eq!(created_b["team_id"].as_str(), Some(team_b.as_str()));
    assert_ne!(created_a["data"]["value"]["task_id"], created_b["data"]["value"]["task_id"]);

    for (repo, name) in [(&repo_a, "alpha"), (&repo_b, "beta")] {
        let list = run_json(&state_dir, repo, &["task", "list", "--team", name, "--json"], &[]);
        assert_eq!(list["data"]["value"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn spawn_request_id_replay_skips_cold_launch_and_keeps_binding() {
    let tmp = TempDir::new().unwrap();
    let (state_dir, repo) = (tmp.path().join("state"), tmp.path().join("repo"));
    init_team(&state_dir, &repo, "alpha");

    // Initial spawn binds `w1` to `t9` and caches `SpawnPrepared` (`warm_reuse=false`) under `req-spawn-1`.
    let first = run_json(
        &state_dir, &repo,
        &["spawn", "--seat", "w1", "--spec", "Implement feature", "--terminal", "t9", "--request-id", "req-spawn-1", "--json"],
        &[],
    );
    assert!(first["failed_stage"].is_null());
    let first_dispatch = first["data"]["value"]["dispatch"]["dispatch_id"].as_str().unwrap().to_owned();
    assert_eq!(first["data"]["value"]["seat"]["address"]["terminal"].as_str(), Some("t9"));

    let count_before = run_json(&state_dir, &repo, &["inbox", "--seat", "w1", "--peek", "--json"], &[])
        ["data"]["value"].as_array().unwrap().len();

    // Point `HC_SOCKET` at a missing path and clear `PATH` so any attempted cold launch fails with `herdr_core_connect`.
    let dead_sock = tmp.path().join("missing-hc.sock").to_string_lossy().into_owned();
    let no_hc = [("HC_SOCKET", dead_sock.as_str()), ("PATH", "")];

    // Replaying `--request-id` without `--terminal`/`--no-launch` must skip cold launch and keep `t9`.
    let replayed = run_json(
        &state_dir, &repo,
        &["spawn", "--seat", "w1", "--spec", "Implement feature", "--request-id", "req-spawn-1", "--json"],
        &no_hc,
    );
    assert!(replayed["failed_stage"].is_null(), "replay should not cold-launch: {replayed}");
    assert_eq!(replayed["data"]["value"]["dispatch"]["dispatch_id"].as_str(), Some(first_dispatch.as_str()));
    assert_eq!(replayed["data"]["value"]["seat"]["address"]["terminal"].as_str(), Some("t9"));

    let inbox_after = run_json(&state_dir, &repo, &["inbox", "--seat", "w1", "--peek", "--json"], &[]);
    let items_after = inbox_after["data"]["value"].as_array().unwrap();
    assert_eq!(items_after.len(), count_before);
    assert_eq!(items_after.iter().filter(|it| it["kind"].as_str() == Some("dispatch")).count(), 1);

    // Control: a fresh cold spawn on unbound seat `w2` still attempts launch and reports `herdr_core_connect`.
    let fresh_out = run_cli(
        &state_dir, &repo,
        &["spawn", "--seat", "w2", "--spec", "Fresh work", "--request-id", "req-spawn-2", "--json"],
        &no_hc,
    );
    assert!(!fresh_out.status.success(), "fresh cold spawn without hc should fail");
    let fresh_json: serde_json::Value = serde_json::from_slice(&fresh_out.stdout).unwrap();
    assert_eq!(fresh_json["failed_stage"].as_str(), Some("herdr_core_connect"));
}
