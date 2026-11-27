//! Board, inbox, decision gate, bulletin, finding, approval, and audit CLI handlers.

use crate::{
    cli::{
        GlobalFlags, execute_op, parse_list_arg, read_optional_file_or_inline,
        read_required_file_or_inline, req_val, run_op_raw,
    },
    output::{format_receipt, print_value},
};
use orcherd_core::{
    Address, Confidence, DoneOutcome, GateStatus, Op, PostKind, Receipt, ReceiptData, TaskStatus,
    TeamProjection, VerdictKind,
};
use orcherd_hc::{Core, Endpoint};
use std::{env, path::PathBuf, str::FromStr, time::Duration};

pub fn handle_seat(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "list" => {
            let include_remote = rest.iter().any(|a| a == "--include-remote");
            execute_op(Op::SeatList { include_remote }, global)
        }
        "show" => {
            let seat = rest
                .first()
                .ok_or_else(|| "`seat show <seat>` requires a seat name".to_owned())?
                .clone();
            execute_op(Op::SeatShow { seat }, global)
        }
        "read" => {
            let seat_name = rest
                .first()
                .ok_or_else(|| "`seat read <seat>` requires a seat name".to_owned())?;
            let mut lines = 40u32;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--lines" | "--limit" => {
                        i += 1;
                        lines = req_val(rest, i, "--lines")?
                            .parse::<u32>()
                            .map_err(|e| format!("invalid `--lines`: {e}"))?;
                    }
                    other => return Err(format!("unknown `seat read` option `{other}`")),
                }
                i += 1;
            }
            read_seat_terminal(seat_name, lines, global)
        }
        "stop" => {
            let target = rest
                .first()
                .ok_or_else(|| "`seat stop <seat|dispatch_id>` requires a target".to_owned())?
                .clone();
            stop_or_release_seat(
                Op::SeatStop {
                    target,
                    reason: Some("user_stopped".to_owned()),
                },
                global,
            )
        }
        "abandon" => {
            let dispatch_id = rest
                .first()
                .ok_or_else(|| "`seat abandon <dispatch_id>` requires a dispatch ID".to_owned())?
                .clone();
            execute_op(Op::SeatAbandon { dispatch_id }, global)
        }
        "release" => {
            let dispatch_id = rest
                .first()
                .ok_or_else(|| "`seat release <dispatch_id>` requires a dispatch ID".to_owned())?
                .clone();
            stop_or_release_seat(Op::SeatRelease { dispatch_id }, global)
        }
        "rebind" => {
            let seat = rest
                .first()
                .ok_or_else(|| "`seat rebind <seat>` requires a seat name".to_owned())?
                .clone();
            let mut terminal = None;
            let mut machine = None;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--terminal" => {
                        i += 1;
                        terminal = Some(req_val(rest, i, "--terminal")?.to_owned());
                    }
                    "--machine" | "--on" => {
                        i += 1;
                        machine = Some(req_val(rest, i, "--machine")?.to_owned());
                    }
                    other => return Err(format!("unknown `seat rebind` option `{other}`")),
                }
                i += 1;
            }
            let term = terminal
                .or_else(|| env::var("HC_TERMINAL_ID").ok())
                .ok_or_else(|| "`seat rebind` requires `--terminal <id>` or `$HC_TERMINAL_ID`".to_owned())?;
            execute_op(
                Op::SeatRebind {
                    seat,
                    address: Address::Core {
                        machine,
                        terminal: term,
                    },
                },
                global,
            )
        }
        "remove" => {
            let seat = rest
                .first()
                .ok_or_else(|| "`seat remove <seat>` requires a seat name".to_owned())?
                .clone();
            execute_op(Op::SeatRemove { seat }, global)
        }
        "handoff" => {
            let mut to_seat = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--to" => {
                        i += 1;
                        to_seat = Some(req_val(rest, i, "--to")?.to_owned());
                    }
                    val if !val.starts_with('-') && to_seat.is_none() => {
                        to_seat = Some(val.to_owned());
                    }
                    other => return Err(format!("unknown `seat handoff` option `{other}`")),
                }
                i += 1;
            }
            let to_seat = to_seat.ok_or_else(|| "`seat handoff` requires `--to <seat>`".to_owned())?;
            execute_op(Op::SeatHandoff { to_seat }, global)
        }
        "brief" => {
            let (seat, brief) = target_and_text(rest, "seat brief <seat>", "--brief")?;
            execute_op(Op::SeatBrief { seat, brief }, global)
        }
        other => Err(format!("unknown `seat` subcommand `{other}`")),
    }
}

