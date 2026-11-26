//! Single-transaction `apply(&mut Store, &Actor, RequestEnvelope) -> Result<Receipt>` engine.

use std::fmt::Write as _;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::store::{
    self, Store, build_team_projection, check_reviewer_blindness, derive_title,
    find_active_dispatch_for_seat, find_active_task_for_seat, find_supervisor_seat_name,
    get_doorbell, list_audit, list_findings, list_gates, list_inbox, list_posts, list_seats,
    list_tasks, map_dispatch, map_inbox_item, max_chain_failures, new_id, now_iso,
    resolve_dispatch, resolve_gate, resolve_seat, resolve_send_recipients, resolve_task,
    resolve_team_for_actor, unblock_ready_dag_tasks, write_audit, write_dedup_escalation_inbox,
    write_gate, write_inbox, write_inbox_with_id,
};
use crate::templates::seed_template_seats;
use crate::types::{
    Actor, DispatchState, DoneOutcome, Doorbell, DoorbellOutcome, Gate, GateKind, GateStatus,
    InboxItem, InboxKind, Op, Receipt, ReceiptData, RequestEnvelope, SeatRole, TaskStatus,
    TemplateKind, VerdictKind,
};

struct OpOutcome {
    team_id: String,
    ref_id: Option<String>,
    next: Option<String>,
    data: ReceiptData,
}

impl OpOutcome {
    fn simple(team_id: &str, ref_id: Option<String>, data: ReceiptData) -> Self {
        Self { team_id: team_id.to_string(), ref_id, next: None, data }
    }
}

/// Apply a `RequestEnvelope` inside a single SQLite transaction.
pub fn apply(store: &mut Store, actor: &Actor, envelope: RequestEnvelope) -> Result<Receipt> {
    let tx = store.conn.transaction()?;
    let now = now_iso();
    let caller_id = actor.caller_id();
    let op_name = envelope.op.name();

    let payload_hash = if let Some(req_id) = envelope.request_id.as_deref() {
        let hash = sha256_hex(&serde_json::to_vec(&envelope.op)?);
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT payload_hash, receipt_json FROM mutation_receipts WHERE caller_id = ?1 AND request_id = ?2",
                params![caller_id, req_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_hash, stored_json)) = existing {
            if stored_hash == hash {
                return Ok(serde_json::from_str(&stored_json)?);
            }
            return Err(Error::RequestConflict { request_id: req_id.to_string() });
        }
        Some(hash)
    } else {
        None
    };

    let outcome = match envelope.op {
        Op::Init { name, template, goal, workspace_root, bootstrap_symlinks, bootstrap_copies } => {
            exec_init(&tx, actor, &now, &name, template, goal.as_deref(), &workspace_root, &bootstrap_symlinks, &bootstrap_copies)?
        }
        other_op => {
            let team = resolve_team_for_actor(&tx, envelope.team.as_deref(), actor)?;
            if let Actor::Seat { seat_name, .. } = actor {
                let clean = seat_name.trim().trim_start_matches('@');
                tx.execute(
                    "UPDATE seats SET last_seen_at = ?1, updated_at = ?1 WHERE team_id = ?2 AND (name = ?3 OR seat_id = ?3)",
                    params![now, team.team_id, clean],
                )?;
            }
            exec_team_op(&tx, &team.team_id, actor, &now, other_op)?
        }
    };

    write_audit(&tx, &outcome.team_id, actor, op_name, outcome.ref_id.as_deref(), "{}", &now)?;

    let receipt = Receipt {
        request_id: envelope.request_id.clone(),
        team_id: outcome.team_id,
        actor: actor.actor_name().to_string(),
        next: outcome.next,
        failed_stage: None,
        data: outcome.data,
    };

    if let (Some(req_id), Some(hash)) = (envelope.request_id.as_deref(), payload_hash.as_deref()) {
        let receipt_json = serde_json::to_string(&receipt)?;
        tx.execute(
            "INSERT INTO mutation_receipts (caller_id, request_id, op, payload_hash, state, receipt_json, created_at) VALUES (?1, ?2, ?3, ?4, 'completed', ?5, ?6)",
            params![caller_id, req_id, op_name, hash, receipt_json, now],
        )?;
    }

    tx.commit()?;
    Ok(receipt)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn exec_init(
    conn: &Connection,
    actor: &Actor,
    now: &str,
    name: &str,
    template: Option<TemplateKind>,
    goal: Option<&str>,
    workspace_root: &str,
    bootstrap_symlinks: &[String],
    bootstrap_copies: &[String],
) -> Result<OpOutcome> {
    let clean_name = name.trim();
    if clean_name.is_empty() {
        return Err(Error::validation("team name must not be empty"));
    }
    let symlinks_json = serde_json::to_string(bootstrap_symlinks)?;
    let copies_json = serde_json::to_string(bootstrap_copies)?;
    let template_str = template.map(TemplateKind::as_str);
    let goal_str = goal.unwrap_or("");

    let existing: Option<String> = conn
        .query_row("SELECT team_id FROM teams WHERE name = ?1", params![clean_name], |row| row.get(0))
        .optional()?;

    let team_id = if let Some(tid) = existing {
        conn.execute(
            "UPDATE teams SET goal = ?1, template = COALESCE(?2, template), workspace_root = ?3, bootstrap_symlinks = ?4, bootstrap_copies = ?5, updated_at = ?6 WHERE team_id = ?7",
            params![goal_str, template_str, workspace_root, symlinks_json, copies_json, now, tid],
        )?;
        tid
    } else {
        let tid = new_id("team");
        conn.execute(
            "INSERT INTO teams (team_id, name, goal, template, workspace_root, bootstrap_symlinks, bootstrap_copies, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![tid, clean_name, goal_str, template_str, workspace_root, symlinks_json, copies_json, now],
        )?;
        tid
    };

    if let Some(tpl) = template {
        seed_template_seats(conn, &team_id, tpl, now)?;
    }

    if let Actor::Seat { seat_name, .. } = actor {
        let clean = seat_name.trim().trim_start_matches('@');
        conn.execute(
            "UPDATE seats SET last_seen_at = ?1, updated_at = ?1 WHERE team_id = ?2 AND (name = ?3 OR seat_id = ?3)",
            params![now, team_id, clean],
        )?;
    }

    let proj = build_team_projection(conn, &team_id)?;
    Ok(OpOutcome {
        team_id: team_id.clone(),
        ref_id: Some(team_id),
        next: Some("orcherd status".to_string()),
        data: ReceiptData::TeamStatus(Box::new(proj)),
    })
}

