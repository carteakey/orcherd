//! Tests for supervisor controls: FIX verdicts reach the supervisor, standing briefs, and
//! reopening a task keeps its review round.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use tempfile::tempdir;

use crate::{
    Actor, DispatchState, DoneOutcome, DoorbellOutcome, Error, InboxKind, Op, ReceiptData,
    RequestEnvelope, Store, Task, TaskStatus, TemplateKind, VerdictKind,
};

fn human() -> Actor {
    Actor::Human { terminal: Some("t0".to_string()) }
}

fn seat(name: &str) -> Actor {
    Actor::Seat { team_id: None, seat_name: name.to_string(), terminal: Some(format!("t-{name}")) }
}

fn team() -> (tempfile::TempDir, Store, String) {
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path().join("store.sqlite3")).unwrap();
    let team_id = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::Init {
                name: "crew".to_string(),
                template: Some(TemplateKind::ReviewCrew),
                goal: None,
                workspace_root: "/tmp/crew".to_string(),
                bootstrap_symlinks: vec![],
                bootstrap_copies: vec![],
            }),
        )
        .unwrap()
        .team_id;
    (dir, store, team_id)
}

fn task_in_review(store: &mut Store) -> Task {
    let created = store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::TaskCreate {
                title: Some("Cut A".to_string()),
                spec: "Build cut A".to_string(),
                deps: vec![],
                assign: Some("zcode1".to_string()),
                reviewer: Some("vicesuper".to_string()),
                issue_ref: None,
                parent_id: None,
                placement: None,
            }),
        )
        .unwrap();
    let ReceiptData::Task(task) = created.data else { panic!("expected a task") };
    hand_in(store, &task.task_id);
    *task
}

fn hand_in(store: &mut Store, task_id: &str) {
    store
        .apply(
            &seat("zcode1"),
            RequestEnvelope::new(Op::Done {
                task_id: Some(task_id.to_string()),
                result: "done".to_string(),
                branch: None,
                commit_sha: None,
                outcome: DoneOutcome::Succeeded,
            }),
        )
        .unwrap();
}

fn verdict(store: &mut Store, task_id: &str, verdict: VerdictKind) -> Task {
    let r = store
        .apply(
            &seat("vicesuper"),
            RequestEnvelope::new(Op::Verdict { task_id: task_id.to_string(), verdict, body: "notes".to_string() }),
        )
        .unwrap();
    let ReceiptData::VerdictResult { task, .. } = r.data else { panic!("expected a verdict result") };
    *task
}

fn unread(store: &Store, team_id: &str, seat: &str) -> Vec<crate::InboxItem> {
    store.list_undelivered_inbox(team_id, seat).unwrap()
}

#[test]
fn a_fix_verdict_reaches_the_supervisor_as_well_as_the_assignee() {
    let (_dir, mut store, team_id) = team();
    let task = task_in_review(&mut store);
    verdict(&mut store, &task.task_id, VerdictKind::Fix);

    let to_builder = unread(&store, &team_id, "zcode1");
    assert!(to_builder.iter().any(|i| i.kind == InboxKind::Verdict && i.subject.starts_with("Verdict FIX (round 1)")));
    let to_super = unread(&store, &team_id, "super");
    assert!(
        to_super.iter().any(|i| i.kind == InboxKind::Verdict && i.subject.contains("FIX (round 1)") && i.subject.contains("sent to zcode1")),
        "{to_super:?}"
    );
}

