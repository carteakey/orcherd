//! Unit tests for `orcherd-core` schema, templates, transactional `apply()`, and design laws.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rusqlite::Connection;
use tempfile::tempdir;

use crate::{
    Actor, Address, Confidence, DispatchState, DoneOutcome, DoorbellOutcome, Error, GateKind,
    GateStatus, InboxKind, Op, PostKind, ReceiptData, RequestEnvelope, SCHEMA_VERSION, SeatRole,
    Store, TaskStatus, TemplateKind, VerdictKind, format_doorbell, init_schema, read_user_version,
};

fn human() -> Actor {
    Actor::Human { terminal: Some("t0".to_string()) }
}

fn seat_actor(name: &str) -> Actor {
    Actor::Seat {
        team_id: None,
        seat_name: name.to_string(),
        terminal: Some(format!("t-{name}")),
    }
}

fn init_review_team(store: &mut Store) -> String {
    store
        .apply(
            &human(),
            RequestEnvelope::new(Op::Init {
                name: "herding".to_string(),
                template: Some(TemplateKind::ReviewCrew),
                goal: Some("Ship zero-fork orchestration".to_string()),
                workspace_root: "/tmp/herding".to_string(),
                bootstrap_symlinks: vec![".env".to_string(), "node_modules".to_string()],
                bootstrap_copies: vec![],
            }),
        )
        .expect("init team")
        .team_id
}

fn spawn_prepare_op(seat: &str, spec: Option<&str>, retry_of: Option<String>) -> Op {
    Op::SpawnPrepare {
        seat: seat.to_string(),
        role: None,
        agent: None,
        model: None,
        account: None,
        effort: None,
        worktree_path: None,
        branch: None,
        machine: None,
        task_id: None,
        spec: spec.map(str::to_string),
        title: spec.map(str::to_string),
        keep_open: Some(true),
        resume_session: None,
        retry_of,
    }
}

fn task_create_op(title: &str, deps: Vec<String>, assign: Option<&str>, reviewer: Option<&str>) -> Op {
    Op::TaskCreate {
        title: Some(title.to_string()),
        spec: format!("Spec for {title}"),
        deps,
        assign: assign.map(str::to_string),
        reviewer: reviewer.map(str::to_string),
        issue_ref: None,
        parent_id: None,
        placement: None,
    }
}

#[test]
fn schema_init_and_schema_too_new_guard() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("store.sqlite3");

    let store = Store::open(&db_path).expect("open store");
    assert_eq!(read_user_version(store.connection()).expect("user_version"), SCHEMA_VERSION);

    let tables: Vec<String> = {
        let mut stmt = store
            .connection()
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("prepare");
        stmt.query_map([], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect")
    };
    for expected in [
        "audit", "bulletin", "dispatches", "doorbell", "findings", "gates",
        "inbox", "mutation_receipts", "seats", "tasks", "teams",
    ] {
        assert!(tables.iter().any(|t| t == expected), "missing table {expected}");
    }
    drop(store);

    let raw = Connection::open(&db_path).expect("raw open");
    raw.execute_batch("PRAGMA user_version = 99;").expect("bump version");
    let err = init_schema(&raw).expect_err("must reject newer schema");
    assert!(matches!(err, Error::SchemaTooNew { found: 99, supported: SCHEMA_VERSION }));
    drop(raw);

    let reopen_err = Store::open(&db_path).err().expect("Store::open must fail");
    assert!(matches!(reopen_err, Error::SchemaTooNew { found: 99, supported: SCHEMA_VERSION }));
}

