//! End-to-end integration tests for `orcherd-cli` (`skills`, `mod`, `worktree`, `remote`, and CLI flow).

use crate::{
    daemon, mod_install, remote, skills,
    worktree::{self, WorktreeSpec},
};
use orcherd_core::{
    Actor, Address, Op, ReceiptData, RequestEnvelope, SeatRole, Store, TemplateKind,
};
use orcherd_hc::{
    event::{Ended, Event, Notice, TerminalChange},
    wire::{Status, StatusState},
};
use std::{
    fs,
    time::{Duration, Instant},
};
use tempfile::TempDir;

#[test]
fn skills_get_and_install_work_without_drift() {
    let list = skills::list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "orcherd");
    assert_eq!(list[0].references.len(), 6);

    let base = skills::get(Some("orcherd"), None, false).unwrap();
    assert!(base.content.contains("ORCHERD inbox"));
    assert!(base.content.contains("references/supervisor-loop.md"));

    let remote_ref = skills::get(
        Some("orcherd"),
        Some("references/placement-and-remote.md"),
        false,
    )
    .unwrap();
    assert!(remote_ref.content.contains("bootstrap_symlinks"));

    let tmp = TempDir::new().unwrap();
    let installed = skills::install(Some(tmp.path())).unwrap();
    assert_eq!(installed.installed_paths.len(), 1);
    let on_disk = fs::read_to_string(&installed.installed_paths[0]).unwrap();
    assert!(on_disk.contains("ORCHERD skills get orcherd"));
}

#[test]
fn mod_install_writes_herdr_app_mod_and_clears_luau_leftovers() {
    let tmp = TempDir::new().unwrap();
    let stale = tmp.path().join("orcherd");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("main.luau"), "-- old").unwrap();
    fs::write(stale.join("mod.toml"), "name = 'orcherd'").unwrap();

    let installed = mod_install::install(Some(tmp.path())).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(installed.mod_dir.join("mod.json")).unwrap()).unwrap();
    assert_eq!(manifest["place"], "right");
    let main = fs::read_to_string(installed.mod_dir.join("main.js")).unwrap();
    assert!(main.contains("export default class"));
    assert!(installed.mod_dir.join("icon.svg").is_file());
    assert!(!installed.mod_dir.join("main.luau").exists());
    assert!(!installed.mod_dir.join("mod.toml").exists());
    assert_eq!(installed.removed_legacy, None);
}

#[test]
fn worktree_bootstrap_symlinks_and_copies_files() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    fs::write(src.path().join(".env"), "SECRET=1\n").unwrap();
    fs::create_dir_all(src.path().join("formats/gta6/work")).unwrap();
    fs::write(src.path().join("formats/gta6/work/index.txt"), "clip1").unwrap();

    let symlinks = vec![".env".to_owned()];
    let copies = vec!["formats/gta6/work".to_owned()];

    let linked = worktree::apply_bootstrap_symlinks(src.path(), dst.path(), &symlinks).unwrap();
    assert_eq!(linked, vec![".env".to_owned()]);
    assert_eq!(
        fs::read_to_string(dst.path().join(".env")).unwrap(),
        "SECRET=1\n"
    );

    let copied = worktree::apply_bootstrap_copies(src.path(), dst.path(), &copies).unwrap();
    assert_eq!(copied, vec!["formats/gta6/work".to_owned()]);
    assert_eq!(
        fs::read_to_string(dst.path().join("formats/gta6/work/index.txt")).unwrap(),
        "clip1"
    );

    let spec = WorktreeSpec::parse("new-child", Some("neon1")).unwrap();
    assert_eq!(
        spec,
        WorktreeSpec::NewChild {
            name: Some("neon1".to_owned())
        }
    );
}

