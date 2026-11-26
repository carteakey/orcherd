//! `orcherd freeze` / `orcherd unfreeze`: stop a whole team at once (#1).
//!
//! Freezing records the reason, notifies every seat, refuses new spawns, claims and
//! assignments, and interrupts every seat that is mid-turn. A bulletin post isn't enough:
//! a busy agent reads it minutes later.

use crate::cli::{GlobalFlags, read_optional_file_or_inline, req_val, run_op_raw};
use crate::output::print_value;
use orcherd_core::{Address, Op, ReceiptData};
use orcherd_hc::{Core, Endpoint, new_command, wire::StatusState};
use std::{path::PathBuf, process::Stdio};

pub fn handle_freeze(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut reason = None;
    let mut reason_file = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--reason" => {
                i += 1;
                reason = Some(req_val(args, i, "--reason")?.to_owned());
            }
            "--reason-file" => {
                i += 1;
                reason_file = Some(PathBuf::from(req_val(args, i, "--reason-file")?));
            }
            other => return Err(format!("unknown `freeze` option `{other}`")),
        }
        i += 1;
    }
    let reason = read_optional_file_or_inline(reason_file.as_deref(), reason, "--reason-file")?
        .ok_or_else(|| "`freeze` requires `--reason <text>` or `--reason-file <path>`".to_owned())?;

    let receipt = run_op_raw(Op::Freeze { reason }, global)?;
    let interrupted = match &receipt.data {
        ReceiptData::TeamStatus(proj) => interrupt_working_seats(
            proj.seats.iter().filter_map(|s| match &s.address {
                Address::Core { machine, terminal } => Some((
                    s.name.clone(),
                    match machine {
                        Some(m) if !terminal.contains('/') => format!("{m}/{terminal}"),
                        _ => terminal.clone(),
                    },
                )),
                _ => None,
            }),
        ),
        _ => Vec::new(),
    };
    print_value(&FreezeReport { receipt: &receipt, interrupted: &interrupted }, global.json, |r| {
        let team = match &r.receipt.data {
            ReceiptData::TeamStatus(p) => p.team.name.clone(),
            _ => r.receipt.team_id.clone(),
        };
        let stopped = if r.interrupted.is_empty() {
            "No seat was mid-turn.".to_owned()
        } else {
            format!("Interrupted mid-turn: {}.", r.interrupted.join(", "))
        };
        format!("Team `{team}` frozen. Every seat was notified; new spawns, claims and assignments are refused.\n{stopped}\nResume with: orcherd unfreeze")
    });
    Ok(0)
}

pub fn handle_unfreeze(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    if let Some(other) = args.first() {
        return Err(format!("unknown `unfreeze` option `{other}`"));
    }
    let receipt = run_op_raw(Op::Unfreeze, global)?;
    print_value(&receipt, global.json, |_| {
        "Team unfrozen. Every seat was notified and will be rung to resume.".to_owned()
    });
    Ok(0)
}

#[derive(serde::Serialize)]
struct FreezeReport<'a> {
    receipt: &'a orcherd_core::Receipt,
    interrupted: &'a [String],
}

/// Interrupts the seats whose terminals report `working`, with `hc interrupt` (which knows
/// each agent's stop key). Idle seats are left alone: a stop key at an idle prompt can clear
/// or quit it. Without a reachable herdr-core nothing is interrupted.
fn interrupt_working_seats(seats: impl Iterator<Item = (String, String)>) -> Vec<String> {
    let Some(core) = Endpoint::resolve_env().ok().and_then(|ep| Core::connect(&ep).ok()) else {
        return Vec::new();
    };
    let Ok(list) = core.list(false) else {
        return Vec::new();
    };
    seats
        .filter(|(_, addr)| {
            list.terminals.iter().any(|t| {
                t.address == *addr && t.status.as_ref().is_some_and(|s| s.state == StatusState::Working)
            })
        })
        .filter(|(_, addr)| {
            new_command("hc")
                .args(["interrupt", addr])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .map(|(name, _)| name)
        .collect()
}
