//! SQLite WAL store and typed query/write helpers for `orcherd-core`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::str::FromStr;

use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;

use crate::apply::{apply, apply_with_replay};
use crate::error::{Error, Result};
use crate::schema;
use crate::types::{
    Actor, Address, AuditRow, Confidence, Dispatch, DispatchState, Doorbell, DoorbellOutcome,
    Finding, Gate, GateKind, GateStatus, InboxItem, InboxKind, Post, PostKind, Receipt,
    RequestEnvelope, Seat, SeatRole, Task, TaskStatus, Team, TeamProjection, TemplateKind,
};

/// Persistent SQLite WAL store for `orcherd`.
pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    /// Open (or create) the SQLite WAL store at `path` and run schema initialization.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        schema::init_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Open an existing SQLite store strictly read-only without running migrations or creating files.
    ///
    /// Fails on missing, corrupt, or newer-schema databases so diagnostic scans never mutate state.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path.as_ref(), flags)?;
        conn.pragma_update(None, "query_only", true)?;
        conn.pragma_update(None, "busy_timeout", 250)?;
        let version = schema::read_user_version(&conn)?;
        if version > schema::SCHEMA_VERSION {
            return Err(Error::SchemaTooNew {
                found: version,
                supported: schema::SCHEMA_VERSION,
            });
        }
        Ok(Self { conn })
    }

    /// Open an isolated in-memory SQLite store for unit and integration testing.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        schema::init_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Borrow the underlying SQLite connection.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Execute `envelope` atomically inside a single SQLite transaction.
    pub fn apply(&mut self, actor: &Actor, envelope: RequestEnvelope) -> Result<Receipt> {
        apply(self, actor, envelope)
    }

    /// Execute `envelope` atomically, also reporting whether it was replayed from `mutation_receipts`.
    pub fn apply_with_replay(&mut self, actor: &Actor, envelope: RequestEnvelope) -> Result<(Receipt, bool)> {
        apply_with_replay(self, actor, envelope)
    }

    /// Read SQLite's `PRAGMA data_version` to detect cross-process writes without polling tables.
    pub fn data_version(&self) -> Result<i64> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA data_version;", [], |row| row.get(0))?;
        Ok(version)
    }

    /// Resolve a team by name, ID, or prefix (or return the default team if `team_ref` is `None`).
    pub fn resolve_team(&self, team_ref: Option<&str>) -> Result<Team> {
        resolve_team(&self.conn, team_ref)
    }

    /// The newest team whose `workspace_root` is `path` or one of its ancestors,
    /// preferring the deepest root. Lets `--team` default to the current repo's team.
    pub fn team_for_path(&self, path: &Path) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT rowid, team_id, workspace_root FROM teams ORDER BY rowid ASC")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Tie-break on rowid so the newest team wins when two teams share a workspace root.
        Ok(rows
            .into_iter()
            .filter(|(_, _, root)| !root.is_empty() && path.starts_with(root))
            .max_by_key(|(rowid, _, root)| (Path::new(root).components().count(), *rowid))
            .map(|(_, id, _)| id))
    }

    /// Build the live `TeamProjection` for `team_id`.
    pub fn team_projection(&self, team_id: &str) -> Result<TeamProjection> {
        build_team_projection(&self.conn, team_id)
    }

    /// List unread & undelivered inbox items for a seat.
    pub fn list_undelivered_inbox(&self, team_id: &str, to_seat: &str) -> Result<Vec<InboxItem>> {
        list_undelivered_inbox(&self.conn, team_id, to_seat)
    }
}

/// Format the payload-free 1-line doorbell message (`SPEC.md` §5.1).
pub fn format_doorbell(unread_count: usize, latest_kind: InboxKind, latest_ref_id: &str) -> String {
    format!(
        "[Orcherd] You have {unread_count} unread item(s) (latest: {latest_kind} {latest_ref_id}). Run: orcherd inbox"
    )
}

pub(crate) fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) fn new_id(prefix: &str) -> String {
    let raw = Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &raw[..12])
}

pub(crate) fn derive_title(text: &str) -> String {
    let first = text
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("Untitled task");
    first.chars().take(80).collect()
}