fn exec_team_op(conn: &Connection, team_id: &str, actor: &Actor, now: &str, op: Op) -> Result<OpOutcome> {
    match op {
        Op::Init { .. } => Err(Error::validation("Init handled at top level")),
        Op::Status => Ok(OpOutcome::simple(team_id, Some(team_id.to_string()), ReceiptData::TeamStatus(Box::new(build_team_projection(conn, team_id)?)))),
        op @ Op::SpawnPrepare { .. } => exec_spawn_prepare(conn, team_id, actor, now, op),
        Op::SpawnBindAddress { seat, dispatch_id, address, worktree_path, branch, resume_session } => {
            let s = resolve_seat(conn, team_id, &seat)?;
            let (addr_kind, addr_machine, addr_term) = address.to_db_columns();
            conn.execute(
                "UPDATE seats SET address_kind = ?1, address_machine = ?2, address_terminal = ?3, worktree_path = COALESCE(?4, worktree_path), branch = COALESCE(?5, branch), resume_session = COALESCE(?6, resume_session), updated_at = ?7 WHERE team_id = ?8 AND seat_id = ?9",
                params![addr_kind, addr_machine, addr_term, worktree_path, branch, resume_session, now, team_id, s.seat_id],
            )?;
            if let Some(did) = dispatch_id.as_deref() {
                let disp = resolve_dispatch(conn, team_id, did)?;
                conn.execute(
                    "UPDATE dispatches SET state = 'working', stage = 'running', machine = COALESCE(?1, machine), terminal_address = COALESCE(?2, terminal_address), worktree_path = COALESCE(?3, worktree_path), updated_at = ?4 WHERE dispatch_id = ?5",
                    params![addr_machine, addr_term, worktree_path, now, disp.dispatch_id],
                )?;
            }
            let updated = resolve_seat(conn, team_id, &s.seat_id)?;
            Ok(OpOutcome::simple(team_id, Some(updated.seat_id.clone()), ReceiptData::Seat(Box::new(updated))))
        }
        Op::SeatList { include_remote } => Ok(OpOutcome::simple(team_id, None, ReceiptData::Seats(list_seats(conn, team_id, include_remote)?))),
        Op::SeatShow { seat } => {
            let s = resolve_seat(conn, team_id, &seat)?;
            Ok(OpOutcome::simple(team_id, Some(s.seat_id.clone()), ReceiptData::Seat(Box::new(s))))
        }
        Op::SeatStop { target, reason } => {
            let stop_reason = reason.as_deref().unwrap_or("user_stopped");
            if let Ok(disp) = resolve_dispatch(conn, team_id, &target) {
                conn.execute("UPDATE dispatches SET state = 'stopped', stage = 'stopped', stop_reason = ?1, settled_at = ?2, updated_at = ?2 WHERE dispatch_id = ?3", params![stop_reason, now, disp.dispatch_id])?;
                let updated = resolve_dispatch(conn, team_id, &disp.dispatch_id)?;
                return Ok(OpOutcome::simple(team_id, Some(updated.dispatch_id.clone()), ReceiptData::Dispatch(Box::new(updated))));
            }
            let seat = resolve_seat(conn, team_id, &target)?;
            conn.execute("UPDATE dispatches SET state = 'stopped', stage = 'stopped', stop_reason = ?1, settled_at = ?2, updated_at = ?2 WHERE team_id = ?3 AND seat_id = ?4 AND state IN ('starting', 'ready', 'working')", params![stop_reason, now, team_id, seat.seat_id])?;
            Ok(OpOutcome::simple(team_id, Some(seat.seat_id.clone()), ReceiptData::Seat(Box::new(seat))))
        }
        Op::SeatAbandon { dispatch_id } => {
            let disp = resolve_dispatch(conn, team_id, &dispatch_id)?;
            conn.execute("UPDATE dispatches SET state = 'abandoned', stage = 'abandoned', stop_reason = COALESCE(stop_reason, 'abandoned'), settled_at = ?1, updated_at = ?1 WHERE dispatch_id = ?2", params![now, disp.dispatch_id])?;
            conn.execute("UPDATE tasks SET status = 'ready', assignee_seat = NULL, updated_at = ?1 WHERE task_id = ?2 AND status IN ('assigned', 'in_progress')", params![now, disp.task_id])?;
            let updated = resolve_dispatch(conn, team_id, &disp.dispatch_id)?;
            Ok(OpOutcome::simple(team_id, Some(updated.dispatch_id.clone()), ReceiptData::Dispatch(Box::new(updated))))
        }
        Op::SeatRelease { dispatch_id } => {
            let disp = resolve_dispatch(conn, team_id, &dispatch_id)?;
            conn.execute("UPDATE dispatches SET state = 'stopped', stage = 'released', stop_reason = 'released', settled_at = ?1, updated_at = ?1 WHERE dispatch_id = ?2", params![now, disp.dispatch_id])?;
            let updated = resolve_dispatch(conn, team_id, &disp.dispatch_id)?;
            Ok(OpOutcome::simple(team_id, Some(updated.dispatch_id.clone()), ReceiptData::Dispatch(Box::new(updated))))
        }
        Op::SeatRebind { seat, address } => {
            let s = resolve_seat(conn, team_id, &seat)?;
            let (addr_kind, addr_machine, addr_term) = address.to_db_columns();
            conn.execute("UPDATE seats SET address_kind = ?1, address_machine = ?2, address_terminal = ?3, updated_at = ?4 WHERE team_id = ?5 AND seat_id = ?6", params![addr_kind, addr_machine, addr_term, now, team_id, s.seat_id])?;
            conn.execute("UPDATE dispatches SET machine = ?1, terminal_address = ?2, updated_at = ?3 WHERE team_id = ?4 AND seat_id = ?5 AND state IN ('starting', 'ready', 'working')", params![addr_machine, addr_term, now, team_id, s.seat_id])?;
            let updated = resolve_seat(conn, team_id, &s.seat_id)?;
            Ok(OpOutcome::simple(team_id, Some(updated.seat_id.clone()), ReceiptData::Seat(Box::new(updated))))
        }
        Op::SeatRemove { seat } => {
            let target = resolve_seat(conn, team_id, &seat)?;
            let supervisor = find_supervisor_seat_name(conn, team_id)?;
            let payload = json!({ "seat_id": target.seat_id, "seat_name": target.name }).to_string();
            let question = format!("Approve removal of seat '{}'", target.name);
            let gate = write_gate(conn, team_id, None, None, GateKind::ApprovalRemove, actor.actor_name(), &supervisor, &question, &payload, now)?;
            write_inbox(conn, team_id, &supervisor, actor.actor_name(), InboxKind::Question, &gate.gate_id, &question, &question, now)?;
            Ok(OpOutcome { team_id: team_id.to_string(), ref_id: Some(gate.gate_id.clone()), next: Some(format!("Human operator must run: orcherd approve {}", gate.gate_id)), data: ReceiptData::Gate(Box::new(gate)) })
        }
        Op::SeatHandoff { to_seat } => {
            let target = resolve_seat(conn, team_id, &to_seat)?;
            let payload = json!({ "to_seat_id": target.seat_id, "to_seat_name": target.name, "from_seat": actor.actor_name() }).to_string();
            let question = format!("Approve supervisor role handoff to '{}'", target.name);
            let gate = write_gate(conn, team_id, None, None, GateKind::ApprovalRole, actor.actor_name(), &target.name, &question, &payload, now)?;
            write_inbox(conn, team_id, &target.name, actor.actor_name(), InboxKind::Question, &gate.gate_id, &question, &question, now)?;
            Ok(OpOutcome { team_id: team_id.to_string(), ref_id: Some(gate.gate_id.clone()), next: Some(format!("Human operator must run: orcherd approve {}", gate.gate_id)), data: ReceiptData::Gate(Box::new(gate)) })
        }
        op @ Op::TaskCreate { .. } => exec_task_create(conn, team_id, actor, now, op),
        Op::TaskList { status, ready_only } => Ok(OpOutcome::simple(team_id, None, ReceiptData::Tasks(list_tasks(conn, team_id, status, ready_only)?))),
        Op::TaskShow { task_id } => {
            let task = resolve_task(conn, team_id, &task_id)?;
            Ok(OpOutcome::simple(team_id, Some(task.task_id.clone()), ReceiptData::Task(Box::new(task))))
        }
        Op::TaskAssign { task_id, to_seat, reviewer, spec } => exec_task_assign(conn, team_id, actor, now, &task_id, &to_seat, reviewer.as_deref(), spec.as_deref()),
        Op::TaskClaim { task_id } => exec_task_claim(conn, team_id, actor, now, &task_id),
        Op::TaskUpdate { task_id, status } => {
            let task = resolve_task(conn, team_id, &task_id)?;
            let completed_at = if status == TaskStatus::Completed { Some(now) } else { task.completed_at.as_deref() };
            conn.execute("UPDATE tasks SET status = ?1, completed_at = ?2, updated_at = ?3 WHERE task_id = ?4", params![status.as_str(), completed_at, now, task.task_id])?;
            if status == TaskStatus::Completed {
                let _ = unblock_ready_dag_tasks(conn, team_id, actor, now)?;
            }
            let updated = resolve_task(conn, team_id, &task.task_id)?;
            Ok(OpOutcome::simple(team_id, Some(updated.task_id.clone()), ReceiptData::Task(Box::new(updated))))
        }
        Op::InboxRead { peek, seat, limit } => exec_inbox_read(conn, team_id, actor, now, peek, seat.as_deref(), limit),
        Op::Send { to, subject, body } => {
            let recipients = resolve_send_recipients(conn, team_id, actor.actor_name(), &to)?;
            if recipients.is_empty() {
                return Err(Error::validation(format!("no recipient seats matched '{to}'")));
            }
            let mut item_ids = Vec::with_capacity(recipients.len());
            for recipient in &recipients {
                let item_id = new_id("msg");
                let item = write_inbox_with_id(conn, &item_id, team_id, recipient, actor.actor_name(), InboxKind::FollowUp, &item_id, &subject, &body, now)?;
                item_ids.push(item.item_id);
            }
            Ok(OpOutcome::simple(team_id, item_ids.first().cloned(), ReceiptData::Sent { recipients, item_ids }))
        }
        Op::Ask { task_id, question, body } => exec_ask(conn, team_id, actor, now, task_id.as_deref(), &question, body.as_deref()),
        Op::Reply { target_id, body } => exec_reply(conn, team_id, actor, now, &target_id, &body),
        Op::Done { task_id, result, branch, commit_sha, outcome } => exec_done(conn, team_id, actor, now, task_id.as_deref(), &result, branch.as_deref(), commit_sha.as_deref(), outcome),
        Op::Verdict { task_id, verdict, body } => exec_verdict(conn, team_id, actor, now, &task_id, verdict, &body),
        Op::GateList { status } => Ok(OpOutcome::simple(team_id, None, ReceiptData::Gates(list_gates(conn, team_id, status)?))),
        Op::BulletinPost { subject, body, mentions, reply_to, kind } => exec_bulletin_post(conn, team_id, actor, now, subject.as_deref(), &body, &mentions, reply_to.as_deref(), kind.unwrap_or_default()),
        Op::BulletinList { after_seq, limit } => Ok(OpOutcome::simple(team_id, None, ReceiptData::Posts(list_posts(conn, team_id, after_seq, limit)?))),
        Op::FindingAdd { title, evidence, task_id, confidence } => {
            let resolved_task_id = if let Some(tref) = task_id.as_deref() { Some(resolve_task(conn, team_id, tref)?.task_id) } else { None };
            let finding_id = new_id("find");
            let conf = confidence.unwrap_or_default();
            conn.execute("INSERT INTO findings (finding_id, team_id, author_seat, task_id, title, evidence, confidence, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![finding_id, team_id, actor.actor_name(), resolved_task_id, title, evidence, conf.as_str(), now])?;
            let finding = conn.query_row("SELECT * FROM findings WHERE finding_id = ?1", params![finding_id], store::map_finding)?;
            Ok(OpOutcome::simple(team_id, Some(finding.finding_id.clone()), ReceiptData::Finding(Box::new(finding))))
        }
        Op::FindingList => Ok(OpOutcome::simple(team_id, None, ReceiptData::Findings(list_findings(conn, team_id)?))),
        Op::PendingList => Ok(OpOutcome::simple(team_id, None, ReceiptData::Gates(list_gates(conn, team_id, Some(GateStatus::Pending))?))),
        Op::Approve { gate_id, resolution } => exec_approve(conn, team_id, actor, now, &gate_id, resolution.as_deref()),
        Op::AuditLog { limit } => Ok(OpOutcome::simple(team_id, None, ReceiptData::Audit(list_audit(conn, team_id, limit)?))),
        Op::DaemonRecordDoorbell { seat, rung_through_seq, outcome } => exec_daemon_record_doorbell(conn, team_id, now, &seat, rung_through_seq, outcome),
        Op::DaemonEscalate { seat, dispatch_id, stop_reason, detail } => exec_daemon_escalate(conn, team_id, now, &seat, dispatch_id.as_deref(), &stop_reason, &detail),
    }
}

fn exec_spawn_prepare(conn: &Connection, team_id: &str, actor: &Actor, now: &str, op: Op) -> Result<OpOutcome> {
    let Op::SpawnPrepare { seat, role, agent, model, account, effort, worktree_path, branch, machine, task_id, spec, title, keep_open, resume_session, retry_of } = op else {
        return Err(Error::validation("expected SpawnPrepare"));
    };
    let clean_name = seat.trim().trim_start_matches('@');
    if clean_name.is_empty() {
        return Err(Error::validation("seat name must not be empty"));
    }

    let existing_seat = resolve_seat(conn, team_id, clean_name).ok();
    let warm_reuse = existing_seat.as_ref().is_some_and(|s| s.address.is_bound() && resume_session.is_none());

    let seat_record = if let Some(existing) = existing_seat {
        let new_role = role.unwrap_or(existing.role);
        let new_agent = agent.unwrap_or(existing.agent);
        let new_model = model.or(existing.model);
        let new_account = account.or(existing.account);
        let new_effort = effort.or(existing.effort);
        let new_wt = worktree_path.clone().or(existing.worktree_path);
        let new_branch = branch.clone().or(existing.branch);
        let new_keep_open = keep_open.unwrap_or(existing.keep_open);
        let new_resume = resume_session.or(existing.resume_session);

        conn.execute(
            "UPDATE seats SET role = ?1, agent = ?2, model = ?3, account = ?4, effort = ?5, worktree_path = ?6, branch = ?7, keep_open = ?8, resume_session = ?9, updated_at = ?10 WHERE team_id = ?11 AND seat_id = ?12",
            params![new_role.as_str(), new_agent, new_model, new_account, new_effort, new_wt, new_branch, i64::from(new_keep_open), new_resume, now, team_id, existing.seat_id],
        )?;
        resolve_seat(conn, team_id, &existing.seat_id)?
    } else {
        let seat_id = new_id("seat");
        let r = role.unwrap_or(SeatRole::Worker);
        let ag = agent.unwrap_or_else(|| "antigravity".to_string());
        let ko = keep_open.unwrap_or(true);
        conn.execute(
            "INSERT INTO seats (team_id, seat_id, name, role, agent, model, account, effort, address_kind, worktree_path, branch, keep_open, resume_session, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'none', ?9, ?10, ?11, ?12, ?13, ?13)",
            params![team_id, seat_id, clean_name, r.as_str(), ag, model, account, effort, worktree_path, branch, i64::from(ko), resume_session, now],
        )?;
        conn.execute("INSERT OR IGNORE INTO doorbell (team_id, seat_id, rung_through_seq, attempts) VALUES (?1, ?2, 0, 0)", params![team_id, seat_id])?;
        resolve_seat(conn, team_id, &seat_id)?
    };

    let prev_dispatch = if let Some(retry_id) = retry_of.as_deref() { Some(resolve_dispatch(conn, team_id, retry_id)?) } else { None };
    if let Some(prev) = &prev_dispatch
        && (prev.failure_count >= 3 || prev.state == DispatchState::CircuitBroken)
    {
        return Err(Error::CircuitBroken { task_id: prev.task_id.clone(), dispatch_id: prev.dispatch_id.clone(), failures: prev.failure_count });
    }

    let task_record = if let Some(tid) = task_id.as_deref() {
        let task = resolve_task(conn, team_id, tid)?;
        if let Some(rev) = task.reviewer_seat.as_deref() {
            check_reviewer_blindness(conn, team_id, &seat_record.name, rev)?;
        }
        let new_spec = spec.as_deref().unwrap_or(&task.spec);
        let new_title = title.as_deref().unwrap_or(&task.title);
        conn.execute("UPDATE tasks SET title = ?1, spec = ?2, assignee_seat = ?3, status = 'in_progress', updated_at = ?4 WHERE task_id = ?5", params![new_title, new_spec, seat_record.name, now, task.task_id])?;
        Some(resolve_task(conn, team_id, &task.task_id)?)
    } else if let Some(prev) = &prev_dispatch {
        let task = resolve_task(conn, team_id, &prev.task_id)?;
        if let Some(rev) = task.reviewer_seat.as_deref() {
            check_reviewer_blindness(conn, team_id, &seat_record.name, rev)?;
        }
        conn.execute("UPDATE tasks SET assignee_seat = ?1, status = 'in_progress', updated_at = ?2 WHERE task_id = ?3", params![seat_record.name, now, task.task_id])?;
        Some(resolve_task(conn, team_id, &task.task_id)?)
    } else if let Some(spec_text) = spec.as_deref() {
        let derived_title = title.unwrap_or_else(|| derive_title(spec_text));
        let tid = new_id("task");
        conn.execute(
            "INSERT INTO tasks (task_id, team_id, title, spec, status, deps, assignee_seat, created_by, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'in_progress', '[]', ?5, ?6, ?7, ?7)",
            params![tid, team_id, derived_title, spec_text, seat_record.name, actor.actor_name(), now],
        )?;
        Some(resolve_task(conn, team_id, &tid)?)
    } else {
        None
    };

    let dispatch_record = if let Some(task) = &task_record {
        let inherited_failures = prev_dispatch.as_ref().map_or(0, |d| d.failure_count);
        let dispatch_id = new_id("disp");
        let state = if warm_reuse { DispatchState::Working } else { DispatchState::Starting };
        let stage = if warm_reuse { "dispatched" } else { "accepted" };
        let mach = machine.or_else(|| seat_record.address.machine().map(str::to_string));
        let term = seat_record.address.terminal_str().map(str::to_string);
        let wt = worktree_path.or_else(|| seat_record.worktree_path.clone());

        conn.execute(
            "INSERT INTO dispatches (dispatch_id, team_id, task_id, seat_id, retry_of_dispatch_id, state, stage, failure_count, machine, terminal_address, worktree_path, consumer_generation, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, ?12, ?12)",
            params![dispatch_id, team_id, task.task_id, seat_record.seat_id, prev_dispatch.as_ref().map(|d| d.dispatch_id.as_str()), state.as_str(), stage, inherited_failures, mach, term, wt, now],
        )?;
        write_inbox(conn, team_id, &seat_record.name, actor.actor_name(), InboxKind::Dispatch, &dispatch_id, &task.title, &task.spec, now)?;
        Some(resolve_dispatch(conn, team_id, &dispatch_id)?)
    } else {
        None
    };

    let ref_id = dispatch_record.as_ref().map(|d| d.dispatch_id.clone()).or_else(|| Some(seat_record.seat_id.clone()));
    Ok(OpOutcome {
        team_id: team_id.to_string(),
        ref_id,
        next: Some("orcherd inbox".to_string()),
        data: ReceiptData::SpawnPrepared { seat: Box::new(seat_record), task: task_record.map(Box::new), dispatch: dispatch_record.map(Box::new), warm_reuse },
    })
}

fn exec_approve(conn: &Connection, team_id: &str, actor: &Actor, now: &str, gate_ref: &str, resolution: Option<&str>) -> Result<OpOutcome> {
    if !actor.is_human() {
        return Err(Error::PermissionDenied("only a human operator may approve pending gates".to_string()));
    }
    let gate = resolve_gate(conn, team_id, gate_ref)?;
    if gate.status != GateStatus::Pending {
        return Err(Error::validation(format!("gate '{}' is already {}", gate.gate_id, gate.status)));
    }
    let parsed_payload: serde_json::Value = serde_json::from_str(&gate.payload_json).unwrap_or_else(|_| json!({}));

    match gate.kind {
        GateKind::ApprovalRemove => {
            let seat_id = parsed_payload.get("seat_id").and_then(serde_json::Value::as_str).unwrap_or("");
            let seat_name = parsed_payload.get("seat_name").and_then(serde_json::Value::as_str).unwrap_or("");
            conn.execute("DELETE FROM doorbell WHERE team_id = ?1 AND seat_id = ?2", params![team_id, seat_id])?;
            conn.execute("DELETE FROM seats WHERE team_id = ?1 AND (seat_id = ?2 OR name = ?3)", params![team_id, seat_id, seat_name])?;
        }
        GateKind::ApprovalRole => {
            let to_seat_id = parsed_payload.get("to_seat_id").and_then(serde_json::Value::as_str).unwrap_or("");
            let to_seat_name = parsed_payload.get("to_seat_name").and_then(serde_json::Value::as_str).unwrap_or(&gate.to_seat);
            conn.execute("UPDATE seats SET role = 'worker', updated_at = ?1 WHERE team_id = ?2 AND role = 'supervisor'", params![now, team_id])?;
            conn.execute("UPDATE seats SET role = 'supervisor', updated_at = ?1 WHERE team_id = ?2 AND (seat_id = ?3 OR name = ?4)", params![now, team_id, to_seat_id, to_seat_name])?;
            write_inbox(conn, team_id, to_seat_name, "human", InboxKind::GateResolved, &gate.gate_id, "Supervisor role handoff approved", &format!("Seat '{to_seat_name}' is now supervisor."), now)?;
        }
        GateKind::Question | GateKind::Escalation => {
            if let Some(tid) = gate.task_id.as_deref() {
                conn.execute("UPDATE tasks SET status = 'in_progress', updated_at = ?1 WHERE task_id = ?2 AND status = 'blocked'", params![now, tid])?;
            }
            write_inbox(conn, team_id, &gate.asked_by_seat, "human", InboxKind::GateResolved, &gate.gate_id, &format!("Approved: {}", gate.question), resolution.unwrap_or("approved"), now)?;
        }
    }

    let res_text = resolution.unwrap_or("approved");
    conn.execute("UPDATE gates SET status = 'resolved', resolution = ?1, resolved_by = 'human', resolved_at = ?2 WHERE gate_id = ?3", params![res_text, now, gate.gate_id])?;
    let updated = resolve_gate(conn, team_id, &gate.gate_id)?;
    Ok(OpOutcome::simple(team_id, Some(updated.gate_id.clone()), ReceiptData::Gate(Box::new(updated))))
}

fn exec_task_create(conn: &Connection, team_id: &str, actor: &Actor, now: &str, op: Op) -> Result<OpOutcome> {
    let Op::TaskCreate { title, spec, deps, assign, reviewer, issue_ref, parent_id, placement } = op else {
        return Err(Error::validation("expected TaskCreate"));
    };
    if spec.trim().is_empty() {
        return Err(Error::validation("task spec must not be empty"));
    }
    let clean_assign = assign.as_deref().map(|s| s.trim().trim_start_matches('@')).filter(|s| !s.is_empty());
    let clean_reviewer = reviewer.as_deref().map(|s| s.trim().trim_start_matches('@')).filter(|s| !s.is_empty());

    if let (Some(a), Some(r)) = (clean_assign, clean_reviewer) {
        check_reviewer_blindness(conn, team_id, a, r)?;
    }

    let mut canonical_deps = Vec::with_capacity(deps.len());
    let mut all_deps_completed = true;
    for dep_ref in &deps {
        let dep_task = resolve_task(conn, team_id, dep_ref)?;
        if dep_task.status != TaskStatus::Completed {
            all_deps_completed = false;
        }
        canonical_deps.push(dep_task.task_id);
    }

    let initial_status = if !canonical_deps.is_empty() && !all_deps_completed {
        TaskStatus::Pending
    } else if clean_assign.is_some() {
        TaskStatus::Assigned
    } else {
        TaskStatus::Ready
    };

    let task_id = new_id("task");
    let final_title = title.as_deref().map(str::trim).filter(|t| !t.is_empty()).map_or_else(|| derive_title(&spec), str::to_string);
    let deps_json = serde_json::to_string(&canonical_deps)?;

    conn.execute(
        "INSERT INTO tasks (task_id, team_id, parent_id, title, spec, placement, status, deps, assignee_seat, reviewer_seat, review_round, issue_ref, created_by, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11, ?12, ?13, ?13)",
        params![task_id, team_id, parent_id, final_title, spec, placement, initial_status.as_str(), deps_json, clean_assign, clean_reviewer, issue_ref, actor.actor_name(), now],
    )?;

    if let Some(assignee) = clean_assign
        && initial_status != TaskStatus::Pending
    {
        write_inbox(conn, team_id, assignee, actor.actor_name(), InboxKind::Assigned, &task_id, &final_title, &spec, now)?;
    }

    let created = resolve_task(conn, team_id, &task_id)?;
    Ok(OpOutcome::simple(team_id, Some(created.task_id.clone()), ReceiptData::Task(Box::new(created))))
}

#[allow(clippy::too_many_arguments)]
fn exec_task_assign(conn: &Connection, team_id: &str, actor: &Actor, now: &str, task_ref: &str, to_seat: &str, reviewer: Option<&str>, spec: Option<&str>) -> Result<OpOutcome> {
    let task = resolve_task(conn, team_id, task_ref)?;
    let clean_to = to_seat.trim().trim_start_matches('@');
    if clean_to.is_empty() {
        return Err(Error::validation("assignee seat must not be empty"));
    }
    let effective_reviewer = reviewer.map(|r| r.trim().trim_start_matches('@').to_string()).or_else(|| task.reviewer_seat.clone());
    if let Some(rev) = effective_reviewer.as_deref() {
        check_reviewer_blindness(conn, team_id, clean_to, rev)?;
    }
    let new_spec = spec.unwrap_or(&task.spec);
    let new_status = match task.status {
        TaskStatus::Ready => TaskStatus::Assigned,
        other => other,
    };
    conn.execute(
        "UPDATE tasks SET assignee_seat = ?1, reviewer_seat = ?2, spec = ?3, status = ?4, updated_at = ?5 WHERE task_id = ?6",
        params![clean_to, effective_reviewer, new_spec, new_status.as_str(), now, task.task_id],
    )?;
    write_inbox(conn, team_id, clean_to, actor.actor_name(), InboxKind::Assigned, &task.task_id, &task.title, new_spec, now)?;
    let updated = resolve_task(conn, team_id, &task.task_id)?;
    Ok(OpOutcome::simple(team_id, Some(updated.task_id.clone()), ReceiptData::Task(Box::new(updated))))
}

fn exec_task_claim(conn: &Connection, team_id: &str, actor: &Actor, now: &str, task_ref: &str) -> Result<OpOutcome> {
    let Actor::Seat { seat_name, .. } = actor else {
        return Err(Error::validation("only an agent seat may claim a task"));
    };
    let clean_seat = seat_name.trim().trim_start_matches('@');
    let task = resolve_task(conn, team_id, task_ref)?;
    if let Some(rev) = task.reviewer_seat.as_deref() {
        check_reviewer_blindness(conn, team_id, clean_seat, rev)?;
    }
    let can_claim = task.status == TaskStatus::Ready
        || (task.status == TaskStatus::Assigned && task.assignee_seat.as_deref() == Some(clean_seat));
    if !can_claim {
        return Err(Error::validation(format!("task '{}' is not claimable (status: {})", task.task_id, task.status)));
    }
    let changed = conn.execute(
        "UPDATE tasks SET status = 'in_progress', assignee_seat = ?1, updated_at = ?2 WHERE task_id = ?3 AND status IN ('ready', 'assigned')",
        params![clean_seat, now, task.task_id],
    )?;
    if changed == 0 {
        return Err(Error::validation(format!("task '{}' was already claimed", task.task_id)));
    }
    let updated = resolve_task(conn, team_id, &task.task_id)?;
    Ok(OpOutcome::simple(team_id, Some(updated.task_id.clone()), ReceiptData::Task(Box::new(updated))))
}

fn exec_inbox_read(conn: &Connection, team_id: &str, actor: &Actor, now: &str, peek: bool, seat: Option<&str>, limit: Option<usize>) -> Result<OpOutcome> {
    let target_seat = if let Some(s) = seat {
        s.trim().trim_start_matches('@').to_string()
    } else if let Actor::Seat { seat_name, .. } = actor {
        seat_name.trim().trim_start_matches('@').to_string()
    } else {
        find_supervisor_seat_name(conn, team_id)?
    };
    let mut items = list_inbox(conn, team_id, &target_seat, true, limit)?;
    if !peek && !items.is_empty() {
        for item in &mut items {
            conn.execute("UPDATE inbox SET read_at = ?1, delivered_at = COALESCE(delivered_at, ?1) WHERE item_id = ?2", params![now, item.item_id])?;
            item.read_at = Some(now.to_string());
            if item.delivered_at.is_none() {
                item.delivered_at = Some(now.to_string());
            }
        }
    }
    Ok(OpOutcome::simple(team_id, Some(target_seat), ReceiptData::Inbox(items)))
}

fn exec_ask(conn: &Connection, team_id: &str, actor: &Actor, now: &str, task_ref: Option<&str>, question: &str, body: Option<&str>) -> Result<OpOutcome> {
    let task = if let Some(tref) = task_ref {
        Some(resolve_task(conn, team_id, tref)?)
    } else {
        find_active_task_for_seat(conn, team_id, actor.actor_name())?
    };
    let active_dispatch = if let Ok(seat) = resolve_seat(conn, team_id, actor.actor_name()) {
        find_active_dispatch_for_seat(conn, team_id, &seat.seat_id, task.as_ref().map(|t| t.task_id.as_str()))?
    } else {
        None
    };
    let supervisor = find_supervisor_seat_name(conn, team_id)?;
    let full_body = body.unwrap_or(question);
    let payload = json!({ "body": full_body }).to_string();
    let gate = write_gate(conn, team_id, task.as_ref().map(|t| t.task_id.as_str()), active_dispatch.as_ref().map(|d| d.dispatch_id.as_str()), GateKind::Question, actor.actor_name(), &supervisor, question, &payload, now)?;
    if let Some(t) = &task {
        conn.execute("UPDATE tasks SET status = 'blocked', updated_at = ?1 WHERE task_id = ?2", params![now, t.task_id])?;
    }
    write_inbox(conn, team_id, &supervisor, actor.actor_name(), InboxKind::Question, &gate.gate_id, question, full_body, now)?;
    Ok(OpOutcome {
        team_id: team_id.to_string(),
        ref_id: Some(gate.gate_id.clone()),
        next: Some("End your turn and wait for reply doorbell.".to_string()),
        data: ReceiptData::Gate(Box::new(gate)),
    })
}

fn exec_reply(conn: &Connection, team_id: &str, actor: &Actor, now: &str, target_id: &str, body: &str) -> Result<OpOutcome> {
    let gate = match resolve_gate(conn, team_id, target_id) {
        Ok(g) => g,
        Err(Error::NotFound { .. }) => {
            let inbox_item: Option<InboxItem> = conn
                .query_row("SELECT * FROM inbox WHERE team_id = ?1 AND item_id = ?2", params![team_id, target_id.trim()], map_inbox_item)
                .optional()?;
            if let Some(item) = inbox_item {
                resolve_gate(conn, team_id, &item.ref_id)?
            } else {
                return Err(Error::not_found("gate", target_id));
            }
        }
        Err(other) => return Err(other),
    };
    conn.execute("UPDATE gates SET status = 'resolved', resolution = ?1, resolved_by = ?2, resolved_at = ?3 WHERE gate_id = ?4", params![body, actor.actor_name(), now, gate.gate_id])?;
    if let Some(tid) = gate.task_id.as_deref() {
        conn.execute("UPDATE tasks SET status = 'in_progress', updated_at = ?1 WHERE task_id = ?2 AND status = 'blocked'", params![now, tid])?;
    }
    write_inbox(conn, team_id, &gate.asked_by_seat, actor.actor_name(), InboxKind::GateResolved, &gate.gate_id, &format!("Reply: {}", gate.question), body, now)?;
    let updated = resolve_gate(conn, team_id, &gate.gate_id)?;
    Ok(OpOutcome::simple(team_id, Some(updated.gate_id.clone()), ReceiptData::Gate(Box::new(updated))))
}

#[allow(clippy::too_many_arguments)]
fn exec_done(
    conn: &Connection,
    team_id: &str,
    actor: &Actor,
    now: &str,
    task_ref: Option<&str>,
    result: &str,
    branch: Option<&str>,
    commit_sha: Option<&str>,
    outcome: DoneOutcome,
) -> Result<OpOutcome> {
    let task = if let Some(tref) = task_ref {
        resolve_task(conn, team_id, tref)?
    } else {
        find_active_task_for_seat(conn, team_id, actor.actor_name())?
            .ok_or_else(|| Error::not_found("task", actor.actor_name()))?
    };

    let settled_dispatch = if let Ok(seat) = resolve_seat(conn, team_id, actor.actor_name()) {
        if let Some(disp) = find_active_dispatch_for_seat(conn, team_id, &seat.seat_id, Some(&task.task_id))? {
            let (new_state, new_stage) = match outcome {
                DoneOutcome::Succeeded => (DispatchState::SettledSucceeded, "completed"),
                DoneOutcome::Failed => (DispatchState::SettledFailed, "failed"),
            };
            conn.execute("UPDATE dispatches SET state = ?1, stage = ?2, settled_at = ?3, updated_at = ?3 WHERE dispatch_id = ?4", params![new_state.as_str(), new_stage, now, disp.dispatch_id])?;
            Some(resolve_dispatch(conn, team_id, &disp.dispatch_id)?)
        } else {
            None
        }
    } else {
        None
    };

    let supervisor = find_supervisor_seat_name(conn, team_id)?;
    let mut unblocked_task_ids = Vec::new();

    match outcome {
        DoneOutcome::Failed => {
            conn.execute("UPDATE tasks SET status = 'failed', result = ?1, branch = COALESCE(?2, branch), commit_sha = COALESCE(?3, commit_sha), updated_at = ?4 WHERE task_id = ?5", params![result, branch, commit_sha, now, task.task_id])?;
            write_inbox(conn, team_id, &supervisor, actor.actor_name(), InboxKind::WorkerDone, &task.task_id, &format!("Task failed: {}", task.title), result, now)?;
        }
        DoneOutcome::Succeeded => {
            if let Some(reviewer) = task.reviewer_seat.as_deref() {
                conn.execute("UPDATE tasks SET status = 'review', result = ?1, branch = COALESCE(?2, branch), commit_sha = COALESCE(?3, commit_sha), updated_at = ?4 WHERE task_id = ?5", params![result, branch, commit_sha, now, task.task_id])?;
                write_inbox(conn, team_id, reviewer, actor.actor_name(), InboxKind::ReviewRequested, &task.task_id, &format!("Review requested: {}", task.title), result, now)?;
                if reviewer != supervisor {
                    write_inbox(conn, team_id, &supervisor, actor.actor_name(), InboxKind::Notice, &task.task_id, &format!("Task in review ({reviewer}): {}", task.title), result, now)?;
                }
            } else {
                conn.execute("UPDATE tasks SET status = 'completed', result = ?1, branch = COALESCE(?2, branch), commit_sha = COALESCE(?3, commit_sha), completed_at = ?4, updated_at = ?4 WHERE task_id = ?5", params![result, branch, commit_sha, now, task.task_id])?;
                write_inbox(conn, team_id, &supervisor, actor.actor_name(), InboxKind::WorkerDone, &task.task_id, &format!("Task completed: {}", task.title), result, now)?;
                unblocked_task_ids = unblock_ready_dag_tasks(conn, team_id, actor, now)?;
            }
        }
    }

    let updated_task = resolve_task(conn, team_id, &task.task_id)?;
    Ok(OpOutcome {
        team_id: team_id.to_string(),
        ref_id: Some(updated_task.task_id.clone()),
        next: Some("End your turn.".to_string()),
        data: ReceiptData::DoneResult { task: Box::new(updated_task), dispatch: settled_dispatch.map(Box::new), unblocked_task_ids },
    })
}

fn exec_verdict(conn: &Connection, team_id: &str, actor: &Actor, now: &str, task_ref: &str, verdict: VerdictKind, body: &str) -> Result<OpOutcome> {
    let task = resolve_task(conn, team_id, task_ref)?;
    if let Some(assignee) = task.assignee_seat.as_deref() {
        check_reviewer_blindness(conn, team_id, assignee, actor.actor_name())?;
        if let Some(rev) = task.reviewer_seat.as_deref() {
            check_reviewer_blindness(conn, team_id, assignee, rev)?;
        }
    }

    let supervisor = find_supervisor_seat_name(conn, team_id)?;
    let post_id = new_id("post");
    conn.execute(
        "INSERT INTO bulletin (post_id, team_id, author_seat, kind, subject, body, mentions_json, created_at) VALUES (?1, ?2, ?3, 'verdict', ?4, ?5, '[]', ?6)",
        params![post_id, team_id, actor.actor_name(), format!("Verdict {} on {}", verdict, task.task_id), body, now],
    )?;

    let mut unblocked_task_ids = Vec::new();
    let mut created_gate: Option<Gate> = None;
    let mut escalated = false;

    match verdict {
        VerdictKind::Pass => {
            conn.execute("UPDATE tasks SET status = 'completed', completed_at = ?1, updated_at = ?1 WHERE task_id = ?2", params![now, task.task_id])?;
            unblocked_task_ids = unblock_ready_dag_tasks(conn, team_id, actor, now)?;
            write_inbox(conn, team_id, &supervisor, actor.actor_name(), InboxKind::Verdict, &task.task_id, &format!("Verdict PASS: {}", task.title), body, now)?;
        }
        VerdictKind::Fix => {
            let new_round = task.review_round + 1;
            if new_round >= 3 {
                escalated = true;
                conn.execute("UPDATE tasks SET status = 'blocked', review_round = ?1, updated_at = ?2 WHERE task_id = ?3", params![new_round, now, task.task_id])?;
                let question = format!("Task '{}' reached {} review fix rounds; auto-escalated to supervisor", task.title, new_round);
                let payload = json!({ "review_round": new_round, "body": body }).to_string();
                let gate = write_gate(conn, team_id, Some(&task.task_id), None, GateKind::Escalation, actor.actor_name(), &supervisor, &question, &payload, now)?;
                write_inbox(conn, team_id, &supervisor, actor.actor_name(), InboxKind::Escalation, &task.task_id, &format!("Auto-escalation (fix round {new_round}): {}", task.title), body, now)?;
                created_gate = Some(gate);
            } else {
                conn.execute("UPDATE tasks SET status = 'in_progress', review_round = ?1, updated_at = ?2 WHERE task_id = ?3", params![new_round, now, task.task_id])?;
                let recipient = task.assignee_seat.as_deref().unwrap_or(supervisor.as_str());
                write_inbox(conn, team_id, recipient, actor.actor_name(), InboxKind::Verdict, &task.task_id, &format!("Verdict FIX (round {new_round}): {}", task.title), body, now)?;
            }
        }
        VerdictKind::Escalate => {
            escalated = true;
            conn.execute("UPDATE tasks SET status = 'blocked', updated_at = ?1 WHERE task_id = ?2", params![now, task.task_id])?;
            let question = format!("Reviewer escalated task '{}'", task.title);
            let payload = json!({ "body": body }).to_string();
            let gate = write_gate(conn, team_id, Some(&task.task_id), None, GateKind::Escalation, actor.actor_name(), &supervisor, &question, &payload, now)?;
            write_inbox(conn, team_id, &supervisor, actor.actor_name(), InboxKind::Escalation, &task.task_id, &format!("Reviewer escalation: {}", task.title), body, now)?;
            created_gate = Some(gate);
        }
    }

    let updated_task = resolve_task(conn, team_id, &task.task_id)?;
    Ok(OpOutcome::simple(
        team_id,
        Some(updated_task.task_id.clone()),
        ReceiptData::VerdictResult { task: Box::new(updated_task), gate: created_gate.map(Box::new), unblocked_task_ids, escalated },
    ))
}

#[allow(clippy::too_many_arguments)]
fn exec_bulletin_post(
    conn: &Connection,
    team_id: &str,
    actor: &Actor,
    now: &str,
    subject: Option<&str>,
    body: &str,
    mentions: &[String],
    reply_to: Option<&str>,
    kind: crate::types::PostKind,
) -> Result<OpOutcome> {
    let mut normalized_mentions = Vec::new();
    for m in mentions {
        let clean = m.trim().trim_start_matches('@');
        if !clean.is_empty() && !normalized_mentions.iter().any(|x| x == clean) {
            normalized_mentions.push(clean.to_string());
        }
    }
    let known_seats = list_seats(conn, team_id, true)?;
    for token in body.split_whitespace() {
        if let Some(rest) = token.strip_prefix('@') {
            let clean = rest.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
            if known_seats.iter().any(|s| s.name == clean) && !normalized_mentions.iter().any(|x| x == clean) {
                normalized_mentions.push(clean.to_string());
            }
        }
    }

    let post_id = new_id("post");
    let derived_subj = derive_title(body);
    let subj = subject.unwrap_or(&derived_subj);
    let mentions_json = serde_json::to_string(&normalized_mentions)?;
    conn.execute(
        "INSERT INTO bulletin (post_id, team_id, author_seat, reply_to_post_id, kind, subject, body, mentions_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![post_id, team_id, actor.actor_name(), reply_to, kind.as_str(), subj, body, mentions_json, now],
    )?;

    for mentioned in &normalized_mentions {
        write_inbox(conn, team_id, mentioned, actor.actor_name(), InboxKind::Mention, &post_id, subj, body, now)?;
    }

    let post = conn.query_row("SELECT * FROM bulletin WHERE post_id = ?1", params![post_id], store::map_post)?;
    Ok(OpOutcome::simple(team_id, Some(post.post_id.clone()), ReceiptData::Post(Box::new(post))))
}

fn exec_daemon_record_doorbell(conn: &Connection, team_id: &str, now: &str, seat_ref: &str, rung_through_seq: i64, outcome: DoorbellOutcome) -> Result<OpOutcome> {
    let seat = resolve_seat(conn, team_id, seat_ref)?;
    let existing = get_doorbell(conn, team_id, &seat.seat_id)?;
    let new_attempts = match outcome {
        DoorbellOutcome::Confirmed | DoorbellOutcome::Unconfirmed => 0,
        _ => existing.as_ref().map_or(1, |d| d.attempts + 1),
    };
    let new_seq = match outcome {
        DoorbellOutcome::Confirmed | DoorbellOutcome::Unconfirmed => rung_through_seq,
        _ => existing.as_ref().map_or(rung_through_seq, |d| d.rung_through_seq),
    };

    conn.execute(
        "INSERT INTO doorbell (team_id, seat_id, rung_through_seq, rung_at, attempts, outcome) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(team_id, seat_id) DO UPDATE SET rung_through_seq = excluded.rung_through_seq, rung_at = excluded.rung_at, attempts = excluded.attempts, outcome = excluded.outcome",
        params![team_id, seat.seat_id, new_seq, now, new_attempts, outcome.as_str()],
    )?;

    if matches!(outcome, DoorbellOutcome::Confirmed | DoorbellOutcome::Unconfirmed) {
        conn.execute("UPDATE inbox SET delivered_at = COALESCE(delivered_at, ?1) WHERE team_id = ?2 AND to_seat = ?3 AND seq <= ?4", params![now, team_id, seat.name, rung_through_seq])?;
    } else if outcome == DoorbellOutcome::Blocked {
        let supervisor = find_supervisor_seat_name(conn, team_id)?;
        write_dedup_escalation_inbox(conn, team_id, &supervisor, &seat.name, &seat.seat_id, &format!("Seat '{}' is blocked on a terminal prompt", seat.name), "Doorbell refused because seat terminal status is blocked.", now)?;
    }

    let doorbell = get_doorbell(conn, team_id, &seat.seat_id)?.unwrap_or(Doorbell {
        team_id: team_id.to_string(),
        seat_id: seat.seat_id,
        rung_through_seq: new_seq,
        rung_at: Some(now.to_string()),
        attempts: new_attempts,
        outcome: Some(outcome),
    });

    Ok(OpOutcome::simple(team_id, Some(doorbell.seat_id.clone()), ReceiptData::Doorbell(doorbell)))
}

fn exec_daemon_escalate(conn: &Connection, team_id: &str, now: &str, seat_ref: &str, dispatch_ref: Option<&str>, stop_reason: &str, detail: &str) -> Result<OpOutcome> {
    let seat = resolve_seat(conn, team_id, seat_ref)?;
    let supervisor = find_supervisor_seat_name(conn, team_id)?;

    let target_dispatch = if let Some(dref) = dispatch_ref {
        Some(resolve_dispatch(conn, team_id, dref)?)
    } else if let Some(active) = find_active_dispatch_for_seat(conn, team_id, &seat.seat_id, None)? {
        Some(active)
    } else {
        conn.query_row(
            "SELECT * FROM dispatches WHERE team_id = ?1 AND seat_id = ?2 AND state != 'settled_succeeded' ORDER BY rowid DESC LIMIT 1",
            params![team_id, seat.seat_id],
            map_dispatch,
        )
        .optional()?
    };

    if stop_reason == "blocked" {
        conn.execute(
            "INSERT INTO doorbell (team_id, seat_id, rung_through_seq, rung_at, attempts, outcome) VALUES (?1, ?2, 0, ?3, 1, 'blocked') ON CONFLICT(team_id, seat_id) DO UPDATE SET rung_at = excluded.rung_at, outcome = 'blocked'",
            params![team_id, seat.seat_id, now],
        )?;
        let ref_id = target_dispatch.as_ref().map_or(seat.seat_id.as_str(), |d| d.dispatch_id.as_str());
        write_dedup_escalation_inbox(conn, team_id, &supervisor, &seat.name, ref_id, &format!("Seat '{}' is blocked ({stop_reason})", seat.name), detail, now)?;
        if let Some(disp) = target_dispatch {
            conn.execute("UPDATE dispatches SET stop_reason = 'blocked', updated_at = ?1 WHERE dispatch_id = ?2", params![now, disp.dispatch_id])?;
            let updated = resolve_dispatch(conn, team_id, &disp.dispatch_id)?;
            return Ok(OpOutcome::simple(team_id, Some(updated.dispatch_id.clone()), ReceiptData::Dispatch(Box::new(updated))));
        }
        return Ok(OpOutcome::simple(team_id, Some(seat.seat_id.clone()), ReceiptData::Seat(Box::new(seat))));
    }

    if let Some(disp) = target_dispatch {
        let chain_max = max_chain_failures(conn, team_id, &disp)?;
        let new_failures = chain_max + 1;
        let new_state = if new_failures >= 3 { DispatchState::CircuitBroken } else { DispatchState::SettledFailed };

        conn.execute(
            "UPDATE dispatches SET state = ?1, stage = ?2, failure_count = ?3, stop_reason = ?4, settled_at = ?5, updated_at = ?5 WHERE dispatch_id = ?6",
            params![new_state.as_str(), new_state.as_str(), new_failures, stop_reason, now, disp.dispatch_id],
        )?;

        if new_failures >= 3 {
            conn.execute("UPDATE tasks SET status = 'failed', updated_at = ?1 WHERE task_id = ?2", params![now, disp.task_id])?;
            let question = format!("Circuit breaker tripped for task '{}' after {} failures ({})", disp.task_id, new_failures, stop_reason);
            let payload = json!({ "dispatch_id": disp.dispatch_id, "task_id": disp.task_id, "failures": new_failures, "stop_reason": stop_reason, "detail": detail }).to_string();
            let _ = write_gate(conn, team_id, Some(&disp.task_id), Some(&disp.dispatch_id), GateKind::Escalation, &seat.name, &supervisor, &question, &payload, now)?;
        }

        write_inbox(conn, team_id, &supervisor, &seat.name, InboxKind::Escalation, &disp.dispatch_id, &format!("Dispatch {} failed (strike {new_failures}, reason: {stop_reason})", disp.dispatch_id), detail, now)?;
        let updated = resolve_dispatch(conn, team_id, &disp.dispatch_id)?;
        return Ok(OpOutcome::simple(team_id, Some(updated.dispatch_id.clone()), ReceiptData::Dispatch(Box::new(updated))));
    }

    write_inbox(conn, team_id, &supervisor, &seat.name, InboxKind::Escalation, &seat.seat_id, &format!("Seat '{}' escalated ({stop_reason})", seat.name), detail, now)?;
    Ok(OpOutcome::simple(team_id, Some(seat.seat_id.clone()), ReceiptData::Seat(Box::new(seat))))
}
