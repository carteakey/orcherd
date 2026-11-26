//! Supervisor-only team controls: freezing the team (#1), standing briefs, and reopening
//! finished tasks for another round.

use rusqlite::{Connection, OptionalExtension, params};

use super::OpOutcome;
use crate::error::{Error, Result};
use crate::store::{build_team_projection, list_seats, resolve_seat, resolve_task, write_inbox};
use crate::types::{Actor, InboxKind, Op, ReceiptData, SeatRole, TaskStatus};

/// Supervisor seats and humans may steer the team; other seats may not.
fn require_supervisor(conn: &Connection, team_id: &str, actor: &Actor, what: &str) -> Result<()> {
    if let Actor::Seat { seat_name, .. } = actor
        && resolve_seat(conn, team_id, seat_name)?.role != SeatRole::Supervisor
    {
        return Err(Error::PermissionDenied(format!("only the supervisor or a human may {what}")));
    }
    Ok(())
}

/// While a team is frozen, nothing may start new work: no spawns, claims, or assignments.
/// Reads, `ask`, `reply`, `done`, and `verdict` still work so seats can report and wind down.
pub(super) fn refuse_if_frozen(conn: &Connection, team_id: &str, op: &Op) -> Result<()> {
    let starts_work = matches!(op, Op::SpawnPrepare { .. } | Op::TaskClaim { .. } | Op::TaskAssign { .. } | Op::TaskReopen { .. })
        || matches!(op, Op::TaskCreate { assign: Some(_), .. });
    if !starts_work {
        return Ok(());
    }
    let frozen: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT paused_at, paused_reason FROM teams WHERE team_id = ?1 AND paused_at IS NOT NULL",
            params![team_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match frozen {
        Some((at, reason)) => Err(Error::validation(format!(
            "team is frozen since {at}: {}. No new work starts until the supervisor runs `orcherd unfreeze`",
            reason.as_deref().unwrap_or("no reason given")
        ))),
        None => Ok(()),
    }
}

/// `Freeze` (with a reason) or `Unfreeze` (`reason: None`). Supervisor or human only.
/// Every other seat gets a notice; the daemon holds doorbells while the team is frozen.
pub(super) fn exec_freeze(conn: &Connection, team_id: &str, actor: &Actor, now: &str, reason: Option<&str>) -> Result<OpOutcome> {
    let verb = if reason.is_some() { "freeze" } else { "unfreeze" };
    require_supervisor(conn, team_id, actor, &format!("{verb} the team"))?;
    let reason = match reason {
        Some(r) if r.trim().is_empty() => return Err(Error::validation("`freeze` needs a reason")),
        Some(r) => Some(r.trim()),
        None => None,
    };
    let paused_at = reason.map(|_| now);
    conn.execute(
        "UPDATE teams SET paused_at = ?1, paused_reason = ?2, updated_at = ?3 WHERE team_id = ?4",
        params![paused_at, reason, now, team_id],
    )?;
    let (subject, body) = match reason {
        Some(r) => ("Team frozen: stop and start nothing new".to_string(), format!("{r}\n\nFinish or abandon the current step, then end your turn. New spawns, claims, and assignments are refused until the supervisor runs `orcherd unfreeze`.")),
        None => ("Team unfrozen: work can resume".to_string(), "The freeze is lifted. Check `orcherd inbox` and continue your task.".to_string()),
    };
    for seat in list_seats(conn, team_id, true)? {
        if seat.name != actor.actor_name() {
            write_inbox(conn, team_id, &seat.name, actor.actor_name(), InboxKind::Notice, team_id, &subject, &body, now)?;
        }
    }
    Ok(OpOutcome {
        team_id: team_id.to_string(),
        ref_id: Some(team_id.to_string()),
        next: Some(if reason.is_some() { "orcherd unfreeze" } else { "orcherd status" }.to_string()),
        data: ReceiptData::TeamStatus(Box::new(build_team_projection(conn, team_id)?)),
    })
}

/// Sets a seat's standing brief: the role instructions it keeps across tasks (a reviewer's
/// rubric, a builder's house rules). The seat gets it now, and again whenever it starts in a
/// fresh terminal.
pub(super) fn exec_seat_brief(conn: &Connection, team_id: &str, actor: &Actor, now: &str, seat: &str, brief: &str) -> Result<OpOutcome> {
    require_supervisor(conn, team_id, actor, "set a standing brief")?;
    let brief = brief.trim();
    if brief.is_empty() {
        return Err(Error::validation("a standing brief must not be empty"));
    }
    let s = resolve_seat(conn, team_id, seat)?;
    conn.execute(
        "UPDATE seats SET standing_brief = ?1, updated_at = ?2 WHERE team_id = ?3 AND seat_id = ?4",
        params![brief, now, team_id, s.seat_id],
    )?;
    write_inbox(conn, team_id, &s.name, actor.actor_name(), InboxKind::Notice, &s.seat_id, "Your standing brief (updated)", brief, now)?;
    let updated = resolve_seat(conn, team_id, &s.seat_id)?;
    Ok(OpOutcome::simple(team_id, Some(updated.seat_id.clone()), ReceiptData::Seat(Box::new(updated))))
}

/// Reopens a finished task for another round with a follow-up brief, back to the same
/// assignee and reviewer. The task keeps its review round, so the third FIX escalates across
/// owner-directed rounds too instead of resetting with each new task.
pub(super) fn exec_task_reopen(conn: &Connection, team_id: &str, actor: &Actor, now: &str, task_ref: &str, spec: &str) -> Result<OpOutcome> {
    require_supervisor(conn, team_id, actor, "reopen a task")?;
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(Error::validation("`task reopen` needs a follow-up brief"));
    }
    let task = resolve_task(conn, team_id, task_ref)?;
    if !matches!(task.status, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled) {
        return Err(Error::validation(format!(
            "task '{}' is {}; only a completed, failed or cancelled task can be reopened (use `task assign` to redirect live work)",
            task.task_id, task.status
        )));
    }
    let Some(assignee) = task.assignee_seat.as_deref() else {
        return Err(Error::validation(format!("task '{}' has no assignee to reopen it for; use `task assign`", task.task_id)));
    };
    conn.execute(
        "UPDATE tasks SET status = 'assigned', spec = ?1, completed_at = NULL, updated_at = ?2 WHERE task_id = ?3",
        params![spec, now, task.task_id],
    )?;
    let subject = format!("Reopened (review round {}): {}", task.review_round, task.title);
    write_inbox(conn, team_id, assignee, actor.actor_name(), InboxKind::Assigned, &task.task_id, &subject, spec, now)?;
    let updated = resolve_task(conn, team_id, &task.task_id)?;
    Ok(OpOutcome::simple(team_id, Some(updated.task_id.clone()), ReceiptData::Task(Box::new(updated))))
}
