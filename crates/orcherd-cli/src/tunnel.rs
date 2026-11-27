//! Per-machine reverse SSH Unix-socket tunnel (`orcherd tunnel <machine>`).
//!
//! Forwards `<remote_state_dir>/orcherd/control.sock` on `<machine>` back to
//! `<local_state_dir>/orcherd/remote/<machine>.sock` on the supervisor host so
//! `orcherd daemon` knows which machine an inbound remote request came from.

use crate::{
    cli::GlobalFlags,
    daemon::{self, process_is_alive, terminate_process},
    identity::resolve_state_dir,
    output::print_value,
};
use orcherd_hc::new_command;
use serde::{Deserialize, Serialize};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Child, Stdio},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TunnelStatus {
    pub local_socket: PathBuf,
    pub machine: String,
    pub pid: Option<u32>,
    pub remote_socket: String,
}

/// On-disk record under `<state_dir>/tunnels/<machine>.json` so `orcherd daemon`
/// knows which tunnels to watch and restart if SSH drops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelRecord {
    pub machine: String,
    #[serde(default)]
    pub remote_socket_override: Option<String>,
    #[serde(default)]
    pub ssh_args: Vec<String>,
    #[serde(default)]
    pub pid: Option<u32>,
}

/// Validates a machine name (`m1`, `fx505`, `yeti-cachy`) so it cannot escape
/// `<state_dir>/orcherd/remote/<machine>.sock`.
pub fn validate_machine_name(machine: &str) -> Result<&str, String> {
    let trimmed = machine.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!("invalid machine name `{machine}`"));
    }
    Ok(trimmed)
}

pub fn local_machine_socket(state_dir: &Path, machine: &str) -> Result<PathBuf, String> {
    let valid = validate_machine_name(machine)?;
    Ok(state_dir.join("remote").join(format!("{valid}.sock")))
}

pub fn tunnels_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("tunnels")
}

pub fn tunnel_record_path(state_dir: &Path, machine: &str) -> Result<PathBuf, String> {
    let valid = validate_machine_name(machine)?;
    Ok(tunnels_dir(state_dir).join(format!("{valid}.json")))
}

pub fn read_tunnel_record(state_dir: &Path, machine: &str) -> Result<Option<TunnelRecord>, String> {
    let path = tunnel_record_path(state_dir, machine)?;
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("failed to read `{}`: {err}", path.display())),
    };
    let rec: TunnelRecord = serde_json::from_str(&content)
        .map_err(|e| format!("failed to parse `{}`: {e}", path.display()))?;
    Ok(Some(rec))
}

pub fn write_tunnel_record(state_dir: &Path, record: &TunnelRecord) -> Result<(), String> {
    let path = tunnel_record_path(state_dir, &record.machine)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create `{}`: {e}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(record)
        .map_err(|e| format!("failed to encode tunnel record: {e}"))?;
    fs::write(&path, body).map_err(|e| format!("failed to write `{}`: {e}", path.display()))
}

pub fn list_tunnel_records(state_dir: &Path) -> Vec<TunnelRecord> {
    let dir = tunnels_dir(state_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&path)
            && let Ok(rec) = serde_json::from_str::<TunnelRecord>(&text)
            && validate_machine_name(&rec.machine).is_ok()
        {
            out.push(rec);
        }
    }
    out.sort_by(|a, b| a.machine.cmp(&b.machine));
    out
}

/// Removes a tunnel's record and kills its recorded SSH child so the daemon
/// does not restart it.
pub fn stop_tunnel(state_dir: &Path, machine: &str) -> Result<TunnelStatus, String> {
    let valid = validate_machine_name(machine)?;
    let local_socket = local_machine_socket(state_dir, valid)?;
    let existing = read_tunnel_record(state_dir, valid).ok().flatten();
    if let Some(pid) = existing.as_ref().and_then(|r| r.pid)
        && process_is_alive(pid)
    {
        terminate_process(pid);
    }
    let path = tunnel_record_path(state_dir, valid)?;
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("failed to remove `{}`: {err}", path.display())),
    }
    Ok(TunnelStatus {
        machine: valid.to_owned(),
        local_socket,
        remote_socket: existing
            .and_then(|r| r.remote_socket_override)
            .unwrap_or_else(|| default_remote_socket_expr().to_owned()),
        pid: None,
    })
}

/// Ensures a tunnel to `machine` is running and recorded, reusing a live recorded
/// tunnel (with any custom SSH args) instead of tearing it down mid-session.
pub fn ensure_background_tunnel(state_dir: &Path, machine: &str) -> Result<TunnelStatus, String> {
    let valid = validate_machine_name(machine)?;
    if let Ok(Some(rec)) = read_tunnel_record(state_dir, valid) {
        if let Some(pid) = rec.pid
            && process_is_alive(pid)
        {
            return Ok(TunnelStatus {
                machine: valid.to_owned(),
                local_socket: local_machine_socket(state_dir, valid)?,
                remote_socket: rec
                    .remote_socket_override
                    .unwrap_or_else(|| default_remote_socket_expr().to_owned()),
                pid: Some(pid),
            });
        }
        return spawn_background_tunnel(
            state_dir,
            valid,
            rec.remote_socket_override.as_deref(),
            &rec.ssh_args,
        );
    }
    spawn_background_tunnel(state_dir, valid, None, &[])
}