#[test]
fn reopening_a_task_keeps_its_review_round_so_the_third_fix_escalates() {
    let (_dir, mut store, team_id) = team();
    let task = task_in_review(&mut store);
    verdict(&mut store, &task.task_id, VerdictKind::Fix);
    hand_in(&mut store, &task.task_id);
    verdict(&mut store, &task.task_id, VerdictKind::Fix);
    hand_in(&mut store, &task.task_id);
    let passed = verdict(&mut store, &task.task_id, VerdictKind::Pass);
    assert_eq!(passed.status, TaskStatus::Completed);
    assert_eq!(passed.review_round, 2);

    let reopen = |spec: &str| RequestEnvelope::new(Op::TaskReopen { task: task.task_id.clone(), spec: spec.to_string() });
    let denied = store.apply(&seat("zcode2"), reopen("swap the socks")).unwrap_err();
    assert!(matches!(denied, Error::PermissionDenied(_)), "{denied}");

    let r = store.apply(&human(), reopen("Owner round: swap the socks")).unwrap();
    let ReceiptData::Task(reopened) = r.data else { panic!("expected a task") };
    assert_eq!(reopened.status, TaskStatus::Assigned);
    assert_eq!(reopened.review_round, 2, "the round carries over");
    assert_eq!(reopened.spec, "Owner round: swap the socks");
    assert!(unread(&store, &team_id, "zcode1").iter().any(|i| i.kind == InboxKind::Assigned && i.subject.starts_with("Reopened (review round 2)")));
    assert!(store.apply(&human(), reopen("again")).is_err(), "only a finished task can be reopened");

    hand_in(&mut store, &task.task_id);
    let third = verdict(&mut store, &task.task_id, VerdictKind::Fix);
    assert_eq!(third.status, TaskStatus::Blocked, "the third FIX escalates across the reopen");
}

#[test]
fn a_standing_brief_is_sent_now_and_again_on_every_fresh_launch() {
    let (_dir, mut store, team_id) = team();
    let brief = |text: &str| RequestEnvelope::new(Op::SeatBrief { seat: "vicesuper".to_string(), brief: text.to_string() });
    assert!(matches!(store.apply(&seat("zcode1"), brief("x")).unwrap_err(), Error::PermissionDenied(_)));

    let r = store.apply(&seat("super"), brief("Grade blind against the rubric in RUBRIC.md.")).unwrap();
    let ReceiptData::Seat(s) = r.data else { panic!("expected a seat") };
    assert_eq!(s.standing_brief.as_deref(), Some("Grade blind against the rubric in RUBRIC.md."));
    let first = unread(&store, &team_id, "vicesuper");
    assert!(first.iter().any(|i| i.kind == InboxKind::Notice && i.body.contains("RUBRIC.md")));

    // Read it, then start the seat in a fresh terminal: the brief comes again.
    store.apply(&seat("vicesuper"), RequestEnvelope::new(Op::InboxRead { peek: false, seat: None, limit: None })).unwrap();
    store
        .apply(
            &human(),
            RequestEnvelope::new(Op::SpawnPrepare {
                seat: "vicesuper".to_string(),
                role: None,
                agent: None,
                model: None,
                account: None,
                effort: None,
                worktree_path: None,
                branch: None,
                machine: None,
                task_id: None,
                spec: None,
                title: None,
                keep_open: None,
                resume_session: None,
                retry_of: None,
            }),
        )
        .unwrap();
    let again = unread(&store, &team_id, "vicesuper");
    assert!(again.iter().any(|i| i.subject == "Your standing brief" && i.body.contains("RUBRIC.md")), "{again:?}");
}

#[test]
fn blocked_supervisor_never_self_escalates() {
    let (_dir, mut store, team_id) = team();

    store
        .apply(
            &human(),
            RequestEnvelope::new(Op::DaemonEscalate {
                seat: "super".to_string(),
                dispatch_id: None,
                stop_reason: "blocked".to_string(),
                detail: "seat `super` blocked on question: Proceed?".to_string(),
            }),
        )
        .unwrap();

    store
        .apply(
            &human(),
            RequestEnvelope::new(Op::DaemonRecordDoorbell {
                seat: "super".to_string(),
                rung_through_seq: 1,
                outcome: DoorbellOutcome::Blocked,
            }),
        )
        .unwrap();

    let items = unread(&store, &team_id, "super");
    assert!(
        items.iter().all(|i| i.kind != InboxKind::Escalation),
        "blocked supervisor must never write an escalation into its own inbox: {items:?}"
    );
}

