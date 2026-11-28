//! Integration tests for preserving a seat's `worktree_path` when `--worktree` is omitted.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(unix)]
mod unix {
    use orcherd_hc::{
        Method,
        frame::{Decoder, Kind, encode},
    };
    use serde_json::{Value, json};
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        os::unix::net::{UnixListener, UnixStream},
        path::PathBuf,
        process::Command,
        sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
        thread::{self, JoinHandle},
        time::Duration,
    };
    use tempfile::TempDir;

    #[derive(Default)]
    struct CoreState {
        live: Vec<Value>,
        next_term: u64,
        run_cwds: Vec<Option<String>>,
    }

    /// Minimal framed `herdr-core` mock tracking live terminals and `terminal.run` cwds.
    struct FakeCore {
        sock: PathBuf,
        state: Arc<Mutex<CoreState>>,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl FakeCore {
        fn start(sock: PathBuf) -> Self {
            let listener = UnixListener::bind(&sock).unwrap();
            let state = Arc::new(Mutex::new(CoreState::default()));
            let stop = Arc::new(AtomicBool::new(false));
            let (st, stp) = (Arc::clone(&state), Arc::clone(&stop));
            let handle = thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    if stp.load(Ordering::Acquire) { break; }
                    serve(stream, &st);
                }
            });
            Self { sock, state, stop, handle: Some(handle) }
        }

        fn run_cwds(&self) -> Vec<Option<String>> { self.state.lock().unwrap().run_cwds.clone() }
        fn clear_live(&self) { self.state.lock().unwrap().live.clear(); }
    }

    impl Drop for FakeCore {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let _ = UnixStream::connect(&self.sock);
            if let Some(h) = self.handle.take() { let _ = h.join(); }
        }
    }

    fn serve(stream: UnixStream, state: &Arc<Mutex<CoreState>>) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new(stream);
        if reader.read_line(&mut String::new()).unwrap_or(0) == 0 { return; }
        let methods: Vec<&str> = Method::ALL.iter().map(|m| m.as_str()).collect();
        let hello = json!({
            "jsonrpc": "2.0", "id": 0,
            "result": {
                "generation": 1, "version": "0.1.13", "framed": true, "features": [],
                "core": { "id": "fake-core", "name": "test-host" }, "methods": methods,
            }
        });
        let mut hello_bytes = serde_json::to_vec(&hello).unwrap();
        hello_bytes.push(b'\n');
        if reader.get_mut().write_all(&hello_bytes).is_err() { return; }

        let (mut decoder, mut buf) = (Decoder::default(), [0u8; 4096]);
        loop {
            while let Ok(Some(f)) = decoder.next_frame() {
                if f.channel != 0 || f.kind != Kind::Data { continue; }
                let Ok(req) = serde_json::from_slice::<Value>(&f.payload) else { continue };
                let result = handle_rpc(req["method"].as_str().unwrap_or(""), &req["params"], state);
                let reply = json!({ "jsonrpc": "2.0", "id": req["id"], "result": result });
                let wire = encode(0, Kind::Data, &serde_json::to_vec(&reply).unwrap()).unwrap();
                if reader.get_mut().write_all(&wire).is_err() { return; }
            }
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => decoder.push(&buf[..n]),
            }
        }
    }

    fn handle_rpc(method: &str, params: &Value, state: &Arc<Mutex<CoreState>>) -> Value {
        match method {
            "terminal.list" => json!({ "terminals": state.lock().unwrap().live.clone() }),
            "terminal.run" => {
                let mut g = state.lock().unwrap();
                let cwd = params["cwd"].as_str().map(str::to_owned);
                g.run_cwds.push(cwd.clone());
                g.next_term += 1;
                let term = json!({
                    "address": format!("t{}", g.next_term), "labels": params["labels"],
                    "argv": params["argv"], "cwd": cwd.unwrap_or_default(),
                    "state": "live", "created_ms": 1, "screen": "", "cols": 80, "rows": 24,
                });
                g.live.push(term.clone());
                term
            }
            "terminal.wait" => json!({ "ended": true, "reason": "status", "status": "idle", "turn": 0 }),
            "terminal.send" => json!({ "started": true, "status": "working", "turn": 1 }),
            _ => json!({}),
        }
    }

    struct Harness {
        repo: PathBuf,
        state_dir: PathBuf,
        core: FakeCore,
        // Dropped last so `hc.sock` still exists when `FakeCore::drop` wakes `accept()`.
        _tmp: TempDir,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let (repo, state_dir) = (tmp.path().join("repo"), tmp.path().join("state"));
            fs::create_dir_all(&repo).unwrap();
            fs::write(repo.join("README.md"), "hello\n").unwrap();
            for args in [&["init", "--initial-branch=main"][..], &["add", "README.md"][..], &["commit", "-m", "init"][..]] {
                let out = Command::new("git").current_dir(&repo)
                    .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
                    .args(args).output().unwrap();
                assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            }
            let core = FakeCore::start(tmp.path().join("hc.sock"));
            let h = Self { repo, state_dir, core, _tmp: tmp };
            h.run(&["init", "--name", "wt-keep", "--template", "small", "--json"]);
            h
        }

        fn run(&self, args: &[&str]) -> Value {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_orcherd"));
            cmd.current_dir(&self.repo).args(args)
                .env("ORCHERD_STATE_DIR", &self.state_dir)
                .env("ORCHERD_NO_AUTO_DAEMON", "1")
                .env("HC_SOCKET", &self.core.sock);
            for var in ["ORCHERD_TEAM", "ORCHERD_SEAT", "ORCHERD_DISPATCH", "ORCHERD_REMOTE_SOCKET", "HC_TERMINAL_ID", "HERDR_PANE_ID", "HC_BINARY"] {
                cmd.env_remove(var);
            }
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "args={args:?}\nstdout={}\nstderr={}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            serde_json::from_slice(&out.stdout).unwrap()
        }

        fn new_task(&self, spec: &str) -> String {
            self.run(&["task", "create", "--spec", spec, "--json"])["data"]["value"]["task_id"].as_str().unwrap().to_owned()
        }
    }

    fn assert_worktree(receipt: &Value, expected: &str) {
        assert_eq!(receipt["data"]["value"]["seat"]["worktree_path"].as_str(), Some(expected));
        assert_eq!(receipt["data"]["value"]["dispatch"]["worktree_path"].as_str(), Some(expected));
    }

    #[test]
    fn spawn_without_worktree_keeps_seat_worktree_on_warm_reuse_and_cold_relaunch() {
        let h = Harness::new();
        let wt = h.repo.parent().unwrap().join("repo-wt").join("neon1").display().to_string();
        let repo = h.repo.display().to_string();

        // 1. Initial `--worktree new-child` records and launches in the child worktree.
        let first = h.run(&["spawn", "--seat", "neon1", "--worktree", "new-child", "--spec", "T1", "--no-ui", "--json"]);
        assert_worktree(&first, &wt);
        assert_eq!(first["data"]["value"]["seat"]["branch"].as_str(), Some("orcherd/neon1"));
        assert_eq!(h.core.run_cwds(), vec![Some(wt.clone())]);

        // 2. Warm reuse without `--worktree` (standing in main checkout) keeps the recorded worktree.
        let t2 = h.new_task("T2");
        let warm = h.run(&["spawn", "--seat", "neon1", "--task", &t2, "--no-ui", "--json"]);
        assert_eq!(warm["data"]["value"]["warm_reuse"].as_bool(), Some(true));
        assert_worktree(&warm, &wt);
        assert_eq!(warm["data"]["value"]["seat"]["branch"].as_str(), Some("orcherd/neon1"));
        assert_eq!(h.core.run_cwds().len(), 1);

        // 3. Cold relaunch without `--worktree` launches with the recorded worktree as `cwd`.
        h.core.clear_live();
        let t3 = h.new_task("T3");
        let cold = h.run(&["spawn", "--seat", "neon1", "--task", &t3, "--no-ui", "--json"]);
        assert_eq!(cold["data"]["value"]["warm_reuse"].as_bool(), Some(false));
        assert_worktree(&cold, &wt);
        assert_eq!(h.core.run_cwds(), vec![Some(wt.clone()), Some(wt.clone())]);

        // 4. Explicit `--worktree current` replaces the worktree (warm and subsequent cold launch).
        let t4 = h.new_task("T4");
        assert_worktree(&h.run(&["spawn", "--seat", "neon1", "--worktree", "current", "--task", &t4, "--no-ui", "--json"]), &repo);
        h.core.clear_live();
        let t5 = h.new_task("T5");
        assert_worktree(&h.run(&["spawn", "--seat", "neon1", "--task", &t5, "--no-ui", "--json"]), &repo);
        assert_eq!(h.core.run_cwds(), vec![Some(wt.clone()), Some(wt), Some(repo.clone())]);

        // 5. New and template-seeded seats without `--worktree` default to `workspace_root`.
        assert_worktree(&h.run(&["spawn", "--seat", "fresh1", "--spec", "Fresh", "--no-launch", "--json"]), &repo);
        assert_worktree(&h.run(&["spawn", "--seat", "w1", "--spec", "Seeded", "--no-ui", "--json"]), &repo);
        assert_eq!(h.core.run_cwds().last().cloned().flatten(), Some(repo));
    }
}