fn stop_or_release_seat(op: Op, global: &GlobalFlags) -> Result<i32, String> {
    let receipt = stop_or_release_with(
        op,
        |o| run_op_raw(o, global),
        |addr| {
            if let Ok(ep) = Endpoint::resolve_env()
                && let Ok(core) = Core::connect(&ep)
            {
                let _ = core.kill(addr);
            }
        },
    )?;
    let failed = receipt.failed_stage.is_some();
    outln!("{}", format_receipt(&receipt, global.json));
    Ok(if failed { 1 } else { 0 })
}

/// Validates and applies `Op::SeatStop` / `Op::SeatRelease` before killing any
/// terminal, so ambiguous prefixes or unknown targets never kill live terminals.
pub(crate) fn stop_or_release_with<R, K>(
    op: Op,
    mut run_op: R,
    mut kill_terminal: K,
) -> Result<Receipt, String>
where
    R: FnMut(Op) -> Result<Receipt, String>,
    K: FnMut(&str),
{
    // Read-only snapshot so we still know the seat's pre-stop terminal address
    // after `apply` clears `seats.address` to `none`.
    let before = run_op(Op::Status).ok().and_then(|r| match r.data {
        ReceiptData::TeamStatus(proj) => Some(*proj),
        _ => None,
    });
    let receipt = run_op(op)?;
    if let Some(addr) = resolved_terminal_to_kill(&receipt, before.as_ref()) {
        kill_terminal(&addr);
    }
    Ok(receipt)
}

fn resolved_terminal_to_kill(
    receipt: &Receipt,
    before: Option<&TeamProjection>,
) -> Option<String> {
    let format_addr = |machine: Option<&str>, terminal: &str| match machine {
        Some(m) if !terminal.contains('/') => format!("{m}/{terminal}"),
        _ => terminal.to_owned(),
    };
    let seat_addr_before = |seat_id: &str| {
        before?
            .seats
            .iter()
            .find(|s| s.seat_id == seat_id)
            .and_then(|s| match s.address {
                Address::Core {
                    ref machine,
                    ref terminal,
                } => Some(format_addr(machine.as_deref(), terminal)),
                _ => None,
            })
    };
    match &receipt.data {
        ReceiptData::Seat(seat) => seat_addr_before(&seat.seat_id),
        ReceiptData::Dispatch(disp) => disp
            .terminal_address
            .as_deref()
            .map(|t| format_addr(disp.machine.as_deref(), t))
            .or_else(|| seat_addr_before(&disp.seat_id)),
        _ => None,
    }
}

fn read_seat_terminal(seat_name: &str, lines: u32, global: &GlobalFlags) -> Result<i32, String> {
    let receipt = run_op_raw(
        Op::SeatShow {
            seat: seat_name.to_owned(),
        },
        global,
    )?;
    let ReceiptData::Seat(seat) = receipt.data else {
        return Err("unexpected receipt for seat show".to_owned());
    };
    let preview = match &seat.address {
        Address::Core { machine, terminal } => {
            let target = match machine {
                Some(m) if !terminal.contains('/') => format!("{m}/{terminal}"),
                _ => terminal.clone(),
            };
            if let Ok(ep) = Endpoint::resolve_env()
                && let Ok(core) = Core::connect(&ep)
            {
                core.read_preview(&target, lines, Duration::from_secs(2))
                    .unwrap_or_else(|e| format!("(terminal `{target}` read preview unavailable: {e})"))
            } else {
                format!("(herdr-core offline; seat `{}` assigned to `{target}`)", seat.name)
            }
        }
        other => format!("(seat `{}` address is `{other}`)", seat.name),
    };
    #[derive(serde::Serialize)]
    struct SeatReadOutput {
        address: String,
        output: String,
        seat: String,
    }
    let out = SeatReadOutput {
        seat: seat.name,
        address: seat.address.to_string(),
        output: preview,
    };
    print_value(&out, global.json, |o| {
        format!("=== {} ({}) ===\n{}", o.seat, o.address, o.output)
    });
    Ok(0)
}

