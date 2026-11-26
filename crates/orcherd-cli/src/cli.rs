//! CLI argument parser and command dispatcher (`SPEC.md` §8).

use crate::{
    board::{
        handle_approve, handle_ask, handle_bulletin, handle_done, handle_finding, handle_gate,
        handle_inbox, handle_log, handle_reply, handle_send, handle_task, handle_verdict,
    },
    daemon,
    identity::{resolve_actor, resolve_db_path, resolve_state_dir, resolve_team_arg},
    mod_install,
    output::{format_receipt, print_value},
    remote, skills, tunnel,
    worktree::{self, WorktreeSpec},
};
use orcherd_core::{
    Address, Op, Receipt, ReceiptData, RequestEnvelope, SeatRole, Store, TemplateKind,
};
use orcherd_hc::{Core, Endpoint, wire::Run};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

pub const HELP_TEXT: &str = "\
orcherd — Multi-agent team orchestration on herdr-core (hc)

Usage: orcherd <command> [options]

Setup & Skills:
  init        Initialize a team (--name, --template review-crew|studio-crew|small, --goal)
  status      Show team roster, tasks, dispatches, and pending gates
  skills      Manage bundled progressive skills (list | get [orcherd] [--reference <ref>] | install)
  mod         Install declarative Herdr UI mod (mod install [--path <mods_dir>])
  daemon      Manage doorbell & escalation daemon (start | stop | status | run [--once])
  tunnel      Start reverse SSH socket tunnel for a remote machine (tunnel <machine>)

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
            println!(
                "{{\"name\":\"orcherd\",\"version\":\"{}\"}}",
                env!("CARGO_PKG_VERSION")
            );
        } else {
            println!("orcherd {}", env!("CARGO_PKG_VERSION"));
        }
        return Ok(0);
    }
    if global.help || args.is_empty() {
        print!("{HELP_TEXT}");
        return Ok(0);
    }

    let cmd = args[0].as_str();
    let rest = &args[1..];

    match cmd {
        "skills" | "skill" => handle_skills(rest, &global),
        "mod" => handle_mod(rest, &global),
        "daemon" => handle_daemon(rest, &global),
        "tunnel" => handle_tunnel(rest, &global),
        "init" => handle_init(rest, &global),
        "status" => execute_op(Op::Status, &global),
        "spawn" | "worker-start" => handle_spawn(rest, &global),
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
                .map_err(|e| format!("failed to install Herdr UI mod: {e}"))?;
            print_value(&installed, global.json, |m| {
                format!(
                    "Installed Herdr UI mod `{}` to `{}`",
                    m.name,
                    m.mod_dir.display()
                )
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

fn handle_tunnel(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let machine = args
        .first()
        .ok_or_else(|| "`orcherd tunnel <machine>` requires a machine name".to_owned())?;
    let mut remote_socket = None;
    let mut ssh_args = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--remote-socket" => {
                i += 1;
                remote_socket = Some(
                    args.get(i)
                        .ok_or_else(|| "`--remote-socket` requires a path".to_owned())?
                        .as_str(),
                );
            }
            "--ssh-arg" => {
                i += 1;
                ssh_args.push(
                    args.get(i)
                        .ok_or_else(|| "`--ssh-arg` requires an argument".to_owned())?
                        .clone(),
                );
            }
            other => return Err(format!("unknown `tunnel` option `{other}`")),
        }
        i += 1;
    }
    let state_dir = resolve_state_dir();
    let st = tunnel::spawn_background_tunnel(&state_dir, machine, remote_socket, &ssh_args)?;
    print_value(&st, global.json, |s| {
        format!(
            "Tunnel active for `{}` (pid {:?}): {} -> {}",
            s.machine,
            s.pid,
            s.remote_socket,
            s.local_socket.display()
        )
    });
    Ok(0)
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

fn handle_spawn(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut seat = None;
    let mut role = None;
    let mut agent = None;
    let mut model = None;
    let mut account = None;
    let mut effort = None;
    let mut worktree_arg = "current".to_owned();
    let mut base_branch = None;
    let mut machine = None;
    let mut task_id = None;
    let mut spec = None;
    let mut spec_file = None;
    let mut title = None;
    let mut keep_open = None;
    let mut resume_session = None;
    let mut retry_of = None;
    let mut explicit_terminal = None;
    let mut no_launch = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seat" | "--name" => {
                i += 1;
                seat = Some(req_val(args, i, "--seat")?.to_owned());
            }
            "--role" => {
                i += 1;
                role = Some(SeatRole::from_str(req_val(args, i, "--role")?).map_err(|e| e.to_string())?);
            }
            "--agent" => {
                i += 1;
                agent = Some(req_val(args, i, "--agent")?.to_owned());
            }
            "--model" => {
                i += 1;
                model = Some(req_val(args, i, "--model")?.to_owned());
            }
            "--account" => {
                i += 1;
                account = Some(req_val(args, i, "--account")?.to_owned());
            }
            "--effort" => {
                i += 1;
                effort = Some(req_val(args, i, "--effort")?.to_owned());
            }
            "--worktree" => {
                i += 1;
                worktree_arg = req_val(args, i, "--worktree")?.to_owned();
            }
            "--base-branch" => {
                i += 1;
                base_branch = Some(req_val(args, i, "--base-branch")?.to_owned());
            }
            "--on" | "--machine" => {
                i += 1;
                machine = Some(req_val(args, i, "--on")?.to_owned());
            }
            "--task" => {
                i += 1;
                task_id = Some(req_val(args, i, "--task")?.to_owned());
            }
            "--spec-file" => {
                i += 1;
                spec_file = Some(PathBuf::from(req_val(args, i, "--spec-file")?));
            }
            "--spec" => {
                i += 1;
                spec = Some(req_val(args, i, "--spec")?.to_owned());
            }
            "--title" | "--task-title" => {
                i += 1;
                title = Some(req_val(args, i, "--title")?.to_owned());
            }
            "--keep-open" => keep_open = Some(true),
            "--resume" => {
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    i += 1;
                    resume_session = Some(args[i].clone());
                } else {
                    resume_session = Some("latest".to_owned());
                }
            }
            "--retry-of" => {
                i += 1;
                retry_of = Some(req_val(args, i, "--retry-of")?.to_owned());
            }
            "--terminal" => {
                i += 1;
                explicit_terminal = Some(req_val(args, i, "--terminal")?.to_owned());
            }
            "--no-launch" => no_launch = true,
            "--environment" => {
                i += 1;
                let env_val = req_val(args, i, "--environment")?;
                if env_val != "local" && machine.is_none() {
                    machine = Some(env_val.to_owned());
                }
            }
            other => return Err(format!("unknown `spawn` option `{other}`")),
        }
        i += 1;
    }

    let seat_name = seat.ok_or_else(|| "`orcherd spawn` requires `--seat <name>`".to_owned())?;
    let resolved_spec = read_optional_file_or_inline(spec_file.as_deref(), spec, "--spec-file")?;

    let state_dir = resolve_state_dir();
    let db_path = resolve_db_path(&state_dir);
    let mut store =
        Store::open(&db_path).map_err(|e| format!("failed to open `{}`: {e}", db_path.display()))?;
    let team_ref = resolve_team_arg(global.team.as_deref());
    let team = store
        .resolve_team(team_ref.as_deref())
        .map_err(|e| e.to_string())?;

    let wt_spec = WorktreeSpec::parse(&worktree_arg, Some(&seat_name))?;
    let prepared_wt = worktree::prepare(
        Path::new(&team.workspace_root),
        &wt_spec,
        &seat_name,
        base_branch.as_deref(),
        &team.bootstrap_symlinks,
        &team.bootstrap_copies,
    )?;

    let actor = resolve_actor(Some(&store), Some(&team.team_id), global.act_as.as_deref());
    let mut env_req = RequestEnvelope::new(Op::SpawnPrepare {
        seat: seat_name.clone(),
        role,
        agent: agent.clone(),
        model: model.clone(),
        account,
        effort,
        worktree_path: Some(prepared_wt.path.display().to_string()),
        branch: prepared_wt.branch.clone(),
        machine: machine.clone(),
        task_id,
        spec: resolved_spec,
        title,
        keep_open,
        resume_session: resume_session.clone(),
        retry_of,
    })
    .with_team(&team.team_id);
    if let Some(ref req_id) = global.request_id {
        env_req = env_req.with_request_id(req_id);
    }

    let mut receipt = store.apply(&actor, env_req).map_err(|e| e.to_string())?;

    if let ReceiptData::SpawnPrepared {
        ref seat,
        ref dispatch,
        warm_reuse,
        ..
    } = receipt.data
    {
        let dispatch_id = dispatch.as_ref().map(|d| d.dispatch_id.clone());
        if let Some(term) = explicit_terminal {
            let bind_receipt = store
                .apply(
                    &actor,
                    RequestEnvelope::new(Op::SpawnBindAddress {
                        seat: seat_name,
                        dispatch_id,
                        address: Address::Core {
                            machine,
                            terminal: term,
                        },
                        worktree_path: Some(prepared_wt.path.display().to_string()),
                        branch: prepared_wt.branch,
                        resume_session,
                    })
                    .with_team(&team.team_id),
                )
                .map_err(|e| e.to_string())?;
            let _ = bind_receipt;
        } else if !warm_reuse
            && !no_launch
            && let Ok(ep) = Endpoint::resolve_env()
            && let Ok(core) = Core::connect(&ep)
        {
            let exe = env::current_exe()
                .ok()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "orcherd".to_owned());
            let mut env_map = BTreeMap::new();
            env_map.insert("ORCHERD_CLI_COMMAND", exe.as_str());
            env_map.insert("ORCHERD_TEAM", team.team_id.as_str());
            env_map.insert("ORCHERD_SEAT", seat.name.as_str());
            if let Some(ref d_id) = dispatch_id {
                env_map.insert("ORCHERD_DISPATCH", d_id.as_str());
            }
            if let Some(ref m) = machine {
                env_map.insert("ORCHERD_MACHINE", m.as_str());
                env_map.insert(
                    "ORCHERD_REMOTE_SOCKET",
                    tunnel::default_remote_socket_expr(),
                );
            }

            let role_str = seat.role.to_string();
            let mut labels = BTreeMap::new();
            labels.insert("orcherd.team", team.team_id.as_str());
            labels.insert("orcherd.seat", seat.name.as_str());
            labels.insert("orcherd.role", role_str.as_str());

            let argv = build_agent_argv(&seat.agent, seat.model.as_deref(), resume_session.as_deref());
            let cwd_str = prepared_wt.path.display().to_string();
            let run_req = Run {
                argv: &argv,
                cwd: Some(&cwd_str),
                env: env_map,
                keep: seat.keep_open,
                labels,
                name: Some(&seat.name),
                on: machine.as_deref(),
            };
            if let Ok(terminal) = core.run(&run_req) {
                let _ = store.apply(
                    &actor,
                    RequestEnvelope::new(Op::SpawnBindAddress {
                        seat: seat.name.clone(),
                        dispatch_id,
                        address: Address::Core {
                            machine,
                            terminal: terminal.address.clone(),
                        },
                        worktree_path: Some(cwd_str),
                        branch: prepared_wt.branch,
                        resume_session,
                    })
                    .with_team(&team.team_id),
                );
                let _ = daemon::tick_once(&mut store, Some(&core));
                if let Ok(updated) = store.apply(
                    &actor,
                    RequestEnvelope::new(Op::SeatShow {
                        seat: seat.name.clone(),
                    })
                    .with_team(&team.team_id),
                ) && let ReceiptData::Seat(updated_seat) = updated.data
                    && let ReceiptData::SpawnPrepared {
                        seat: ref mut r_seat,
                        ..
                    } = receipt.data
                {
                    *r_seat = updated_seat;
                }
            }
        }
    }

    println!("{}", format_receipt(&receipt, global.json));
    Ok(0)
}