#[cfg(unix)]
#[test]
fn remote_socket_qualifies_machine_identity_and_applies_ops() {
    use std::os::unix::net::UnixListener;

    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("store.sqlite3");
    let sock_path = tmp.path().join("m1.sock");

    let mut store = Store::open(&db_path).unwrap();
    let human = Actor::Human { terminal: None };
    store
        .apply(
            &human,
            RequestEnvelope::new(Op::Init {
                name: "fleet".to_owned(),
                template: Some(TemplateKind::Small),
                goal: Some("Cross-machine test".to_owned()),
                workspace_root: tmp.path().display().to_string(),
                bootstrap_symlinks: Vec::new(),
                bootstrap_copies: Vec::new(),
            }),
        )
        .unwrap();

    let listener = UnixListener::bind(&sock_path).unwrap();
    let sock_clone = sock_path.clone();
    let client_thread = std::thread::spawn(move || {
        let remote_actor = Actor::Seat {
            team_id: Some("fleet".to_owned()),
            seat_name: "w1".to_owned(),
            terminal: Some("t7".to_owned()),
        };
        remote::send_remote_request(
            &sock_clone,
            &remote_actor,
            RequestEnvelope::new(Op::InboxRead {
                // A non-peek read marks items read, so it is audited.
                peek: false,
                seat: None,
                limit: None,
            })
            .with_team("fleet"),
            None,
        )
        .unwrap()
    });

    let (stream, _) = listener.accept().unwrap();
    remote::handle_client_stream(&mut store, stream, Some("m1"));
    let receipt = client_thread.join().unwrap();
    assert!(matches!(receipt.data, ReceiptData::Inbox(_)));

    // Audit row must record the qualified remote terminal `m1/t7`.
    let log_receipt = store
        .apply(
            &human,
            RequestEnvelope::new(Op::AuditLog { limit: Some(5) }).with_team("fleet"),
        )
        .unwrap();
    let ReceiptData::Audit(rows) = log_receipt.data else {
        panic!("expected audit rows");
    };
    assert!(
        rows.iter()
            .any(|r| r.actor == "w1" && r.terminal.as_deref() == Some("m1/t7"))
    );
}

