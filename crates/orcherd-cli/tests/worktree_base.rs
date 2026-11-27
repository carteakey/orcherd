//! Integration tests for upstream-aware `new-child` worktree base resolution.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn commit(dir: &Path, file: &str, content: &str, msg: &str) -> String {
    fs::write(dir.join(file), content).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

fn spawn_worktree(state_dir: &Path, repo: &Path, seat: &str, base: &str) -> serde_json::Value {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_orcherd"));
    cmd.current_dir(repo)
        .args([
            "spawn",
            "--seat",
            seat,
            "--worktree",
            "new-child",
            "--base-branch",
            base,
            "--no-launch",
            "--json",
        ])
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
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn new_child_worktree_uses_newer_upstream_and_warns_on_divergence() {
    let tmp = TempDir::new().unwrap();
    let origin = tmp.path().join("origin.git");
    let local = tmp.path().join("repo");
    let pusher = tmp.path().join("pusher");
    let state_dir = tmp.path().join("state");
    fs::create_dir_all(&origin).unwrap();
    fs::create_dir_all(&local).unwrap();

    git(&origin, &["init", "--bare", "--initial-branch=main"]);
    git(&local, &["init", "--initial-branch=main"]);
    let c1 = commit(&local, "a.txt", "v1\n", "initial");
    git(&local, &["remote", "add", "origin", &origin.display().to_string()]);
    git(&local, &["push", "-u", "origin", "main"]);

    // Second clone simulates work pushed to origin/main from elsewhere.
    git(tmp.path(), &["clone", &origin.display().to_string(), &pusher.display().to_string()]);
    let c2 = commit(&pusher, "a.txt", "v2\n", "upstream advance");
    git(&pusher, &["push", "origin", "main"]);

    // Initialize an orcherd team rooted at `local`.
    let init = Command::new(env!("CARGO_BIN_EXE_orcherd"))
        .current_dir(&local)
        .args(["init", "--name", "wt-test", "--template", "small", "--json"])
        .env("ORCHERD_STATE_DIR", &state_dir)
        .env("ORCHERD_NO_AUTO_DAEMON", "1")
        .env_remove("ORCHERD_TEAM")
        .env_remove("ORCHERD_SEAT")
        .env_remove("ORCHERD_REMOTE_SOCKET")
        .output()
        .unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));

    // 1. Local behind upstream: fetches origin/main, starts worktree at c2, leaves local `main` at c1.
    let r_behind = spawn_worktree(&state_dir, &local, "w-behind", "main");
    let wt_behind = Path::new(r_behind["data"]["value"]["seat"]["worktree_path"].as_str().unwrap());
    assert_eq!(git(wt_behind, &["rev-parse", "HEAD"]), c2);
    assert_eq!(git(&local, &["rev-parse", "main"]), c1, "must not move local main");
    assert!(!r_behind["next"].as_str().unwrap_or("").contains("diverged"));

    // 2. Local ahead of upstream: local wins, no warning.
    git(&local, &["checkout", "-b", "ahead"]);
    git(&local, &["push", "-u", "origin", "ahead"]);
    let c_ahead = commit(&local, "ahead.txt", "local\n", "local ahead");
    let r_ahead = spawn_worktree(&state_dir, &local, "w-ahead", "ahead");
    let wt_ahead = Path::new(r_ahead["data"]["value"]["seat"]["worktree_path"].as_str().unwrap());
    assert_eq!(git(wt_ahead, &["rev-parse", "HEAD"]), c_ahead);
    assert_eq!(git(&local, &["rev-parse", "ahead"]), c_ahead);
    assert!(!r_ahead["next"].as_str().unwrap_or("").contains("diverged"));

    // 3. Diverged: uses local commit and puts a warning in the spawn receipt's `next`.
    git(&local, &["checkout", "-b", "div", "main"]);
    git(&local, &["push", "-u", "origin", "div"]);
    git(&pusher, &["fetch", "origin", "div"]);
    git(&pusher, &["checkout", "-b", "div", "origin/div"]);
    commit(&pusher, "remote.txt", "remote\n", "remote diverge");
    git(&pusher, &["push", "origin", "div"]);
    let c_div_local = commit(&local, "local.txt", "local\n", "local diverge");

    let r_div = spawn_worktree(&state_dir, &local, "w-div", "div");
    let wt_div = Path::new(r_div["data"]["value"]["seat"]["worktree_path"].as_str().unwrap());
    assert_eq!(git(wt_div, &["rev-parse", "HEAD"]), c_div_local);
    assert_eq!(git(&local, &["rev-parse", "div"]), c_div_local);
    let next = r_div["next"].as_str().unwrap_or("");
    assert!(next.contains("diverged") && next.contains("div"), "expected divergence warning in next: {next}");

    // 4. No upstream: behaves as before.
    git(&local, &["checkout", "-b", "solo", "main"]);
    let c_solo = commit(&local, "solo.txt", "solo\n", "no upstream");
    let r_solo = spawn_worktree(&state_dir, &local, "w-solo", "solo");
    let wt_solo = Path::new(r_solo["data"]["value"]["seat"]["worktree_path"].as_str().unwrap());
    assert_eq!(git(wt_solo, &["rev-parse", "HEAD"]), c_solo);
    assert!(!r_solo["next"].as_str().unwrap_or("").contains("diverged"));

    // 5. Failed fetch is non-fatal: broken remote URL still succeeds using the cached tracking ref.
    git(&local, &["remote", "set-url", "origin", &tmp.path().join("missing.git").display().to_string()]);
    let r_offline = spawn_worktree(&state_dir, &local, "w-offline", "main");
    let wt_offline = Path::new(r_offline["data"]["value"]["seat"]["worktree_path"].as_str().unwrap());
    assert_eq!(git(wt_offline, &["rev-parse", "HEAD"]), c2);
    assert_eq!(git(&local, &["rev-parse", "main"]), c1);
}
