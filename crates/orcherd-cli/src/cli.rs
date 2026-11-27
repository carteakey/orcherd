//! CLI argument parser and command dispatcher (`SPEC.md` §8).

use crate::{
    board::{
        handle_approve, handle_ask, handle_bulletin, handle_done, handle_finding, handle_gate,
        handle_inbox, handle_log, handle_reply, handle_seat, handle_send, handle_task,
        handle_verdict,
    },
    daemon,
    identity::{
        annotate_team_not_found, resolve_actor, resolve_db_path, resolve_state_dir,
        resolve_team_ref,
    },
    mod_install,
    output::{format_receipt, print_value, subcommand_help},
    preflight, remote, skills, spawn, tunnel, worktree,
};
use orcherd_core::{Op, Receipt, RequestEnvelope, Store, TemplateKind};
use std::{
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

pub const HELP_TEXT: &str = "\
orcherd — Multi-agent team orchestration on herdr-core (hc)

Usage: orcherd <command> [options]

Setup & Skills:
  init        Initialize a team (--name, --template review-crew|studio-crew|small, --goal)
  status      Show team roster, tasks, dispatches, and pending gates
  skills      Manage bundled progressive skills (list | get [orcherd] [--reference <ref>] | install)
  mod         Install the Herdr desktop app mod (mod install [--path <mods_dir>])
  daemon      Manage doorbell & escalation daemon (start | stop | status | run [--once])
  preflight   Compare the agent binary path and version in hc vs. your shell (--agent <id>)
  tunnel      Start or stop a supervised reverse SSH tunnel (tunnel <machine> [--stop])

Seats & Dispatches:
  spawn       Start or reuse a persistent seat on a task (--seat, --agent, --worktree, --spec-file)
  seat        Manage seats (list | show | read | stop | abandon | release | rebind | remove | handoff)

Task Board & DAGs:
  task        Manage tasks (create | list | show | assign | claim | update)

Turn Outcomes & Gates:
  inbox       Read unread assignments, mentions, questions, and verdicts (--peek)
  send        Send a direct or @group message (--to, --subject, --body-file)
  ask         Create a blocking decision gate for the supervisor (--question, --body-file)
  reply       Answer a decision gate or inbox item (<gate_id>, --body-file)
  done        Complete an active task (--result-file, --outcome succeeded|failed)
  verdict     Record a blind reviewer verdict (<task_id>, --verdict pass|fix|escalate, --body-file)
  gate        List decision and approval gates (gate list [--status pending|resolved])

Bulletin, Findings & Governance:
  bulletin    Team-wide threaded board (post --body-file | list [--after <seq>])
  finding     Durable evidence records (add --title --evidence-file | list)
  pending     List pending human approval requests (seat removal / role handoff)
  approve     Approve a pending human approval gate (<gate_id>)
  log         Inspect the append-only audit log (--limit <n>)
  freeze      Stop the whole team now: refuse new work, interrupt busy seats (--reason)
  unfreeze    Lift a freeze and ring every seat to resume

Global Options:
  --json                Output machine-readable JSON
  --team <name|id>      Target team (defaults to $ORCHERD_TEAM or current repo team)
  --request-id <id>     Idempotency request key for safe mutation replay
  --as <seat>           Act as a named seat (logged in audit)
  -h, --help            Print help
  -V, --version         Print version
";

#[derive(Debug, Default, Clone)]
pub struct GlobalFlags {
    pub act_as: Option<String>,
    pub help: bool,
    pub json: bool,
    pub request_id: Option<String>,
    pub team: Option<String>,
    pub version: bool,
}

pub fn run_from_args(raw_args: Vec<String>) -> Result<i32, String> {
    let (global, args) = extract_global_flags(raw_args)?;
    if global.version {
        if global.json {
            outln!(
                "{{\"name\":\"orcherd\",\"version\":\"{}\"}}",
                env!("CARGO_PKG_VERSION")
            );
        } else {
            outln!("orcherd {}", env!("CARGO_PKG_VERSION"));
        }
        return Ok(0);
    }
    if args.is_empty() {
        out!("{HELP_TEXT}");
        return Ok(0);
    }
    if global.help {
        out!("{}", subcommand_help(args[0].as_str()));
        return Ok(0);
    }

    let cmd = args[0].as_str();
    let rest = &args[1..];

    match cmd {
        "skills" | "skill" => handle_skills(rest, &global),
        "mod" => handle_mod(rest, &global),
        "daemon" => handle_daemon(rest, &global),
        "preflight" => preflight::handle_preflight(rest, &global),
        "tunnel" => tunnel::handle_tunnel(rest, &global),
        "init" => handle_init(rest, &global),
        "status" => execute_op(Op::Status, &global),
        "spawn" | "worker-start" => spawn::handle_spawn(rest, &global),
        "seat" => handle_seat(rest, &global),
        "task" => handle_task(rest, &global),
        "inbox" | "check" => handle_inbox(rest, &global),
        "send" => handle_send(rest, &global),
        "ask" => handle_ask(rest, &global),
        "reply" => handle_reply(rest, &global),
        "done" | "worker-done" => handle_done(rest, &global),
        "verdict" => handle_verdict(rest, &global),
        "gate" => handle_gate(rest, &global),
        "bulletin" => handle_bulletin(rest, &global),
        "finding" => handle_finding(rest, &global),
        "pending" => execute_op(Op::PendingList, &global),
        "approve" => handle_approve(rest, &global),
        "freeze" => crate::freeze::handle_freeze(rest, &global),
        "unfreeze" => crate::freeze::handle_unfreeze(rest, &global),
        "log" => handle_log(rest, &global),
        other => Err(format!(
            "unknown command `{other}`; run `orcherd --help` for usage"
        )),
    }
}

fn extract_global_flags(raw: Vec<String>) -> Result<(GlobalFlags, Vec<String>), String> {
    let mut g = GlobalFlags::default();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--json" => g.json = true,
            "-h" | "--help" => g.help = true,
            "-V" | "--version" => g.version = true,
            "--team" => {
                i += 1;
                g.team = Some(
                    raw.get(i)
                        .ok_or_else(|| "`--team` requires a value".to_owned())?
                        .clone(),
                );
            }
            "--request-id" => {
                i += 1;
                g.request_id = Some(
                    raw.get(i)
                        .ok_or_else(|| "`--request-id` requires a value".to_owned())?
                        .clone(),
                );
            }
            "--as" => {
                i += 1;
                g.act_as = Some(
                    raw.get(i)
                        .ok_or_else(|| "`--as` requires a seat name".to_owned())?
                        .clone(),
                );
            }
            arg => out.push(arg.to_owned()),
        }
        i += 1;
    }
    Ok((g, out))
}