#[test]
fn daemon_notice_auto_escalates_on_blocked_and_unexpected_exit() {
    let mut store = Store::open_in_memory().unwrap();
    let human = Actor::Human { terminal: None };
    store
        .apply(
            &human,
            RequestEnvelope::new(Op::Init {
                name: "ksquared".to_owned(),
                template: Some(TemplateKind::StudioCrew),
                goal: Some("Ship W38".to_owned()),
                workspace_root: "/tmp/ksquared".to_owned(),
                bootstrap_symlinks: Vec::new(),
                bootstrap_copies: Vec::new(),
            }),
        )
        .unwrap();

    // Spawn builder-a on a task and bind it to terminal `t4`.
    let prep = store
        .apply(
            &human,
            RequestEnvelope::new(Op::SpawnPrepare {
                seat: "builder-a".to_owned(),
                role: Some(SeatRole::Worker),
                agent: Some("antigravity".to_owned()),
                model: Some("gemini-4-neon-high".to_owned()),
                account: None,
                effort: None,
                worktree_path: Some("/tmp/ksquared-wt/builder-a".to_owned()),
                branch: Some("orcherd/builder-a".to_owned()),
                machine: None,
                task_id: None,
                spec: Some("Cut A brief".to_owned()),
                title: Some("Build Cut A".to_owned()),
                keep_open: Some(true),
                resume_session: None,
                retry_of: None,
            })
            .with_team("ksquared"),
        )
        .unwrap();

    let ReceiptData::SpawnPrepared { dispatch, .. } = prep.data else {
        panic!("expected SpawnPrepared");
    };
    let dispatch_id = dispatch.unwrap().dispatch_id;

    store
        .apply(
            &human,
            RequestEnvelope::new(Op::SpawnBindAddress {
                seat: "builder-a".to_owned(),
                dispatch_id: Some(dispatch_id),
                address: Address::Core {
                    machine: None,
                    terminal: "t4".to_owned(),
                },
                worktree_path: None,
                branch: None,
                resume_session: None,
            })
            .with_team("ksquared"),
        )
        .unwrap();

    // Simulate herdr-core reporting t4 active, then StatusState::Blocked (auth dialog).
    let mut backoff = daemon::RingBackoff::default();
    let t0 = Instant::now();
    daemon::handle_core_notice(
        &mut store,
        &mut backoff,
        Notice::Event(Event {
            seq: 1,
            terminal: "t4".to_owned(),
            change: TerminalChange::Status {
                reported: true,
                status: Some(Status {
                    app: "antigravity".to_owned(),
                    kind: None,
                    msg: String::new(),
                    progress: None,
                    state: StatusState::Working,
                    title: String::new(),
                }),
            },
        }),
        t0,
    );
    daemon::handle_core_notice(
        &mut store,
        &mut backoff,
        Notice::Event(Event {
            seq: 2,
            terminal: "t4".to_owned(),
            change: TerminalChange::Status {
                reported: true,
                status: Some(Status {
                    app: "antigravity".to_owned(),
                    kind: Some("auth".to_owned()),
                    msg: "Sign in required".to_owned(),
                    progress: None,
                    state: StatusState::Blocked,
                    title: String::new(),
                }),
            },
        }),
        t0 + Duration::from_secs(1),
    );

    // Director's inbox must now contain the auto-escalation item.
    let dir_actor = Actor::Seat {
        team_id: None,
        seat_name: "director".to_owned(),
        terminal: None,
    };
    let inbox = store
        .apply(
            &dir_actor,
            RequestEnvelope::new(Op::InboxRead {
                peek: false,
                seat: None,
                limit: None,
            })
            .with_team("ksquared"),
        )
        .unwrap();
    let ReceiptData::Inbox(items) = inbox.data else {
        panic!("expected inbox");
    };
    assert!(
        items
            .iter()
            .any(|i| i.kind == orcherd_core::InboxKind::Escalation
                && i.body.contains("Sign in required"))
    );

    // Now simulate t4 exiting unexpectedly before calling `orcherd done`.
    daemon::handle_core_notice(
        &mut store,
        &mut backoff,
        Notice::Event(Event {
            seq: 3,
            terminal: "t4".to_owned(),
            change: TerminalChange::Finished(Ended {
                exit_code: Some(1),
                reason: Some("exited".to_owned()),
                signal: None,
            }),
        }),
        t0 + Duration::from_secs(2),
    );

    let inbox2 = store
        .apply(
            &dir_actor,
            RequestEnvelope::new(Op::InboxRead {
                peek: false,
                seat: None,
                limit: None,
            })
            .with_team("ksquared"),
        )
        .unwrap();
    let ReceiptData::Inbox(items2) = inbox2.data else {
        panic!("expected inbox");
    };
    assert!(
        items2
            .iter()
            .any(|i| i.kind == orcherd_core::InboxKind::Escalation && i.body.contains("exited"))
    );
}