#[test]
fn template_seeding_for_all_three_templates() {
    let mut store = Store::open_in_memory().expect("in-memory store");

    for (name, tpl, expected) in [
        ("t-review", TemplateKind::ReviewCrew, vec!["super", "vicesuper", "planner", "zcode1", "zcode2", "zcode3"]),
        ("t-studio", TemplateKind::StudioCrew, vec!["director", "builder-a", "builder-b", "gauntlet", "viewer-value"]),
        ("t-small", TemplateKind::Small, vec!["super", "w1", "w2", "w3"]),
    ] {
        let r = store
            .apply(
                &human(),
                RequestEnvelope::new(Op::Init {
                    name: name.to_string(),
                    template: Some(tpl),
                    goal: None,
                    workspace_root: format!("/ws/{name}"),
                    bootstrap_symlinks: vec![],
                    bootstrap_copies: vec![],
                }),
            )
            .expect("init template");
        let ReceiptData::TeamStatus(proj) = r.data else { panic!("expected TeamStatus") };
        let names: Vec<&str> = proj.seats.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, expected);
    }
}

#[test]
fn idempotent_mutation_receipts_and_conflict_detection() {
    let mut store = Store::open_in_memory().expect("store");
    init_review_team(&mut store);

    let actor = seat_actor("super");
    let op1 = task_create_op("Build P1", vec![], Some("zcode1"), Some("vicesuper"));

    let receipt_a = store
        .apply(&actor, RequestEnvelope::new(op1.clone()).with_request_id("req-100"))
        .expect("first apply");
    let receipt_b = store
        .apply(&actor, RequestEnvelope::new(op1).with_request_id("req-100"))
        .expect("idempotent replay");
    assert_eq!(receipt_a, receipt_b);

    let ReceiptData::Tasks(tasks) = store
        .apply(&actor, RequestEnvelope::new(Op::TaskList { status: None, ready_only: false }))
        .expect("task list")
        .data
    else {
        panic!("expected Tasks");
    };
    assert_eq!(tasks.len(), 1);

    let ReceiptData::Seat(super_seat) = store
        .apply(&human(), RequestEnvelope::new(Op::SeatShow { seat: "super".to_string() }))
        .expect("seat show")
        .data
    else {
        panic!("expected Seat");
    };
    assert!(super_seat.last_seen_at.is_some());

    let conflict_err = store
        .apply(
            &actor,
            RequestEnvelope::new(task_create_op("Different", vec![], None, None)).with_request_id("req-100"),
        )
        .expect_err("must detect request conflict");
    assert!(matches!(conflict_err, Error::RequestConflict { ref request_id } if request_id == "req-100"));
}

#[test]
fn reviewer_blindness_invariant_enforced_across_ops() {
    let mut store = Store::open_in_memory().expect("store");
    init_review_team(&mut store);

    let err1 = store
        .apply(&seat_actor("super"), RequestEnvelope::new(task_create_op("Self review", vec![], Some("zcode1"), Some("zcode1"))))
        .expect_err("self-review on TaskCreate");
    assert!(matches!(err1, Error::ReviewerBlindness { ref seat } if seat == "zcode1"));

    let ReceiptData::Task(task) = store
        .apply(&seat_actor("super"), RequestEnvelope::new(task_create_op("Valid task", vec![], None, Some("vicesuper"))))
        .expect("create task")
        .data
    else {
        panic!("expected Task");
    };

    let err2 = store
        .apply(
            &seat_actor("super"),
            RequestEnvelope::new(Op::TaskAssign {
                task_id: task.task_id.clone(),
                to_seat: "vicesuper".to_string(),
                reviewer: None,
                spec: None,
            }),
        )
        .expect_err("assign to reviewer must fail");
    assert!(matches!(err2, Error::ReviewerBlindness { ref seat } if seat == "vicesuper"));

    let err3 = store
        .apply(&seat_actor("vicesuper"), RequestEnvelope::new(Op::TaskClaim { task_id: task.task_id.clone() }))
        .expect_err("claim by reviewer must fail");
    assert!(matches!(err3, Error::ReviewerBlindness { ref seat } if seat == "vicesuper"));

    store
        .apply(
            &seat_actor("super"),
            RequestEnvelope::new(Op::TaskAssign {
                task_id: task.task_id.clone(),
                to_seat: "zcode1".to_string(),
                reviewer: Some("vicesuper".to_string()),
                spec: None,
            }),
        )
        .expect("assign to zcode1");
    store
        .apply(
            &seat_actor("zcode1"),
            RequestEnvelope::new(Op::Done {
                task_id: Some(task.task_id.clone()),
                result: "All tests pass".to_string(),
                branch: Some("feat/p1".to_string()),
                commit_sha: Some("abc1234".to_string()),
                outcome: DoneOutcome::Succeeded,
            }),
        )
        .expect("done");

    let err4 = store
        .apply(
            &seat_actor("zcode1"),
            RequestEnvelope::new(Op::Verdict {
                task_id: task.task_id,
                verdict: VerdictKind::Pass,
                body: "LGTM to myself".to_string(),
            }),
        )
        .expect_err("self-verdict must fail");
    assert!(matches!(err4, Error::ReviewerBlindness { ref seat } if seat == "zcode1"));
}