pub fn handle_task(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "create" => {
            let mut title = None;
            let mut spec = None;
            let mut spec_file = None;
            let mut deps = Vec::new();
            let mut assign = None;
            let mut reviewer = None;
            let mut issue_ref = None;
            let mut parent_id = None;
            let mut placement = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--title" => {
                        i += 1;
                        title = Some(req_val(rest, i, "--title")?.to_owned());
                    }
                    "--spec-file" => {
                        i += 1;
                        spec_file = Some(PathBuf::from(req_val(rest, i, "--spec-file")?));
                    }
                    "--spec" => {
                        i += 1;
                        spec = Some(req_val(rest, i, "--spec")?.to_owned());
                    }
                    "--deps" => {
                        i += 1;
                        deps.extend(parse_list_arg(req_val(rest, i, "--deps")?));
                    }
                    "--assign" | "--to" => {
                        i += 1;
                        assign = Some(req_val(rest, i, "--assign")?.to_owned());
                    }
                    "--reviewer" => {
                        i += 1;
                        reviewer = Some(req_val(rest, i, "--reviewer")?.to_owned());
                    }
                    "--issue" => {
                        i += 1;
                        issue_ref = Some(req_val(rest, i, "--issue")?.to_owned());
                    }
                    "--parent" => {
                        i += 1;
                        parent_id = Some(req_val(rest, i, "--parent")?.to_owned());
                    }
                    "--placement" => {
                        i += 1;
                        placement = Some(req_val(rest, i, "--placement")?.to_owned());
                    }
                    other => return Err(format!("unknown `task create` option `{other}`")),
                }
                i += 1;
            }
            let spec = read_required_file_or_inline(spec_file.as_deref(), spec, "--spec-file")?;
            execute_op(
                Op::TaskCreate {
                    title,
                    spec,
                    deps,
                    assign,
                    reviewer,
                    issue_ref,
                    parent_id,
                    placement,
                },
                global,
            )
        }
        "list" => {
            let mut status = None;
            let mut ready_only = false;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--ready" => ready_only = true,
                    "--status" => {
                        i += 1;
                        let raw = req_val(rest, i, "--status")?;
                        if raw != "all" {
                            status = Some(TaskStatus::from_str(raw).map_err(|e| e.to_string())?);
                        }
                    }
                    "--brief" => {}
                    other => return Err(format!("unknown `task list` option `{other}`")),
                }
                i += 1;
            }
            execute_op(Op::TaskList { status, ready_only }, global)
        }
        "show" => {
            let task_id = rest
                .first()
                .ok_or_else(|| "`task show <task_id>` requires a task ID".to_owned())?
                .clone();
            execute_op(Op::TaskShow { task_id }, global)
        }
        "assign" => {
            let task_id = rest
                .first()
                .ok_or_else(|| "`task assign <task_id>` requires a task ID".to_owned())?
                .clone();
            let mut to_seat = None;
            let mut reviewer = None;
            let mut spec_file = None;
            let mut spec = None;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--to" | "--assign" => {
                        i += 1;
                        to_seat = Some(req_val(rest, i, "--to")?.to_owned());
                    }
                    "--reviewer" => {
                        i += 1;
                        reviewer = Some(req_val(rest, i, "--reviewer")?.to_owned());
                    }
                    "--spec-file" => {
                        i += 1;
                        spec_file = Some(PathBuf::from(req_val(rest, i, "--spec-file")?));
                    }
                    "--spec" => {
                        i += 1;
                        spec = Some(req_val(rest, i, "--spec")?.to_owned());
                    }
                    other => return Err(format!("unknown `task assign` option `{other}`")),
                }
                i += 1;
            }
            let to_seat = to_seat.ok_or_else(|| "`task assign` requires `--to <seat>`".to_owned())?;
            let spec = read_optional_file_or_inline(spec_file.as_deref(), spec, "--spec-file")?;
            execute_op(
                Op::TaskAssign {
                    task_id,
                    to_seat,
                    reviewer,
                    spec,
                },
                global,
            )
        }
        "claim" => {
            let task_id = rest
                .first()
                .ok_or_else(|| "`task claim <task_id>` requires a task ID".to_owned())?
                .clone();
            execute_op(Op::TaskClaim { task_id }, global)
        }
        "update" => {
            let mut task_id = None;
            let mut status = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--id" | "--task" => {
                        i += 1;
                        task_id = Some(req_val(rest, i, "--id")?.to_owned());
                    }
                    "--status" => {
                        i += 1;
                        status = Some(
                            TaskStatus::from_str(req_val(rest, i, "--status")?)
                                .map_err(|e| e.to_string())?,
                        );
                    }
                    val if !val.starts_with('-') && task_id.is_none() => {
                        task_id = Some(val.to_owned());
                    }
                    other => return Err(format!("unknown `task update` option `{other}`")),
                }
                i += 1;
            }
            let task_id = task_id.ok_or_else(|| "`task update` requires `<task_id>`".to_owned())?;
            let status = status.ok_or_else(|| "`task update` requires `--status <status>`".to_owned())?;
            execute_op(Op::TaskUpdate { task_id, status }, global)
        }
        "reopen" => {
            let (task, spec) = target_and_text(rest, "task reopen <task_id>", "--spec")?;
            execute_op(Op::TaskReopen { task, spec }, global)
        }
        other => Err(format!("unknown `task` subcommand `{other}`")),
    }
}