#[test]
fn daemon_startup_grace_and_supervisor_self_escalation_guard() {
    let mut store = Store::open_in_memory().unwrap();
    let human = Actor::Human { terminal: None };
    store
        .apply(
            &human,
            RequestEnvelope::new(Op::Init {
                name: "ksquared".to_owned(),
                template: Some(TemplateKind::StudioCrew),
                goal: Some("Escalation hygiene".to_owned()),
                workspace_root: "/tmp/ksquared".to_owned(),
                bootstrap_symlinks: Vec::new(),
                bootstrap_copies: Vec::new(),
            }),
        )
        .unwrap();

    for (seat, term) in [("director", "t1"), ("builder-a", "t4"), ("builder-b", "t5")] {
        store
            .apply(
                &human,
                RequestEnvelope::new(Op::SeatRebind {
                    seat: seat.to_owned(),
                    address: Address::Core { machine: None, terminal: term.to_owned() },
                })
                .with_team("ksquared"),
            )
            .unwrap();
    }

    let status_ev = |seq, term: &str, state, kind: Option<&str>, msg: &str| {
        Notice::Event(Event {
            seq,
            terminal: term.to_owned(),
            change: TerminalChange::Status {
                reported: true,
                status: Some(Status {
                    app: "antigravity".to_owned(),
                    kind: kind.map(str::to_owned),
                    msg: msg.to_owned(),
                    progress: None,
                    state,
                    title: String::new(),
                }),
            },
        })
    };
    let read_director = |store: &mut Store| {
        let dir = Actor::Seat { team_id: None, seat_name: "director".to_owned(), terminal: None };
        let ReceiptData::Inbox(items) = store
            .apply(&dir, RequestEnvelope::new(Op::InboxRead { peek: false, seat: None, limit: None }).with_team("ksquared"))
            .unwrap()
            .data
        else {
            panic!("expected inbox");
        };
        items
    };

    let mut backoff = daemon::RingBackoff::default();
    let t0 = Instant::now();

    // 1. Transient startup auth blip on t4 clears to Working -> never escalates.
    daemon::handle_core_notice(&mut store, &mut backoff, status_ev(1, "t4", StatusState::Blocked, Some("auth"), "Sign in required"), t0);
    daemon::handle_core_notice(&mut store, &mut backoff, status_ev(2, "t4", StatusState::Working, None, ""), t0 + Duration::from_secs(3));

    // 2. Supervisor blocked on a question for its human -> never self-escalates.
    daemon::handle_core_notice(&mut store, &mut backoff, status_ev(3, "t1", StatusState::Working, None, ""), t0);
    daemon::handle_core_notice(&mut store, &mut backoff, status_ev(4, "t1", StatusState::Blocked, Some("question"), "Ship it?"), t0 + Duration::from_secs(1));

    // 3. Persistent startup block on t5 waits out STARTUP_GRACE, then escalates once.
    daemon::handle_core_notice(&mut store, &mut backoff, status_ev(5, "t5", StatusState::Blocked, Some("auth"), "Sign in required"), t0);
    assert_eq!(backoff.flush_pending_blocks(&mut store, t0 + Duration::from_secs(5)), 0);
    assert!(read_director(&mut store).is_empty());

    assert_eq!(backoff.flush_pending_blocks(&mut store, t0 + daemon::STARTUP_GRACE), 1);
    let items = read_director(&mut store);
    assert_eq!(items.len(), 1, "only t5's persistent block should escalate: {items:?}");
    assert_eq!(items[0].kind, orcherd_core::InboxKind::Escalation);
    assert!(items[0].body.contains("builder-b") && items[0].body.contains("Sign in required"));
}

#[test]
fn antigravity_seats_launch_agy_without_permission_prompts() {
    use crate::agent_argv::build_agent_argv;
    use std::collections::BTreeMap;

    let none = BTreeMap::new();
    assert_eq!(
        build_agent_argv("antigravity", Some("gemini-4-neon-high"), None, &none),
        ["agy", "--dangerously-skip-permissions", "--model", "gemini-4-neon-high"]
    );
    assert_eq!(
        build_agent_argv("agy", None, Some("latest"), &none),
        ["agy", "--dangerously-skip-permissions", "--continue"]
    );
    assert_eq!(
        build_agent_argv("antigravity", None, Some("conv-1"), &none),
        ["agy", "--dangerously-skip-permissions", "--resume", "conv-1"]
    );
    assert_eq!(
        build_agent_argv("claude", None, Some("latest"), &none),
        ["claude", "--resume"]
    );
}

