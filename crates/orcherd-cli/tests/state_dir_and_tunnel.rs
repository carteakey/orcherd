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

#[cfg(unix)]
#[test]
fn team_not_found_points_at_sibling_store_locally_and_through_tunnel() {
    use orcherd_core::Store;
    use std::{
        fs,
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        path::Path,
        process::{Command, Output},
        thread,
    };
    use tempfile::TempDir;

    let run_in_home = |home: &Path, cwd: &Path, extra_env: &[(&str, &str)], args: &[&str]| -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_orcherd"));
        cmd.current_dir(cwd)
            .args(args)
            .env("HOME", home)
            .env("ORCHERD_NO_AUTO_DAEMON", "1");
        for var in [
            "XDG_STATE_HOME",
            "ORCHERD_STATE_DIR",
            "ORCHERD_DB_PATH",
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
    };

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let state_base = home.join(".local").join("state");
    let default_store = state_base.join("orcherd");
    let somalog_store = state_base.join("orcherd-somalog");
    let corrupt_store = state_base.join("orcherd-corrupt");
    let future_store = state_base.join("orcherd-future");
    let repo = tmp.path().join("somalog");
    fs::create_dir_all(&default_store).unwrap();
    fs::create_dir_all(&corrupt_store).unwrap();
    fs::create_dir_all(&future_store).unwrap();
    fs::create_dir_all(repo.join(".git")).unwrap();

    // Corrupt sibling store and newer-schema sibling store (both sort before `orcherd-somalog`).
    let corrupt_db = corrupt_store.join("store.sqlite3");
    let corrupt_bytes = b"not a valid sqlite database";
    fs::write(&corrupt_db, corrupt_bytes).unwrap();

    let future_db = future_store.join("store.sqlite3");
    {
        let store = Store::open(&future_db).unwrap();
        store
            .connection()
            .pragma_update(None, "user_version", 99u32)
            .unwrap();
    }

    // Create the real team in `orcherd-somalog`.
    let init_out = run_in_home(
        &home,
        &repo,
        &[("ORCHERD_STATE_DIR", somalog_store.to_str().unwrap())],
        &["init", "--name", "somalog", "--template", "small", "--json"],
    );
    assert!(
        init_out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&init_out.stdout).unwrap();
    let team_id = receipt["team_id"].as_str().unwrap().to_owned();

    // 1. Local lookup against the default store names the sibling store holding `team_id`.
    let local_miss = run_in_home(&home, &repo, &[], &["status", "--team", &team_id]);
    assert!(!local_miss.status.success());
    let local_err = String::from_utf8_lossy(&local_miss.stderr);
    let expected_hint = format!(
        "{team_id} is in {}; run with ORCHERD_STATE_DIR={}",
        somalog_store.display(),
        somalog_store.display()
    );
    assert!(
        local_err.contains(&expected_hint),
        "expected sibling hint in local error, got: {local_err}"
    );
    assert_eq!(fs::read(&corrupt_db).unwrap(), corrupt_bytes);

    // 2. When no sibling store has the team, nothing extra is suggested.
    let unknown_miss = run_in_home(&home, &repo, &[], &["status", "--team", "team-nowhere"]);
    assert!(!unknown_miss.status.success());
    let unknown_err = String::from_utf8_lossy(&unknown_miss.stderr);
    assert!(unknown_err.contains("team not found: team-nowhere"), "{unknown_err}");
    assert!(!unknown_err.contains("ORCHERD_STATE_DIR="), "{unknown_err}");

    // 3. Through an inbound tunnel (`control.sock` with empty default store), the error names
    //    both the tunnel host/socket and the local sibling store that holds the team.
    let _ = fs::remove_file(default_store.join("store.sqlite3"));
    let sock_path = default_store.join("control.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();
    let team_id_for_srv = team_id.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        let mut reader = BufReader::new(&stream);
        let _ = reader.read_line(&mut line);
        let resp = serde_json::json!({
            "ok": false,
            "host": "yeti-cachy",
            "receipt": null,
            "error": format!("team not found: {team_id_for_srv}"),
        });
        let mut bytes = serde_json::to_vec(&resp).unwrap();
        bytes.push(b'\n');
        stream.write_all(&bytes).unwrap();
        stream.flush().unwrap();
    });

    let tunneled_miss = run_in_home(
        &home,
        &repo,
        &[("ORCHERD_TEAM", &team_id), ("ORCHERD_SEAT", "w1")],
        &["inbox"],
    );
    server.join().unwrap();
    assert!(!tunneled_miss.status.success());
    let tunneled_err = String::from_utf8_lossy(&tunneled_miss.stderr);
    assert!(
        tunneled_err.contains(&format!(
            "team not found on yeti-cachy (via the tunnel at {}): {team_id}; {expected_hint}",
            sock_path.display()
        )),
        "expected tunnel + sibling hint, got: {tunneled_err}"
    );
}