pub fn handle_inbox(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut peek = false;
    let mut seat = None;
    let mut limit = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--peek" => peek = true,
            "--seat" | "--terminal" => {
                i += 1;
                seat = Some(req_val(args, i, "--seat")?.to_owned());
            }
            "--limit" => {
                i += 1;
                limit = Some(
                    req_val(args, i, "--limit")?
                        .parse::<usize>()
                        .map_err(|e| format!("invalid `--limit`: {e}"))?,
                );
            }
            other => return Err(format!("unknown `inbox` option `{other}`")),
        }
        i += 1;
    }
    execute_op(Op::InboxRead { peek, seat, limit }, global)
}

pub fn handle_send(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut to = None;
    let mut subject = None;
    let mut body_file = None;
    let mut body = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => {
                i += 1;
                to = Some(req_val(args, i, "--to")?.to_owned());
            }
            "--subject" => {
                i += 1;
                subject = Some(req_val(args, i, "--subject")?.to_owned());
            }
            "--body-file" => {
                i += 1;
                body_file = Some(PathBuf::from(req_val(args, i, "--body-file")?));
            }
            "--body" => {
                i += 1;
                body = Some(req_val(args, i, "--body")?.to_owned());
            }
            other => return Err(format!("unknown `send` option `{other}`")),
        }
        i += 1;
    }
    let to = to.ok_or_else(|| "`send` requires `--to <seat|dispatch:id|@group>`".to_owned())?;
    let body = read_required_file_or_inline(body_file.as_deref(), body, "--body-file")?;
    let subject = subject.unwrap_or_else(|| "Message".to_owned());
    execute_op(Op::Send { to, subject, body }, global)
}

pub fn handle_ask(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut task_id = None;
    let mut question = None;
    let mut body_file = None;
    let mut body = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--task" => {
                i += 1;
                task_id = Some(req_val(args, i, "--task")?.to_owned());
            }
            "--question" => {
                i += 1;
                question = Some(req_val(args, i, "--question")?.to_owned());
            }
            "--body-file" => {
                i += 1;
                body_file = Some(PathBuf::from(req_val(args, i, "--body-file")?));
            }
            "--body" => {
                i += 1;
                body = Some(req_val(args, i, "--body")?.to_owned());
            }
            val if !val.starts_with('-') && task_id.is_none() => {
                task_id = Some(val.to_owned());
            }
            other => return Err(format!("unknown `ask` option `{other}`")),
        }
        i += 1;
    }
    let body = read_optional_file_or_inline(body_file.as_deref(), body, "--body-file")?;
    let question = question
        .or_else(|| body.clone())
        .ok_or_else(|| "`ask` requires `--question <text>` or `--body-file <path>`".to_owned())?;
    execute_op(
        Op::Ask {
            task_id,
            question,
            body,
        },
        global,
    )
}

pub fn handle_reply(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut target_id = None;
    let mut body_file = None;
    let mut body = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--id" | "--gate" => {
                i += 1;
                target_id = Some(req_val(args, i, "--id")?.to_owned());
            }
            "--body-file" => {
                i += 1;
                body_file = Some(PathBuf::from(req_val(args, i, "--body-file")?));
            }
            "--body" => {
                i += 1;
                body = Some(req_val(args, i, "--body")?.to_owned());
            }
            val if !val.starts_with('-') && target_id.is_none() => {
                target_id = Some(val.to_owned());
            }
            other => return Err(format!("unknown `reply` option `{other}`")),
        }
        i += 1;
    }
    let target_id =
        target_id.ok_or_else(|| "`reply` requires `<gate_id>` or `--id <id>`".to_owned())?;
    let body = read_required_file_or_inline(body_file.as_deref(), body, "--body-file")?;
    execute_op(Op::Reply { target_id, body }, global)
}

