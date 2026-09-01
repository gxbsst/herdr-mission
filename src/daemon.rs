//! Persistent delivery daemon: single-instance lock, graceful stop, delivery loop.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::json;

use crate::{
    kernel_deliver, open_writable, reconcile_peer_relay, ErrorCategory, KernelError, ProcessRunner,
    SystemSshPeerTransport, OWNER_IDENTITY,
};

/// An exclusive lockfile guarding a single daemon instance per database.
///
/// The lock stores the owning pid; a stale lock whose pid is dead is reclaimed
/// on acquire. The lock is removed on drop.
#[derive(Debug)]
pub struct DaemonLock {
    path: PathBuf,
}

impl DaemonLock {
    pub fn acquire(database: &Path) -> Result<Self, KernelError> {
        let path = lock_path(database);
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                Ok(Self { path })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale_lock(&path) {
                    let _ = fs::remove_file(&path);
                    return Self::acquire(database);
                }
                Err(already_running(&path))
            }
            Err(error) => Err(lock_failed(&path, error)),
        }
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Request a graceful shutdown: write the per-database stop marker that the
/// running daemon observes on its next loop iteration.
pub fn request_stop(database: &Path) -> Result<(), KernelError> {
    fs::write(stop_path(database), b"stop").map_err(|error| KernelError {
        category: ErrorCategory::Infrastructure,
        code: "daemon_stop_write_failed".into(),
        message: "failed to write the daemon stop marker".into(),
        retryable: false,
        details: BTreeMap::from([
            ("path".into(), json!(stop_path(database))),
            ("reason".into(), json!(error.to_string())),
        ]),
    })
}

/// Run the delivery daemon: verify the database, acquire the lock, then
/// repeatedly deliver queued outbox messages until a stop marker appears.
pub fn run_daemon(
    database: &Path,
    interval: Duration,
    runner: &dyn ProcessRunner,
    herdr: &str,
) -> Result<(), KernelError> {
    let _ = open_writable(database, OWNER_IDENTITY)?;
    let _lock = DaemonLock::acquire(database)?;
    loop {
        if stop_requested(database) {
            let _ = fs::remove_file(stop_path(database));
            return Ok(());
        }
        match kernel_deliver(database, runner, herdr) {
            Ok(report) if report.delivered > 0 || report.failed > 0 => {
                eprintln!(
                    "delivery: delivered={} failed={}",
                    report.delivered, report.failed
                );
            }
            Ok(_) => {}
            Err(error) => eprintln!("delivery error: {} ({})", error.message, error.code),
        }
        match reconcile_peer_relay(database, &SystemSshPeerTransport, runner, herdr) {
            Ok(report)
                if report.sent > 0
                    || report.retried > 0
                    || report.notified > 0
                    || report.notify_failed > 0 =>
            {
                eprintln!(
                    "peer relay: sent={} retried={} notified={} notify_failed={}",
                    report.sent, report.retried, report.notified, report.notify_failed
                );
            }
            Ok(_) => {}
            Err(error) => eprintln!("peer relay error: {} ({})", error.message, error.code),
        }
        std::thread::sleep(interval);
    }
}

fn is_stale_lock(path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    let Some(pid) = content.trim().parse::<u32>().ok() else {
        return false;
    };
    !is_pid_alive(pid)
}

fn is_pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn stop_requested(database: &Path) -> bool {
    stop_path(database).exists()
}

fn lock_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

fn stop_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(".stop");
    PathBuf::from(name)
}

fn already_running(path: &Path) -> KernelError {
    KernelError {
        category: ErrorCategory::Infrastructure,
        code: "daemon_already_running".into(),
        message: "another herdr-mission daemon is running".into(),
        retryable: false,
        details: BTreeMap::from([("lock".into(), json!(path))]),
    }
}

fn lock_failed(path: &Path, error: std::io::Error) -> KernelError {
    KernelError {
        category: ErrorCategory::Infrastructure,
        code: "daemon_lock_failed".into(),
        message: "failed to acquire the daemon lock".into(),
        retryable: false,
        details: BTreeMap::from([
            ("path".into(), json!(path)),
            ("reason".into(), json!(error.to_string())),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_database() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-mission-daemon-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join("mission.sqlite3")
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let database = temp_database();
        let first = DaemonLock::acquire(&database).unwrap();
        let second = DaemonLock::acquire(&database).unwrap_err();
        assert_eq!(second.code, "daemon_already_running");
        drop(first);
        let third = DaemonLock::acquire(&database).unwrap();
        drop(third);
        fs::remove_dir_all(database.parent().unwrap()).unwrap();
    }

    #[test]
    fn stale_lock_with_dead_pid_is_reclaimed() {
        let database = temp_database();
        // A dead pid (999999) is unreachable, so the lock is treated as stale.
        fs::write(lock_path(&database), "999999").unwrap();
        let lock = DaemonLock::acquire(&database).unwrap();
        drop(lock);
        fs::remove_dir_all(database.parent().unwrap()).unwrap();
    }
}
