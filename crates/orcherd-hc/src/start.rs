//! Starting a core when none is running: the user's own `herdr-core`, or a
//! specified binary.

use crate::{Error, Result};
use std::{
    ffi::OsStr,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// The core's executable name; `hc` is only a link to it.
pub const PROGRAM: &str = if cfg!(windows) {
    "herdr-core.exe"
} else {
    "herdr-core"
};

/// `CreateProcess` flag: the child gets a console with no window.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Output of a failed start kept for diagnostics.
const MAX_STDERR: u64 = 4 << 10;

/// A command for `program` that never opens a console window on Windows.
pub fn new_command(program: impl AsRef<OsStr>) -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new(program);
        std::os::windows::process::CommandExt::creation_flags(&mut command, CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    Command::new(program)
}

/// Runs `binary server start --json`, which returns once the session's core
/// listens, and waits at most `timeout`. `configure` sets the environment,
/// such as the login shell's. Blocking.
pub fn start(binary: &Path, configure: impl FnOnce(&mut Command), timeout: Duration) -> Result<()> {
    let mut command = new_command(binary);
    command
        .args(["server", "start", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    configure(&mut command);
    let start_error = |source| Error::Start {
        binary: binary.to_owned(),
        source,
    };
    let mut child = command.spawn().map_err(start_error)?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(start_error)? {
            break status;
        }
        if Instant::now() >= deadline {
            // Only the start command dies here; a core it started keeps running.
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::StartTimeout(timeout));
        }
        thread::sleep(Duration::from_millis(25));
    };
    if status.success() {
        return Ok(());
    }
    let mut stderr = String::new();
    if let Some(pipe) = child.stderr.take() {
        let _ = pipe.take(MAX_STDERR).read_to_string(&mut stderr);
    }
    Err(Error::StartFailed { status, stderr })
}