pub fn handle_done(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut task_id = None;
    let mut result_file = None;
    let mut result_text = None;
    let mut branch = None;
    let mut commit_sha = None;
    let mut outcome = DoneOutcome::Succeeded;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--task" => {
                i += 1;
                task_id = Some(req_val(args, i, "--task")?.to_owned());
            }
            "--result-file" | "--body-file" => {
                i += 1;
                result_file = Some(PathBuf::from(req_val(args, i, "--result-file")?));
            }
            "--result" | "--body" => {
                i += 1;
                result_text = Some(req_val(args, i, "--result")?.to_owned());
            }
            "--branch" => {
                i += 1;
                branch = Some(req_val(args, i, "--branch")?.to_owned());
            }
            "--commit" => {
                i += 1;
                commit_sha = Some(req_val(args, i, "--commit")?.to_owned());
            }
            "--outcome" => {
                i += 1;
                outcome =
                    DoneOutcome::from_str(req_val(args, i, "--outcome")?).map_err(|e| e.to_string())?;
            }
            val if !val.starts_with('-') && task_id.is_none() => {
                task_id = Some(val.to_owned());
            }
            other => return Err(format!("unknown `done` option `{other}`")),
        }
        i += 1;
    }
    let result = read_required_file_or_inline(result_file.as_deref(), result_text, "--result-file")?;
    execute_op(
        Op::Done {
            task_id,
            result,
            branch,
            commit_sha,
            outcome,
        },
        global,
    )
}

pub fn handle_verdict(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut task_id = None;
    let mut verdict = None;
    let mut body_file = None;
    let mut body = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--task" => {
                i += 1;
                task_id = Some(req_val(args, i, "--task")?.to_owned());
            }
            "--verdict" => {
                i += 1;
                verdict = Some(
                    VerdictKind::from_str(req_val(args, i, "--verdict")?)
                        .map_err(|e| e.to_string())?,
                );
            }
            "--body-file" => {
                i += 1;
                body_file = Some(PathBuf::from(req_val(args, i, "--body-file")?));
            }
            "--body" => {
                i += 1;
                body = Some(req_val(args, i, "--body")?.to_owned());
            }
            val if !val.starts_with('-') && task_id.is_none() => {
                task_id = Some(val.to_owned());
            }
            other => return Err(format!("unknown `verdict` option `{other}`")),
        }
        i += 1;
    }
    let task_id = task_id.ok_or_else(|| "`verdict` requires `<task_id>`".to_owned())?;
    let verdict =
        verdict.ok_or_else(|| "`verdict` requires `--verdict pass|fix|escalate`".to_owned())?;
    let body = read_required_file_or_inline(body_file.as_deref(), body, "--body-file")?;
    execute_op(
        Op::Verdict {
            task_id,
            verdict,
            body,
        },
        global,
    )
}

pub fn handle_gate(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "list" => {
            let mut status = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--status" => {
                        i += 1;
                        let raw = req_val(rest, i, "--status")?;
                        if raw != "all" {
                            status = Some(GateStatus::from_str(raw).map_err(|e| e.to_string())?);
                        }
                    }
                    other => return Err(format!("unknown `gate list` option `{other}`")),
                }
                i += 1;
            }
            execute_op(Op::GateList { status }, global)
        }
        other => Err(format!("unknown `gate` subcommand `{other}`")),
    }
}

pub fn handle_bulletin(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "post" => {
            let mut subject = None;
            let mut body_file = None;
            let mut body = None;
            let mut mentions = Vec::new();
            let mut reply_to = None;
            let mut kind = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--subject" => {
                        i += 1;
                        subject = Some(req_val(rest, i, "--subject")?.to_owned());
                    }
                    "--body-file" => {
                        i += 1;
                        body_file = Some(PathBuf::from(req_val(rest, i, "--body-file")?));
                    }
                    "--body" => {
                        i += 1;
                        body = Some(req_val(rest, i, "--body")?.to_owned());
                    }
                    "--to" => {
                        i += 1;
                        mentions.extend(parse_list_arg(req_val(rest, i, "--to")?));
                    }
                    "--reply-to" => {
                        i += 1;
                        reply_to = Some(req_val(rest, i, "--reply-to")?.to_owned());
                    }
                    "--kind" => {
                        i += 1;
                        kind = Some(
                            PostKind::from_str(req_val(rest, i, "--kind")?)
                                .map_err(|e| e.to_string())?,
                        );
                    }
                    other => return Err(format!("unknown `bulletin post` option `{other}`")),
                }
                i += 1;
            }
            let body = read_required_file_or_inline(body_file.as_deref(), body, "--body-file")?;
            execute_op(
                Op::BulletinPost {
                    subject,
                    body,
                    mentions,
                    reply_to,
                    kind,
                },
                global,
            )
        }
        "list" => {
            let mut after_seq = None;
            let mut limit = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--after" | "--from" => {
                        i += 1;
                        after_seq = Some(
                            req_val(rest, i, "--after")?
                                .parse::<i64>()
                                .map_err(|e| format!("invalid `--after`: {e}"))?,
                        );
                    }
                    "--limit" => {
                        i += 1;
                        limit = Some(
                            req_val(rest, i, "--limit")?
                                .parse::<usize>()
                                .map_err(|e| format!("invalid `--limit`: {e}"))?,
                        );
                    }
                    other => return Err(format!("unknown `bulletin list` option `{other}`")),
                }
                i += 1;
            }
            execute_op(Op::BulletinList { after_seq, limit }, global)
        }
        other => Err(format!("unknown `bulletin` subcommand `{other}`")),
    }
}