#[test]
fn blocked_seat_produces_one_escalation_and_preserves_strike_counting() {
    let (_dir, mut store, team_id) = team();

    let ReceiptData::SpawnPrepared { dispatch: Some(disp), .. } = store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::SpawnPrepare {
                seat: "zcode1".to_string(),
                role: None,
                agent: Some("antigravity".to_string()),
                model: None,
                account: None,
                effort: None,
                worktree_path: None,
                branch: None,
                machine: None,
                task_id: None,
                spec: Some("Fix #11".to_string()),
                title: Some("Fix #11".to_string()),
                keep_open: None,
                resume_session: None,
                retry_of: None,
            }),
        )
        .unwrap()
        .data
    else {
        panic!("expected SpawnPrepared with dispatch");
    };

    // Both doorbell refusal and status notice for the same block must yield one escalation.
    store
        .apply(
            &human(),
            RequestEnvelope::new(Op::DaemonRecordDoorbell {
                seat: "zcode1".to_string(),
                rung_through_seq: 1,
                outcome: DoorbellOutcome::Blocked,
            }),
        )
        .unwrap();
    let ReceiptData::Dispatch(after_block) = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::DaemonEscalate {
                seat: "zcode1".to_string(),
                dispatch_id: Some(disp.dispatch_id.clone()),
                stop_reason: "blocked".to_string(),
                detail: "seat `zcode1` blocked on permission: Trust this folder?".to_string(),
            }),
        )
        .unwrap()
        .data
    else {
        panic!("expected Dispatch");
    };
    // A second blocked escalation for the same dispatch deduplicates cleanly.
    store
        .apply(
            &human(),
            RequestEnvelope::new(Op::DaemonEscalate {
                seat: "zcode1".to_string(),
                dispatch_id: Some(disp.dispatch_id.clone()),
                stop_reason: "blocked".to_string(),
                detail: "terminal `t4` is blocked on a dialog: blocked".to_string(),
            }),
        )
        .unwrap();

    let escalations: Vec<_> = unread(&store, &team_id, "super")
        .into_iter()
        .filter(|i| i.kind == InboxKind::Escalation)
        .collect();
    assert_eq!(escalations.len(), 1, "one block must produce one escalation: {escalations:?}");
    assert_eq!(escalations[0].ref_id, disp.dispatch_id);
    assert!(escalations[0].body.contains("Trust this folder?"));
    assert_eq!(after_block.stop_reason.as_deref(), Some("blocked"));
    assert_eq!(after_block.failure_count, 0, "blocking must not spend a failure strike");

    // A real crash afterwards still increments strike count and settles the dispatch as failed.
    let ReceiptData::Dispatch(after_exit) = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::DaemonEscalate {
                seat: "zcode1".to_string(),
                dispatch_id: Some(disp.dispatch_id),
                stop_reason: "exited".to_string(),
                detail: "exit 1".to_string(),
            }),
        )
        .unwrap()
        .data
    else {
        panic!("expected Dispatch");
    };
    assert_eq!(after_exit.failure_count, 1);
    assert_eq!(after_exit.state, DispatchState::SettledFailed);
}