fn handle_skills(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" => {
            let list = skills::list();
            print_value(&list, global.json, |items| {
                items
                    .iter()
                    .map(|s| format!("{}: {}", s.name, s.description))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            Ok(0)
        }
        "get" => {
            let mut skill_name = None;
            let mut reference = None;
            let mut full = false;
            let mut list_refs = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--reference" | "--ref" => {
                        i += 1;
                        reference = Some(
                            args.get(i)
                                .ok_or_else(|| "`--reference` requires a path".to_owned())?
                                .as_str(),
                        );
                    }
                    "--full" => full = true,
                    "--references" => list_refs = true,
                    val if !val.starts_with('-') && skill_name.is_none() => {
                        skill_name = Some(val);
                    }
                    other => return Err(format!("unknown `skills get` option `{other}`")),
                }
                i += 1;
            }
            if list_refs {
                let refs = skills::reference_names();
                print_value(&refs, global.json, |r| r.join("\n"));
                return Ok(0);
            }
            let doc = skills::get(skill_name, reference, full)?;
            print_value(&doc, global.json, |d| d.content.clone());
            Ok(0)
        }
        "install" | "update" => {
            let mut custom_dir = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--path" | "--dir" => {
                        i += 1;
                        custom_dir = Some(PathBuf::from(
                            args.get(i)
                                .ok_or_else(|| "`--path` requires a directory".to_owned())?,
                        ));
                    }
                    other => return Err(format!("unknown `skills install` option `{other}`")),
                }
                i += 1;
            }
            let installed = skills::install(custom_dir.as_deref())
                .map_err(|e| format!("failed to install discovery stub: {e}"))?;
            print_value(&installed, global.json, |res| {
                format!(
                    "Installed `{}` discovery stub to:\n  {}",
                    res.skill,
                    res.installed_paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n  ")
                )
            });
            Ok(0)
        }
        other => Err(format!(
            "unknown `skills` subcommand `{other}`; expected `list`, `get`, or `install`"
        )),
    }
}

