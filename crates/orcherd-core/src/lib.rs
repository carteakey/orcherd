//! Orcherd core domain types, SQLite WAL store, and transactional `apply()` engine.

pub mod apply;
pub mod error;
pub mod schema;
pub mod store;
pub mod templates;
pub mod types;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_lifecycle;
#[cfg(test)]
mod tests_supervise;
#[cfg(test)]
mod tests_team;

pub use apply::apply;
pub use error::{Error, Result};
pub use schema::{SCHEMA_VERSION, init_schema, read_user_version};
pub use store::{Store, format_doorbell};
pub use templates::{SeatTemplate, template_seats};
pub use types::{
    Actor, Address, AuditRow, Confidence, Dispatch, DispatchState, DoneOutcome, Doorbell,
    DoorbellOutcome, Finding, Gate, GateKind, GateStatus, InboxItem, InboxKind, ModelCheck,
    ModelCheckStatus, Op, Post, PostKind, Receipt, ReceiptData, RequestEnvelope, Seat, SeatRole,
    Task, TaskStatus, Team, TeamProjection, TemplateKind, VerdictKind,
};