#[test]
fn stalled_escalation_deduplicates_sets_stop_reason_and_skips_supervisor() {
    let (_dir, mut store, team_id) = team();

    let ReceiptData::SpawnPrepared { dispatch: Some(disp), .. } = store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::SpawnPrepare {
                seat: "zcode1".to_string(),
                role: None,
                agent: Some("zcode".to_string()),
                model: None,
                account: None,
                effort: None,
                worktree_path: None,
                branch: None,
                machine: None,
                task_id: None,
                spec: Some("Stall check".to_string()),
                title: Some("Stall check".to_string()),
                keep_open: None,
                resume_session: None,
                retry_of: None,
            }),
        )
        .unwrap()
        .data
    else {
        panic!("expected SpawnPrepared with dispatch");
    };

    for _ in 0..2 {
        let ReceiptData::Dispatch(after) = store
            .apply(
                &human(),
                RequestEnvelope::new(Op::DaemonEscalate {
                    seat: "zcode1".to_string(),
                    dispatch_id: Some(disp.dispatch_id.clone()),
                    stop_reason: "stalled".to_string(),
                    detail: "unchanged screen for 5 min".to_string(),
                }),
            )
            .unwrap()
            .data
        else {
            panic!("expected Dispatch");
        };
        assert_eq!(after.stop_reason.as_deref(), Some("stalled"));
        assert_eq!(after.failure_count, 0);
        assert_eq!(after.state, DispatchState::Starting);
    }

    // Supervisor stalling never writes into its own inbox.
    store
        .apply(
            &human(),
            RequestEnvelope::new(Op::DaemonEscalate {
                seat: "super".to_string(),
                dispatch_id: None,
                stop_reason: "stalled".to_string(),
                detail: "supervisor screen unchanged".to_string(),
            }),
        )
        .unwrap();

    let escalations: Vec<_> = unread(&store, &team_id, "super")
        .into_iter()
        .filter(|i| i.kind == InboxKind::Escalation)
        .collect();
    assert_eq!(escalations.len(), 1, "expected one deduplicated stall escalation: {escalations:?}");
    assert_eq!(escalations[0].ref_id, disp.dispatch_id);
    assert!(escalations[0].subject.contains("stalled"));
}

#[test]
fn done_verdict_and_ask_refuse_finished_tasks_without_mutating_state() {
    let (_dir, mut store, team_id) = team();
    let task = task_in_review(&mut store);

    store
        .apply(
            &seat("vicesuper"),
            RequestEnvelope::new(Op::Verdict {
                task_id: task.task_id.clone(),
                verdict: VerdictKind::Pass,
                body: "looks good".to_string(),
            }),
        )
        .unwrap();

    // Mark existing inbox items read so we can verify refused ops ring nobody.
    for s in ["super", "vicesuper", "zcode1"] {
        store
            .apply(
                &seat(s),
                RequestEnvelope::new(Op::InboxRead {
                    peek: false,
                    seat: None,
                    limit: None,
                }),
            )
            .unwrap();
    }

    let done_err = store
        .apply(
            &seat("zcode1"),
            RequestEnvelope::new(Op::Done {
                task_id: Some(task.task_id.clone()),
                result: "crossed repeat done".to_string(),
                branch: None,
                commit_sha: None,
                outcome: DoneOutcome::Succeeded,
            }),
        )
        .unwrap_err();
    let done_msg = done_err.to_string();
    assert!(done_msg.contains("completed"), "{done_msg}");
    assert!(done_msg.contains("orcherd ask"), "{done_msg}");
    assert!(done_msg.contains("task reopen"), "{done_msg}");

    for verdict in [VerdictKind::Pass, VerdictKind::Fix, VerdictKind::Escalate] {
        let err = store
            .apply(
                &seat("vicesuper"),
                RequestEnvelope::new(Op::Verdict {
                    task_id: task.task_id.clone(),
                    verdict,
                    body: "late verdict".to_string(),
                }),
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("completed") && msg.contains("task reopen"), "{msg}");
    }

    let ask_err = store
        .apply(
            &seat("zcode1"),
            RequestEnvelope::new(Op::Ask {
                task_id: Some(task.task_id.clone()),
                question: "follow-up on finished task".to_string(),
                body: None,
            }),
        )
        .unwrap_err();
    let ask_msg = ask_err.to_string();
    assert!(ask_msg.contains("completed") && ask_msg.contains("orcherd ask"), "{ask_msg}");

    // Task stays completed and neither reviewer nor supervisor was rung by the refused ops.
    let ReceiptData::Task(still_done) = store
        .apply(&human(), RequestEnvelope::new(Op::TaskShow { task_id: task.task_id.clone() }))
        .unwrap()
        .data
    else {
        panic!("expected Task");
    };
    assert_eq!(still_done.status, TaskStatus::Completed);
    assert!(unread(&store, &team_id, "vicesuper").is_empty());
    assert!(unread(&store, &team_id, "super").is_empty());

    // Asking the supervisor without a finished task id still succeeds cleanly.
    store
        .apply(
            &seat("zcode1"),
            RequestEnvelope::new(Op::Ask {
                task_id: None,
                question: "general question".to_string(),
                body: None,
            }),
        )
        .unwrap();

    // `failed` and `cancelled` tasks are refused the same way.
    for terminal in [TaskStatus::Failed, TaskStatus::Cancelled] {
        let t = task_in_review(&mut store);
        store
            .apply(
                &seat("super"),
                RequestEnvelope::new(Op::TaskUpdate {
                    task_id: t.task_id.clone(),
                    status: terminal,
                }),
            )
            .unwrap();
        let err = store
            .apply(
                &seat("zcode1"),
                RequestEnvelope::new(Op::Done {
                    task_id: Some(t.task_id.clone()),
                    result: "late done".to_string(),
                    branch: None,
                    commit_sha: None,
                    outcome: DoneOutcome::Succeeded,
                }),
            )
            .unwrap_err();
        assert!(err.to_string().contains(terminal.as_str()), "{err}");
        let ReceiptData::Task(after) = store
            .apply(&human(), RequestEnvelope::new(Op::TaskShow { task_id: t.task_id }))
            .unwrap()
            .data
        else {
            panic!("expected Task");
        };
        assert_eq!(after.status, terminal);
    }
}