fn build_agent_argv(agent: &str, model: Option<&str>, resume: Option<&str>) -> Vec<String> {
    let mut argv = vec![agent.to_owned()];
    if let Some(m) = model.filter(|s| !s.is_empty()) {
        argv.push("--model".to_owned());
        argv.push(m.to_owned());
    }
    if let Some(r) = resume.filter(|s| !s.is_empty()) {
        argv.push("--resume".to_owned());
        if r != "latest" {
            argv.push(r.to_owned());
        }
    }
    argv
}

fn handle_seat(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
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
            execute_op(
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
            execute_op(Op::SeatRelease { dispatch_id }, global)
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
        other => Err(format!("unknown `seat` subcommand `{other}`")),
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

pub fn run_op_raw(op: Op, global: &GlobalFlags) -> Result<Receipt, String> {
    let team_ref = resolve_team_arg(global.team.as_deref());
    let mut envelope = RequestEnvelope::new(op);
    if let Some(ref t) = team_ref {
        envelope = envelope.with_team(t);
    }
    if let Some(ref r) = global.request_id {
        envelope = envelope.with_request_id(r);
    }

    if let Ok(remote_sock) = env::var("ORCHERD_REMOTE_SOCKET") {
        let trimmed = remote_sock.trim();
        if !trimmed.is_empty() {
            let actor = resolve_actor(None, team_ref.as_deref(), global.act_as.as_deref());
            let machine = env::var("ORCHERD_MACHINE").ok();
            return remote::send_remote_request(Path::new(trimmed), &actor, envelope, machine);
        }
    }

    let state_dir = resolve_state_dir();
    let db_path = resolve_db_path(&state_dir);
    let mut store =
        Store::open(&db_path).map_err(|e| format!("failed to open `{}`: {e}", db_path.display()))?;
    let actor = resolve_actor(Some(&store), team_ref.as_deref(), global.act_as.as_deref());
    store.apply(&actor, envelope).map_err(|e| e.to_string())
}

pub(crate) fn execute_op(op: Op, global: &GlobalFlags) -> Result<i32, String> {
    let receipt = run_op_raw(op, global)?;
    println!("{}", format_receipt(&receipt, global.json));
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
