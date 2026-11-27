//! Bounded one-request Unix socket transport for remote members (`SPEC.md` §6).
//!
//! Each remote machine `<M>` forwards its `<remote_state>/orcherd/control.sock`
//! over SSH to `<local_state>/orcherd/remote/<M>.sock`. Because requests arrive
//! on `<M>.sock`, the daemon knows which machine the caller belongs to and
//! qualifies bare `HC_TERMINAL_ID=t7` as `<M>/t7`.

use crate::identity::find_team_in_sibling_stores;
use orcherd_core::{Actor, Receipt, RequestEnvelope, Store};
use orcherd_hc::new_command;
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
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
        let raw_err = resp
            .error
            .unwrap_or_else(|| "remote request failed".to_owned());
        Err(format_remote_error(raw_err, resp.host.as_deref(), socket_path))
    }
}

/// Annotates `team not found` errors returned over a tunnel with the answering host,
/// socket path, and any local sibling `orcherd*` store that holds the requested team.
pub(crate) fn format_remote_error(err: String, host: Option<&str>, socket_path: &Path) -> String {
    let sibling = err
        .strip_prefix("team not found: ")
        .and_then(|team_ref| find_team_in_sibling_stores(team_ref, None));
    format_remote_error_with_sibling(err, host, socket_path, sibling.as_deref())
}

pub(crate) fn format_remote_error_with_sibling(
    err: String,
    host: Option<&str>,
    socket_path: &Path,
    sibling_dir: Option<&Path>,
) -> String {
    if let Some(rest) = err.strip_prefix("team not found: ") {
        let host = host
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("remote host");
        return match sibling_dir {
            Some(dir) => format!(
                "team not found on {host} (via the tunnel at {}): {rest}; {rest} is in {}; run with ORCHERD_STATE_DIR={}",
                socket_path.display(),
                dir.display(),
                dir.display(),
            ),
            None => format!(
                "team not found on {host} (via the tunnel at {}): {rest}; set ORCHERD_STATE_DIR or --team",
                socket_path.display()
            ),
        };
    }
    err
}

fn local_hostname() -> Option<String> {
    static HOST: OnceLock<Option<String>> = OnceLock::new();
    HOST.get_or_init(|| {
        if let Ok(name) = fs::read_to_string("/proc/sys/kernel/hostname") {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
        if let Ok(out) = new_command("hostname").output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let trimmed = text.trim().trim_end_matches(".local");
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
        env::var("HOSTNAME")
            .or_else(|_| env::var("COMPUTERNAME"))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    })
    .clone()
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
    handle_client_stream_with_host(store, stream, socket_machine, local_hostname());
}

#[cfg(unix)]
pub(crate) fn handle_client_stream_with_host(
    store: &mut Store,
    stream: std::os::unix::net::UnixStream,
    socket_machine: Option<&str>,
    host: Option<String>,
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
                        host,
                        receipt: Some(receipt),
                        error: None,
                    },
                    Err(err) => RemoteResponse {
                        ok: false,
                        host,
                        receipt: None,
                        error: Some(err.to_string()),
                    },
                }
            }
            Err(err) => RemoteResponse {
                ok: false,
                host,
                receipt: None,
                error: Some(format!("invalid remote request JSON: {err}")),
            },
        },
        Err(err) => RemoteResponse {
            ok: false,
            host,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::build_seat_env;
    use orcherd_core::Op;
    use tempfile::TempDir;

    #[test]
    fn spawn_seat_env_inherits_non_default_state_dir_for_local_seats_only() {
        let default_dir = Path::new("/home/u/.local/state/orcherd");
        let custom_dir = Path::new("/home/u/.local/state/orcherd-somalog");

        // Local seat with custom store inherits ORCHERD_STATE_DIR.
        let local_custom = build_seat_env(
            "/usr/local/bin/orcherd",
            "team-1",
            "w1",
            Some("disp-1"),
            None,
            custom_dir,
            default_dir,
        );
        assert_eq!(
            local_custom.get("ORCHERD_STATE_DIR").map(String::as_str),
            Some("/home/u/.local/state/orcherd-somalog")
        );
        assert!(!local_custom.contains_key("ORCHERD_REMOTE_SOCKET"));

        // Local seat with default store omits ORCHERD_STATE_DIR.
        let local_default = build_seat_env(
            "/usr/local/bin/orcherd",
            "team-1",
            "w1",
            None,
            None,
            default_dir,
            default_dir,
        );
        assert!(!local_default.contains_key("ORCHERD_STATE_DIR"));

        // Remote seat keeps the tunnel socket route even when local store is custom.
        let remote = build_seat_env(
            "/usr/local/bin/orcherd",
            "team-1",
            "w1",
            Some("disp-1"),
            Some("m1"),
            custom_dir,
            default_dir,
        );
        assert!(!remote.contains_key("ORCHERD_STATE_DIR"));
        assert_eq!(remote.get("ORCHERD_MACHINE").map(String::as_str), Some("m1"));
        assert!(remote.contains_key("ORCHERD_REMOTE_SOCKET"));
    }

    #[cfg(unix)]
    #[test]
    fn tunnel_team_not_found_error_names_host_and_socket() {
        use std::os::unix::net::UnixListener;

        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("store.sqlite3");
        let sock_path = tmp.path().join("control.sock");
        let mut store = Store::open(&db_path).unwrap();

        let listener = UnixListener::bind(&sock_path).unwrap();
        let sock_clone = sock_path.clone();
        let client = std::thread::spawn(move || {
            let actor = Actor::Seat {
                team_id: Some("team-70c1f6ea8670".to_owned()),
                seat_name: "w1".to_owned(),
                terminal: Some("t7".to_owned()),
            };
            send_remote_request(
                &sock_clone,
                &actor,
                RequestEnvelope::new(Op::InboxRead {
                    peek: true,
                    seat: None,
                    limit: None,
                })
                .with_team("team-70c1f6ea8670"),
                None,
            )
        });

        let (stream, _) = listener.accept().unwrap();
        handle_client_stream_with_host(&mut store, stream, Some("m1"), Some("yeti-cachy".to_owned()));
        let err = client.join().unwrap().unwrap_err();
        assert_eq!(
            err,
            format!(
                "team not found on yeti-cachy (via the tunnel at {}): team-70c1f6ea8670; set ORCHERD_STATE_DIR or --team",
                sock_path.display()
            )
        );

        // Older daemon omitting `host` falls back to "remote host", and non-team errors stay verbatim.
        assert_eq!(
            format_remote_error("team not found: missing".to_owned(), None, &sock_path),
            format!(
                "team not found on remote host (via the tunnel at {}): missing; set ORCHERD_STATE_DIR or --team",
                sock_path.display()
            )
        );
        assert_eq!(
            format_remote_error("seat not found in team demo: ghost".to_owned(), Some("yeti"), &sock_path),
            "seat not found in team demo: ghost"
        );
    }
}