#[test]
fn replayed_done_and_verdict_with_same_request_id_return_original_receipt() {
    let (_dir, mut store, _team_id) = team();

    // 1. Replaying `done` (no reviewer -> completes task) with the same request_id returns the original receipt.
    let ReceiptData::Task(unreviewed) = store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::TaskCreate {
                title: Some("Solo task".to_string()),
                spec: "finish it".to_string(),
                deps: vec![],
                assign: Some("zcode1".to_string()),
                reviewer: None,
                issue_ref: None,
                parent_id: None,
                placement: None,
            }),
        )
        .unwrap()
        .data
    else {
        panic!("expected Task");
    };

    let done_op = Op::Done {
        task_id: Some(unreviewed.task_id),
        result: "shipped".to_string(),
        branch: None,
        commit_sha: None,
        outcome: DoneOutcome::Succeeded,
    };
    let first = store
        .apply(&seat("zcode1"), RequestEnvelope::new(done_op.clone()).with_request_id("req-done-1"))
        .unwrap();
    let replay = store
        .apply(&seat("zcode1"), RequestEnvelope::new(done_op.clone()).with_request_id("req-done-1"))
        .unwrap();
    assert_eq!(first, replay);
    assert!(
        store
            .apply(&seat("zcode1"), RequestEnvelope::new(done_op).with_request_id("req-done-2"))
            .is_err(),
        "a fresh request_id on the completed task must be refused"
    );

    // 2. Replaying `verdict pass` with the same request_id returns the original receipt.
    let reviewed = task_in_review(&mut store);
    let verdict_op = Op::Verdict {
        task_id: reviewed.task_id,
        verdict: VerdictKind::Pass,
        body: "approved".to_string(),
    };
    let v_first = store
        .apply(
            &seat("vicesuper"),
            RequestEnvelope::new(verdict_op.clone()).with_request_id("req-verdict-1"),
        )
        .unwrap();
    let v_replay = store
        .apply(
            &seat("vicesuper"),
            RequestEnvelope::new(verdict_op.clone()).with_request_id("req-verdict-1"),
        )
        .unwrap();
    assert_eq!(v_first, v_replay);
    assert!(
        store
            .apply(
                &seat("vicesuper"),
                RequestEnvelope::new(verdict_op).with_request_id("req-verdict-2"),
            )
            .is_err(),
        "a fresh verdict on the completed task must be refused"
    );
}