pub(crate) fn map_team(row: &Row<'_>) -> rusqlite::Result<Team> {
    let template_str: Option<String> = row.get("template")?;
    let symlinks_json: String = row.get("bootstrap_symlinks")?;
    let copies_json: String = row.get("bootstrap_copies")?;
    Ok(Team {
        team_id: row.get("team_id")?,
        name: row.get("name")?,
        goal: row.get("goal")?,
        template: template_str.as_deref().and_then(|s| TemplateKind::from_str(s).ok()),
        workspace_root: row.get("workspace_root")?,
        bootstrap_symlinks: serde_json::from_str(&symlinks_json).unwrap_or_default(),
        bootstrap_copies: serde_json::from_str(&copies_json).unwrap_or_default(),
        paused_at: row.get("paused_at")?,
        paused_reason: row.get("paused_reason")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) fn map_seat(row: &Row<'_>) -> rusqlite::Result<Seat> {
    let role_str: String = row.get("role")?;
    let addr_kind: String = row.get("address_kind")?;
    let addr_machine: Option<String> = row.get("address_machine")?;
    let addr_terminal: Option<String> = row.get("address_terminal")?;
    let keep_open_int: i64 = row.get("keep_open")?;
    Ok(Seat {
        team_id: row.get("team_id")?,
        seat_id: row.get("seat_id")?,
        name: row.get("name")?,
        role: SeatRole::from_str(&role_str).unwrap_or_default(),
        agent: row.get("agent")?,
        model: row.get("model")?,
        account: row.get("account")?,
        effort: row.get("effort")?,
        address: Address::from_db_columns(&addr_kind, addr_machine, addr_terminal).unwrap_or(Address::None),
        worktree_path: row.get("worktree_path")?,
        branch: row.get("branch")?,
        keep_open: keep_open_int != 0,
        resume_session: row.get("resume_session")?,
        standing_brief: row.get("standing_brief")?,
        paused_at: row.get("paused_at")?,
        last_seen_at: row.get("last_seen_at")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) fn map_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    let status_str: String = row.get("status")?;
    let deps_json: String = row.get("deps")?;
    Ok(Task {
        task_id: row.get("task_id")?,
        team_id: row.get("team_id")?,
        parent_id: row.get("parent_id")?,
        title: row.get("title")?,
        spec: row.get("spec")?,
        placement: row.get("placement")?,
        status: TaskStatus::from_str(&status_str).unwrap_or_default(),
        deps: serde_json::from_str(&deps_json).unwrap_or_default(),
        assignee_seat: row.get("assignee_seat")?,
        reviewer_seat: row.get("reviewer_seat")?,
        review_round: row.get("review_round")?,
        issue_ref: row.get("issue_ref")?,
        result: row.get("result")?,
        branch: row.get("branch")?,
        commit_sha: row.get("commit_sha")?,
        created_by: row.get("created_by")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        completed_at: row.get("completed_at")?,
    })
}

pub(crate) fn map_dispatch(row: &Row<'_>) -> rusqlite::Result<Dispatch> {
    let state_str: String = row.get("state")?;
    Ok(Dispatch {
        dispatch_id: row.get("dispatch_id")?,
        team_id: row.get("team_id")?,
        task_id: row.get("task_id")?,
        seat_id: row.get("seat_id")?,
        retry_of_dispatch_id: row.get("retry_of_dispatch_id")?,
        state: DispatchState::from_str(&state_str).unwrap_or_default(),
        stage: row.get("stage")?,
        failure_count: row.get("failure_count")?,
        stop_reason: row.get("stop_reason")?,
        machine: row.get("machine")?,
        terminal_address: row.get("terminal_address")?,
        worktree_path: row.get("worktree_path")?,
        consumer_generation: row.get("consumer_generation")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        settled_at: row.get("settled_at")?,
    })
}

pub(crate) fn map_gate(row: &Row<'_>) -> rusqlite::Result<Gate> {
    let kind_str: String = row.get("kind")?;
    let status_str: String = row.get("status")?;
    Ok(Gate {
        gate_id: row.get("gate_id")?,
        team_id: row.get("team_id")?,
        task_id: row.get("task_id")?,
        dispatch_id: row.get("dispatch_id")?,
        kind: GateKind::from_str(&kind_str).unwrap_or(GateKind::Question),
        asked_by_seat: row.get("asked_by_seat")?,
        to_seat: row.get("to_seat")?,
        question: row.get("question")?,
        payload_json: row.get("payload_json")?,
        status: GateStatus::from_str(&status_str).unwrap_or_default(),
        resolution: row.get("resolution")?,
        resolved_by: row.get("resolved_by")?,
        created_at: row.get("created_at")?,
        resolved_at: row.get("resolved_at")?,
    })
}

pub(crate) fn map_post(row: &Row<'_>) -> rusqlite::Result<Post> {
    let kind_str: String = row.get("kind")?;
    let mentions_json: String = row.get("mentions_json")?;
    Ok(Post {
        seq: row.get("seq")?,
        post_id: row.get("post_id")?,
        team_id: row.get("team_id")?,
        author_seat: row.get("author_seat")?,
        reply_to_post_id: row.get("reply_to_post_id")?,
        kind: PostKind::from_str(&kind_str).unwrap_or_default(),
        subject: row.get("subject")?,
        body: row.get("body")?,
        mentions: serde_json::from_str(&mentions_json).unwrap_or_default(),
        created_at: row.get("created_at")?,
    })
}

pub(crate) fn map_finding(row: &Row<'_>) -> rusqlite::Result<Finding> {
    let conf_str: String = row.get("confidence")?;
    Ok(Finding {
        finding_id: row.get("finding_id")?,
        team_id: row.get("team_id")?,
        author_seat: row.get("author_seat")?,
        task_id: row.get("task_id")?,
        title: row.get("title")?,
        evidence: row.get("evidence")?,
        confidence: Confidence::from_str(&conf_str).unwrap_or_default(),
        created_at: row.get("created_at")?,
    })
}

pub(crate) fn map_inbox_item(row: &Row<'_>) -> rusqlite::Result<InboxItem> {
    let kind_str: String = row.get("kind")?;
    Ok(InboxItem {
        seq: row.get("seq")?,
        item_id: row.get("item_id")?,
        team_id: row.get("team_id")?,
        to_seat: row.get("to_seat")?,
        from_seat: row.get("from_seat")?,
        kind: InboxKind::from_str(&kind_str).unwrap_or(InboxKind::Notice),
        ref_id: row.get("ref_id")?,
        subject: row.get("subject")?,
        body: row.get("body")?,
        created_at: row.get("created_at")?,
        delivered_at: row.get("delivered_at")?,
        read_at: row.get("read_at")?,
    })
}

pub(crate) fn map_doorbell(row: &Row<'_>) -> rusqlite::Result<Doorbell> {
    let outcome_str: Option<String> = row.get("outcome")?;
    Ok(Doorbell {
        team_id: row.get("team_id")?,
        seat_id: row.get("seat_id")?,
        rung_through_seq: row.get("rung_through_seq")?,
        rung_at: row.get("rung_at")?,
        attempts: row.get("attempts")?,
        outcome: outcome_str.as_deref().and_then(|s| DoorbellOutcome::from_str(s).ok()),
    })
}

pub(crate) fn map_audit(row: &Row<'_>) -> rusqlite::Result<AuditRow> {
    Ok(AuditRow {
        seq: row.get("seq")?,
        team_id: row.get("team_id")?,
        actor: row.get("actor")?,
        terminal: row.get("terminal")?,
        op: row.get("op")?,
        ref_id: row.get("ref_id")?,
        detail_json: row.get("detail_json")?,
        at: row.get("at")?,
    })
}

fn finish_prefix<T, F>(entity: &'static str, q: &str, matches: Vec<T>, id_fn: F) -> Result<T>
where
    F: Fn(&T) -> String,
{
    match matches.len() {
        0 => Err(Error::not_found(entity, q)),
        1 => matches.into_iter().next().ok_or_else(|| Error::not_found(entity, q)),
        _ => Err(Error::AmbiguousPrefix {
            entity,
            prefix: q.to_string(),
            candidates: matches.iter().map(id_fn).collect(),
        }),
    }
}

pub(crate) fn resolve_team(conn: &Connection, team_ref: Option<&str>) -> Result<Team> {
    if let Some(q) = team_ref {
        let q = q.trim();
        if q.is_empty() {
            return Err(Error::not_found("team", q));
        }
        if let Some(exact) = conn
            .query_row("SELECT * FROM teams WHERE team_id = ?1 OR name = ?1", params![q], map_team)
            .optional()?
        {
            return Ok(exact);
        }
        let mut stmt = conn.prepare("SELECT * FROM teams ORDER BY rowid ASC")?;
        let all: Vec<Team> = stmt.query_map([], map_team)?.collect::<rusqlite::Result<Vec<_>>>()?;
        let matches = all.into_iter().filter(|t| t.team_id.starts_with(q) || t.name.starts_with(q)).collect();
        return finish_prefix("team", q, matches, |t| t.team_id.clone());
    }
    conn.query_row("SELECT * FROM teams ORDER BY rowid DESC LIMIT 1", [], map_team)
        .optional()?
        .ok_or_else(|| Error::not_found("team", "<default>"))
}

pub(crate) fn resolve_team_for_actor(conn: &Connection, envelope_team: Option<&str>, actor: &Actor) -> Result<Team> {
    if let Some(t) = envelope_team {
        return resolve_team(conn, Some(t));
    }
    if let Actor::Seat { team_id: Some(t), .. } = actor {
        return resolve_team(conn, Some(t.as_str()));
    }
    resolve_team(conn, None)
}

pub(crate) fn resolve_seat(conn: &Connection, team_id: &str, name_or_id: &str) -> Result<Seat> {
    let q = name_or_id.trim().trim_start_matches('@');
    if q.is_empty() {
        return Err(Error::not_found("seat", name_or_id));
    }
    if let Some(exact) = conn
        .query_row("SELECT * FROM seats WHERE team_id = ?1 AND (name = ?2 OR seat_id = ?2)", params![team_id, q], map_seat)
        .optional()?
    {
        return Ok(exact);
    }
    let matches = list_seats(conn, team_id, true)?
        .into_iter()
        .filter(|s| s.seat_id.starts_with(q) || s.name.starts_with(q))
        .collect();
    finish_prefix("seat", q, matches, |s| s.name.clone())
}

pub(crate) fn find_supervisor_seat_name(conn: &Connection, team_id: &str) -> Result<String> {
    let found: Option<String> = conn
        .query_row(
            "SELECT name FROM seats WHERE team_id = ?1 AND role = 'supervisor' ORDER BY rowid ASC LIMIT 1",
            params![team_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.unwrap_or_else(|| "super".to_string()))
}

pub(crate) fn list_seats(conn: &Connection, team_id: &str, include_remote: bool) -> Result<Vec<Seat>> {
    let mut stmt = conn.prepare("SELECT * FROM seats WHERE team_id = ?1 ORDER BY rowid ASC")?;
    let seats: Vec<Seat> = stmt.query_map(params![team_id], map_seat)?.collect::<rusqlite::Result<Vec<_>>>()?;
    if include_remote {
        Ok(seats)
    } else {
        Ok(seats.into_iter().filter(|s| s.address.machine().is_none()).collect())
    }
}

pub(crate) fn resolve_task(conn: &Connection, team_id: &str, id_or_prefix: &str) -> Result<Task> {
    let q = id_or_prefix.trim();
    if q.is_empty() {
        return Err(Error::not_found("task", id_or_prefix));
    }
    if let Some(exact) = conn
        .query_row("SELECT * FROM tasks WHERE team_id = ?1 AND task_id = ?2", params![team_id, q], map_task)
        .optional()?
    {
        return Ok(exact);
    }
    let matches = list_tasks(conn, team_id, None, false)?
        .into_iter()
        .filter(|t| t.task_id.starts_with(q))
        .collect();
    finish_prefix("task", q, matches, |t| t.task_id.clone())
}

pub(crate) fn find_active_task_for_seat(conn: &Connection, team_id: &str, seat_name: &str) -> Result<Option<Task>> {
    Ok(conn
        .query_row(
            "SELECT * FROM tasks WHERE team_id = ?1 AND assignee_seat = ?2 AND status IN ('in_progress', 'assigned', 'blocked') ORDER BY rowid DESC LIMIT 1",
            params![team_id, seat_name],
            map_task,
        )
        .optional()?)
}

pub(crate) fn list_tasks(conn: &Connection, team_id: &str, status: Option<TaskStatus>, ready_only: bool) -> Result<Vec<Task>> {
    let effective = if ready_only { Some(TaskStatus::Ready) } else { status };
    if let Some(st) = effective {
        let mut stmt = conn.prepare("SELECT * FROM tasks WHERE team_id = ?1 AND status = ?2 ORDER BY rowid ASC")?;
        let rows = stmt.query_map(params![team_id, st.as_str()], map_task)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    } else {
        let mut stmt = conn.prepare("SELECT * FROM tasks WHERE team_id = ?1 ORDER BY rowid ASC")?;
        let rows = stmt.query_map(params![team_id], map_task)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

pub(crate) fn resolve_dispatch(conn: &Connection, team_id: &str, id_or_prefix: &str) -> Result<Dispatch> {
    let q = id_or_prefix.trim().trim_start_matches("dispatch:");
    if q.is_empty() {
        return Err(Error::not_found("dispatch", id_or_prefix));
    }
    if let Some(exact) = conn
        .query_row("SELECT * FROM dispatches WHERE team_id = ?1 AND dispatch_id = ?2", params![team_id, q], map_dispatch)
        .optional()?
    {
        return Ok(exact);
    }
    let mut stmt = conn.prepare("SELECT * FROM dispatches WHERE team_id = ?1 ORDER BY rowid ASC")?;
    let all: Vec<Dispatch> = stmt.query_map(params![team_id], map_dispatch)?.collect::<rusqlite::Result<Vec<_>>>()?;
    let matches = all.into_iter().filter(|d| d.dispatch_id.starts_with(q)).collect();
    finish_prefix("dispatch", q, matches, |d| d.dispatch_id.clone())
}

pub(crate) fn find_active_dispatch_for_seat(
    conn: &Connection,
    team_id: &str,
    seat_id: &str,
    task_id: Option<&str>,
) -> Result<Option<Dispatch>> {
    if let Some(tid) = task_id {
        let d = conn
            .query_row(
                "SELECT * FROM dispatches WHERE team_id = ?1 AND seat_id = ?2 AND task_id = ?3 AND state IN ('starting', 'ready', 'working') ORDER BY rowid DESC LIMIT 1",
                params![team_id, seat_id, tid],
                map_dispatch,
            )
            .optional()?;
        if d.is_some() {
            return Ok(d);
        }
    }
    Ok(conn
        .query_row(
            "SELECT * FROM dispatches WHERE team_id = ?1 AND seat_id = ?2 AND state IN ('starting', 'ready', 'working') ORDER BY rowid DESC LIMIT 1",
            params![team_id, seat_id],
            map_dispatch,
        )
        .optional()?)
}

pub(crate) fn list_active_dispatches(conn: &Connection, team_id: &str) -> Result<Vec<Dispatch>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM dispatches WHERE team_id = ?1 AND state IN ('starting', 'ready', 'working') ORDER BY rowid ASC",
    )?;
    let rows = stmt.query_map(params![team_id], map_dispatch)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn resolve_gate(conn: &Connection, team_id: &str, id_or_prefix: &str) -> Result<Gate> {
    let q = id_or_prefix.trim();
    if q.is_empty() {
        return Err(Error::not_found("gate", id_or_prefix));
    }
    if let Some(exact) = conn
        .query_row("SELECT * FROM gates WHERE team_id = ?1 AND gate_id = ?2", params![team_id, q], map_gate)
        .optional()?
    {
        return Ok(exact);
    }
    let matches = list_gates(conn, team_id, None)?.into_iter().filter(|g| g.gate_id.starts_with(q)).collect();
    finish_prefix("gate", q, matches, |g| g.gate_id.clone())
}

pub(crate) fn list_gates(conn: &Connection, team_id: &str, status: Option<GateStatus>) -> Result<Vec<Gate>> {
    if let Some(st) = status {
        let mut stmt = conn.prepare("SELECT * FROM gates WHERE team_id = ?1 AND status = ?2 ORDER BY rowid ASC")?;
        let rows = stmt.query_map(params![team_id, st.as_str()], map_gate)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    } else {
        let mut stmt = conn.prepare("SELECT * FROM gates WHERE team_id = ?1 ORDER BY rowid ASC")?;
        let rows = stmt.query_map(params![team_id], map_gate)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

pub(crate) fn list_posts(conn: &Connection, team_id: &str, after_seq: Option<i64>, limit: Option<usize>) -> Result<Vec<Post>> {
    let after = after_seq.unwrap_or(0);
    let mut stmt = conn.prepare("SELECT * FROM bulletin WHERE team_id = ?1 AND seq > ?2 ORDER BY seq ASC")?;
    let mut rows: Vec<Post> = stmt.query_map(params![team_id, after], map_post)?.collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some(lim) = limit {
        rows.truncate(lim);
    }
    Ok(rows)
}

pub(crate) fn list_findings(conn: &Connection, team_id: &str) -> Result<Vec<Finding>> {
    let mut stmt = conn.prepare("SELECT * FROM findings WHERE team_id = ?1 ORDER BY rowid ASC")?;
    let rows = stmt.query_map(params![team_id], map_finding)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn list_inbox(
    conn: &Connection,
    team_id: &str,
    to_seat: &str,
    unread_only: bool,
    limit: Option<usize>,
) -> Result<Vec<InboxItem>> {
    let sql = if unread_only {
        "SELECT * FROM inbox WHERE team_id = ?1 AND to_seat = ?2 AND read_at IS NULL ORDER BY seq ASC"
    } else {
        "SELECT * FROM inbox WHERE team_id = ?1 AND to_seat = ?2 ORDER BY seq ASC"
    };
    let mut stmt = conn.prepare(sql)?;
    let mut rows: Vec<InboxItem> = stmt.query_map(params![team_id, to_seat], map_inbox_item)?.collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some(lim) = limit {
        rows.truncate(lim);
    }
    Ok(rows)
}

pub(crate) fn list_undelivered_inbox(conn: &Connection, team_id: &str, to_seat: &str) -> Result<Vec<InboxItem>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM inbox WHERE team_id = ?1 AND to_seat = ?2 AND delivered_at IS NULL AND read_at IS NULL ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map(params![team_id, to_seat], map_inbox_item)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn get_doorbell(conn: &Connection, team_id: &str, seat_id: &str) -> Result<Option<Doorbell>> {
    Ok(conn
        .query_row("SELECT * FROM doorbell WHERE team_id = ?1 AND seat_id = ?2", params![team_id, seat_id], map_doorbell)
        .optional()?)
}

pub(crate) fn list_doorbells(conn: &Connection, team_id: &str) -> Result<Vec<Doorbell>> {
    let mut stmt = conn.prepare("SELECT * FROM doorbell WHERE team_id = ?1 ORDER BY rowid ASC")?;
    let rows = stmt.query_map(params![team_id], map_doorbell)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn list_audit(conn: &Connection, team_id: &str, limit: Option<usize>) -> Result<Vec<AuditRow>> {
    let mut stmt = conn.prepare("SELECT * FROM audit WHERE team_id = ?1 ORDER BY seq ASC")?;
    let mut rows: Vec<AuditRow> = stmt.query_map(params![team_id], map_audit)?.collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some(lim) = limit
        && rows.len() > lim
    {
        rows = rows.split_off(rows.len() - lim);
    }
    Ok(rows)
}

pub(crate) fn build_team_projection(conn: &Connection, team_id: &str) -> Result<TeamProjection> {
    let team = resolve_team(conn, Some(team_id))?;
    let seats = list_seats(conn, &team.team_id, true)?;
    let tasks = list_tasks(conn, &team.team_id, None, false)?;
    let active_dispatches = list_active_dispatches(conn, &team.team_id)?;
    let pending_gates = list_gates(conn, &team.team_id, Some(GateStatus::Pending))?;
    let doorbells = list_doorbells(conn, &team.team_id)?;

    let mut unread_counts = BTreeMap::new();
    for seat in &seats {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM inbox WHERE team_id = ?1 AND to_seat = ?2 AND read_at IS NULL",
            params![team.team_id, seat.name],
            |row| row.get(0),
        )?;
        unread_counts.insert(seat.name.clone(), usize::try_from(count).unwrap_or(0));
    }

    Ok(TeamProjection {
        team,
        seats,
        tasks,
        active_dispatches,
        pending_gates,
        unread_counts,
        doorbells,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_gate(
    conn: &Connection,
    team_id: &str,
    task_id: Option<&str>,
    dispatch_id: Option<&str>,
    kind: GateKind,
    asked_by_seat: &str,
    to_seat: &str,
    question: &str,
    payload_json: &str,
    now: &str,
) -> Result<Gate> {
    let gate_id = new_id("gate");
    conn.execute(
        "INSERT INTO gates (gate_id, team_id, task_id, dispatch_id, kind, asked_by_seat, to_seat, question, payload_json, status, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10)",
        params![gate_id, team_id, task_id, dispatch_id, kind.as_str(), asked_by_seat, to_seat, question, payload_json, now],
    )?;
    Ok(conn.query_row("SELECT * FROM gates WHERE gate_id = ?1", params![gate_id], map_gate)?)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_inbox(
    conn: &Connection,
    team_id: &str,
    to_seat: &str,
    from_seat: &str,
    kind: InboxKind,
    ref_id: &str,
    subject: &str,
    body: &str,
    now: &str,
) -> Result<InboxItem> {
    let item_id = new_id("inb");
    write_inbox_with_id(conn, &item_id, team_id, to_seat, from_seat, kind, ref_id, subject, body, now)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_inbox_with_id(
    conn: &Connection,
    item_id: &str,
    team_id: &str,
    to_seat: &str,
    from_seat: &str,
    kind: InboxKind,
    ref_id: &str,
    subject: &str,
    body: &str,
    now: &str,
) -> Result<InboxItem> {
    let clean_to = to_seat.trim().trim_start_matches('@');
    let clean_from = from_seat.trim().trim_start_matches('@');
    conn.execute(
        "INSERT INTO inbox (item_id, team_id, to_seat, from_seat, kind, ref_id, subject, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![item_id, team_id, clean_to, clean_from, kind.as_str(), ref_id, subject, body, now],
    )?;
    Ok(conn.query_row("SELECT * FROM inbox WHERE item_id = ?1", params![item_id], map_inbox_item)?)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_dedup_escalation_inbox(
    conn: &Connection,
    team_id: &str,
    to_seat: &str,
    from_seat: &str,
    ref_id: &str,
    subject: &str,
    body: &str,
    now: &str,
) -> Result<()> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT item_id FROM inbox WHERE team_id = ?1 AND to_seat = ?2 AND kind = 'escalation' AND ref_id = ?3 AND read_at IS NULL LIMIT 1",
            params![team_id, to_seat, ref_id],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_none() {
        let _ = write_inbox(conn, team_id, to_seat, from_seat, InboxKind::Escalation, ref_id, subject, body, now)?;
    }
    Ok(())
}

pub(crate) fn write_audit(
    conn: &Connection,
    team_id: &str,
    actor: &Actor,
    op: &str,
    ref_id: Option<&str>,
    detail_json: &str,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO audit (team_id, actor, terminal, op, ref_id, detail_json, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![team_id, actor.actor_name(), actor.terminal(), op, ref_id, detail_json, now],
    )?;
    Ok(())
}

pub(crate) fn max_chain_failures(conn: &Connection, team_id: &str, disp: &Dispatch) -> Result<u32> {
    let mut max_f = disp.failure_count;
    let mut cursor = disp.retry_of_dispatch_id.clone();
    let mut visited = BTreeSet::new();
    visited.insert(disp.dispatch_id.clone());

    while let Some(prev_id) = cursor {
        if !visited.insert(prev_id.clone()) {
            break;
        }
        if let Ok(prev) = resolve_dispatch(conn, team_id, &prev_id) {
            if prev.failure_count > max_f {
                max_f = prev.failure_count;
            }
            cursor = prev.retry_of_dispatch_id;
        } else {
            break;
        }
    }
    Ok(max_f)
}

pub(crate) fn check_reviewer_blindness(
    conn: &Connection,
    team_id: &str,
    assignee: &str,
    reviewer: &str,
) -> Result<()> {
    let a = assignee.trim().trim_start_matches('@');
    let r = reviewer.trim().trim_start_matches('@');
    if a.is_empty() || r.is_empty() {
        return Ok(());
    }
    if a == r {
        return Err(Error::ReviewerBlindness {
            seat: a.to_string(),
        });
    }
    if let (Ok(sa), Ok(sr)) = (resolve_seat(conn, team_id, a), resolve_seat(conn, team_id, r))
        && sa.seat_id == sr.seat_id
    {
        return Err(Error::ReviewerBlindness { seat: sa.name });
    }
    Ok(())
}

pub(crate) fn unblock_ready_dag_tasks(
    conn: &Connection,
    team_id: &str,
    actor: &Actor,
    now: &str,
) -> Result<Vec<String>> {
    let pending_tasks = list_tasks(conn, team_id, Some(TaskStatus::Pending), false)?;
    let mut unblocked = Vec::new();
    for task in pending_tasks {
        if task.deps.is_empty() {
            continue;
        }
        let all_done = task.deps.iter().all(|dep_id| {
            matches!(resolve_task(conn, team_id, dep_id), Ok(dep) if dep.status == TaskStatus::Completed)
        });
        if all_done {
            conn.execute(
                "UPDATE tasks SET status = 'ready', updated_at = ?1 WHERE task_id = ?2",
                params![now, task.task_id],
            )?;
            if let Some(assignee) = task.assignee_seat.as_deref() {
                write_inbox(
                    conn,
                    team_id,
                    assignee,
                    actor.actor_name(),
                    InboxKind::Assigned,
                    &task.task_id,
                    &task.title,
                    &task.spec,
                    now,
                )?;
            }
            unblocked.push(task.task_id);
        }
    }
    Ok(unblocked)
}

pub(crate) fn resolve_send_recipients(
    conn: &Connection,
    team_id: &str,
    sender: &str,
    to: &str,
) -> Result<Vec<String>> {
    let raw = to.trim();
    let all_seats = list_seats(conn, team_id, true)?;
    let active_dispatches = list_active_dispatches(conn, team_id)?;
    let busy_seat_ids: BTreeSet<&str> = active_dispatches.iter().map(|d| d.seat_id.as_str()).collect();

    match raw {
        "@all" => Ok(all_seats.into_iter().filter(|s| s.name != sender).map(|s| s.name).collect()),
        "@workers" => Ok(all_seats.into_iter().filter(|s| s.role == SeatRole::Worker).map(|s| s.name).collect()),
        "@reviewers" => Ok(all_seats.into_iter().filter(|s| s.role == SeatRole::Reviewer).map(|s| s.name).collect()),
        "@idle" => Ok(all_seats.into_iter().filter(|s| !busy_seat_ids.contains(s.seat_id.as_str())).map(|s| s.name).collect()),
        _ if raw.starts_with("dispatch:") => {
            let disp = resolve_dispatch(conn, team_id, raw)?;
            let seat = resolve_seat(conn, team_id, &disp.seat_id)?;
            Ok(vec![seat.name])
        }
        _ => {
            let mut out = Vec::new();
            for part in raw.split(',') {
                let clean = part.trim().trim_start_matches('@');
                if clean.is_empty() {
                    continue;
                }
                let target_name = resolve_seat(conn, team_id, clean)
                    .map(|s| s.name)
                    .unwrap_or_else(|_| clean.to_string());
                if !out.contains(&target_name) {
                    out.push(target_name);
                }
            }
            Ok(out)
        }
    }
}