#[test]
fn dag_auto_unblocking_on_done_and_verdict_pass() {
    let mut store = Store::open_in_memory().expect("store");
    init_review_team(&mut store);

    let ReceiptData::Task(task_a) = store
        .apply(&seat_actor("super"), RequestEnvelope::new(task_create_op("Task A", vec![], Some("zcode1"), None)))
        .expect("create A")
        .data
    else {
        panic!("expected Task");
    };
    let ReceiptData::Task(task_b) = store
        .apply(&seat_actor("super"), RequestEnvelope::new(task_create_op("Task B", vec![], Some("zcode2"), Some("vicesuper"))))
        .expect("create B")
        .data
    else {
        panic!("expected Task");
    };
    let ReceiptData::Task(task_c) = store
        .apply(
            &seat_actor("super"),
            RequestEnvelope::new(task_create_op("Task C", vec![task_a.task_id.clone(), task_b.task_id.clone()], None, None)),
        )
        .expect("create C")
        .data
    else {
        panic!("expected Task");
    };
    assert_eq!(task_c.status, TaskStatus::Pending);

    let ReceiptData::DoneResult { unblocked_task_ids, .. } = store
        .apply(
            &seat_actor("zcode1"),
            RequestEnvelope::new(Op::Done {
                task_id: Some(task_a.task_id),
                result: "A finished".to_string(),
                branch: None,
                commit_sha: None,
                outcome: DoneOutcome::Succeeded,
            }),
        )
        .expect("done A")
        .data
    else {
        panic!("expected DoneResult");
    };
    assert!(unblocked_task_ids.is_empty());

    store
        .apply(
            &seat_actor("zcode2"),
            RequestEnvelope::new(Op::Done {
                task_id: Some(task_b.task_id.clone()),
                result: "B ready for review".to_string(),
                branch: None,
                commit_sha: None,
                outcome: DoneOutcome::Succeeded,
            }),
        )
        .expect("done B");

    let ReceiptData::VerdictResult { unblocked_task_ids: unblocked_by_b, .. } = store
        .apply(
            &seat_actor("vicesuper"),
            RequestEnvelope::new(Op::Verdict {
                task_id: task_b.task_id,
                verdict: VerdictKind::Pass,
                body: "Verified clean diff".to_string(),
            }),
        )
        .expect("verdict B")
        .data
    else {
        panic!("expected VerdictResult");
    };
    assert_eq!(unblocked_by_b, vec![task_c.task_id.clone()]);

    let ReceiptData::Task(updated_c) = store
        .apply(&seat_actor("super"), RequestEnvelope::new(Op::TaskShow { task_id: task_c.task_id }))
        .expect("show C")
        .data
    else {
        panic!("expected Task");
    };
    assert_eq!(updated_c.status, TaskStatus::Ready);
}

