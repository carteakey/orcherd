//! Human-readable and `--json` formatting for `Receipt` and CLI commands.

use orcherd_core::{Receipt, ReceiptData};
use serde::Serialize;
use std::io::{self, Write as _};

/// Writes `text` to stdout. A reader that went away (`| head`) ends the process with
/// status 0, as `cat` and `git` do; other write errors exit 1 with the reason.
pub fn emit(text: &str, newline: bool) {
    let mut stdout = io::stdout().lock();
    let written = stdout
        .write_all(text.as_bytes())
        .and_then(|()| if newline { stdout.write_all(b"\n") } else { Ok(()) })
        .and_then(|()| stdout.flush());
    match written {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => {
            eprintln!("orcherd error: failed writing to stdout: {e}");
            std::process::exit(1);
        }
    }
}

pub fn print_value<T: Serialize>(value: &T, json: bool, human: impl FnOnce(&T) -> String) {
    if json {
        if let Ok(s) = serde_json::to_string_pretty(value) {
            outln!("{s}");
        }
    } else {
        outln!("{}", human(value));
    }
}

pub fn format_receipt(receipt: &Receipt, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(receipt).unwrap_or_else(|_| "{}".to_owned());
    }

    let mut out = String::new();
    match &receipt.data {
        ReceiptData::TeamStatus(proj) => {
            out.push_str(&format!(
                "Team: {} ({})\nGoal: {}\nWorkspace: {}\n",
                proj.team.name, proj.team.team_id, proj.team.goal, proj.team.workspace_root
            ));
            out.push_str(&format!("Seats ({}):\n", proj.seats.len()));
            for seat in &proj.seats {
                let unread = proj.unread_counts.get(&seat.name).copied().unwrap_or(0);
                out.push_str(&format!(
                    "  - {} [{}] agent={} address={} unread={}\n",
                    seat.name, seat.role, seat.agent, seat.address, unread
                ));
            }
            out.push_str(&format!(
                "Tasks ({}) | Active Dispatches ({}) | Pending Gates ({})\n",
                proj.tasks.len(),
                proj.active_dispatches.len(),
                proj.pending_gates.len()
            ));
            for task in &proj.tasks {
                out.push_str(&format!(
                    "  - {} [{}] {} (assignee: {}, reviewer: {})\n",
                    task.task_id,
                    task.status,
                    task.title,
                    task.assignee_seat.as_deref().unwrap_or("-"),
                    task.reviewer_seat.as_deref().unwrap_or("-")
                ));
            }
        }
        ReceiptData::SpawnPrepared {
            seat,
            task,
            dispatch,
            warm_reuse,
        } => {
            out.push_str(&format!(
                "Seat `{}` ({}) [warm_reuse={warm_reuse}] address={}\n",
                seat.name, seat.role, seat.address
            ));
            if let Some(t) = task {
                out.push_str(&format!("Task: {} [{}] {}\n", t.task_id, t.status, t.title));
            }
            if let Some(d) = dispatch {
                out.push_str(&format!("Dispatch: {} [{}]\n", d.dispatch_id, d.state));
            }
        }
        ReceiptData::Seat(seat) => {
            out.push_str(&format!(
                "Seat `{}` ({}) agent={} address={} worktree={}\n",
                seat.name,
                seat.role,
                seat.agent,
                seat.address,
                seat.worktree_path.as_deref().unwrap_or("-")
            ));
        }
        ReceiptData::Seats(seats) => {
            out.push_str(&format!("Seats ({}):\n", seats.len()));
            for s in seats {
                out.push_str(&format!(
                    "  - {} [{}] agent={} address={} worktree={}\n",
                    s.name,
                    s.role,
                    s.agent,
                    s.address,
                    s.worktree_path.as_deref().unwrap_or("-")
                ));
            }
        }
        ReceiptData::Dispatch(d) => {
            out.push_str(&format!(
                "Dispatch {} [{}] seat={} task={}\n",
                d.dispatch_id, d.state, d.seat_id, d.task_id
            ));
        }
        ReceiptData::Task(t) => {
            out.push_str(&format!(
                "Task {} [{}] {}\nAssignee: {} | Reviewer: {} | Deps: {:?}\n",
                t.task_id,
                t.status,
                t.title,
                t.assignee_seat.as_deref().unwrap_or("-"),
                t.reviewer_seat.as_deref().unwrap_or("-"),
                t.deps
            ));
        }
        ReceiptData::Tasks(tasks) => {
            out.push_str(&format!("Tasks ({}):\n", tasks.len()));
            for t in tasks {
                out.push_str(&format!(
                    "  - {} [{}] {} (assignee: {}, reviewer: {})\n",
                    t.task_id,
                    t.status,
                    t.title,
                    t.assignee_seat.as_deref().unwrap_or("-"),
                    t.reviewer_seat.as_deref().unwrap_or("-")
                ));
            }
        }
        ReceiptData::Inbox(items) => {
            if items.is_empty() {
                out.push_str("Inbox: 0 unread items.\n");
            } else {
                out.push_str(&format!("Inbox ({} item(s)):\n", items.len()));
                for item in items {
                    out.push_str(&format!(
                        "--- [seq {}] {} ({}) from {} ref={} ---\nSubject: {}\n{}\n",
                        item.seq, item.item_id, item.kind, item.from_seat, item.ref_id, item.subject, item.body
                    ));
                }
            }
        }
        ReceiptData::Sent {
            recipients,
            item_ids,
        } => {
            out.push_str(&format!(
                "Sent to {:?} (inbox items: {:?})\n",
                recipients, item_ids
            ));
        }
        ReceiptData::Gate(g) => {
            out.push_str(&format!(
                "Gate {} [{}] ({}) from {} -> {}: {}\n",
                g.gate_id, g.status, g.kind, g.asked_by_seat, g.to_seat, g.question
            ));
        }
        ReceiptData::Gates(gates) => {
            out.push_str(&format!("Gates ({}):\n", gates.len()));
            for g in gates {
                out.push_str(&format!(
                    "  - {} [{}] ({}) {} -> {}: {}\n",
                    g.gate_id, g.status, g.kind, g.asked_by_seat, g.to_seat, g.question
                ));
            }
        }
        ReceiptData::Post(p) => {
            out.push_str(&format!(
                "Bulletin [seq {}] {} ({}) by {}: {}\n{}\n",
                p.seq, p.post_id, p.kind, p.author_seat, p.subject, p.body
            ));
        }
        ReceiptData::Posts(posts) => {
            out.push_str(&format!("Bulletin ({} post(s)):\n", posts.len()));
            for p in posts {
                out.push_str(&format!(
                    "  [seq {}] {} ({}) @{}: {}\n    {}\n",
                    p.seq, p.post_id, p.kind, p.author_seat, p.subject, p.body
                ));
            }
        }
        ReceiptData::Finding(f) => {
            out.push_str(&format!(
                "Finding {} [{}] by {}: {}\n",
                f.finding_id, f.confidence, f.author_seat, f.title
            ));
        }
        ReceiptData::Findings(findings) => {
            out.push_str(&format!("Findings ({}):\n", findings.len()));
            for f in findings {
                out.push_str(&format!(
                    "  - {} [{}] by {}: {}\n",
                    f.finding_id, f.confidence, f.author_seat, f.title
                ));
            }
        }
        ReceiptData::Audit(rows) => {
            out.push_str(&format!("Audit Log ({} row(s)):\n", rows.len()));
            for r in rows {
                out.push_str(&format!(
                    "  [seq {}] {} actor={} term={} op={} ref={}\n",
                    r.seq,
                    r.at,
                    r.actor,
                    r.terminal.as_deref().unwrap_or("-"),
                    r.op,
                    r.ref_id.as_deref().unwrap_or("-")
                ));
            }
        }
        ReceiptData::Doorbell(d) => {
            out.push_str(&format!(
                "Doorbell seat={} seq={} attempts={} outcome={:?}\n",
                d.seat_id, d.rung_through_seq, d.attempts, d.outcome
            ));
        }
        ReceiptData::DoneResult {
            task,
            unblocked_task_ids,
            ..
        } => {
            out.push_str(&format!(
                "Task {} settled -> [{}] (unblocked: {:?})\n",
                task.task_id, task.status, unblocked_task_ids
            ));
        }
        ReceiptData::VerdictResult {
            task,
            unblocked_task_ids,
            escalated,
            ..
        } => {
            out.push_str(&format!(
                "Verdict recorded for {} -> [{}] (escalated={escalated}, unblocked={:?})\n",
                task.task_id, task.status, unblocked_task_ids
            ));
        }
    }

    if let Some(ref next) = receipt.next {
        out.push_str(&format!("Next: {next}\n"));
    }
    out.trim_end().to_owned()
}

