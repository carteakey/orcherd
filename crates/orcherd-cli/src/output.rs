//! Human-readable and `--json` formatting for `Receipt` and CLI commands.

use orcherd_core::{Receipt, ReceiptData};
use serde::Serialize;

pub fn print_value<T: Serialize>(value: &T, json: bool, human: impl FnOnce(&T) -> String) {
    if json {
        if let Ok(s) = serde_json::to_string_pretty(value) {
            println!("{s}");
        }
    } else {
        println!("{}", human(value));
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