#[test]
fn orcherd_toml_agent_argv_overrides_base_and_appends_model_and_resume() {
    use crate::agent_argv::build_agent_argv;

    let tmp = TempDir::new().unwrap();

    // Without `.orcherd.toml`, built-in defaults are unchanged.
    let empty_cfg = worktree::load_project_config(tmp.path()).unwrap();
    assert!(empty_cfg.agents.is_empty());
    assert_eq!(
        build_agent_argv("antigravity", Some("m1"), Some("latest"), &empty_cfg.agents),
        ["agy", "--dangerously-skip-permissions", "--model", "m1", "--continue"]
    );
    assert_eq!(
        build_agent_argv("codex", None, None, &empty_cfg.agents),
        ["codex"]
    );

    fs::write(
        tmp.path().join(".orcherd.toml"),
        r#"
[team]
name = "demo"

[agents.antigravity]
argv = ["/opt/bin/agy", "--custom-flag"]

[agents.claude]
argv = ["claude", "--dangerously-skip-permissions"]

[agents.codex]
"#,
    )
    .unwrap();

    let cfg = worktree::load_project_config(tmp.path()).unwrap();
    assert_eq!(
        build_agent_argv(
            "antigravity",
            Some("gemini-4-neon-high"),
            Some("latest"),
            &cfg.agents,
        ),
        ["/opt/bin/agy", "--custom-flag", "--model", "gemini-4-neon-high", "--continue"]
    );
    // Harness aliases resolve to their canonical `[agents.<id>]` entry when no exact key exists.
    assert_eq!(
        build_agent_argv("agy", None, Some("conv-9"), &cfg.agents),
        ["/opt/bin/agy", "--custom-flag", "--resume", "conv-9"]
    );
    assert_eq!(
        build_agent_argv("claude", Some("opus"), Some("latest"), &cfg.agents),
        ["claude", "--dangerously-skip-permissions", "--model", "opus", "--resume"]
    );
    assert_eq!(
        build_agent_argv("claude", None, Some("sess-1"), &cfg.agents),
        ["claude", "--dangerously-skip-permissions", "--resume", "sess-1"]
    );
    // An agent table without `argv` or an unlisted agent keeps the built-in base.
    assert_eq!(
        build_agent_argv("codex", Some("o3"), None, &cfg.agents),
        ["codex", "--model", "o3"]
    );
    assert_eq!(
        build_agent_argv("zcode", None, None, &cfg.agents),
        ["zcode"]
    );

    // An exact `[agents.agy]` entry wins over `[agents.antigravity]` when spawning `--agent agy`.
    fs::write(
        tmp.path().join(".orcherd.toml"),
        r#"
[agents.antigravity]
argv = ["agy", "--from-canonical"]

[agents.agy]
argv = ["custom-wrapper", "--from-exact"]
"#,
    )
    .unwrap();
    let cfg = worktree::load_project_config(tmp.path()).unwrap();
    assert_eq!(
        build_agent_argv("agy", None, Some("latest"), &cfg.agents),
        ["custom-wrapper", "--from-exact", "--resume"]
    );
    assert_eq!(
        build_agent_argv("antigravity", None, Some("latest"), &cfg.agents),
        ["agy", "--from-canonical", "--continue"]
    );

    // Empty or blank `argv` overrides are rejected on config load.
    fs::write(
        tmp.path().join(".orcherd.toml"),
        "[agents.antigravity]\nargv = []\n",
    )
    .unwrap();
    assert!(worktree::load_project_config(tmp.path()).is_err());

    fs::write(
        tmp.path().join(".orcherd.toml"),
        "[agents.antigravity]\nargv = [\"  \"]\n",
    )
    .unwrap();
    assert!(worktree::load_project_config(tmp.path()).is_err());
}

#[test]
fn doorbells_ring_seats_that_end_turns_as_done() {
    use crate::daemon::{Ring, ring_condition};
    use orcherd_hc::wire::StatusState;
    // agy ends a turn as `done` and stays there; gating on `idle` alone never rang it (#12).
    assert_eq!(ring_condition(Some(StatusState::Done)), Ring::Now);
    assert_eq!(ring_condition(Some(StatusState::Idle)), Ring::IfIdle);
    assert_eq!(ring_condition(Some(StatusState::Working)), Ring::Wait);
    assert_eq!(ring_condition(Some(StatusState::Error)), Ring::Now);
    assert_eq!(ring_condition(None), Ring::Now);
}

