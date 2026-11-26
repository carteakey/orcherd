//! Tests for supervisor controls: FIX verdicts reach the supervisor, standing briefs, and
//! reopening a task keeps its review round.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use tempfile::tempdir;

use crate::{
    Actor, DoneOutcome, Error, InboxKind, Op, ReceiptData, RequestEnvelope, Store, Task, TaskStatus,
    TemplateKind, VerdictKind,
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
