//! A running daemon picks up a new `orcherd` binary by re-executing itself.
//!
//! Installs replace the binary on disk (atomic `mv`, `cargo install`), but a long-running
//! daemon keeps executing the old inode until someone restarts it. On 2026-11-26 that left
//! a team on a pre-fix daemon for hours after the doorbell fix shipped. So the daemon
//! remembers its binary at start and, once a replacement has settled and answers
//! `--version`, `exec`s it in place: same pid, so the lock file stays valid.

use orcherd_hc::new_command;
use std::{
    env, fs,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant, SystemTime},
};

const CHECK_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    inode: u64,
}

fn fingerprint(path: &PathBuf) -> Option<Fingerprint> {
    let meta = fs::metadata(path).ok()?;
    Some(Fingerprint {
        len: meta.len(),
        modified: meta.modified().ok(),
        #[cfg(unix)]
        inode: std::os::unix::fs::MetadataExt::ino(&meta),
    })
}

pub struct BinaryWatch {
    path: PathBuf,
    started_as: Fingerprint,
    pending: Option<Fingerprint>,
    last_check: Instant,
}

impl BinaryWatch {
    /// `None` when the binary's path can't be read (nothing to watch).
    pub fn start() -> Option<Self> {
        Self::watching(env::current_exe().ok()?)
    }

    fn watching(path: PathBuf) -> Option<Self> {
        let started_as = fingerprint(&path)?;
        Some(Self { path, started_as, pending: None, last_check: Instant::now() })
    }

    /// The new binary's path, once it differs from the one this process started as, has
    /// stayed the same for two checks (so a copy in progress isn't run), and runs.
    pub fn replaced(&mut self) -> Option<PathBuf> {
        if self.last_check.elapsed() < CHECK_EVERY {
            return None;
        }
        self.last_check = Instant::now();
        self.check_now()
    }

    fn check_now(&mut self) -> Option<PathBuf> {
        let now = fingerprint(&self.path).filter(|f| *f != self.started_as);
        let settled = now.is_some() && now == self.pending;
        self.pending = now;
        if !settled {
            return None;
        }
        let runs = new_command(&self.path)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if runs {
            Some(self.path.clone())
        } else {
            // A broken install: keep serving on the old code and stop retrying it.
            self.started_as = self.pending.take()?;
            None
        }
    }
}

/// Replaces this process with `path daemon run`. Returns only if `exec` fails.
#[cfg(unix)]
pub fn reexec(path: &PathBuf) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(path).args(["daemon", "run"]).exec()
}

#[cfg(not(unix))]
pub fn reexec(_path: &PathBuf) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Unsupported, "re-exec needs unix")
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::BinaryWatch;
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    fn write_script(path: &Path, body: &str) {
        let tmp = path.with_extension("new");
        fs::write(&tmp, body).unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, path).unwrap();
    }

    #[test]
    fn a_replaced_binary_is_picked_up_once_settled_and_only_if_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("orcherd");
        write_script(&bin, "#!/bin/sh\nexit 0\n");
        let mut watch = BinaryWatch::watching(bin.clone()).unwrap();
        assert_eq!(watch.check_now(), None, "unchanged");

        write_script(&bin, "#!/bin/sh\nexit 1\n# broken install\n");
        assert_eq!(watch.check_now(), None, "first sighting waits to settle");
        assert_eq!(watch.check_now(), None, "a binary that fails --version is not run");
        assert_eq!(watch.check_now(), None, "and is not retried");

        write_script(&bin, "#!/bin/sh\nexit 0\n# good install\n");
        assert_eq!(watch.check_now(), None);
        assert_eq!(watch.check_now(), Some(bin));
    }
}