#[test]
fn done_enforces_reviewer_blindness_and_records_assignee_on_unassigned_task() {
    let (_dir, mut store, _team_id) = team();
    let ReceiptData::Task(task) = store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::TaskCreate {
                title: Some("Unassigned reviewed task".to_string()),
                spec: "build it".to_string(),
                deps: vec![],
                assign: None,
                reviewer: Some("vicesuper".to_string()),
                issue_ref: None,
                parent_id: None,
                placement: None,
            }),
        )
        .unwrap()
        .data
    else {
        panic!("expected Task");
    };
    assert!(task.assignee_seat.is_none());

    // The reviewer cannot submit `done` on an unassigned task they are reviewing.
    let self_done = store
        .apply(
            &seat("vicesuper"),
            RequestEnvelope::new(Op::Done {
                task_id: Some(task.task_id.clone()),
                result: "built by reviewer".to_string(),
                branch: None,
                commit_sha: None,
                outcome: DoneOutcome::Succeeded,
            }),
        )
        .unwrap_err();
    assert!(matches!(self_done, Error::ReviewerBlindness { ref seat } if seat == "vicesuper"));

    // Another seat submitting `done` records itself as `assignee_seat`, so it cannot self-verdict.
    let ReceiptData::DoneResult { task: after_done, .. } = store
        .apply(
            &seat("zcode1"),
            RequestEnvelope::new(Op::Done {
                task_id: Some(task.task_id.clone()),
                result: "built by zcode1".to_string(),
                branch: None,
                commit_sha: None,
                outcome: DoneOutcome::Succeeded,
            }),
        )
        .unwrap()
        .data
    else {
        panic!("expected DoneResult");
    };
    assert_eq!(after_done.status, TaskStatus::Review);
    assert_eq!(after_done.assignee_seat.as_deref(), Some("zcode1"));

    let self_verdict = store
        .apply(
            &seat("zcode1"),
            RequestEnvelope::new(Op::Verdict {
                task_id: task.task_id.clone(),
                verdict: VerdictKind::Pass,
                body: "self approve".to_string(),
            }),
        )
        .unwrap_err();
    assert!(matches!(self_verdict, Error::ReviewerBlindness { ref seat } if seat == "zcode1"));

    // The designated reviewer can still pass it.
    store
        .apply(
            &seat("vicesuper"),
            RequestEnvelope::new(Op::Verdict {
                task_id: task.task_id,
                verdict: VerdictKind::Pass,
                body: "verified".to_string(),
            }),
        )
        .unwrap();
}

#[test]
fn task_update_requires_supervisor_and_respects_freeze() {
    let (_dir, mut store, _team_id) = team();
    let task = task_in_review(&mut store);
    let to_cancel = task_in_review(&mut store);

    // Workers cannot jump task status directly via `task update`.
    let denied = store
        .apply(
            &seat("zcode1"),
            RequestEnvelope::new(Op::TaskUpdate {
                task_id: task.task_id.clone(),
                status: TaskStatus::Completed,
            }),
        )
        .unwrap_err();
    assert!(matches!(denied, Error::PermissionDenied(_)), "{denied}");

    // Freezing the team refuses `task update` to active/unblocking states even for the supervisor.
    store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::Freeze {
                reason: "stop everything".to_string(),
            }),
        )
        .unwrap();
    for blocked_status in [
        TaskStatus::Assigned,
        TaskStatus::InProgress,
        TaskStatus::Completed,
    ] {
        let err = store
            .apply(
                &seat("super"),
                RequestEnvelope::new(Op::TaskUpdate {
                    task_id: task.task_id.clone(),
                    status: blocked_status,
                }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("team is frozen"), "{err}");
    }

    // Stopping a task (`cancelled`) is still allowed while frozen.
    store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::TaskUpdate {
                task_id: to_cancel.task_id,
                status: TaskStatus::Cancelled,
            }),
        )
        .unwrap();

    // After unfreeze, supervisor `task update` to `completed` works again.
    store.apply(&seat("super"), RequestEnvelope::new(Op::Unfreeze)).unwrap();
    let ReceiptData::Task(done) = store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::TaskUpdate {
                task_id: task.task_id,
                status: TaskStatus::Completed,
            }),
        )
        .unwrap()
        .data
    else {
        panic!("expected Task");
    };
    assert_eq!(done.status, TaskStatus::Completed);
}

