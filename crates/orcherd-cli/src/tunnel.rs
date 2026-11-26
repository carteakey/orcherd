//! Per-machine reverse SSH Unix-socket tunnel (`orcherd tunnel <machine>`).
//!
//! Forwards `<remote_state_dir>/orcherd/control.sock` on `<machine>` back to
//! `<local_state_dir>/orcherd/remote/<machine>.sock` on the supervisor host so
//! `orcherd daemon` knows which machine an inbound remote request came from.

use orcherd_hc::new_command;
use serde::Serialize;
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TunnelStatus {
    pub local_socket: PathBuf,
    pub machine: String,
    pub pid: Option<u32>,
    pub remote_socket: String,
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

    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn ssh tunnel to `{valid}`: {e}"))?;

    Ok(TunnelStatus {
        machine: valid.to_owned(),
        local_socket,
        remote_socket,
        pid: Some(child.id()),
    })
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