pub fn default_remote_socket_expr() -> &'static str {
    "${XDG_STATE_HOME:-$HOME/.local/state}/orcherd/control.sock"
}

/// Probes the remote host via `ssh` to resolve its concrete socket path and remove
/// any stale socket file left behind by a prior dropped SSH session.
pub fn preflight_remote_socket(
    machine: &str,
    remote_socket_override: Option<&str>,
    ssh_args: &[String],
) -> Result<String, String> {
    let valid = validate_machine_name(machine)?;
    let expr = remote_socket_override.unwrap_or(default_remote_socket_expr());
    let script = format!(
        "sock=\"{expr}\"; mkdir -p \"$(dirname \"$sock\")\" && rm -f \"$sock\" && printf '%s\\n' \"$sock\""
    );
    let mut cmd = new_command("ssh");
    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
    ]);
    for arg in ssh_args {
        cmd.arg(arg);
    }
    cmd.arg(valid).arg(script);
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run `ssh {valid}` preflight: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ssh preflight to `{valid}` failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    let resolved = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if resolved.is_empty() {
        return Err(format!("ssh preflight to `{valid}` returned an empty socket path"));
    }
    Ok(resolved)
}

/// Spawns a background `ssh -N -R <remote_sock>:<local_sock> <machine>` tunnel process.
pub fn spawn_background_tunnel(
    state_dir: &Path,
    machine: &str,
    remote_socket_override: Option<&str>,
    ssh_args: &[String],
) -> Result<TunnelStatus, String> {
    spawn_background_tunnel_child(state_dir, machine, remote_socket_override, ssh_args)
        .map(|(status, _child)| status)
}

/// Like [`spawn_background_tunnel`], returning the `Child` handle so long-running
/// callers (`orcherd daemon`) can reap it via `try_wait()` instead of leaving zombies.
pub fn spawn_background_tunnel_child(
    state_dir: &Path,
    machine: &str,
    remote_socket_override: Option<&str>,
    ssh_args: &[String],
) -> Result<(TunnelStatus, Child), String> {
    let valid = validate_machine_name(machine)?;
    let local_socket = local_machine_socket(state_dir, valid)?;
    if let Some(parent) = local_socket.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create `{}`: {e}", parent.display()))?;
    }
    // `ssh -R` connects to the local socket only when a request arrives, and the daemon
    // listens only on `remote/*.sock` paths that exist. A placeholder file tells the daemon
    // to bind this machine's socket on its next loop (it replaces the file with a listener).
    if !local_socket.exists() {
        fs::write(&local_socket, b"")
            .map_err(|e| format!("failed to create `{}`: {e}", local_socket.display()))?;
    }
    let remote_socket = preflight_remote_socket(valid, remote_socket_override, ssh_args)?;
    let forward_spec = format!("{remote_socket}:{}", local_socket.display());

    let mut cmd = new_command("ssh");
    cmd.args([
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "StreamLocalBindMask=0177",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-R",
        &forward_spec,
    ]);
    for arg in ssh_args {
        cmd.arg(arg);
    }
    cmd.arg(valid)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Ok(Some(existing)) = read_tunnel_record(state_dir, valid)
        && let Some(old_pid) = existing.pid
        && process_is_alive(old_pid)
    {
        terminate_process(old_pid);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn ssh tunnel to `{valid}`: {e}"))?;
    let pid = child.id();
    let _ = write_tunnel_record(
        state_dir,
        &TunnelRecord {
            machine: valid.to_owned(),
            remote_socket_override: remote_socket_override.map(str::to_owned),
            ssh_args: ssh_args.to_vec(),
            pid: Some(pid),
        },
    );

    Ok((
        TunnelStatus {
            machine: valid.to_owned(),
            local_socket,
            remote_socket,
            pid: Some(pid),
        },
        child,
    ))
}

/// Removes a stale Unix socket path before binding a new listener.
pub fn remove_stale_socket(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// CLI handler for `orcherd tunnel <machine> [--remote-socket ...] [--ssh-arg ...] [--stop]`.
pub fn handle_tunnel(args: &[String], global: &GlobalFlags) -> Result<i32, String> {
    let mut machine: Option<&str> = None;
    let mut remote_socket = None;
    let mut ssh_args = Vec::new();
    let mut stop = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--stop" => stop = true,
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
            other if other.starts_with('-') => {
                return Err(format!("unknown `tunnel` option `{other}`"));
            }
            other if machine.is_none() => machine = Some(other),
            other => return Err(format!("unexpected `tunnel` argument `{other}`")),
        }
        i += 1;
    }
    let machine = machine
        .ok_or_else(|| "`orcherd tunnel <machine>` requires a machine name".to_owned())?;
    let state_dir = resolve_state_dir();
    if stop {
        let st = stop_tunnel(&state_dir, machine)?;
        print_value(&st, global.json, |s| {
            format!("Tunnel stopped for `{}`", s.machine)
        });
        return Ok(0);
    }
    // Requests through the tunnel are answered by this host's daemon.
    if env::var_os("ORCHERD_NO_AUTO_DAEMON").is_none() {
        daemon::start_detached(&state_dir)?;
    }
    let st = spawn_background_tunnel(&state_dir, machine, remote_socket, &ssh_args)?;
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
