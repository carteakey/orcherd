//! Typed errors for `orcherd-core`.

use std::io;

use thiserror::Error;

/// Crate-wide result alias for `orcherd-core`.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors produced by `orcherd-core` store and transaction operations.
#[derive(Debug, Error)]
pub enum Error {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("json serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    #[error("database schema version {found} is newer than supported version {supported}")]
    SchemaTooNew { found: u32, supported: u32 },

    #[error("{entity} not found: {id}")]
    NotFound { entity: &'static str, id: String },

    #[error("ambiguous {entity} prefix '{prefix}': matches {candidates:?}")]
    AmbiguousPrefix {
        entity: &'static str,
        prefix: String,
        candidates: Vec<String>,
    },

    #[error("validation error: {0}")]
    Validation(String),

    #[error(
        "reviewer blindness invariant violated: seat '{seat}' cannot both build and review the same task"
    )]
    ReviewerBlindness { seat: String },

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error(
        "circuit breaker tripped for task '{task_id}' (dispatch '{dispatch_id}') after {failures} consecutive failures"
    )]
    CircuitBroken {
        task_id: String,
        dispatch_id: String,
        failures: u32,
    },

    #[error(
        "idempotency conflict: request_id '{request_id}' was already used with a different operation payload"
    )]
    RequestConflict { request_id: String },
}

impl Error {
    /// Construct a `NotFound` error for the given entity kind and identifier.
    pub fn not_found(entity: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound {
            entity,
            id: id.into(),
        }
    }

    /// Construct a `Validation` error.
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }
}