#[test]
fn three_round_fix_auto_escalation() {
    let mut store = Store::open_in_memory().expect("store");
    init_review_team(&mut store);

    let ReceiptData::Task(task) = store
        .apply(&seat_actor("super"), RequestEnvelope::new(task_create_op("Tricky parser", vec![], Some("zcode1"), Some("vicesuper"))))
        .expect("create task")
        .data
    else {
        panic!("expected Task");
    };

    for expected_round in 1..=2 {
        store
            .apply(
                &seat_actor("zcode1"),
                RequestEnvelope::new(Op::Done {
                    task_id: Some(task.task_id.clone()),
                    result: format!("Attempt {expected_round}"),
                    branch: None,
                    commit_sha: None,
                    outcome: DoneOutcome::Succeeded,
                }),
            )
            .expect("done");

        let ReceiptData::VerdictResult { task: t_after, gate, escalated, .. } = store
            .apply(
                &seat_actor("vicesuper"),
                RequestEnvelope::new(Op::Verdict {
                    task_id: task.task_id.clone(),
                    verdict: VerdictKind::Fix,
                    body: format!("Fix needed round {expected_round}"),
                }),
            )
            .expect("verdict fix")
            .data
        else {
            panic!("expected VerdictResult");
        };
        assert!(!escalated);
        assert!(gate.is_none());
        assert_eq!(t_after.review_round, expected_round);
        assert_eq!(t_after.status, TaskStatus::InProgress);
    }

    store
        .apply(
            &seat_actor("zcode1"),
            RequestEnvelope::new(Op::Done {
                task_id: Some(task.task_id.clone()),
                result: "Attempt 3".to_string(),
                branch: None,
                commit_sha: None,
                outcome: DoneOutcome::Succeeded,
            }),
        )
        .expect("done 3");

    let ReceiptData::VerdictResult { task: t3, gate: gate3, escalated, .. } = store
        .apply(
            &seat_actor("vicesuper"),
            RequestEnvelope::new(Op::Verdict {
                task_id: task.task_id,
                verdict: VerdictKind::Fix,
                body: "Still broken on round 3".to_string(),
            }),
        )
        .expect("verdict fix 3")
        .data
    else {
        panic!("expected VerdictResult");
    };
    assert!(escalated);
    assert_eq!(t3.review_round, 3);
    assert_eq!(t3.status, TaskStatus::Blocked);
    let gate3 = gate3.expect("escalation gate must be created");
    assert_eq!(gate3.kind, GateKind::Escalation);
    assert_eq!(gate3.status, GateStatus::Pending);

    let ReceiptData::Inbox(items) = store
        .apply(&seat_actor("super"), RequestEnvelope::new(Op::InboxRead { peek: true, seat: Some("super".to_string()), limit: None }))
        .expect("read super inbox")
        .data
    else {
        panic!("expected Inbox");
    };
    assert!(items.iter().any(|i| i.kind == InboxKind::Escalation));
}

