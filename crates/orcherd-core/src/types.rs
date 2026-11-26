//! Core domain types, operation requests, and mutation receipts for `orcherd`.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

macro_rules! impl_str_enum {
    ($ty:ty, $label:literal, $( $variant:ident => $primary:literal $(| $alias:literal)* ),+ $(,)?) => {
        impl $ty {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $primary, )+
                }
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $ty {
            type Err = Error;

            fn from_str(s: &str) -> Result<Self> {
                match s.trim() {
                    $( $primary $(| $alias)* => Ok(Self::$variant), )+
                    other => Err(Error::validation(format!(concat!("unknown ", $label, ": {}"), other))),
                }
            }
        }
    };
}

/// Built-in team roster templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemplateKind {
    #[serde(rename = "review-crew", alias = "review_crew")]
    ReviewCrew,
    #[serde(rename = "studio-crew", alias = "studio_crew")]
    StudioCrew,
    #[serde(rename = "small")]
    Small,
}

impl_str_enum!(TemplateKind, "template",
    ReviewCrew => "review-crew" | "review_crew",
    StudioCrew => "studio-crew" | "studio_crew",
    Small => "small",
);

/// A project-scoped orchestration team (`teams` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub team_id: String,
    pub name: String,
    pub goal: String,
    pub template: Option<TemplateKind>,
    pub workspace_root: String,
    pub bootstrap_symlinks: Vec<String>,
    pub bootstrap_copies: Vec<String>,
    pub paused_at: Option<String>,
    /// Why the team is frozen (`orcherd freeze --reason`), while `paused_at` is set.
    #[serde(default)]
    pub paused_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Role assigned to a persistent seat on the team roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatRole {
    Supervisor,
    Reviewer,
    Planner,
    #[default]
    Worker,
}

impl_str_enum!(SeatRole, "seat role",
    Supervisor => "supervisor",
    Reviewer => "reviewer",
    Planner => "planner",
    Worker => "worker",
);

/// Terminal address binding for a seat (`SPEC.md` §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Address {
    Core {
        machine: Option<String>,
        terminal: String,
    },
    #[serde(rename = "herdr1", alias = "herdr1_pane")]
    Herdr1Pane {
        pane: String,
    },
    #[default]
    None,
}

impl Address {
    /// Convert to `(address_kind, address_machine, address_terminal)` SQLite columns.
    pub fn to_db_columns(&self) -> (&'static str, Option<&str>, Option<&str>) {
        match self {
            Self::Core { machine, terminal } => {
                ("core", machine.as_deref(), Some(terminal.as_str()))
            }
            Self::Herdr1Pane { pane } => ("herdr1", None, Some(pane.as_str())),
            Self::None => ("none", None, None),
        }
    }

    /// Reconstruct from `(address_kind, address_machine, address_terminal)` SQLite columns.
    pub fn from_db_columns(
        kind: &str,
        machine: Option<String>,
        terminal: Option<String>,
    ) -> Result<Self> {
        match kind {
            "core" => Ok(Self::Core {
                machine,
                terminal: terminal.unwrap_or_default(),
            }),
            "herdr1" => Ok(Self::Herdr1Pane {
                pane: terminal.unwrap_or_default(),
            }),
            "none" | "" => Ok(Self::None),
            other => Err(Error::validation(format!("unknown address_kind: {other}"))),
        }
    }