#[test]
fn team_resolution_precedence_and_project_config_git_boundary() {
    use crate::identity::resolve_team_ref_in;

    let tmp = TempDir::new().unwrap();
    let parent = tmp.path();
    let repo = parent.join("repo");
    let deep = repo.join("sub").join("deep");
    let other_repo = parent.join("other-repo");
    let other_src = other_repo.join("src");
    let unrelated = parent.join("unrelated");

    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(&deep).unwrap();
    fs::create_dir_all(&other_src).unwrap();
    fs::create_dir_all(&unrelated).unwrap();
    // Simulate a git worktree `.git` file in `other_repo` with no `.orcherd.toml`.
    fs::write(other_repo.join(".git"), "gitdir: /nowhere\n").unwrap();
    // Parent config must not leak across `other_repo`'s git boundary.
    fs::write(parent.join(".orcherd.toml"), "[team]\nname = \"parent-team\"\n").unwrap();
    fs::write(repo.join(".orcherd.toml"), "[team]\nname = \"toml-team\"\n").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let human = Actor::Human { terminal: None };
    let init_team = |store: &mut Store, name: &str, root: &std::path::Path| -> String {
        store
            .apply(
                &human,
                RequestEnvelope::new(Op::Init {
                    name: name.to_owned(),
                    template: Some(TemplateKind::Small),
                    goal: None,
                    workspace_root: root.display().to_string(),
                    bootstrap_symlinks: Vec::new(),
                    bootstrap_copies: Vec::new(),
                }),
            )
            .unwrap()
            .team_id
    };

    let _toml_id = init_team(&mut store, "toml-team", &repo);
    let _newer_same_root_id = init_team(&mut store, "newer-same-root", &repo);
    let other_id = init_team(&mut store, "other-team", &other_repo);
    let _newest_elsewhere = init_team(&mut store, "newest-elsewhere", std::path::Path::new("/elsewhere"));

    // 1. `--team` beats `ORCHERD_TEAM`, `.orcherd.toml`, and `workspace_root`.
    assert_eq!(
        resolve_team_ref_in(Some("flag-team"), Some("env-team"), Some(&deep), Some(&store)).unwrap(),
        Some("flag-team".to_owned())
    );
    // 2. `ORCHERD_TEAM` beats `.orcherd.toml` and `workspace_root`.
    assert_eq!(
        resolve_team_ref_in(None, Some("env-team"), Some(&deep), Some(&store)).unwrap(),
        Some("env-team".to_owned())
    );
    // 3. `.orcherd.toml` (walked up from `deep` to git root) beats `workspace_root` even when
    //    another newer team is registered at the same `workspace_root`.
    assert_eq!(
        resolve_team_ref_in(None, None, Some(&deep), Some(&store)).unwrap(),
        Some("toml-team".to_owned())
    );
    // 4. Walking up stops at `.git`, so `other_repo` ignores `parent/.orcherd.toml` and resolves
    //    via `workspace_root`.
    assert_eq!(
        resolve_team_ref_in(None, None, Some(&other_src), Some(&store)).unwrap(),
        Some(other_id)
    );
    // 5. Outside any matching repo, returns `None` so `Store::resolve_team(None)` picks the newest team.
    let outside = TempDir::new().unwrap();
    assert_eq!(
        resolve_team_ref_in(None, None, Some(outside.path()), Some(&store)).unwrap(),
        None
    );
}

