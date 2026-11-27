//! Tests for spawn inline doorbell isolation and daemon deduplication.

use crate::daemon::{lock_file_path, ring_seat_if_no_daemon_with};
use orcherd_core::{
    Actor, Address, DispatchState, InboxKind, Op, ReceiptData, RequestEnvelope, SeatRole, Store,
    TemplateKind,
};
use orcherd_hc::wire::{Foreground, Sent, Status, StatusState, Terminal, TerminalState};
use std::{collections::BTreeMap, fs};
use tempfile::TempDir;

fn spawn_seat(store: &mut Store, team_id: &str, name: &str, terminal: &str) -> String {
    let human = Actor::Human { terminal: None };
    let ReceiptData::SpawnPrepared {
        dispatch: Some(disp),
        ..
    } = store
        .apply(
            &human,
            RequestEnvelope::new(Op::SpawnPrepare {
                seat: name.to_owned(),
                role: Some(SeatRole::Worker),
                agent: Some("antigravity".to_owned()),
                model: None,
                account: None,
                effort: None,
                worktree_path: None,
                branch: None,
                machine: None,
                task_id: None,
                spec: Some(format!("Work for {name}")),
                title: Some(format!("Work for {name}")),
                keep_open: Some(true),
                resume_session: None,
                retry_of: None,
            })
            .with_team(team_id),
        )
        .unwrap()
        .data
    else {
        panic!("expected SpawnPrepared with dispatch");
    };
    store
        .apply(
            &human,
            RequestEnvelope::new(Op::SpawnBindAddress {
                seat: name.to_owned(),
                dispatch_id: Some(disp.dispatch_id.clone()),
                address: Address::Core {
                    machine: None,
                    terminal: terminal.to_owned(),
                },
                worktree_path: None,
                branch: None,
                resume_session: None,
            })
            .with_team(team_id),
        )
        .unwrap();
    disp.dispatch_id
}

fn make_terminal(address: &str, fg_name: Option<&str>, state: StatusState) -> Terminal {
    Terminal {
        address: address.to_owned(),
        argv: Vec::new(),
        cols: 120,
        cwd: "/tmp/crew".to_owned(),
        exit_code: None,
        foreground: fg_name.map(|n| Foreground {
            argv: vec![n.to_owned()],
            cwd: "/tmp/crew".to_owned(),
            name: n.to_owned(),
            pid: 42,
        }),
        keep: true,
        labels: BTreeMap::new(),
        name: None,
        parent: None,
        pid: Some(42),
        reason: None,
        rows: 40,
        state: TerminalState::Live,
        status: Some(Status {
            app: "antigravity".to_owned(),
            kind: None,
            msg: String::new(),
            progress: None,
            state,
            title: String::new(),
        }),
        title: None,
    }
}

#[test]
fn spawn_inline_ring_skips_when_daemon_running_and_isolates_seat_when_not() {
    let state_dir = TempDir::new().unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let human = Actor::Human { terminal: None };
    let team_id = store
        .apply(
            &human,
            RequestEnvelope::new(Op::Init {
                name: "crew".to_owned(),
                template: Some(TemplateKind::Small),
                goal: Some("Spawn isolation test".to_owned()),
                workspace_root: "/tmp/crew".to_owned(),
                bootstrap_symlinks: Vec::new(),
                bootstrap_copies: Vec::new(),
            }),
        )
        .unwrap()
        .team_id;

    // Unrelated seat `w1` exited back to `bash` with unread mail; `w2` is newly spawned.
    let w1_disp = spawn_seat(&mut store, &team_id, "w1", "t1");
    let _w2_disp = spawn_seat(&mut store, &team_id, "w2", "t2");
    let w2_unread_before = store.list_undelivered_inbox(&team_id, "w2").unwrap().len();
    assert!(w2_unread_before >= 1);

    // 1. When a daemon is running, spawn must not send an inline doorbell at all.
    #[cfg(unix)]
    {
        fs::write(
            lock_file_path(state_dir.path()),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        let mut sent = 0usize;
        let delivered = ring_seat_if_no_daemon_with(
            &mut store,
            state_dir.path(),
            &team_id,
            "w2",
            || Some(vec![make_terminal("t2", Some("agy"), StatusState::Done)]),
            |_| {
                sent += 1;
                Ok(Sent {
                    started: Some(true),
                    status: Some("done".to_owned()),
                    turn: 1,
                })
            },
        )
        .unwrap();
        assert!(!delivered);
        assert_eq!(sent, 0, "must not double-ring while daemon is running");
        assert_eq!(
            store.list_undelivered_inbox(&team_id, "w2").unwrap().len(),
            w2_unread_before
        );
        fs::remove_file(lock_file_path(state_dir.path())).unwrap();
    }

    // 2. When no daemon is running, spawn rings ONLY `w2` and never strikes unrelated `w1`.
    let mut sent_to = Vec::new();
    let delivered = ring_seat_if_no_daemon_with(
        &mut store,
        state_dir.path(),
        &team_id,
        "w2",
        || {
            Some(vec![
                make_terminal("t1", Some("bash"), StatusState::Idle),
                make_terminal("t2", Some("agy"), StatusState::Done),
            ])
        },
        |send| {
            sent_to.push(send.terminal.to_owned());
            Ok(Sent {
                started: Some(true),
                status: Some("done".to_owned()),
                turn: 1,
            })
        },
    )
    .unwrap();

    assert!(delivered);
    assert_eq!(sent_to, ["t2"], "must ring only the spawned seat");
    assert!(store.list_undelivered_inbox(&team_id, "w2").unwrap().is_empty());

    let proj = store.team_projection(&team_id).unwrap();
    let d1 = proj
        .active_dispatches
        .iter()
        .find(|d| d.dispatch_id == w1_disp)
        .expect("w1 dispatch must stay active");
    assert_eq!(d1.failure_count, 0);
    assert_eq!(d1.state, DispatchState::Working);
    let sup_escalations: Vec<_> = store
        .list_undelivered_inbox(&team_id, "super")
        .unwrap()
        .into_iter()
        .filter(|i| i.kind == InboxKind::Escalation)
        .collect();
    assert!(
        sup_escalations.is_empty(),
        "must not emit exited escalation for unrelated seat: {sup_escalations:?}"
    );
}
