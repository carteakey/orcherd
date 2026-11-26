//! SQLite WAL initialization, `PRAGMA user_version` guard, and schema DDL (`SPEC.md` §4).

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Current supported SQLite schema version (`PRAGMA user_version`).
pub const SCHEMA_VERSION: u32 = 2;

const SCHEMA_V1_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS teams (
    team_id             TEXT PRIMARY KEY,
    name                TEXT NOT NULL UNIQUE,
    goal                TEXT NOT NULL DEFAULT '',
    template            TEXT,
    workspace_root      TEXT NOT NULL,
    bootstrap_symlinks  TEXT NOT NULL DEFAULT '[]',
    bootstrap_copies    TEXT NOT NULL DEFAULT '[]',
    paused_at           TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS seats (
    team_id             TEXT NOT NULL,
    seat_id             TEXT NOT NULL,
    name                TEXT NOT NULL,
    role                TEXT NOT NULL DEFAULT 'worker'
                        CHECK(role IN ('supervisor', 'reviewer', 'planner', 'worker')),
    agent               TEXT NOT NULL,
    model               TEXT,
    account             TEXT,
    effort              TEXT,
    address_kind        TEXT NOT NULL DEFAULT 'none',
    address_machine     TEXT,
    address_terminal    TEXT,
    worktree_path       TEXT,
    branch              TEXT,
    keep_open           INTEGER NOT NULL DEFAULT 1,
    resume_session      TEXT,
    standing_brief      TEXT,
    paused_at           TEXT,
    last_seen_at        TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    PRIMARY KEY (team_id, seat_id),
    UNIQUE (team_id, name)
);

CREATE TABLE IF NOT EXISTS tasks (
    task_id             TEXT PRIMARY KEY,
    team_id             TEXT NOT NULL,
    parent_id           TEXT,
    title               TEXT NOT NULL,
    spec                TEXT NOT NULL,
    placement           TEXT,
    status              TEXT NOT NULL DEFAULT 'ready'
                        CHECK(status IN (
                            'pending', 'ready', 'assigned', 'in_progress',
                            'blocked', 'review', 'completed', 'failed', 'cancelled'
                        )),
    deps                TEXT NOT NULL DEFAULT '[]',
    assignee_seat       TEXT,
    reviewer_seat       TEXT,
    review_round        INTEGER NOT NULL DEFAULT 0,
    issue_ref           TEXT,
    result              TEXT,
    branch              TEXT,
    commit_sha          TEXT,
    created_by          TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    completed_at        TEXT
);
CREATE INDEX IF NOT EXISTS idx_tasks_team_status ON tasks(team_id, status);
CREATE INDEX IF NOT EXISTS idx_tasks_assignee ON tasks(team_id, assignee_seat, status);

CREATE TABLE IF NOT EXISTS dispatches (
    dispatch_id          TEXT PRIMARY KEY,
    team_id              TEXT NOT NULL,
    task_id              TEXT NOT NULL,
    seat_id              TEXT NOT NULL,
    retry_of_dispatch_id TEXT,
    state                TEXT NOT NULL DEFAULT 'starting'
                         CHECK(state IN (
                             'starting', 'ready', 'working',
                             'settled_succeeded', 'settled_failed',
                             'stopping', 'stop_unknown', 'stopped',
                             'abandoned', 'circuit_broken'
                         )),
    stage                TEXT NOT NULL DEFAULT 'accepted',
    failure_count        INTEGER NOT NULL DEFAULT 0,
    stop_reason          TEXT,
    machine              TEXT,
    terminal_address     TEXT,
    worktree_path        TEXT,
    consumer_generation  INTEGER NOT NULL DEFAULT 0,
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL,
    settled_at           TEXT
);
CREATE INDEX IF NOT EXISTS idx_dispatches_task ON dispatches(task_id);
CREATE INDEX IF NOT EXISTS idx_dispatches_seat ON dispatches(team_id, seat_id, state);

CREATE TABLE IF NOT EXISTS gates (
    gate_id             TEXT PRIMARY KEY,
    team_id             TEXT NOT NULL,
    task_id             TEXT,
    dispatch_id         TEXT,
    kind                TEXT NOT NULL
                        CHECK(kind IN ('question', 'approval_role', 'approval_remove', 'escalation')),
    asked_by_seat       TEXT NOT NULL,
    to_seat             TEXT NOT NULL,
    question            TEXT NOT NULL,
    payload_json        TEXT NOT NULL DEFAULT '{}',
    status              TEXT NOT NULL DEFAULT 'pending'
                        CHECK(status IN ('pending', 'resolved', 'denied')),
    resolution          TEXT,
    resolved_by         TEXT,
    created_at          TEXT NOT NULL,
    resolved_at         TEXT
);
CREATE INDEX IF NOT EXISTS idx_gates_team_status ON gates(team_id, status);

CREATE TABLE IF NOT EXISTS bulletin (
    seq                 INTEGER PRIMARY KEY AUTOINCREMENT,
    post_id             TEXT NOT NULL UNIQUE,
    team_id             TEXT NOT NULL,
    author_seat         TEXT NOT NULL,
    reply_to_post_id    TEXT,
    kind                TEXT NOT NULL DEFAULT 'post'
                        CHECK(kind IN ('post', 'verdict', 'handoff', 'authorized', 'system')),
    subject             TEXT NOT NULL DEFAULT '',
    body                TEXT NOT NULL,
    mentions_json       TEXT NOT NULL DEFAULT '[]',
    created_at          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_bulletin_team_seq ON bulletin(team_id, seq);

CREATE TABLE IF NOT EXISTS findings (
    finding_id          TEXT PRIMARY KEY,
    team_id             TEXT NOT NULL,
    author_seat         TEXT NOT NULL,
    task_id             TEXT,
    title               TEXT NOT NULL,
    evidence            TEXT NOT NULL,
    confidence          TEXT NOT NULL DEFAULT 'high'
                        CHECK(confidence IN ('low', 'medium', 'high', 'verified')),
    created_at          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_findings_team ON findings(team_id, created_at);

CREATE TABLE IF NOT EXISTS inbox (
    seq                 INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id             TEXT NOT NULL UNIQUE,
    team_id             TEXT NOT NULL,
    to_seat             TEXT NOT NULL,
    from_seat           TEXT NOT NULL,
    kind                TEXT NOT NULL
                        CHECK(kind IN (
                            'assigned', 'dispatch', 'follow_up', 'mention',
                            'question', 'gate_resolved', 'worker_done',
                            'review_requested', 'verdict', 'escalation', 'notice'
                        )),
    ref_id              TEXT NOT NULL,
    subject             TEXT NOT NULL,
    body                TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    delivered_at        TEXT,
    read_at             TEXT
);
CREATE INDEX IF NOT EXISTS idx_inbox_unread ON inbox(team_id, to_seat, read_at, seq);
CREATE INDEX IF NOT EXISTS idx_inbox_undelivered ON inbox(team_id, to_seat, delivered_at, seq);

CREATE TABLE IF NOT EXISTS doorbell (
    team_id             TEXT NOT NULL,
    seat_id             TEXT NOT NULL,
    rung_through_seq    INTEGER NOT NULL DEFAULT 0,
    rung_at             TEXT,
    attempts            INTEGER NOT NULL DEFAULT 0,
    outcome             TEXT,
    PRIMARY KEY (team_id, seat_id)
);

CREATE TABLE IF NOT EXISTS mutation_receipts (
    caller_id           TEXT NOT NULL,
    request_id          TEXT NOT NULL,
    op                  TEXT NOT NULL,
    payload_hash        TEXT NOT NULL,
    state               TEXT NOT NULL DEFAULT 'completed'
                        CHECK(state IN ('pending', 'completed')),
    receipt_json        TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    PRIMARY KEY (caller_id, request_id)
);

CREATE TABLE IF NOT EXISTS audit (
    seq                 INTEGER PRIMARY KEY AUTOINCREMENT,
    team_id             TEXT NOT NULL,
    actor               TEXT NOT NULL,
    terminal            TEXT,
    op                  TEXT NOT NULL,
    ref_id              TEXT,
    detail_json         TEXT NOT NULL DEFAULT '{}',
    at                  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_team_seq ON audit(team_id, seq);
"#;

/// Read `PRAGMA user_version` from a SQLite connection.
pub fn read_user_version(conn: &Connection) -> Result<u32> {
    let version: u32 = conn.query_row("PRAGMA user_version;", [], |row| row.get(0))?;
    Ok(version)
}

/// Configure WAL pragmas, verify `user_version <= SCHEMA_VERSION`, and apply schema migrations.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;\n\
         PRAGMA foreign_keys = ON;",
    )?;

    let found = read_user_version(conn)?;
    if found > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }

    conn.execute_batch("PRAGMA journal_mode = WAL;")?;

    if found < 1 {
        conn.execute_batch(SCHEMA_V1_DDL)?;
    }
    if found < 2 {
        // v2: why a team is frozen (`orcherd freeze`), shown to every caller it refuses.
        conn.execute_batch("ALTER TABLE teams ADD COLUMN paused_reason TEXT;")?;
    }
    if found < SCHEMA_VERSION {
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    }

    Ok(())
}