#[test]
fn model_matcher_handles_display_names_and_rejects_mismatches() {
    use crate::model_check::{extract_observed_model, model_matches};

    // Positive matches
    assert!(model_matches("gemini-4-neon-high", "Gemini 4 Neon (High)"));
    assert!(model_matches("gemini-4-neon-high", "Gemini 4 Neon · high"));
    assert!(model_matches("gemini-4-neon", "Gemini 4 Neon (High)"));
    assert!(model_matches("gemini-2.5-pro", "Gemini 2.5 Pro"));
    assert!(model_matches("claude-opus-4-6", "Claude Opus 4.6"));
    assert!(model_matches("claude-opus-4-6", "Opus 4.6"));
    assert!(model_matches("claude-sonnet-4-5-20250929", "Sonnet 4.5"));
    assert!(model_matches("opus", "Claude Opus 4.6"));
    assert!(model_matches("gpt-5.4", "GPT-5.4 (High)"));
    assert!(model_matches("gpt-5.4-high", "gpt-5.4 high"));

    // Negative mismatches
    assert!(!model_matches("gemini-4-neon-high", "Claude Opus 5.5"));
    assert!(!model_matches("gemini-4-neon-high", "Gemini 4 Neon (Low)"));
    assert!(!model_matches("gemini-4-neon-high", "Gemini 4 Flash (High)"));
    assert!(!model_matches("claude-opus-4-6", "Claude Sonnet 4.6"));
    assert!(!model_matches("claude-opus-4-6", "Claude Opus 4.5"));
    assert!(!model_matches("gpt-5.4", "GPT-5.3"));

    // Compact agy header (with ANSI color codes)
    let compact = "\x1b[1mAntigravity CLI 1.5.17 · Gemini 4 Neon (High)\x1b[0m\n> ";
    let obs = extract_observed_model("antigravity", compact).unwrap();
    assert_eq!(obs.model, "Gemini 4 Neon (High)");
    assert_eq!(obs.line, "Antigravity CLI 1.5.17 · Gemini 4 Neon (High)");

    // Multi-line agy ASCII logo banner
    let banner = "\
      ▄▀▀▄        Antigravity CLI 1.5.18
     ▀▀▀▀▀▀       user@example.com (Google AI Ultra)
    ▀▀▀▀▀▀▀▀      Gemini 4 Neon (High)
   ▄▀▀    ▀▀▄     ~/repos/orcherd
";
    let obs = extract_observed_model("agy", banner).unwrap();
    assert_eq!(obs.model, "Gemini 4 Neon (High)");
    assert_eq!(obs.line, "Antigravity CLI 1.5.18 · Gemini 4 Neon (High)");

    // agy footer status bar fallback
    let footer = "  Press up to edit queued messages          auto · Gemini 4 Neon · high\n";
    let obs = extract_observed_model("antigravity", footer).unwrap();
    assert!(model_matches("gemini-4-neon-high", &obs.model));

    // Transient startup/auth/status lines beneath the banner must NOT be misparsed as models.
    assert!(
        extract_observed_model(
            "antigravity",
            "Antigravity CLI 1.5.18\nYou are not logged into Antigravity\n"
        )
        .is_none()
    );
    assert!(extract_observed_model("antigravity", "Antigravity CLI 1.5.18\nLoading...\n").is_none());
    assert!(
        extract_observed_model("antigravity", "Antigravity CLI 1.5.18\nSign in required\n")
            .is_none()
    );
    assert!(
        extract_observed_model("antigravity", "Antigravity CLI 1.5.18\nUpdate available\n")
            .is_none()
    );

    // Unrecognisable screen and other harnesses return None.
    assert!(extract_observed_model("antigravity", "Loading...\n> ").is_none());
    assert!(extract_observed_model("claude", "Claude Code v2.1.50\nOpus 4.6 · Claude Max\n").is_none());
}

