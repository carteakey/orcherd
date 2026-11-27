//! Per-team idempotent `mutation_receipts` lookup and persistence (`SPEC.md` §4).

use std::fmt::Write as _;

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::types::{Op, Receipt};

pub(super) struct ReceiptKey<'a> {
    pub(super) team_id: &'a str,
    pub(super) caller_id: &'a str,
    pub(super) request_id: &'a str,
}

pub(super) enum ReceiptLookup {
    Replay(Receipt),
    Fresh(String),
}

/// Check `mutation_receipts` for `(team_id, caller_id, request_id)` and return either a cached
/// replay receipt or the payload hash to record after the mutation succeeds.
pub(super) fn lookup_receipt(
    conn: &Connection,
    key: &ReceiptKey<'_>,
    op: &Op,
) -> Result<ReceiptLookup> {
    let op_bytes = serde_json::to_vec(op)?;
    let hash = sha256_hex(&serde_json::to_vec(&(key.team_id, op))?);
    let existing: Option<(String, String)> = conn
        .query_row(
            "SELECT payload_hash, receipt_json FROM mutation_receipts \
             WHERE team_id = ?1 AND caller_id = ?2 AND request_id = ?3",
            params![key.team_id, key.caller_id, key.request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((stored_hash, stored_json)) = existing {
        // Also accept v2 op-only hashes on rows migrated to v3, since the primary key
        // lookup already isolates receipts by `team_id`.
        if stored_hash == hash || stored_hash == sha256_hex(&op_bytes) {
            return Ok(ReceiptLookup::Replay(serde_json::from_str(&stored_json)?));
        }
        return Err(Error::RequestConflict {
            request_id: key.request_id.to_string(),
        });
    }
    Ok(ReceiptLookup::Fresh(hash))
}

/// Persist a completed mutation receipt under `(team_id, caller_id, request_id)`.
pub(super) fn store_receipt(
    conn: &Connection,
    key: &ReceiptKey<'_>,
    op_name: &str,
    payload_hash: &str,
    receipt: &Receipt,
    now: &str,
) -> Result<()> {
    let receipt_json = serde_json::to_string(receipt)?;
    conn.execute(
        "INSERT INTO mutation_receipts \
         (team_id, caller_id, request_id, op, payload_hash, state, receipt_json, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'completed', ?6, ?7)",
        params![
            key.team_id,
            key.caller_id,
            key.request_id,
            op_name,
            payload_hash,
            receipt_json,
            now
        ],
    )?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}
