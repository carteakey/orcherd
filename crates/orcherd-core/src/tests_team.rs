//! Tests for workspace-root tie-breaking and team-scoped `NotFound` errors (`#13`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use crate::{Actor, Op, RequestEnvelope, Store, TemplateKind};

fn human() -> Actor {
    Actor::Human {
        terminal: Some("t0".to_string()),
    }
}

#[test]
fn team_for_path_breaks_ties_by_newest_and_prefers_deepest_root() {
    let mut store = Store::open_in_memory().unwrap();
    let init_at = |store: &mut Store, name: &str, root: &str| -> String {
        store
            .apply(
                &human(),
                RequestEnvelope::new(Op::Init {
                    name: name.to_string(),
                    template: Some(TemplateKind::Small),
                    goal: None,
                    workspace_root: root.to_string(),
                    bootstrap_symlinks: vec![],
                    bootstrap_copies: vec![],
                }),
            )
            .unwrap()
            .team_id
    };

    let older = init_at(&mut store, "older", "/ws/shared");
    let nested = init_at(&mut store, "nested", "/ws/shared/pkg");
    let newer = init_at(&mut store, "newer", "/ws/shared");
    assert_ne!(older, newer);

    // Two teams share `/ws/shared`: the newest team wins the tie.
    assert_eq!(
        store
            .team_for_path(Path::new("/ws/shared/src"))
            .unwrap()
            .as_deref(),
        Some(newer.as_str())
    );
    // A deeper root still beats a newer shallower ancestor.
    assert_eq!(
        store
            .team_for_path(Path::new("/ws/shared/pkg/lib"))
            .unwrap()
            .as_deref(),
        Some(nested.as_str())
    );
}

#[test]
fn not_found_errors_name_the_searched_team() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .apply(
            &human(),
            RequestEnvelope::new(Op::Init {
                name: "lvl2".to_string(),
                template: Some(TemplateKind::Small),
                goal: None,
                workspace_root: "/ws/lvl2".to_string(),
                bootstrap_symlinks: vec![],
                bootstrap_copies: vec![],
            }),
        )
        .unwrap();

    for (op, expected) in [
        (
            Op::TaskShow {
                task_id: "task-missing".to_string(),
            },
            "task not found in team lvl2: task-missing",
        ),
        (
            Op::SeatShow {
                seat: "ghost".to_string(),
            },
            "seat not found in team lvl2: ghost",
        ),
        (
            Op::Approve {
                gate_id: "gate-missing".to_string(),
                resolution: None,
            },
            "gate not found in team lvl2: gate-missing",
        ),
        (
            Op::Reply {
                target_id: "gate-missing".to_string(),
                body: "answer".to_string(),
            },
            "gate not found in team lvl2: gate-missing",
        ),
    ] {
        let err = store.apply(&human(), RequestEnvelope::new(op)).unwrap_err();
        assert_eq!(err.to_string(), expected);
    }

    let team_err = store
        .apply(
            &human(),
            RequestEnvelope::new(Op::Status).with_team("no-such-team"),
        )
        .unwrap_err();
    assert_eq!(team_err.to_string(), "team not found: no-such-team");
}

fn init_op(name: &str) -> Op {
    Op::Init {
        name: name.to_string(),
        template: Some(TemplateKind::Small),
        goal: None,
        workspace_root: format!("/ws/{name}"),
        bootstrap_symlinks: vec![],
        bootstrap_copies: vec![],
    }
}

fn init_team(store: &mut Store, name: &str) -> String {
    store.apply(&human(), RequestEnvelope::new(init_op(name))).unwrap().team_id
}

fn task_create(title: &str) -> Op {
    Op::TaskCreate {
        title: Some(title.to_string()),
        spec: format!("Spec for {title}"),
        deps: vec![],
        assign: None,
        reviewer: None,
        issue_ref: None,
        parent_id: None,
        placement: None,
    }
}

