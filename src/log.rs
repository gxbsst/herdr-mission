//! Append-only diagnostic log beside the mission database.
//!
//! Mission creation/launch failures are otherwise only shown transiently in
//! the TUI message line (or on stderr for the CLI). Persisting them to a file
//! keeps the reason durable so a `blocked` mission can be diagnosed afterwards.

use std::{fs::OpenOptions, io::Write, path::Path};

use crate::utc_timestamp;

/// Append a single timestamped line to `<database_dir>/herdr-mission.log`.
///
/// Logging is best-effort on purpose: a failure to write the log must never
/// mask the outcome of the operation being logged, so errors are ignored.
pub fn log_event(database: &Path, message: &str) {
    let Some(parent) = database.parent() else {
        return;
    };
    let path = parent.join("herdr-mission.log");
    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => file,
        Err(_) => return,
    };
    let _ = writeln!(file, "{} {}", utc_timestamp(), message);
}

/// Log a mission lifecycle failure with the full serialized `KernelError`.
pub fn log_mission_error(database: &Path, mission_id: &str, error: &crate::KernelError) {
    log_error(
        database,
        &format!("mission={mission_id} stage=blocked"),
        error,
    );
}

/// Log a free-form error context with the full serialized `KernelError`.
pub fn log_error(database: &Path, context: &str, error: &crate::KernelError) {
    let payload = serde_json::to_string(error)
        .unwrap_or_else(|_| format!("{} ({})", error.message, error.code));
    log_event(database, &format!("{context} error={payload}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorCategory, KernelError};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_db(label: &str) -> std::path::PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "herdr-mission-log-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("missions.sqlite3")
    }

    #[test]
    fn log_event_appends_a_timestamped_line() {
        let database = temp_db("event");
        log_event(&database, "mission=m1 launch ok");

        let log =
            std::fs::read_to_string(database.parent().unwrap().join("herdr-mission.log")).unwrap();
        assert!(log.contains("mission=m1 launch ok"));
        assert!(log.contains("T")); // ISO-8601 timestamp separator
        assert!(log.contains("Z ")); // UTC marker followed by the message

        std::fs::remove_dir_all(database.parent().unwrap()).unwrap();
    }

    #[test]
    fn log_error_writes_full_serialized_error() {
        let database = temp_db("error");
        let error = KernelError {
            category: ErrorCategory::Infrastructure,
            code: "launch_effect_failed".into(),
            message: "mission launch effect failed".into(),
            retryable: true,
            details: BTreeMap::from([("operation".into(), serde_json::json!("pane split"))]),
        };
        log_error(&database, "mission=m1 create failed", &error);

        let log =
            std::fs::read_to_string(database.parent().unwrap().join("herdr-mission.log")).unwrap();
        assert!(log.contains("mission=m1 create failed error="));
        assert!(log.contains("launch_effect_failed"));
        assert!(log.contains("pane split"));

        std::fs::remove_dir_all(database.parent().unwrap()).unwrap();
    }
}