    pub const fn is_bound(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn machine(&self) -> Option<&str> {
        match self {
            Self::Core { machine, .. } => machine.as_deref(),
            Self::Herdr1Pane { .. } | Self::None => None,
        }
    }

    pub fn terminal_str(&self) -> Option<&str> {
        match self {
            Self::Core { terminal, .. } => Some(terminal.as_str()),
            Self::Herdr1Pane { pane } => Some(pane.as_str()),
            Self::None => None,
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core {
                machine: Some(m),
                terminal,
            } if !terminal.contains('/') => write!(f, "core:{m}/{terminal}"),
            Self::Core { terminal, .. } => write!(f, "core:{terminal}"),
            Self::Herdr1Pane { pane } => write!(f, "herdr1:{pane}"),
            Self::None => f.write_str("none"),
        }
    }
}

/// Persistent named seat on a team roster (`seats` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seat {
    pub team_id: String,
    pub seat_id: String,
    pub name: String,
    pub role: SeatRole,
    pub agent: String,
    pub model: Option<String>,
    pub account: Option<String>,
    pub effort: Option<String>,
    pub address: Address,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub keep_open: bool,
    pub resume_session: Option<String>,
    pub standing_brief: Option<String>,
    pub paused_at: Option<String>,
    pub last_seen_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Lifecycle state of a task on the board (`tasks.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    #[default]
    Ready,
    Assigned,
    InProgress,
    Blocked,
    Review,
    Completed,
    Failed,
    Cancelled,
}

impl_str_enum!(TaskStatus, "task status",
    Pending => "pending",
    Ready => "ready",
    Assigned => "assigned",
    InProgress => "in_progress" | "in-progress",
    Blocked => "blocked",
    Review => "review",
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled" | "canceled",
);

/// A unit of work on the team task board (`tasks` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub task_id: String,
    pub team_id: String,
    pub parent_id: Option<String>,
    pub title: String,
    pub spec: String,
    pub placement: Option<String>,
    pub status: TaskStatus,
    pub deps: Vec<String>,
    pub assignee_seat: Option<String>,
    pub reviewer_seat: Option<String>,
    pub review_round: u32,
    pub issue_ref: Option<String>,
    pub result: Option<String>,
    pub branch: Option<String>,
    pub commit_sha: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

/// Lifecycle state of a task execution attempt (`dispatches.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    #[default]
    Starting,
    Ready,
    Working,
    SettledSucceeded,
    SettledFailed,
    Stopping,
    StopUnknown,
    Stopped,
    Abandoned,
    CircuitBroken,
}

impl DispatchState {
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Ready | Self::Working)
    }
}

impl_str_enum!(DispatchState, "dispatch state",
    Starting => "starting",
    Ready => "ready",
    Working => "working",
    SettledSucceeded => "settled_succeeded",
    SettledFailed => "settled_failed",
    Stopping => "stopping",
    StopUnknown => "stop_unknown",
    Stopped => "stopped",
    Abandoned => "abandoned",
    CircuitBroken => "circuit_broken",
);

/// A tracked execution attempt of a task on a seat (`dispatches` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispatch {
    pub dispatch_id: String,
    pub team_id: String,
    pub task_id: String,
    pub seat_id: String,
    pub retry_of_dispatch_id: Option<String>,
    pub state: DispatchState,
    pub stage: String,
    pub failure_count: u32,
    pub stop_reason: Option<String>,
    pub machine: Option<String>,
    pub terminal_address: Option<String>,
    pub worktree_path: Option<String>,
    pub consumer_generation: u32,
    pub created_at: String,
    pub updated_at: String,
    pub settled_at: Option<String>,
}

/// Kind of decision or governance gate (`gates.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    Question,
    ApprovalRole,
    ApprovalRemove,
    Escalation,
}

impl_str_enum!(GateKind, "gate kind",
    Question => "question",
    ApprovalRole => "approval_role",
    ApprovalRemove => "approval_remove",
    Escalation => "escalation",
);

/// Resolution status of a gate (`gates.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    #[default]
    Pending,
    Resolved,
    Denied,
}

impl_str_enum!(GateStatus, "gate status",
    Pending => "pending",
    Resolved => "resolved",
    Denied => "denied",
);

/// Decision, escalation, or human approval gate (`gates` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    pub gate_id: String,
    pub team_id: String,
    pub task_id: Option<String>,
    pub dispatch_id: Option<String>,
    pub kind: GateKind,
    pub asked_by_seat: String,
    pub to_seat: String,
    pub question: String,
    pub payload_json: String,
    pub status: GateStatus,
    pub resolution: Option<String>,
    pub resolved_by: Option<String>,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

/// Kind of shared bulletin post (`bulletin.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostKind {
    #[default]
    Post,
    Verdict,
    Handoff,
    Authorized,
    System,
}

impl_str_enum!(PostKind, "post kind",
    Post => "post",
    Verdict => "verdict",
    Handoff => "handoff",
    Authorized => "authorized",
    System => "system",
);

/// A shared bulletin post (`bulletin` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Post {
    pub seq: i64,
    pub post_id: String,
    pub team_id: String,
    pub author_seat: String,
    pub reply_to_post_id: Option<String>,
    pub kind: PostKind,
    pub subject: String,
    pub body: String,
    pub mentions: Vec<String>,
    pub created_at: String,
}