pub fn subcommand_help(cmd: &str) -> &'static str {
    match cmd {
        "init" => "\
orcherd init — Initialize a team in the current repository

Usage: orcherd init [--name <name>] [--template <review-crew|studio-crew|small>] [--goal <text>] [--workspace-root <dir>] [--json]
",
        "spawn" | "worker-start" => "\
orcherd spawn — Start or reuse a persistent seat on a task

Usage: orcherd spawn --seat <name> [--role <supervisor|reviewer|planner|worker>]
                     [--agent <id>] [--model <id>] [--account <id>] [--effort <low|medium|high|xhigh>]
                     [--worktree <current|new-child|new-top-level|path:DIR>] [--base-branch <ref>]
                     [--on <hc_machine>] [--task <task_id>] [--spec-file <path>] [--title <text>]
                     [--keep-open] [--resume [<session_id>]] [--retry-of <dispatch_id>] [--no-launch] [--json]
",
        "seat" => "\
orcherd seat — Manage persistent seats and their herdr-core terminals

Usage:
  orcherd seat list [--include-remote] [--json]
  orcherd seat show <seat> [--json]
  orcherd seat read <seat> [--lines <n>] [--json]
  orcherd seat stop <seat|dispatch_id> [--json]
  orcherd seat abandon <dispatch_id> [--json]
  orcherd seat release <dispatch_id> [--json]
  orcherd seat rebind <seat> [--terminal <hc_terminal>] [--machine <m>] [--json]
  orcherd seat remove <seat> [--json]
  orcherd seat handoff --to <seat> [--json]
",
        "task" => "\
orcherd task — Manage the Task Board and DAG dependencies

Usage:
  orcherd task create --spec-file <path> [--title <text>] [--deps <id,id>] [--assign <seat>] [--reviewer <seat>] [--issue <ref>] [--json]
  orcherd task list [--status <pending|ready|assigned|in_progress|blocked|review|completed|failed|all>] [--ready] [--json]
  orcherd task show <task_id> [--json]
  orcherd task assign <task_id> --to <seat> [--reviewer <seat>] [--spec-file <path>] [--json]
  orcherd task claim <task_id> [--json]
  orcherd task update <task_id> --status <status> [--json]
",
        "inbox" | "check" => "\
orcherd inbox — Read and mark-read unread assignments, mentions, gate answers, and review requests

Usage: orcherd inbox [--peek] [--seat <seat>] [--limit <n>] [--json]
",
        "send" => "\
orcherd send — Send a direct or @group message to seats

Usage: orcherd send --to <seat|dispatch:id|@all|@idle|@workers|@reviewers> [--subject <text>] --body-file <path> [--json]
",
        "ask" => "\
orcherd ask — Create a blocking decision gate for the supervisor and pause the task

Usage: orcherd ask [<task_id>] --question <text> [--body-file <path>] [--json]
",
        "reply" => "\
orcherd reply — Answer a pending decision gate or inbox item and wake the worker

Usage: orcherd reply <gate_id|item_id> --body-file <path> [--json]
",
        "done" | "worker-done" => "\
orcherd done — Settle an active task and trigger blind review or DAG unblocking

Usage: orcherd done [<task_id>] --result-file <path> [--branch <name>] [--commit <sha>] [--outcome <succeeded|failed>] [--json]
",
        "verdict" => "\
orcherd verdict — Record a blind reviewer verdict (pass, fix, or escalate)

Usage: orcherd verdict <task_id> --verdict <pass|fix|escalate> --body-file <path> [--json]
",
        "bulletin" => "\
orcherd bulletin — Shared threaded team board

Usage:
  orcherd bulletin post --body-file <path> [--subject <text>] [--to <@seat,@seat>] [--reply-to <post_id>] [--kind <post|handoff|authorized>] [--json]
  orcherd bulletin list [--after <seq>] [--limit <n>] [--json]
",
        "finding" => "\
orcherd finding — Record or list durable evidence-backed findings

Usage:
  orcherd finding add --title <text> --evidence-file <path> [--task <task_id>] [--confidence <low|medium|high|verified>] [--json]
  orcherd finding list [--json]
",
        "skills" | "skill" => "\
orcherd skills — Load version-matched skill guides or install the discovery stub

Usage:
  orcherd skills list [--json]
  orcherd skills get [orcherd] [--reference references/<file>.md] [--full] [--json]
  orcherd skills install [--path <dir>] [--json]
",
        _ => crate::cli::HELP_TEXT,
    }
}