#[test]
fn spawn_model_verification_passes_mismatches_and_records_unverified() {
    use crate::{
        model_check::{apply_model_check, verify_model_with_reader},
        output::format_receipt,
    };
    use orcherd_core::{ModelCheckStatus, Receipt, Seat};
    use std::time::Duration;

    let make_receipt = || Receipt {
        request_id: None,
        team_id: "team-1".to_owned(),
        actor: "super".to_owned(),
        next: Some("orcherd inbox".to_owned()),
        failed_stage: None,
        data: ReceiptData::SpawnPrepared {
            seat: Box::new(Seat {
                team_id: "team-1".to_owned(),
                seat_id: "seat-1".to_owned(),
                name: "neon3".to_owned(),
                role: SeatRole::Worker,
                agent: "antigravity".to_owned(),
                model: Some("gemini-4-neon-high".to_owned()),
                account: None,
                effort: None,
                address: Address::None,
                worktree_path: None,
                branch: None,
                keep_open: true,
                resume_session: None,
                standing_brief: None,
                paused_at: None,
                last_seen_at: None,
                created_at: String::new(),
                updated_at: String::new(),
            }),
            task: None,
            dispatch: None,
            warm_reuse: false,
            model_check: None,
        },
    };

    // 1. Matching screen passes (verified), even after a transient startup/auth frame.
    let mut polls = 0;
    let check = verify_model_with_reader(
        "antigravity",
        "gemini-4-neon-high",
        Duration::from_millis(200),
        Duration::from_millis(5),
        || {
            polls += 1;
            if polls == 1 {
                Some("Antigravity CLI 1.5.18\nYou are not logged into Antigravity\n".to_owned())
            } else {
                Some("Antigravity CLI 1.5.17 · Gemini 4 Neon (High)\n".to_owned())
            }
        },
    );
    assert_eq!(check.status, ModelCheckStatus::Verified);
    let mut receipt = make_receipt();
    assert!(!apply_model_check(&mut receipt, "t23", "antigravity", check));
    assert_eq!(receipt.failed_stage, None);
    let formatted = format_receipt(&receipt, false);
    assert!(formatted.contains("verified: Antigravity CLI 1.5.17 · Gemini 4 Neon (High)"));

    // 2. Mismatching screen fails loudly with `model_mismatch` and observed line in `next`.
    let check = verify_model_with_reader(
        "antigravity",
        "gemini-4-neon-high",
        Duration::from_millis(200),
        Duration::from_millis(5),
        || Some("Antigravity CLI 1.1.7 · Claude Opus 5.5\n".to_owned()),
    );
    assert_eq!(check.status, ModelCheckStatus::Mismatch);
    let mut receipt = make_receipt();
    assert!(apply_model_check(&mut receipt, "t23", "antigravity", check));
    assert_eq!(receipt.failed_stage.as_deref(), Some("model_mismatch"));
    let next = receipt.next.as_deref().unwrap();
    assert!(next.contains("Antigravity CLI 1.1.7 · Claude Opus 5.5"));
    assert!(next.contains("terminal stopped"));
    assert!(next.contains("orcherd preflight --agent antigravity"));

    // 3. Screen with no recognisable model line is recorded as unverified, without failing.
    let check = verify_model_with_reader(
        "antigravity",
        "gemini-4-neon-high",
        Duration::from_millis(15),
        Duration::from_millis(5),
        || Some("Antigravity CLI 1.5.18\nLoading...\n".to_owned()),
    );
    assert_eq!(check.status, ModelCheckStatus::Unverified);
    assert_eq!(check.observed, None);
    let mut receipt = make_receipt();
    assert!(!apply_model_check(&mut receipt, "t23", "antigravity", check));
    assert_eq!(receipt.failed_stage, None);
    let formatted = format_receipt(&receipt, false);
    assert!(formatted.contains("Model: gemini-4-neon-high (unverified)"));

    // 4. Harnesses without a model screen parser return unverified immediately.
    let mut unknown_polls = 0;
    let check = verify_model_with_reader(
        "claude",
        "opus",
        Duration::from_secs(5),
        Duration::from_millis(5),
        || {
            unknown_polls += 1;
            Some(String::new())
        },
    );
    assert_eq!(check.status, ModelCheckStatus::Unverified);
    assert_eq!(unknown_polls, 0);
}

#[test]
fn preflight_parses_probe_output_and_flags_path_and_version_drift() {
    use crate::preflight::{ProbeResult, build_report, parse_probe_output};

    let hc = parse_probe_output("/home/u/.local/bin/agy\n1.1.7\n");
    let shell = parse_probe_output("/home/linuxbrew/.linuxbrew/bin/agy\n1.5.18\n");
    assert_eq!(hc.path.as_deref(), Some("/home/u/.local/bin/agy"));
    assert_eq!(hc.version.as_deref(), Some("1.1.7"));

    let drifted = build_report(
        "antigravity".to_owned(),
        "agy".to_owned(),
        None,
        hc.clone(),
        shell.clone(),
    );
    assert!(drifted.path_drift);
    assert!(drifted.version_drift);

    let matched = build_report(
        "antigravity".to_owned(),
        "agy".to_owned(),
        None,
        shell.clone(),
        shell,
    );
    assert!(!matched.path_drift);
    assert!(!matched.version_drift);

    let missing = parse_probe_output("\n");
    assert_eq!(
        missing,
        ProbeResult {
            error: None,
            path: None,
            version: None,
        }
    );
}
