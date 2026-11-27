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
