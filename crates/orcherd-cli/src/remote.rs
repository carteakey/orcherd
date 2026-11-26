//! Bounded one-request Unix socket transport for remote members (`SPEC.md` §6).
//!
//! Each remote machine `<M>` forwards its `<remote_state>/orcherd/control.sock`
//! over SSH to `<local_state>/orcherd/remote/<M>.sock`. Because requests arrive
//! on `<M>.sock`, the daemon knows which machine the caller belongs to and
//! qualifies bare `HC_TERMINAL_ID=t7` as `<M>/t7`.

use orcherd_core::{Actor, Receipt, RequestEnvelope, Store};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const MAX_REMOTE_FRAME: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteRequest {
    pub actor: Actor,
    pub envelope: RequestEnvelope,
    pub machine: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteResponse {
    pub error: Option<String>,
    pub ok: bool,
    pub receipt: Option<Receipt>,
}

pub fn qualify_actor_for_machine(actor: Actor, machine: Option<&str>) -> Actor {
    let Some(m) = machine.map(str::trim).filter(|s| !s.is_empty()) else {
        return actor;
    };
    let qualify = |term: Option<String>| -> Option<String> {
        term.map(|t| {
            if t.contains('/') {
                t
            } else {
                format!("{m}/{t}")
            }
        })
    };
    match actor {
        Actor::Seat {
            team_id,
            seat_name,
            terminal,
        } => Actor::Seat {
            team_id,
            seat_name,
            terminal: qualify(terminal),
        },
        Actor::Human { terminal } => Actor::Human {
            terminal: qualify(terminal),
        },
    }
}

#[cfg(unix)]
pub fn send_remote_request(
    socket_path: &Path,
    actor: &Actor,
    envelope: RequestEnvelope,
    machine: Option<String>,
) -> Result<Receipt, String> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path).map_err(|e| {
        format!(
            "failed to connect to remote socket `{}`: {e}. The tunnel from the team host is down; \
             rerun `orcherd tunnel <this machine>` there",
            socket_path.display()
        )
    })?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));

    let req = RemoteRequest {
        actor: actor.clone(),
        envelope,
        machine,
    };
    let mut payload =
        serde_json::to_vec(&req).map_err(|e| format!("failed to serialize request: {e}"))?;
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .map_err(|e| format!("failed to send request to `{}`: {e}", socket_path.display()))?;
    stream
        .flush()
        .map_err(|e| format!("failed to flush request: {e}"))?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream.take(MAX_REMOTE_FRAME));
    reader
        .read_line(&mut line)
        .map_err(|e| format!("failed to read reply from `{}`: {e}", socket_path.display()))?;
    if line.trim().is_empty() {
        return Err(format!(
            "no reply through `{}`: the tunnel is up, but no orcherd daemon answered on the team host. \
             On the team host run `orcherd daemon start`, then `orcherd tunnel <this machine>`",
            socket_path.display()
        ));
    }
    let resp: RemoteResponse = serde_json::from_str(line.trim())
        .map_err(|e| format!("invalid JSON reply from `{}`: {e}", socket_path.display()))?;

    if resp.ok {
        resp.receipt
            .ok_or_else(|| "remote server returned ok without receipt".to_owned())
    } else {
        Err(resp
            .error
            .unwrap_or_else(|| "remote request failed".to_owned()))
    }
}

#[cfg(not(unix))]
pub fn send_remote_request(
    socket_path: &Path,
    _actor: &Actor,
    _envelope: RequestEnvelope,
    _machine: Option<String>,
) -> Result<Receipt, String> {
    Err(format!(
        "unix domain socket tunnel (`{}`) is only supported on unix platforms",
        socket_path.display()
    ))
}

#[cfg(unix)]
pub fn handle_client_stream(
    store: &mut Store,
    stream: std::os::unix::net::UnixStream,
    socket_machine: Option<&str>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let mut line = String::new();
    let mut reader = BufReader::new((&stream).take(MAX_REMOTE_FRAME));
    let response = match reader.read_line(&mut line) {
        Ok(0) => return,
        Ok(_) => match serde_json::from_str::<RemoteRequest>(line.trim()) {
            Ok(req) => {
                let machine = socket_machine.or(req.machine.as_deref());
                let actor = qualify_actor_for_machine(req.actor, machine);
                match store.apply(&actor, req.envelope) {
                    Ok(receipt) => RemoteResponse {
                        ok: true,
                        receipt: Some(receipt),
                        error: None,
                    },
                    Err(err) => RemoteResponse {
                        ok: false,
                        receipt: None,
                        error: Some(err.to_string()),
                    },
                }
            }
            Err(err) => RemoteResponse {
                ok: false,
                receipt: None,
                error: Some(format!("invalid remote request JSON: {err}")),
            },
        },
        Err(err) => RemoteResponse {
            ok: false,
            receipt: None,
            error: Some(format!("failed reading remote request: {err}")),
        },
    };

    if let Ok(mut bytes) = serde_json::to_vec(&response) {
        bytes.push(b'\n');
        let mut writer = &stream;
        let _ = writer.write_all(&bytes);
        let _ = writer.flush();
    }
}

/// On a machine that a team host reaches with `orcherd tunnel <machine>`, the tunnel's
/// socket sits at `<state_dir>/control.sock`. Use it when this machine has no team of its
/// own, so an agent started here by hand reaches the team without extra setup. A local
/// team, or an explicit `ORCHERD_REMOTE_SOCKET`, takes precedence.
pub fn tunnel_socket_without_local_team(state_dir: &Path, db_path: &Path) -> Option<PathBuf> {
    let sock = state_dir.join("control.sock");
    if !sock.exists() {
        return None;
    }
    let has_local_team = db_path.exists()
        && Store::open(db_path).is_ok_and(|store| store.resolve_team(None).is_ok());
    (!has_local_team).then_some(sock)
}