/// Confidence level of a recorded finding (`findings.confidence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    #[default]
    High,
    Verified,
}

impl_str_enum!(Confidence, "confidence",
    Low => "low",
    Medium => "medium",
    High => "high",
    Verified => "verified",
);

/// A durable technical finding recorded by a seat (`findings` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub finding_id: String,
    pub team_id: String,
    pub author_seat: String,
    pub task_id: Option<String>,
    pub title: String,
    pub evidence: String,
    pub confidence: Confidence,
    pub created_at: String,
}

/// Category of an inbox message (`inbox.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    Assigned,
    Dispatch,
    FollowUp,
    Mention,
    Question,
    GateResolved,
    WorkerDone,
    ReviewRequested,
    Verdict,
    Escalation,
    Notice,
}

impl_str_enum!(InboxKind, "inbox kind",
    Assigned => "assigned",
    Dispatch => "dispatch",
    FollowUp => "follow_up",
    Mention => "mention",
    Question => "question",
    GateResolved => "gate_resolved",
    WorkerDone => "worker_done",
    ReviewRequested => "review_requested",
    Verdict => "verdict",
    Escalation => "escalation",
    Notice => "notice",
);

/// A point-to-point inbox item queued for a seat (`inbox` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxItem {
    pub seq: i64,
    pub item_id: String,
    pub team_id: String,
    pub to_seat: String,
    pub from_seat: String,
    pub kind: InboxKind,
    pub ref_id: String,
    pub subject: String,
    pub body: String,
    pub created_at: String,
    pub delivered_at: Option<String>,
    pub read_at: Option<String>,
}

/// Delivery receipt status for a payload-free terminal doorbell (`doorbell.outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoorbellOutcome {
    Confirmed,
    Unconfirmed,
    StatusMismatch,
    Blocked,
    Unreachable,
    Stalled,
}

impl_str_enum!(DoorbellOutcome, "doorbell outcome",
    Confirmed => "confirmed",
    Unconfirmed => "unconfirmed",
    StatusMismatch => "status_mismatch",
    Blocked => "blocked",
    Unreachable => "unreachable",
    Stalled => "stalled",
);

/// Per-seat doorbell delivery cursor and status (`doorbell` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Doorbell {
    pub team_id: String,
    pub seat_id: String,
    pub rung_through_seq: i64,
    pub rung_at: Option<String>,
    pub attempts: u32,
    pub outcome: Option<DoorbellOutcome>,
}

/// Reviewer verdict on a task in `review` status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Pass,
    Fix,
    Escalate,
}

impl_str_enum!(VerdictKind, "verdict",
    Pass => "pass",
    Fix => "fix",
    Escalate => "escalate",
);

/// Worker completion outcome for `orcherd done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoneOutcome {
    #[default]
    Succeeded,
    Failed,
}

impl_str_enum!(DoneOutcome, "done outcome",
    Succeeded => "succeeded" | "success",
    Failed => "failed" | "failure",
);

/// An append-only audit log entry (`audit` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRow {
    pub seq: i64,
    pub team_id: String,
    pub actor: String,
    pub terminal: Option<String>,
    pub op: String,
    pub ref_id: Option<String>,
    pub detail_json: String,
    pub at: String,
}

/// Live snapshot projection published to `doc.set("orcherd/team/<id>")` and `orcherd status --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamProjection {
    pub team: Team,
    pub seats: Vec<Seat>,
    pub tasks: Vec<Task>,
    pub active_dispatches: Vec<Dispatch>,
    pub pending_gates: Vec<Gate>,
    pub unread_counts: BTreeMap<String, usize>,
    pub doorbells: Vec<Doorbell>,
}

/// Authenticated caller identity executing a store operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    Seat {
        team_id: Option<String>,
        seat_name: String,
        terminal: Option<String>,
    },
    Human {
        terminal: Option<String>,
    },
}

impl Actor {
    pub fn caller_id(&self) -> String {
        match self {
            Self::Seat { seat_name, .. } => format!("seat:{seat_name}"),
            Self::Human { .. } => "human".to_string(),
        }
    }

    pub fn actor_name(&self) -> &str {
        match self {
            Self::Seat { seat_name, .. } => seat_name.as_str(),
            Self::Human { .. } => "human",
        }
    }

    pub fn terminal(&self) -> Option<&str> {
        match self {
            Self::Seat { terminal, .. } | Self::Human { terminal } => terminal.as_deref(),
        }
    }

