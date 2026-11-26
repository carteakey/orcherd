//! Built-in team seat templates (`review-crew`, `studio-crew`, `small`).

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::store::{new_id, resolve_seat};
use crate::types::{SeatRole, TemplateKind};

/// Static definition for a template-seeded seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeatTemplate {
    pub name: &'static str,
    pub role: SeatRole,
    pub default_agent: &'static str,
    pub standing_brief: &'static str,
}

const REVIEW_CREW_SEATS: &[SeatTemplate] = &[
    SeatTemplate {
        name: "super",
        role: SeatRole::Supervisor,
        default_agent: "claude",
        standing_brief: "Team supervisor: decompose work, dispatch workers, verify diffs, and authorize landings.",
    },
    SeatTemplate {
        name: "vicesuper",
        role: SeatRole::Reviewer,
        default_agent: "claude",
        standing_brief: "Dedicated blind diff reviewer: inspect worker completions against spec and standards.",
    },
    SeatTemplate {
        name: "planner",
        role: SeatRole::Planner,
        default_agent: "opencode",
        standing_brief: "Architecture and task planner: research codebase seams and author task specs.",
    },
    SeatTemplate {
        name: "zcode1",
        role: SeatRole::Worker,
        default_agent: "antigravity",
        standing_brief: "Builder seat 1: execute assigned tasks in isolated worktrees and report verification.",
    },
    SeatTemplate {
        name: "zcode2",
        role: SeatRole::Worker,
        default_agent: "antigravity",
        standing_brief: "Builder seat 2: execute assigned tasks in isolated worktrees and report verification.",
    },
    SeatTemplate {
        name: "zcode3",
        role: SeatRole::Worker,
        default_agent: "antigravity",
        standing_brief: "Builder seat 3: execute assigned tasks in isolated worktrees and report verification.",
    },
];

const STUDIO_CREW_SEATS: &[SeatTemplate] = &[
    SeatTemplate {
        name: "director",
        role: SeatRole::Supervisor,
        default_agent: "claude",
        standing_brief: "Studio director: coordinate creative/engineering waves, resolve gates, and ship.",
    },
    SeatTemplate {
        name: "builder-a",
        role: SeatRole::Worker,
        default_agent: "antigravity",
        standing_brief: "Primary studio builder A: implement assigned slice and run verification.",
    },
    SeatTemplate {
        name: "builder-b",
        role: SeatRole::Worker,
        default_agent: "antigravity",
        standing_brief: "Primary studio builder B: implement assigned slice and run verification.",
    },
    SeatTemplate {
        name: "gauntlet",
        role: SeatRole::Reviewer,
        default_agent: "antigravity",
        standing_brief: "Gauntlet blind QA reviewer: verify correctness, edge cases, and test gates.",
    },
    SeatTemplate {
        name: "viewer-value",
        role: SeatRole::Reviewer,
        default_agent: "antigravity",
        standing_brief: "Viewer-value reviewer: evaluate user experience, polish, and domain quality.",
    },
];

const SMALL_SEATS: &[SeatTemplate] = &[
    SeatTemplate {
        name: "super",
        role: SeatRole::Supervisor,
        default_agent: "claude",
        standing_brief: "Team supervisor: coordinate tasks, review diffs, and land verified changes.",
    },
    SeatTemplate {
        name: "w1",
        role: SeatRole::Worker,
        default_agent: "antigravity",
        standing_brief: "Worker seat 1: implement assigned tasks and report completion.",
    },
    SeatTemplate {
        name: "w2",
        role: SeatRole::Worker,
        default_agent: "antigravity",
        standing_brief: "Worker seat 2: implement assigned tasks and report completion.",
    },
    SeatTemplate {
        name: "w3",
        role: SeatRole::Worker,
        default_agent: "antigravity",
        standing_brief: "Worker seat 3: implement assigned tasks and report completion.",
    },
];

/// Return the static roster of seats for a given `TemplateKind`.
pub const fn template_seats(kind: TemplateKind) -> &'static [SeatTemplate] {
    match kind {
        TemplateKind::ReviewCrew => REVIEW_CREW_SEATS,
        TemplateKind::StudioCrew => STUDIO_CREW_SEATS,
        TemplateKind::Small => SMALL_SEATS,
    }
}

/// Seed all roster seats and initial doorbell cursors for `tpl` into `team_id`.
pub(crate) fn seed_template_seats(
    conn: &Connection,
    team_id: &str,
    tpl: TemplateKind,
    now: &str,
) -> Result<()> {
    for seat_tpl in template_seats(tpl) {
        let seat_id = new_id("seat");
        conn.execute(
            "INSERT OR IGNORE INTO seats (team_id, seat_id, name, role, agent, \
             address_kind, keep_open, standing_brief, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'none', 1, ?6, ?7, ?7)",
            params![
                team_id,
                seat_id,
                seat_tpl.name,
                seat_tpl.role.as_str(),
                seat_tpl.default_agent,
                seat_tpl.standing_brief,
                now
            ],
        )?;
        if let Ok(seeded) = resolve_seat(conn, team_id, seat_tpl.name) {
            conn.execute(
                "INSERT OR IGNORE INTO doorbell (team_id, seat_id, rung_through_seq, attempts) \
                 VALUES (?1, ?2, 0, 0)",
                params![team_id, seeded.seat_id],
            )?;
        }
    }
    Ok(())
}
