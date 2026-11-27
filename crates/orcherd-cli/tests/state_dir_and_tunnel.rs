//! Integration tests verifying `orcherd init` never routes through an inbound tunnel.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(unix)]
#[test]
fn init_creates_team_locally_even_with_live_control_sock_and_empty_store() {
    use orcherd_core::Store;
    use std::{
        fs,
        os::unix::net::UnixListener,
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let repo = tmp.path().join("somalog");
    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(repo.join(".git")).unwrap();

    // Bind a live `control.sock` in an empty state dir (no local `store.sqlite3` yet),
    // simulating an inbound tunnel from another host like yeti -> m1.
    let sock_path = state_dir.join("control.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();
    listener.set_nonblocking(true).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let touched = Arc::new(AtomicBool::new(false));
    let stop_bg = Arc::clone(&stop);
    let touched_bg = Arc::clone(&touched);
    let server = thread::spawn(move || {
        while !stop_bg.load(Ordering::Acquire) {
            if listener.accept().is_ok() {
                touched_bg.store(true, Ordering::Release);
            }
            thread::sleep(Duration::from_millis(10));
        }
    });

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_orcherd"));
    cmd.current_dir(&repo)
        .args(["init", "--name", "somalog-v3", "--template", "small", "--json"])
        .env("ORCHERD_STATE_DIR", &state_dir)
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
    stop.store(true, Ordering::Release);
    server.join().unwrap();

    assert!(
        out.status.success(),
        "orcherd init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !touched.load(Ordering::Acquire),
        "orcherd init must never connect to control.sock"
    );

    let receipt: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let team_id = receipt["team_id"].as_str().unwrap();

    // Confirm the team exists in the local SQLite store on this machine.
    let store = Store::open(state_dir.join("store.sqlite3")).unwrap();
    let team = store.resolve_team(Some("somalog-v3")).unwrap();
    assert_eq!(team.team_id, team_id);
    assert_eq!(team.workspace_root, repo.display().to_string());
}