    pub const fn is_human(&self) -> bool {
        matches!(self, Self::Human { .. })
    }
}

/// Store operation covering the complete CLI & daemon contract (`SPEC.md` §8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Init {
        name: String,
        template: Option<TemplateKind>,
        goal: Option<String>,
        workspace_root: String,
        #[serde(default)]
        bootstrap_symlinks: Vec<String>,
        #[serde(default)]
        bootstrap_copies: Vec<String>,
    },
    Status,
    SpawnPrepare {
        seat: String,
        role: Option<SeatRole>,
        agent: Option<String>,
        model: Option<String>,
        account: Option<String>,
        effort: Option<String>,
        worktree_path: Option<String>,
        branch: Option<String>,
        machine: Option<String>,
        task_id: Option<String>,
        spec: Option<String>,
        title: Option<String>,
        keep_open: Option<bool>,
        resume_session: Option<String>,
        retry_of: Option<String>,
    },
    SpawnBindAddress {
        seat: String,
        dispatch_id: Option<String>,
        address: Address,
        worktree_path: Option<String>,
        branch: Option<String>,
        resume_session: Option<String>,
    },
    SeatList {
        #[serde(default)]
        include_remote: bool,
    },
    SeatShow {
        seat: String,
    },
    SeatStop {
        target: String,
        reason: Option<String>,
    },
    SeatAbandon {
        dispatch_id: String,
    },
    SeatRelease {
        dispatch_id: String,
    },
    SeatRebind {
        seat: String,
        address: Address,
    },
    SeatRemove {
        seat: String,
    },
    SeatHandoff {
        to_seat: String,
    },
    SeatBrief {
        seat: String,
        brief: String,
    },
    TaskCreate {
        title: Option<String>,
        spec: String,
        #[serde(default)]
        deps: Vec<String>,
        assign: Option<String>,
        reviewer: Option<String>,
        issue_ref: Option<String>,
        parent_id: Option<String>,
        placement: Option<String>,
    },
    TaskList {
        status: Option<TaskStatus>,
        #[serde(default)]
        ready_only: bool,
    },
    TaskShow {
        task_id: String,
    },
    TaskAssign {
        task_id: String,
        to_seat: String,
        reviewer: Option<String>,
        spec: Option<String>,
    },
    TaskClaim {
        task_id: String,
    },
    TaskUpdate {
        task_id: String,
        status: TaskStatus,
    },
    TaskReopen {
        task: String,
        spec: String,
    },
    InboxRead {
        #[serde(default)]
        peek: bool,
        seat: Option<String>,
        limit: Option<usize>,
    },
    Send {
        to: String,
        subject: String,
        body: String,
    },
    Ask {
        task_id: Option<String>,
        question: String,
        body: Option<String>,
    },
    Reply {
        target_id: String,
        body: String,
    },
    Done {
        task_id: Option<String>,
        result: String,
        branch: Option<String>,
        commit_sha: Option<String>,
        #[serde(default)]
        outcome: DoneOutcome,
    },
    Verdict {
        task_id: String,
        verdict: VerdictKind,
        body: String,
    },
    GateList {
        status: Option<GateStatus>,
    },
    BulletinPost {
        subject: Option<String>,
        body: String,
        #[serde(default)]
        mentions: Vec<String>,
        reply_to: Option<String>,
        kind: Option<PostKind>,
    },
    BulletinList {
        after_seq: Option<i64>,
        limit: Option<usize>,
    },
    FindingAdd {
        title: String,
        evidence: String,
        task_id: Option<String>,
        confidence: Option<Confidence>,
    },
    FindingList,
    PendingList,
    Approve {
        gate_id: String,
        resolution: Option<String>,
    },
    AuditLog {
        limit: Option<usize>,
    },
    /// Stop the team: refuse new work until `Unfreeze`. Supervisor or human only.
    Freeze {
        reason: String,
    },
    Unfreeze,
    DaemonRecordDoorbell {
        seat: String,
        rung_through_seq: i64,
        outcome: DoorbellOutcome,
    },
    DaemonEscalate {
        seat: String,
        dispatch_id: Option<String>,
        stop_reason: String,
        detail: String,
    },
}

