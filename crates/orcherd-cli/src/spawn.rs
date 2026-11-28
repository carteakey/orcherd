//! `orcherd spawn` handler: worktree preparation, warm reuse, and `herdr-core` launch.

use crate::{
    agent_argv, agy_trust,
    cli::{GlobalFlags, read_optional_file_or_inline, req_val},
    daemon, herdr_space,
    identity::{
        annotate_team_not_found, build_seat_env, default_state_dir, resolve_actor,
        resolve_db_path, resolve_state_dir, resolve_team_ref,
    },
    model_check,
    output::format_receipt,
    spawn_replay, tunnel,
    worktree::{self, WorktreeSpec},
};
use orcherd_core::{Address, Op, ReceiptData, RequestEnvelope, SeatRole, Store};
use orcherd_hc::{
    Core, Endpoint, PROGRAM as HC_PROGRAM,
    agent::{self, Until},
    reply::agent_in_front,
    start as start_hc,
    wire::Run,
};
use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    str::FromStr,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

fn connect_or_start_core() -> Result<Core, String> {
    let ep = Endpoint::resolve_env().map_err(|e| format!("failed to resolve herdr-core endpoint: {e}"))?;
    if let Ok(core) = Core::connect(&ep) {
        return Ok(core);
    }
    let binary = find_hc_binary().ok_or_else(|| {
        format!(
            "`{HC_PROGRAM}` (`hc`) is not running at `{}` and could not be found on PATH",
            ep.socket.display()
        )
    })?;
    start_hc(&binary, |_| {}, Duration::from_secs(10))
        .map_err(|e| format!("failed to start `{}`: {e}", binary.display()))?;
    Core::connect(&ep).map_err(|e| format!("failed to connect to herdr-core after start: {e}"))
}