fn handle_mod(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("install");
    match sub {
        "install" => {
            let mut custom_dir = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--path" | "--dir" => {
                        i += 1;
                        custom_dir = Some(PathBuf::from(
                            args.get(i)
                                .ok_or_else(|| "`--path` requires a directory".to_owned())?,
                        ));
                    }
                    other => return Err(format!("unknown `mod install` option `{other}`")),
                }
                i += 1;
            }
            let installed = mod_install::install(custom_dir.as_deref())
                .map_err(|e| format!("failed to install Herdr app mod: {e}"))?;
            print_value(&installed, global.json, |m| {
                let mut out = format!(
                    "Installed Herdr app mod `{}` to `{}`\nCheck it with: herdr-app check {}",
                    m.name,
                    m.mod_dir.display(),
                    m.mod_dir.display()
                );
                if let Some(old) = &m.removed_legacy {
                    out.push_str(&format!("\nRemoved retired Luau mod `{}`", old.display()));
                }
                out
            });
            Ok(0)
        }
        other => Err(format!(
            "unknown `mod` subcommand `{other}`; expected `install`"
        )),
    }
}

fn handle_daemon(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let state_dir = resolve_state_dir();
    let sub = args.first().map(String::as_str).unwrap_or("status");
    match sub {
        "status" => {
            let st = daemon::status(&state_dir);
            print_value(&st, global.json, |s| {
                format!(
                    "daemon running={} pid={:?} state_dir={}",
                    s.running,
                    s.pid,
                    s.state_dir.display()
                )
            });
            Ok(0)
        }
        "start" => {
            let st = daemon::start_detached(&state_dir)?;
            print_value(&st, global.json, |s| {
                format!("daemon started pid={:?}", s.pid)
            });
            Ok(0)
        }
        "stop" => {
            let st = daemon::stop(&state_dir)?;
            print_value(&st, global.json, |_| "daemon stopped".to_owned());
            Ok(0)
        }
        "run" => {
            let once = args.iter().any(|a| a == "--once");
            daemon::run_foreground(once)?;
            Ok(0)
        }
        other => Err(format!(
            "unknown `daemon` subcommand `{other}`; expected `start`, `stop`, `status`, or `run`"
        )),
    }
}

fn handle_init(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let cwd = env::current_dir().map_err(|e| format!("failed to read current dir: {e}"))?;
    let mut workspace_root = cwd;
    let mut name = None;
    let mut template = None;
    let mut goal = None;
    let mut symlinks = Vec::new();
    let mut copies = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                i += 1;
                name = Some(req_val(args, i, "--name")?.to_owned());
            }
            "--template" => {
                i += 1;
                let raw = req_val(args, i, "--template")?;
                template = Some(TemplateKind::from_str(raw).map_err(|e| e.to_string())?);
            }
            "--goal" | "--objective" => {
                i += 1;
                goal = Some(req_val(args, i, "--goal")?.to_owned());
            }
            "--workspace-root" => {
                i += 1;
                workspace_root = PathBuf::from(req_val(args, i, "--workspace-root")?);
            }
            "--bootstrap-symlink" => {
                i += 1;
                symlinks.push(req_val(args, i, "--bootstrap-symlink")?.to_owned());
            }
            other => return Err(format!("unknown `init` option `{other}`")),
        }
        i += 1;
    }

    let cfg = worktree::load_project_config(&workspace_root)?;
    let resolved_name = name
        .or(cfg.team.name)
        .or_else(|| {
            workspace_root
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "default".to_owned());
    let resolved_template = match template {
        Some(t) => Some(t),
        None => match cfg.team.template {
            Some(ref s) => Some(TemplateKind::from_str(s).map_err(|e| e.to_string())?),
            None => None,
        },
    };
    let resolved_goal = goal.or(cfg.team.goal);
    if symlinks.is_empty() {
        symlinks = cfg.team.bootstrap_symlinks;
    }
    if copies.is_empty() {
        copies = cfg.team.bootstrap_copies;
    }

    if env::var_os("ORCHERD_NO_AUTO_DAEMON").is_none() {
        let state_dir = resolve_state_dir();
        let _ = daemon::start_detached(&state_dir);
    }

    execute_op(
        Op::Init {
            name: resolved_name,
            template: resolved_template,
            goal: resolved_goal,
            workspace_root: workspace_root.display().to_string(),
            bootstrap_symlinks: symlinks,
            bootstrap_copies: copies,
        },
        global,
    )
}