impl Op {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Init { .. } => "init",
            Self::Status => "status",
            Self::SpawnPrepare { .. } => "spawn_prepare",
            Self::SpawnBindAddress { .. } => "spawn_bind_address",
            Self::SeatList { .. } => "seat_list",
            Self::SeatShow { .. } => "seat_show",
            Self::SeatStop { .. } => "seat_stop",
            Self::SeatAbandon { .. } => "seat_abandon",
            Self::SeatRelease { .. } => "seat_release",
            Self::SeatRebind { .. } => "seat_rebind",
            Self::SeatRemove { .. } => "seat_remove",
            Self::SeatHandoff { .. } => "seat_handoff",
            Self::SeatBrief { .. } => "seat_brief",
            Self::TaskCreate { .. } => "task_create",
            Self::TaskList { .. } => "task_list",
            Self::TaskShow { .. } => "task_show",
            Self::TaskAssign { .. } => "task_assign",
            Self::TaskClaim { .. } => "task_claim",
            Self::TaskUpdate { .. } => "task_update",
            Self::TaskReopen { .. } => "task_reopen",
            Self::InboxRead { .. } => "inbox_read",
            Self::Send { .. } => "send",
            Self::Ask { .. } => "ask",
            Self::Reply { .. } => "reply",
            Self::Done { .. } => "done",
            Self::Verdict { .. } => "verdict",
            Self::GateList { .. } => "gate_list",
            Self::BulletinPost { .. } => "bulletin_post",
            Self::BulletinList { .. } => "bulletin_list",
            Self::FindingAdd { .. } => "finding_add",
            Self::FindingList => "finding_list",
            Self::PendingList => "pending_list",
            Self::Approve { .. } => "approve",
            Self::AuditLog { .. } => "audit_log",
            Self::Freeze { .. } => "freeze",
            Self::Unfreeze => "unfreeze",
            Self::DaemonRecordDoorbell { .. } => "daemon_record_doorbell",
            Self::DaemonEscalate { .. } => "daemon_escalate",
        }
    }

    /// Pure queries: they change no records, so `apply` writes no audit row for them.
    /// UI mods poll `status`, and auditing every poll would bury real mutations.
    pub const fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::Status
                | Self::SeatList { .. }
                | Self::SeatShow { .. }
                | Self::TaskList { .. }
                | Self::TaskShow { .. }
                | Self::GateList { .. }
                | Self::BulletinList { .. }
                | Self::FindingList
                | Self::PendingList
                | Self::AuditLog { .. }
                | Self::InboxRead { peek: true, .. }
        )
    }
}

/// Envelope wrapping an `Op` with optional team target and idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub request_id: Option<String>,
    pub team: Option<String>,
    pub op: Op,
}

impl RequestEnvelope {
    pub fn new(op: Op) -> Self {
        Self {
            request_id: None,
            team: None,
            op,
        }
    }

    pub fn with_team(mut self, team: impl Into<String>) -> Self {
        self.team = Some(team.into());
        self
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }
}

/// Structured result payload inside a `Receipt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ReceiptData {
    TeamStatus(Box<TeamProjection>),
    SpawnPrepared {
        seat: Box<Seat>,
        task: Option<Box<Task>>,
        dispatch: Option<Box<Dispatch>>,
        warm_reuse: bool,
    },
    Seat(Box<Seat>),
    Seats(Vec<Seat>),
    Dispatch(Box<Dispatch>),
    Task(Box<Task>),
    Tasks(Vec<Task>),
    Inbox(Vec<InboxItem>),
    Sent {
        recipients: Vec<String>,
        item_ids: Vec<String>,
    },
    Gate(Box<Gate>),
    Gates(Vec<Gate>),
    Post(Box<Post>),
    Posts(Vec<Post>),
    Finding(Box<Finding>),
    Findings(Vec<Finding>),
    Audit(Vec<AuditRow>),
    Doorbell(Doorbell),
    DoneResult {
        task: Box<Task>,
        dispatch: Option<Box<Dispatch>>,
        unblocked_task_ids: Vec<String>,
    },
    VerdictResult {
        task: Box<Task>,
        gate: Option<Box<Gate>>,
        unblocked_task_ids: Vec<String>,
        escalated: bool,
    },
}

/// Actionable receipt returned by every `Store::apply` transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub request_id: Option<String>,
    pub team_id: String,
    pub actor: String,
    pub next: Option<String>,
    pub failed_stage: Option<String>,
    pub data: ReceiptData,
}