#[test]
fn three_strike_circuit_breaker_across_retry_chain() {
    let mut store = Store::open_in_memory().expect("store");
    init_review_team(&mut store);

    let ReceiptData::SpawnPrepared { task: Some(task1), dispatch: Some(d1), .. } = store
        .apply(&seat_actor("super"), RequestEnvelope::new(spawn_prepare_op("zcode1", Some("Flaky build task"), None)))
        .expect("spawn 1")
        .data
    else {
        panic!("expected SpawnPrepared");
    };

    let ReceiptData::Dispatch(d1_after) = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::DaemonEscalate {
                seat: "zcode1".to_string(),
                dispatch_id: Some(d1.dispatch_id.clone()),
                stop_reason: "exited".to_string(),
                detail: "Exit 1".to_string(),
            }),
        )
        .expect("escalate 1")
        .data
    else {
        panic!("expected Dispatch");
    };
    assert_eq!(d1_after.failure_count, 1);
    assert_eq!(d1_after.state, DispatchState::SettledFailed);

    let ReceiptData::SpawnPrepared { dispatch: Some(d2), .. } = store
        .apply(&seat_actor("super"), RequestEnvelope::new(spawn_prepare_op("zcode1", None, Some(d1.dispatch_id))))
        .expect("spawn 2")
        .data
    else {
        panic!("expected SpawnPrepared");
    };
    assert_eq!(d2.failure_count, 1);

    let ReceiptData::Dispatch(d2_after) = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::DaemonEscalate {
                seat: "zcode1".to_string(),
                dispatch_id: Some(d2.dispatch_id.clone()),
                stop_reason: "lost".to_string(),
                detail: "Lost connection".to_string(),
            }),
        )
        .expect("escalate 2")
        .data
    else {
        panic!("expected Dispatch");
    };
    assert_eq!(d2_after.failure_count, 2);
    assert_eq!(d2_after.state, DispatchState::SettledFailed);

    let ReceiptData::SpawnPrepared { dispatch: Some(d3), .. } = store
        .apply(&seat_actor("super"), RequestEnvelope::new(spawn_prepare_op("zcode1", None, Some(d2.dispatch_id))))
        .expect("spawn 3")
        .data
    else {
        panic!("expected SpawnPrepared");
    };
    assert_eq!(d3.failure_count, 2);

    let ReceiptData::Dispatch(d3_after) = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::DaemonEscalate {
                seat: "zcode1".to_string(),
                dispatch_id: Some(d3.dispatch_id.clone()),
                stop_reason: "exited".to_string(),
                detail: "Third crash".to_string(),
            }),
        )
        .expect("escalate 3")
        .data
    else {
        panic!("expected Dispatch");
    };
    assert_eq!(d3_after.failure_count, 3);
    assert_eq!(d3_after.state, DispatchState::CircuitBroken);

    let ReceiptData::Task(failed_task) = store
        .apply(&seat_actor("super"), RequestEnvelope::new(Op::TaskShow { task_id: task1.task_id }))
        .expect("task show")
        .data
    else {
        panic!("expected Task");
    };
    assert_eq!(failed_task.status, TaskStatus::Failed);

    let retry4_err = store
        .apply(&seat_actor("super"), RequestEnvelope::new(spawn_prepare_op("zcode1", None, Some(d3.dispatch_id))))
        .expect_err("4th retry must be rejected by circuit breaker");
    assert!(matches!(retry4_err, Error::CircuitBroken { failures: 3, .. }));
}

#[test]
fn human_approval_guard_for_seat_remove_and_handoff() {
    let mut store = Store::open_in_memory().expect("store");
    init_review_team(&mut store);

    let ReceiptData::Gate(rem_gate) = store
        .apply(&seat_actor("super"), RequestEnvelope::new(Op::SeatRemove { seat: "zcode3".to_string() }))
        .expect("request remove")
        .data
    else {
        panic!("expected Gate");
    };
    assert_eq!(rem_gate.kind, GateKind::ApprovalRemove);
    assert_eq!(rem_gate.status, GateStatus::Pending);

    let deny_err = store
        .apply(&seat_actor("super"), RequestEnvelope::new(Op::Approve { gate_id: rem_gate.gate_id.clone(), resolution: None }))
        .expect_err("agent seat cannot approve");
    assert!(matches!(deny_err, Error::PermissionDenied(_)));

    store
        .apply(&human(), RequestEnvelope::new(Op::Approve { gate_id: rem_gate.gate_id, resolution: Some("confirmed idle".to_string()) }))
        .expect("human approve remove");

    let missing_err = store
        .apply(&human(), RequestEnvelope::new(Op::SeatShow { seat: "zcode3".to_string() }))
        .expect_err("zcode3 must be deleted");
    assert!(matches!(missing_err, Error::NotFound { entity: "seat", .. }));

    let ReceiptData::Gate(role_gate) = store
        .apply(&seat_actor("super"), RequestEnvelope::new(Op::SeatHandoff { to_seat: "vicesuper".to_string() }))
        .expect("request handoff")
        .data
    else {
        panic!("expected Gate");
    };
    assert_eq!(role_gate.kind, GateKind::ApprovalRole);

    let role_deny = store
        .apply(&seat_actor("vicesuper"), RequestEnvelope::new(Op::Approve { gate_id: role_gate.gate_id.clone(), resolution: None }))
        .expect_err("agent seat cannot approve handoff");
    assert!(matches!(role_deny, Error::PermissionDenied(_)));

    store
        .apply(&human(), RequestEnvelope::new(Op::Approve { gate_id: role_gate.gate_id, resolution: Some("shift handoff".to_string()) }))
        .expect("human approve handoff");

    let ReceiptData::Seat(new_sup) = store
        .apply(&human(), RequestEnvelope::new(Op::SeatShow { seat: "vicesuper".to_string() }))
        .expect("show vicesuper")
        .data
    else {
        panic!("expected Seat");
    };
    assert_eq!(new_sup.role, SeatRole::Supervisor);

    let ReceiptData::Seat(old_sup) = store
        .apply(&human(), RequestEnvelope::new(Op::SeatShow { seat: "super".to_string() }))
        .expect("show super")
        .data
    else {
        panic!("expected Seat");
    };
    assert_eq!(old_sup.role, SeatRole::Worker);
}