pub fn run_op_raw(op: Op, global: &GlobalFlags) -> Result<Receipt, String> {
    // Creating a team is always local to the machine where `orcherd init` runs; routing it
    // through an inbound tunnel would create a phantom team in the remote host's store.
    let allow_tunnel = !matches!(op, Op::Init { .. });
    let mut envelope = RequestEnvelope::new(op);
    if let Some(ref r) = global.request_id {
        envelope = envelope.with_request_id(r);
    }

    let state_dir = resolve_state_dir();
    let db_path = resolve_db_path(&state_dir);

    let remote_sock = if allow_tunnel {
        env::var("ORCHERD_REMOTE_SOCKET")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| remote::tunnel_socket_without_local_team(&state_dir, &db_path))
    } else {
        None
    };
    if let Some(sock) = remote_sock {
        let team_ref = resolve_team_ref(global.team.as_deref(), None)?;
        if let Some(ref t) = team_ref {
            envelope = envelope.with_team(t);
        }
        let actor = resolve_actor(None, team_ref.as_deref(), global.act_as.as_deref());
        let machine = env::var("ORCHERD_MACHINE").ok();
        return remote::send_remote_request(&sock, &actor, envelope, machine);
    }

    let mut store =
        Store::open(&db_path).map_err(|e| format!("failed to open `{}`: {e}", db_path.display()))?;
    let team_ref = resolve_team_ref(global.team.as_deref(), Some(&store))?;
    if let Some(ref t) = team_ref {
        envelope = envelope.with_team(t);
    }
    let actor = resolve_actor(Some(&store), team_ref.as_deref(), global.act_as.as_deref());
    store
        .apply(&actor, envelope)
        .map_err(|e| annotate_team_not_found(e.to_string(), &state_dir))
}

pub(crate) fn execute_op(op: Op, global: &GlobalFlags) -> Result<i32, String> {
    let receipt = run_op_raw(op, global)?;
    outln!("{}", format_receipt(&receipt, global.json));
    Ok(0)
}

pub(crate) fn req_val<'a>(args: &'a [String], idx: usize, flag: &str) -> Result<&'a str, String> {
    args.get(idx)
        .map(String::as_str)
        .ok_or_else(|| format!("`{flag}` requires a value"))
}

pub(crate) fn read_optional_file_or_inline(
    file: Option<&Path>,
    inline: Option<String>,
    flag: &str,
) -> Result<Option<String>, String> {
    if let Some(path) = file {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("failed to read `{flag}` `{}`: {e}", path.display()))?;
        return Ok(Some(content));
    }
    Ok(inline)
}

pub(crate) fn read_required_file_or_inline(
    file: Option<&Path>,
    inline: Option<String>,
    flag: &str,
) -> Result<String, String> {
    read_optional_file_or_inline(file, inline, flag)?
        .ok_or_else(|| format!("missing required content; pass `{flag} <path>`"))
}

pub(crate) fn parse_list_arg(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('[')
        && let Ok(parsed) = serde_json::from_str::<Vec<String>>(trimmed)
    {
        return parsed;
    }
    trimmed
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