fn find_hc_binary() -> Option<PathBuf> {
    if let Some(p) = env::var_os("HC_BINARY").filter(|v| !v.is_empty()) {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            for candidate in [HC_PROGRAM, "hc"] {
                let full = dir.join(candidate);
                if full.is_file() {
                    return Some(full);
                }
            }
        }
    }
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        for candidate in [
            home.join(".local/bin").join(HC_PROGRAM),
            home.join(".local/bin/hc"),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub(crate) fn handle_spawn(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut seat = None;
    let mut role = None;
    let mut agent = None;
    let mut model = None;
    let mut account = None;
    let mut effort = None;
    let mut worktree_arg = None;
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
    let mut no_ui = false;

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
                worktree_arg = Some(req_val(args, i, "--worktree")?.to_owned());
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
            "--no-ui" => no_ui = true,
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
    let team_ref = resolve_team_ref(global.team.as_deref(), Some(&store))?;
    let team = store
        .resolve_team(team_ref.as_deref())
        .map_err(|e| annotate_team_not_found(e.to_string(), &state_dir))?;
    let project_cfg = worktree::load_project_config(Path::new(&team.workspace_root))?;

    // Prepare a worktree only when `--worktree` was passed explicitly; omitting it lets
    // `SpawnPrepare` keep an existing seat's recorded `worktree_path` (defaulting new seats to `current`).
    let prepared_wt = match worktree_arg.as_deref() {
        Some(raw) => {
            let wt_spec = WorktreeSpec::parse(raw, Some(&seat_name))?;
            let prepared = worktree::prepare(
                Path::new(&team.workspace_root),
                &wt_spec,
                &seat_name,
                base_branch.as_deref(),
                &team.bootstrap_symlinks,
                &team.bootstrap_copies,
            )?;
            Some((wt_spec, prepared))
        }
        None => None,
    };

    let actor = resolve_actor(Some(&store), Some(&team.team_id), global.act_as.as_deref());

    if !no_launch && env::var_os("ORCHERD_NO_AUTO_DAEMON").is_none() {
        let _ = daemon::start_detached(&state_dir);
    }

    let core_res = if no_launch || explicit_terminal.is_some() {
        Err("skipped".to_owned())
    } else {
        connect_or_start_core()
    };

    // Before SpawnPrepare decides warm_reuse, verify against live herdr-core that any
    // bound terminal is still alive and still has the expected agent in front (SPEC.md §3.2).
    if let Ok(ref core) = core_res
        && let Ok(proj) = store.team_projection(&team.team_id)
        && let Some(existing) = proj.seats.iter().find(|s| s.name == seat_name)
        && let Address::Core {
            machine: ref m,
            terminal: ref t,
        } = existing.address
    {
        let target_addr = match m {
            Some(mach) if !t.contains('/') => format!("{mach}/{t}"),
            _ => t.clone(),
        };
        let expected_agent = agent.as_deref().unwrap_or(&existing.agent);
        let still_alive = core.list(false).ok().is_some_and(|list| {
            list.terminals
                .iter()
                .find(|r| r.address == target_addr || r.address == *t)
                .is_some_and(|rec| {
                    rec.foreground.is_none() || agent_in_front(rec, expected_agent)
                })
        });
        if !still_alive {
            let _ = store.apply(
                &actor,
                RequestEnvelope::new(Op::SeatRebind {
                    seat: seat_name.clone(),
                    address: Address::None,
                })
                .with_team(&team.team_id),
            );
        }
    }

    let mut env_req = RequestEnvelope::new(Op::SpawnPrepare {
        seat: seat_name.clone(),
        role,
        agent: agent.clone(),
        model: model.clone(),
        account,
        effort,
        worktree_path: prepared_wt
            .as_ref()
            .map(|(_, wt)| wt.path.display().to_string()),
        branch: prepared_wt.as_ref().and_then(|(_, wt)| wt.branch.clone()),
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

    let (mut receipt, replayed) = store.apply_with_replay(&actor, env_req).map_err(|e| e.to_string())?;
    let mut spawn_model_check = None;
    let mut mismatch_dispatch = None;
    let mut trust_note = None;

    if let ReceiptData::SpawnPrepared {
        ref seat,
        ref dispatch,
        warm_reuse,
        ..
    } = receipt.data
    {
        let dispatch_id = dispatch.as_ref().map(|d| d.dispatch_id.clone());
        let seat_name_cloned = seat.name.clone();
        let seat_agent_cloned = seat.agent.clone();
        let seat_model_cloned = seat.model.clone();
        let seat_role_str = seat.role.to_string();
        let seat_keep_open = seat.keep_open;
        let launch_cwd = prepared_wt
            .as_ref()
            .map(|(_, wt)| wt.path.display().to_string())
            .or_else(|| seat.worktree_path.clone())
            .unwrap_or_else(|| team.workspace_root.clone());
        let prepared_branch = prepared_wt.as_ref().and_then(|(_, wt)| wt.branch.clone());
        let seat_bound_now = spawn_replay::seat_is_bound(&store, &team.team_id, &seat_name_cloned);

        if let Some(term) = explicit_terminal {
            if !replayed || !seat_bound_now {
                store
                    .apply(
                        &actor,
                        RequestEnvelope::new(Op::SpawnBindAddress {
                            seat: seat_name,
                            dispatch_id,
                            address: Address::Core {
                                machine,
                                terminal: term,
                            },
                            worktree_path: Some(launch_cwd),
                            branch: prepared_branch,
                            resume_session,
                        })
                        .with_team(&team.team_id),
                    )
                    .map_err(|e| e.to_string())?;
            }
        } else if warm_reuse || (replayed && seat_bound_now) {
            if let Ok(ref core) = core_res {
                let _ = daemon::ring_seat_if_no_daemon(
                    &mut store,
                    &state_dir,
                    core,
                    &team.team_id,
                    &seat_name_cloned,
                );
            }
        } else if spawn_replay::should_cold_launch(replayed, warm_reuse, seat_bound_now, resume_session.is_some(), no_launch) {
            if let Some(ref m) = machine {
                let _ = tunnel::ensure_background_tunnel(&state_dir, m);
            }
            match core_res {
                Ok(core) => {
                    let exe = env::current_exe()
                        .ok()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "orcherd".to_owned());
                    let seat_env = build_seat_env(
                        &exe,
                        &team.team_id,
                        &seat_name_cloned,
                        dispatch_id.as_deref(),
                        machine.as_deref(),
                        &state_dir,
                        &default_state_dir(),
                    );
                    let env_map: BTreeMap<&str, &str> =
                        seat_env.iter().map(|(&k, v)| (k, v.as_str())).collect();

                    let mut labels = BTreeMap::new();
                    labels.insert("orcherd.team", team.team_id.as_str());
                    labels.insert("orcherd.seat", seat_name_cloned.as_str());
                    labels.insert("orcherd.role", seat_role_str.as_str());
                    let space = (!no_ui)
                        .then(|| herdr_space::team_space(&team.team_id, machine.as_deref()))
                        .flatten();
                    if let Some(ref space) = space {
                        labels.insert(herdr_space::SPACE_LABEL, space.as_str());
                    }

                    trust_note = prepared_wt.as_ref().and_then(|(wt_spec, wt)| {
                        agy_trust::maybe_inherit_worktree_trust(
                            &seat_agent_cloned,
                            wt_spec,
                            Path::new(&team.workspace_root),
                            &wt.path,
                            machine.as_deref(),
                        )
                    });
                    let argv = agent_argv::build_agent_argv(
                        &seat_agent_cloned,
                        seat_model_cloned.as_deref(),
                        resume_session.as_deref(),
                        &project_cfg.agents,
                    );
                    let run_req = Run {
                        argv: &argv,
                        cwd: Some(&launch_cwd),
                        env: env_map,
                        keep: seat_keep_open,
                        labels,
                        name: Some(&seat_name_cloned),
                        on: machine.as_deref(),
                    };
                    match core.run(&run_req) {
                        Ok(terminal) => {
                            let stop_flag = AtomicBool::new(false);
                            let until = Until {
                                deadline: Instant::now() + Duration::from_secs(3),
                                stop: &stop_flag,
                            };
                            let _ = agent::ready(&core, &terminal.address, until);
                            let check = seat_model_cloned
                                .as_deref()
                                .filter(|m| !m.is_empty())
                                .map(|requested| {
                                    model_check::verify_launched_model(
                                        &core,
                                        &terminal.address,
                                        &seat_agent_cloned,
                                        requested,
                                    )
                                });
                            let mismatch = check.as_ref().is_some_and(|c| {
                                c.status == orcherd_core::ModelCheckStatus::Mismatch
                            });
                            spawn_model_check =
                                check.map(|c| (terminal.address.clone(), seat_agent_cloned.clone(), c));
                            if mismatch {
                                let _ = core.kill(&terminal.address);
                                mismatch_dispatch = dispatch_id;
                            } else {
                                let bound = store.apply(
                                    &actor,
                                    RequestEnvelope::new(Op::SpawnBindAddress {
                                        seat: seat_name_cloned.clone(),
                                        dispatch_id,
                                        address: Address::Core {
                                            machine,
                                            terminal: terminal.address.clone(),
                                        },
                                        worktree_path: Some(launch_cwd),
                                        branch: prepared_branch,
                                        resume_session,
                                    })
                                    .with_team(&team.team_id),
                                );
                                if let Err(err) = bound {
                                    receipt.failed_stage = Some("bind_address".to_owned());
                                    receipt.next = Some(format!(
                                        "terminal {} started but couldn't be recorded ({err}); bind it with `orcherd spawn --seat {seat_name_cloned} --terminal {}`",
                                        terminal.address, terminal.address
                                    ));
                                }
                                let _ = daemon::ring_seat_if_no_daemon(
                                    &mut store,
                                    &state_dir,
                                    &core,
                                    &team.team_id,
                                    &seat_name_cloned,
                                );
                            }
                        }
                        Err(err) => {
                            receipt.failed_stage = Some("terminal_run".to_owned());
                            receipt.next = Some(format!(
                                "herdr-core terminal.run failed ({err}); check `{seat_agent_cloned}` on PATH and retry with `orcherd spawn --seat {seat_name_cloned}`"
                            ));
                        }
                    }
                }
                Err(err) => {
                    receipt.failed_stage = Some("herdr_core_connect".to_owned());
                    receipt.next = Some(format!(
                        "herdr-core unavailable ({err}); start `hc server start` and rerun `orcherd spawn --seat {seat_name_cloned}`"
                    ));
                }
            }
        }
    }

    if let Some((term_addr, seat_agent, check)) = spawn_model_check
        && model_check::apply_model_check(&mut receipt, &term_addr, &seat_agent, check)
    {
        model_check::cleanup_model_mismatch(
            &mut store,
            &actor,
            &team.team_id,
            mismatch_dispatch.as_deref(),
            &mut receipt,
        );
    }

    // The receipt is printed only after the terminal is bound, and always carries the seat
    // as stored now, so scripts never read a pre-bind `address=none`.
    if let ReceiptData::SpawnPrepared { seat: ref mut r_seat, .. } = receipt.data
        && let Ok(updated) = store.apply(
            &actor,
            RequestEnvelope::new(Op::SeatShow { seat: r_seat.name.clone() }).with_team(&team.team_id),
        )
        && let ReceiptData::Seat(updated_seat) = updated.data
    {
        *r_seat = updated_seat;
    }
    agy_trust::append_note(&mut receipt, trust_note);
    if let Some(warning) = prepared_wt.and_then(|(_, wt)| wt.warning) {
        receipt.next = Some(match receipt.next.take() {
            Some(existing) if !existing.trim().is_empty() => format!("{warning}. {existing}"),
            _ => warning,
        });
    }
    outln!("{}", format_receipt(&receipt, global.json));
    Ok(if receipt.failed_stage.is_some() { 1 } else { 0 })
}