#[test]
fn warm_seat_reuse_ask_reply_bulletin_findings_and_doorbell() {
    let mut store = Store::open_in_memory().expect("store");
    let team_id = init_review_team(&mut store);

    let ReceiptData::SpawnPrepared { dispatch: Some(d1), warm_reuse: warm1, .. } = store
        .apply(&seat_actor("super"), RequestEnvelope::new(spawn_prepare_op("zcode1", Some("Task 1 spec"), None)))
        .expect("cold spawn")
        .data
    else {
        panic!("expected SpawnPrepared");
    };
    assert!(!warm1);

    store
        .apply(
            &seat_actor("super"),
            RequestEnvelope::new(Op::SpawnBindAddress {
                seat: "zcode1".to_string(),
                dispatch_id: Some(d1.dispatch_id),
                address: Address::Core { machine: None, terminal: "t4".to_string() },
                worktree_path: None,
                branch: None,
                resume_session: Some("sess-123".to_string()),
            }),
        )
        .expect("bind address");

    let ReceiptData::Gate(q_gate) = store
        .apply(
            &seat_actor("zcode1"),
            RequestEnvelope::new(Op::Ask {
                task_id: None,
                question: "Which crate owns the socket?".to_string(),
                body: Some("Checking crate boundaries".to_string()),
            }),
        )
        .expect("ask")
        .data
    else {
        panic!("expected Gate");
    };
    assert_eq!(q_gate.kind, GateKind::Question);

    store
        .apply(
            &seat_actor("super"),
            RequestEnvelope::new(Op::Reply {
                target_id: q_gate.gate_id,
                body: "orcherd-hc owns the socket".to_string(),
            }),
        )
        .expect("reply");

    store
        .apply(
            &seat_actor("zcode1"),
            RequestEnvelope::new(Op::Done {
                task_id: None,
                result: "Done task 1".to_string(),
                branch: None,
                commit_sha: None,
                outcome: DoneOutcome::Succeeded,
            }),
        )
        .expect("done 1");

    let ReceiptData::SpawnPrepared { dispatch: Some(d2), warm_reuse: warm2, .. } = store
        .apply(&seat_actor("super"), RequestEnvelope::new(spawn_prepare_op("zcode1", Some("Task 2 on warm seat"), None)))
        .expect("warm spawn")
        .data
    else {
        panic!("expected SpawnPrepared");
    };
    assert!(warm2);
    assert_eq!(d2.state, DispatchState::Working);

    store
        .apply(
            &seat_actor("super"),
            RequestEnvelope::new(Op::BulletinPost {
                subject: Some("Wave 1 update".to_string()),
                body: "Heads up @zcode2 please check findings".to_string(),
                mentions: vec![],
                reply_to: None,
                kind: Some(PostKind::Authorized),
            }),
        )
        .expect("bulletin post");

    let undelivered = store.list_undelivered_inbox(&team_id, "zcode2").expect("undelivered");
    assert_eq!(undelivered.len(), 1);
    assert_eq!(undelivered[0].kind, InboxKind::Mention);

    let line = format_doorbell(undelivered.len(), undelivered[0].kind, &undelivered[0].ref_id);
    assert!(line.starts_with("[Orcherd] You have 1 unread item(s)"));
    assert!(line.ends_with("Run: orcherd inbox"));

    store
        .apply(
            &human(),
            RequestEnvelope::new(Op::DaemonRecordDoorbell {
                seat: "zcode2".to_string(),
                rung_through_seq: undelivered[0].seq,
                outcome: DoorbellOutcome::Confirmed,
            }),
        )
        .expect("record doorbell");
    assert!(store.list_undelivered_inbox(&team_id, "zcode2").expect("after delivery").is_empty());

    store
        .apply(
            &seat_actor("zcode1"),
            RequestEnvelope::new(Op::FindingAdd {
                title: "WAL busy timeout prevents SQLITE_BUSY".to_string(),
                evidence: "5000ms busy_timeout tested under concurrent writers".to_string(),
                task_id: None,
                confidence: Some(Confidence::Verified),
            }),
        )
        .expect("finding add");

    let ReceiptData::Findings(findings) = store
        .apply(&human(), RequestEnvelope::new(Op::FindingList))
        .expect("finding list")
        .data
    else {
        panic!("expected Findings");
    };
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].confidence, Confidence::Verified);
}

