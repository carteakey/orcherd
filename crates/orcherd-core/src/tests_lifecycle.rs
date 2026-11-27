//! Tests for seat stop/release/abandon lifecycle cleanup and prefix validation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::{
    Actor, Address, DispatchState, Error, InboxKind, Op, ReceiptData, RequestEnvelope, SeatRole,
    Store, TaskStatus, TemplateKind,
};

fn human() -> Actor {
    Actor::Human { terminal: Some("t0".to_owned()) }
}

fn setup_team() -> (Store, String) {
    let mut store = Store::open_in_memory().unwrap();
    let team_id = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::Init {
                name: "crew".to_owned(),
                template: Some(TemplateKind::ReviewCrew),
                goal: None,
                workspace_root: "/tmp/crew".to_owned(),
                bootstrap_symlinks: vec![],
                bootstrap_copies: vec![],
            }),
        )
        .unwrap()
        .team_id;
    (store, team_id)
}

fn spawn_and_bind(store: &mut Store, team_id: &str, seat: &str, machine: Option<&str>, terminal: &str) -> (String, String) {
    let ReceiptData::SpawnPrepared {
        task: Some(task),
        dispatch: Some(disp),
        ..
    } = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::SpawnPrepare {
                seat: seat.to_owned(),
                role: Some(SeatRole::Worker),
                agent: Some("antigravity".to_owned()),
                model: None,
                account: None,
                effort: None,
                worktree_path: None,
                branch: None,
                machine: machine.map(str::to_owned),
                task_id: None,
                spec: Some(format!("Work for {seat}")),
                title: Some(format!("Work for {seat}")),
                keep_open: Some(true),
                resume_session: None,
                retry_of: None,
            })
            .with_team(team_id),
        )
        .unwrap()
        .data
    else {
        panic!("expected SpawnPrepared with task and dispatch");
    };
    store
        .apply(
            &human(),
            RequestEnvelope::new(Op::SpawnBindAddress {
                seat: seat.to_owned(),
                dispatch_id: Some(disp.dispatch_id.clone()),
                address: Address::Core {
                    machine: machine.map(str::to_owned),
                    terminal: terminal.to_owned(),
                },
                worktree_path: None,
                branch: None,
                resume_session: None,
            })
            .with_team(team_id),
        )
        .unwrap();
    (task.task_id, disp.dispatch_id)
}

#[test]
fn seat_stop_and_release_clear_seat_address_and_reject_ambiguous_prefix() {
    let (mut store, team_id) = setup_team();
    let (_, d1) = spawn_and_bind(&mut store, &team_id, "zcode1", Some("m1"), "t8");
    let (_, _d2) = spawn_and_bind(&mut store, &team_id, "zcode2", None, "t9");

    // Ambiguous dispatch prefix must return AmbiguousPrefix instead of falling through.
    let err = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::SeatStop {
                target: "disp-".to_owned(),
                reason: Some("user_stopped".to_owned()),
            })
            .with_team(&team_id),
        )
        .unwrap_err();
    assert!(matches!(err, Error::AmbiguousPrefix { .. }), "got {err:?}");

    // Stopping a seat by name clears its bound address so daemon/UI mod stop targeting t9.
    let ReceiptData::Seat(stopped) = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::SeatStop {
                target: "zcode2".to_owned(),
                reason: Some("user_stopped".to_owned()),
            })
            .with_team(&team_id),
        )
        .unwrap()
        .data
    else {
        panic!("expected Seat receipt");
    };
    assert_eq!(stopped.address, Address::None);

    // Releasing a dispatch also clears its seat's bound address.
    let ReceiptData::Dispatch(released) = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::SeatRelease { dispatch_id: d1 }).with_team(&team_id),
        )
        .unwrap()
        .data
    else {
        panic!("expected Dispatch receipt");
    };
    assert_eq!(released.state, DispatchState::Stopped);
    assert_eq!(released.stop_reason.as_deref(), Some("released"));
    let ReceiptData::Seat(z1) = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::SeatShow { seat: "zcode1".to_owned() }).with_team(&team_id),
        )
        .unwrap()
        .data
    else {
        panic!("expected Seat");
    };
    assert_eq!(z1.address, Address::None);
}

#[test]
fn seat_abandon_returns_task_ready_and_clears_unread_dispatch_inbox() {
    let (mut store, team_id) = setup_team();
    let (task_id, disp_id) = spawn_and_bind(&mut store, &team_id, "zcode1", None, "t8");

    assert!(
        store
            .list_undelivered_inbox(&team_id, "zcode1")
            .unwrap()
            .iter()
            .any(|i| i.kind == InboxKind::Dispatch && i.ref_id == disp_id)
    );

    let ReceiptData::Dispatch(abandoned) = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::SeatAbandon { dispatch_id: disp_id.clone() }).with_team(&team_id),
        )
        .unwrap()
        .data
    else {
        panic!("expected Dispatch");
    };
    assert_eq!(abandoned.state, DispatchState::Abandoned);

    let ReceiptData::Task(task) = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::TaskShow { task_id }).with_team(&team_id),
        )
        .unwrap()
        .data
    else {
        panic!("expected Task");
    };
    assert_eq!(task.status, TaskStatus::Ready);
    assert_eq!(task.assignee_seat, None);

    // Neither undelivered nor unread inbox should carry the abandoned dispatch item.
    assert!(
        store
            .list_undelivered_inbox(&team_id, "zcode1")
            .unwrap()
            .iter()
            .all(|i| i.ref_id != disp_id)
    );
    let ReceiptData::Inbox(unread) = store
        .apply(
            &Actor::Seat {
                team_id: Some(team_id.clone()),
                seat_name: "zcode1".to_owned(),
                terminal: None,
            },
            RequestEnvelope::new(Op::InboxRead { peek: true, seat: None, limit: None })
                .with_team(&team_id),
        )
        .unwrap()
        .data
    else {
        panic!("expected Inbox");
    };
    assert!(
        unread.iter().all(|i| i.ref_id != disp_id),
        "abandoned dispatch must not stay unread in inbox: {unread:?}"
    );
}