#[test]
fn task_assign_skips_inbox_while_pending_and_assign_spawn_refuse_finished_tasks() {
    let (_dir, mut store, team_id) = team();
    let ReceiptData::Task(dep) = store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::TaskCreate {
                title: Some("Prereq".to_string()),
                spec: "first".to_string(),
                deps: vec![],
                assign: None,
                reviewer: None,
                issue_ref: None,
                parent_id: None,
                placement: None,
            }),
        )
        .unwrap()
        .data
    else {
        panic!("expected Task");
    };
    let ReceiptData::Task(child) = store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::TaskCreate {
                title: Some("Dependent".to_string()),
                spec: "second".to_string(),
                deps: vec![dep.task_id.clone()],
                assign: None,
                reviewer: None,
                issue_ref: None,
                parent_id: None,
                placement: None,
            }),
        )
        .unwrap()
        .data
    else {
        panic!("expected Task");
    };
    assert_eq!(child.status, TaskStatus::Pending);

    // Assigning a `pending` task records the assignee without ringing them early.
    let ReceiptData::Task(assigned_child) = store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::TaskAssign {
                task_id: child.task_id.clone(),
                to_seat: "zcode2".to_string(),
                reviewer: None,
                spec: None,
            }),
        )
        .unwrap()
        .data
    else {
        panic!("expected Task");
    };
    assert_eq!(assigned_child.status, TaskStatus::Pending);
    assert_eq!(assigned_child.assignee_seat.as_deref(), Some("zcode2"));
    assert!(
        unread(&store, &team_id, "zcode2").is_empty(),
        "assignee must not be rung while prerequisites are unfinished"
    );

    // Completing the prerequisite unblocks `child` and writes one `Assigned` inbox item.
    store
        .apply(
            &seat("super"),
            RequestEnvelope::new(Op::TaskUpdate {
                task_id: dep.task_id.clone(),
                status: TaskStatus::Completed,
            }),
        )
        .unwrap();
    let z2_items: Vec<_> = unread(&store, &team_id, "zcode2")
        .into_iter()
        .filter(|i| i.kind == InboxKind::Assigned && i.ref_id == child.task_id)
        .collect();
    assert_eq!(z2_items.len(), 1, "expected exactly one Assigned item on unblock: {z2_items:?}");

    // Finished tasks (`completed`, `failed`, `cancelled`) refuse both `task assign` and `spawn --task`.
    for terminal in [
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ] {
        let t = task_in_review(&mut store);
        store
            .apply(
                &seat("super"),
                RequestEnvelope::new(Op::TaskUpdate {
                    task_id: t.task_id.clone(),
                    status: terminal,
                }),
            )
            .unwrap();

        let assign_err = store
            .apply(
                &seat("super"),
                RequestEnvelope::new(Op::TaskAssign {
                    task_id: t.task_id.clone(),
                    to_seat: "zcode2".to_string(),
                    reviewer: None,
                    spec: None,
                }),
            )
            .unwrap_err();
        let assign_msg = assign_err.to_string();
        assert!(
            assign_msg.contains(terminal.as_str()) && assign_msg.contains("task reopen"),
            "{assign_msg}"
        );

        let spawn_err = store
            .apply(
                &seat("super"),
                RequestEnvelope::new(Op::SpawnPrepare {
                    seat: "zcode2".to_string(),
                    role: None,
                    agent: None,
                    model: None,
                    account: None,
                    effort: None,
                    worktree_path: None,
                    branch: None,
                    machine: None,
                    task_id: Some(t.task_id.clone()),
                    spec: None,
                    title: None,
                    keep_open: None,
                    resume_session: None,
                    retry_of: None,
                }),
            )
            .unwrap_err();
        let spawn_msg = spawn_err.to_string();
        assert!(
            spawn_msg.contains(terminal.as_str()) && spawn_msg.contains("task reopen"),
            "{spawn_msg}"
        );

        let ReceiptData::Task(unchanged) = store
            .apply(&human(), RequestEnvelope::new(Op::TaskShow { task_id: t.task_id }))
            .unwrap()
            .data
        else {
            panic!("expected Task");
        };
        assert_eq!(unchanged.status, terminal);
    }
}