#[test]
fn send_selectors_prefix_resolution_and_seat_lifecycle_ops() {
    let mut store = Store::open_in_memory().expect("store");
    init_review_team(&mut store);

    // Prefix resolution: "zcode" matches zcode1, zcode2, zcode3 -> AmbiguousPrefix
    let ambig = store
        .apply(&human(), RequestEnvelope::new(Op::SeatShow { seat: "zcode".to_string() }))
        .expect_err("ambiguous seat prefix");
    assert!(matches!(ambig, Error::AmbiguousPrefix { entity: "seat", .. }));

    // Unique prefix "vice" resolves to vicesuper
    let ReceiptData::Seat(vice) = store
        .apply(&human(), RequestEnvelope::new(Op::SeatShow { seat: "vice".to_string() }))
        .expect("unique seat prefix")
        .data
    else {
        panic!("expected Seat");
    };
    assert_eq!(vice.name, "vicesuper");

    // Send to @workers (zcode1, zcode2, zcode3) and @reviewers (vicesuper)
    let ReceiptData::Sent { recipients: w_recips, .. } = store
        .apply(
            &seat_actor("super"),
            RequestEnvelope::new(Op::Send {
                to: "@workers".to_string(),
                subject: "Ping workers".to_string(),
                body: "Status check".to_string(),
            }),
        )
        .expect("send @workers")
        .data
    else {
        panic!("expected Sent");
    };
    assert_eq!(w_recips, vec!["zcode1", "zcode2", "zcode3"]);

    // Spawn a dispatch on zcode1, then test SeatAbandon returning task to Ready
    let ReceiptData::SpawnPrepared { task: Some(t), dispatch: Some(d), .. } = store
        .apply(&seat_actor("super"), RequestEnvelope::new(spawn_prepare_op("zcode1", Some("Abandon test"), None)))
        .expect("spawn")
        .data
    else {
        panic!("expected SpawnPrepared");
    };

    let ReceiptData::Dispatch(abandoned) = store
        .apply(&seat_actor("super"), RequestEnvelope::new(Op::SeatAbandon { dispatch_id: d.dispatch_id }))
        .expect("abandon")
        .data
    else {
        panic!("expected Dispatch");
    };
    assert_eq!(abandoned.state, DispatchState::Abandoned);

    let ReceiptData::Task(t_ready) = store
        .apply(&seat_actor("super"), RequestEnvelope::new(Op::TaskShow { task_id: t.task_id }))
        .expect("task show")
        .data
    else {
        panic!("expected Task");
    };
    assert_eq!(t_ready.status, TaskStatus::Ready);
    assert!(t_ready.assignee_seat.is_none());

    // AuditLog has recorded every mutation, and none of the reads (seat show, task show).
    let ReceiptData::Audit(audit_rows) = store
        .apply(&human(), RequestEnvelope::new(Op::AuditLog { limit: Some(10) }))
        .expect("audit log")
        .data
    else {
        panic!("expected Audit");
    };
    let ops: Vec<&str> = audit_rows.iter().map(|r| r.op.as_str()).collect();
    assert_eq!(ops, ["init", "send", "spawn_prepare", "seat_abandon"]);
}