#[test]
fn mutation_receipts_isolated_per_team_and_init_replays() {
    use crate::{Error, ReceiptData};

    let mut store = Store::open_in_memory().unwrap();
    let actor = human();

    let (init_r1, replayed1) = store
        .apply_with_replay(&actor, RequestEnvelope::new(init_op("alpha")).with_request_id("req-init"))
        .unwrap();
    let (init_r2, replayed2) = store
        .apply_with_replay(&actor, RequestEnvelope::new(init_op("alpha")).with_request_id("req-init"))
        .unwrap();
    assert!(!replayed1 && replayed2);
    assert_eq!(init_r1, init_r2);
    let team_a = init_r1.team_id;
    let team_b = init_team(&mut store, "beta");

    // Reusing `req-init` inside `team_a` must not collide with store-wide `Op::Init`.
    let (_, init_key_reused) = store
        .apply_with_replay(
            &actor,
            RequestEnvelope::new(task_create("After init")).with_team(&team_a).with_request_id("req-init"),
        )
        .unwrap();
    assert!(!init_key_reused);

    // Same caller, same Op, same `--request-id` across two teams must execute independently.
    let op = task_create("Ship feature");
    let req = |team: &str, op: Op| RequestEnvelope::new(op).with_team(team).with_request_id("req-shared");
    let (ra, ra_replayed) = store.apply_with_replay(&actor, req(&team_a, op.clone())).unwrap();
    let (rb, rb_replayed) = store.apply_with_replay(&actor, req(&team_b, op.clone())).unwrap();
    assert!(!ra_replayed && !rb_replayed);
    assert_eq!((ra.team_id.as_str(), rb.team_id.as_str()), (team_a.as_str(), team_b.as_str()));
    let (ReceiptData::Task(ta), ReceiptData::Task(tb)) = (&ra.data, &rb.data) else {
        panic!("expected Task receipts");
    };
    assert_ne!(ta.task_id, tb.task_id);

    // Replaying within each team returns that team's own cached receipt.
    let (ra2, ra2_replayed) = store.apply_with_replay(&actor, req(&team_a, op.clone())).unwrap();
    let (rb2, rb2_replayed) = store.apply_with_replay(&actor, req(&team_b, op)).unwrap();
    assert!(ra2_replayed && rb2_replayed);
    assert_eq!((ra, rb), (ra2, rb2));

    // Same team + same request_id + different Op still raises RequestConflict.
    let err = store.apply(&actor, req(&team_a, task_create("Different payload"))).unwrap_err();
    assert!(matches!(err, Error::RequestConflict { ref request_id } if request_id == "req-shared"));
}

#[test]
fn schema_v2_to_v3_migration_backfills_team_id_and_preserves_replay() {
    use rusqlite::Connection;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use crate::{Error, SCHEMA_VERSION, read_user_version};

    let dir = tempdir().unwrap();
    let db_path = dir.path().join("store.sqlite3");
    let actor = human();

    let (team_a, op_a, original_receipt) = {
        let mut store = Store::open(&db_path).unwrap();
        let team_a = init_team(&mut store, "alpha");
        let op_a = task_create("Pre-migration task");
        let receipt = store
            .apply(&actor, RequestEnvelope::new(op_a.clone()).with_team(&team_a).with_request_id("req-v2"))
            .unwrap();
        (team_a, op_a, receipt)
    };

    // Simulate a v2 database: drop `team_id` from `mutation_receipts`, store v2 op-only hash,
    // and reset `PRAGMA user_version = 2`.
    {
        let raw = Connection::open(&db_path).unwrap();
        let legacy_hash: String = Sha256::digest(serde_json::to_vec(&op_a).unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let receipt_json = serde_json::to_string(&original_receipt).unwrap();
        raw.execute_batch(
            "DROP TABLE mutation_receipts;\n\
             CREATE TABLE mutation_receipts (\n\
                 caller_id TEXT NOT NULL, request_id TEXT NOT NULL, op TEXT NOT NULL,\n\
                 payload_hash TEXT NOT NULL,\n\
                 state TEXT NOT NULL DEFAULT 'completed' CHECK(state IN ('pending', 'completed')),\n\
                 receipt_json TEXT NOT NULL, created_at TEXT NOT NULL,\n\
                 PRIMARY KEY (caller_id, request_id)\n\
             );\n\
             PRAGMA user_version = 2;",
        )
        .unwrap();
        raw.execute(
            "INSERT INTO mutation_receipts (caller_id, request_id, op, payload_hash, state, receipt_json, created_at) \
             VALUES (?1, 'req-v2', 'task_create', ?2, 'completed', ?3, '2026-11-27T00:00:00Z')",
            rusqlite::params![actor.caller_id(), legacy_hash, receipt_json],
        )
        .unwrap();
        assert_eq!(read_user_version(&raw).unwrap(), 2);
    }

    // Reopening upgrades v2 -> v3, backfills `team_id`, preserves replay, and isolates new teams.
    let mut store = Store::open(&db_path).unwrap();
    assert_eq!(read_user_version(store.connection()).unwrap(), SCHEMA_VERSION);
    let migrated_team_id: String = store
        .connection()
        .query_row(
            "SELECT team_id FROM mutation_receipts WHERE caller_id = ?1 AND request_id = 'req-v2'",
            rusqlite::params![actor.caller_id()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(migrated_team_id, team_a);

    let (replayed_receipt, replayed) = store
        .apply_with_replay(&actor, RequestEnvelope::new(op_a.clone()).with_team(&team_a).with_request_id("req-v2"))
        .unwrap();
    assert!(replayed);
    assert_eq!(replayed_receipt, original_receipt);

    let conflict = store
        .apply(&actor, RequestEnvelope::new(task_create("Other")).with_team(&team_a).with_request_id("req-v2"))
        .unwrap_err();
    assert!(matches!(conflict, Error::RequestConflict { .. }));

    let team_b = init_team(&mut store, "beta");
    let (beta_receipt, beta_replayed) = store
        .apply_with_replay(&actor, RequestEnvelope::new(op_a).with_team(&team_b).with_request_id("req-v2"))
        .unwrap();
    assert!(!beta_replayed);
    assert_eq!(beta_receipt.team_id, team_b);
}