pub fn handle_finding(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "add" => {
            let mut title = None;
            let mut evidence_file = None;
            let mut evidence = None;
            let mut task_id = None;
            let mut confidence = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--title" => {
                        i += 1;
                        title = Some(req_val(rest, i, "--title")?.to_owned());
                    }
                    "--evidence-file" | "--body-file" => {
                        i += 1;
                        evidence_file = Some(PathBuf::from(req_val(rest, i, "--evidence-file")?));
                    }
                    "--evidence" | "--body" => {
                        i += 1;
                        evidence = Some(req_val(rest, i, "--evidence")?.to_owned());
                    }
                    "--task" => {
                        i += 1;
                        task_id = Some(req_val(rest, i, "--task")?.to_owned());
                    }
                    "--confidence" => {
                        i += 1;
                        confidence = Some(
                            Confidence::from_str(req_val(rest, i, "--confidence")?)
                                .map_err(|e| e.to_string())?,
                        );
                    }
                    other => return Err(format!("unknown `finding add` option `{other}`")),
                }
                i += 1;
            }
            let title = title.ok_or_else(|| "`finding add` requires `--title <text>`".to_owned())?;
            let evidence =
                read_required_file_or_inline(evidence_file.as_deref(), evidence, "--evidence-file")?;
            execute_op(
                Op::FindingAdd {
                    title,
                    evidence,
                    task_id,
                    confidence,
                },
                global,
            )
        }
        "list" => execute_op(Op::FindingList, global),
        other => Err(format!("unknown `finding` subcommand `{other}`")),
    }
}

pub fn handle_approve(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let gate_id = args
        .first()
        .ok_or_else(|| "`approve <gate_id>` requires a gate ID".to_owned())?
        .clone();
    let mut resolution = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--resolution" | "--note" => {
                i += 1;
                resolution = Some(req_val(args, i, "--resolution")?.to_owned());
            }
            other => return Err(format!("unknown `approve` option `{other}`")),
        }
        i += 1;
    }
    execute_op(
        Op::Approve {
            gate_id,
            resolution,
        },
        global,
    )
}

pub fn handle_log(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut limit = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--limit" => {
                i += 1;
                limit = Some(
                    req_val(args, i, "--limit")?
                        .parse::<usize>()
                        .map_err(|e| format!("invalid `--limit`: {e}"))?,
                );
            }
            other => return Err(format!("unknown `log` option `{other}`")),
        }
        i += 1;
    }
    execute_op(Op::AuditLog { limit }, global)
}

/// Parses `<target> (--<flag>-file <path> | --<flag> <text>)`, as `seat brief` and `task reopen` take.
fn target_and_text(args: &[String], usage: &str, flag: &str) -> Result<(String, String), String> {
    let file_flag = format!("{flag}-file");
    let (mut target, mut inline, mut file) = (None, None, None);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            f if f == file_flag => {
                i += 1;
                file = Some(PathBuf::from(req_val(args, i, &file_flag)?));
            }
            f if f == flag => {
                i += 1;
                inline = Some(req_val(args, i, flag)?.to_owned());
            }
            val if !val.starts_with('-') && target.is_none() => target = Some(val.to_owned()),
            other => return Err(format!("unknown `{usage}` option `{other}`")),
        }
        i += 1;
    }
    let target = target.ok_or_else(|| format!("`{usage}` needs its target"))?;
    let text = read_required_file_or_inline(file.as_deref(), inline, &file_flag)?;
    Ok((target, text))
}