#[test]
fn read_only_ops_leave_no_audit_rows_and_teams_resolve_by_repo_path() {
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path().join("store.sqlite3")).unwrap();
    let team_id = init_review_team(&mut store);

    let audit_len = |store: &mut Store| match store
        .apply(&human(), RequestEnvelope::new(Op::AuditLog { limit: None }))
        .unwrap()
        .data
    {
        ReceiptData::Audit(rows) => rows.len(),
        other => panic!("expected audit rows, got {other:?}"),
    };
    let before = audit_len(&mut store);
    for _ in 0..3 {
        store
            .apply(&human(), RequestEnvelope::new(Op::Status))
            .unwrap();
    }
    store
        .apply(
            &human(),
            RequestEnvelope::new(Op::TaskList {
                status: None,
                ready_only: false,
            }),
        )
        .unwrap();
    assert_eq!(
        audit_len(&mut store),
        before,
        "polling status/list must not grow the audit log"
    );

    // Mutations still audit.
    store
        .apply(
            &human(),
            RequestEnvelope::new(task_create_op("audited", vec![], None, None)),
        )
        .unwrap();
    assert_eq!(audit_len(&mut store), before + 1);

    let inside = std::path::Path::new("/tmp/herding/crates/core");
    assert_eq!(
        store.team_for_path(inside).unwrap().as_deref(),
        Some(team_id.as_str())
    );
    assert_eq!(
        store
            .team_for_path(std::path::Path::new("/tmp/herding-other"))
            .unwrap(),
        None
    );
}

#[test]
fn a_stuck_doorbell_escalates_to_the_supervisor_once_and_stays_undelivered() {
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path().join("store.sqlite3")).unwrap();
    let team_id = init_review_team(&mut store);
    store
        .apply(
            &seat_actor("super"),
            RequestEnvelope::new(Op::Send {
                to: "zcode1".to_string(),
                subject: "craft check".to_string(),
                body: "answer".to_string(),
            }),
        )
        .unwrap();
    let seq = store.list_undelivered_inbox(&team_id, "zcode1").unwrap()[0].seq;

    let escalations = |store: &mut Store| {
        let ReceiptData::Inbox(items) = store
            .apply(
                &seat_actor("super"),
                RequestEnvelope::new(Op::InboxRead { peek: true, seat: Some("super".to_string()), limit: None }),
            )
            .unwrap()
            .data
        else {
            panic!("expected inbox");
        };
        items.iter().filter(|i| i.kind == InboxKind::Escalation).count()
    };
    for _ in 0..2 {
        store
            .apply(
                &human(),
                RequestEnvelope::new(Op::DaemonRecordDoorbell {
                    seat: "zcode1".to_string(),
                    rung_through_seq: seq,
                    outcome: DoorbellOutcome::StatusMismatch,
                }),
            )
            .unwrap();
    }
    assert_eq!(escalations(&mut store), 1, "deduplicated to one notice");
    assert_eq!(store.list_undelivered_inbox(&team_id, "zcode1").unwrap().len(), 1);
}
