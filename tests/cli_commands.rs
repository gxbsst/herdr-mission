use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::Connection;
use serde_json::Value;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn kernel_binary() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-mission")
}

fn temp_db_path(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "herdr-mission-cli-{label}-{}-{id}.sqlite3",
        std::process::id()
    ))
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn create_cli_mission(path: &Path, title: &str) -> String {
    let output = Command::new(kernel_binary())
        .args(["new", "--json", "--no-start"])
        .arg(format!("--title={title}"))
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "new failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    value["mission_id"].as_str().unwrap().to_string()
}

fn mission_write_snapshot(
    connection: &Connection,
    mission_id: &str,
) -> (i64, String, i64, i64, i64, i64, i64, i64) {
    connection
        .query_row(
            "SELECT team_missions.context_rev, team_missions.updated_at,
                    (SELECT COUNT(*) FROM assignments WHERE mission_id = ?1),
                    (SELECT COUNT(*) FROM messages WHERE mission_id = ?1),
                    (SELECT COUNT(*) FROM outbox WHERE mission_id = ?1),
                    (SELECT COUNT(*) FROM context_ledger WHERE mission_id = ?1),
                    (SELECT COUNT(*) FROM review_revisions WHERE mission_id = ?1),
                    (SELECT COUNT(*) FROM processed_events WHERE mission_id = ?1)
             FROM team_missions WHERE mission_id = ?1",
            [mission_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap()
}

#[test]
fn direct_review_is_rejected_before_any_mission_write() {
    let path = temp_db_path("direct-review-parent");
    let mission_id = create_cli_mission(&path, "Review parent required");
    let connection = Connection::open(&path).unwrap();
    let before = mission_write_snapshot(&connection, &mission_id);
    drop(connection);

    let direct_review = Command::new(kernel_binary())
        .args([
            "send",
            "--json",
            "--role=pm",
            "--target=reviewer",
            "--kind=review",
            "--body=review this directly",
        ])
        .arg(format!("--mission-id={mission_id}"))
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    assert!(!direct_review.status.success());
    let direct_review: Value = serde_json::from_slice(&direct_review.stdout).unwrap();
    assert_eq!(direct_review["error"]["category"], "contract");
    assert_eq!(direct_review["error"]["code"], "review_parent_required");
    assert_eq!(direct_review["error"]["retryable"], false);

    let send_help = Command::new(kernel_binary())
        .args(["send", "--help"])
        .output()
        .unwrap();
    assert!(send_help.status.success());
    let send_help = String::from_utf8(send_help.stdout).unwrap();
    assert!(send_help.contains("--kind <task|fix|context>"));
    assert!(!send_help.contains("review"));

    let connection = Connection::open(&path).unwrap();
    assert_eq!(mission_write_snapshot(&connection, &mission_id), before);
    cleanup(&path);
}

#[test]
fn doctor_bootstraps_and_reports_json() {
    let path = temp_db_path("doctor");
    let output = Command::new(kernel_binary())
        .args(["doctor", "--json"])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();

    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["owner"], "herdr-mission");
    assert_eq!(value["schema_version"], "3");
    assert_eq!(value["generation"], 1);
    assert_eq!(value["created"], true);

    cleanup(&path);
}

#[test]
fn doctor_is_idempotent_on_existing_database() {
    let path = temp_db_path("doctor-repeat");
    let run = || {
        Command::new(kernel_binary())
            .args(["doctor", "--json"])
            .arg(format!("--database={}", path.display()))
            .output()
            .unwrap()
    };

    let first: Value = serde_json::from_slice(&run().stdout).unwrap();
    let second: Value = serde_json::from_slice(&run().stdout).unwrap();
    assert_eq!(first["created"], true);
    assert_eq!(second["created"], false);
    assert_eq!(first["generation"], second["generation"]);

    cleanup(&path);
}

#[test]
fn unknown_command_returns_distinct_exit_code() {
    let output = Command::new(kernel_binary())
        .arg("frobnicate")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(64));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown command"));
}

#[test]
fn new_creates_mission_with_four_roles() {
    let path = temp_db_path("new");
    let output = Command::new(kernel_binary())
        .args([
            "new",
            "--json",
            "--no-start",
            "--title=Fix Team Mission dispatch",
        ])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();

    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["state"], "preparing");
    assert_eq!(value["created"], true);
    let mission_id = value["mission_id"].as_str().unwrap().to_string();
    assert!(mission_id.starts_with("msn-"));

    let connection = Connection::open(&path).unwrap();
    let role_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM team_roles WHERE mission_id = ?1",
            [&mission_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(role_count, 4);

    let launch_mode: String = connection
        .query_row(
            "SELECT launch_mode FROM mission_state WHERE mission_id = ?1",
            [&mission_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(launch_mode, "manual");

    cleanup(&path);
}

#[test]
fn new_persists_auto_and_status_and_init_return_the_same_mode() {
    let path = temp_db_path("new-auto-mode");
    let created = Command::new(kernel_binary())
        .args([
            "new",
            "--json",
            "--no-start",
            "--title=Persisted Auto",
            "--launch-mode=auto",
        ])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    assert!(created.status.success());
    let created: Value = serde_json::from_slice(&created.stdout).unwrap();
    let mission_id = created["mission_id"].as_str().unwrap();
    assert_eq!(created["launch_mode"], "auto");

    let status = Command::new(kernel_binary())
        .args(["status", "--json"])
        .arg(format!("--mission-id={mission_id}"))
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["launch_mode"], "auto");

    let init = Command::new(kernel_binary())
        .args(["init", "--json", "--role=pm"])
        .arg(format!("--mission-id={mission_id}"))
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    assert!(init.status.success());
    let init: Value = serde_json::from_slice(&init.stdout).unwrap();
    assert_eq!(init["launch_mode"], "auto");

    cleanup(&path);
}

#[test]
fn set_launch_mode_switches_both_ways_and_rejects_unknown_values() {
    let path = temp_db_path("set-launch-mode");
    let created = Command::new(kernel_binary())
        .args(["new", "--json", "--no-start", "--title=Switch Mode"])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    let created: Value = serde_json::from_slice(&created.stdout).unwrap();
    let mission_id = created["mission_id"].as_str().unwrap();

    for expected in ["auto", "manual"] {
        let switched = Command::new(kernel_binary())
            .args(["set-launch-mode", "--json"])
            .arg(format!("--mission-id={mission_id}"))
            .arg(format!("--launch-mode={expected}"))
            .arg(format!("--database={}", path.display()))
            .output()
            .unwrap();
        assert!(switched.status.success());
        let switched: Value = serde_json::from_slice(&switched.stdout).unwrap();
        assert_eq!(switched["launch_mode"], expected);
    }

    let invalid = Command::new(kernel_binary())
        .args(["set-launch-mode", "--json", "--launch-mode=fast"])
        .arg(format!("--mission-id={mission_id}"))
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(65));

    let connection = Connection::open(&path).unwrap();
    let persisted: String = connection
        .query_row(
            "SELECT launch_mode FROM mission_state WHERE mission_id = ?1",
            [mission_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(persisted, "manual");

    cleanup(&path);
}

#[test]
fn new_requires_title() {
    let path = temp_db_path("new-notitle");
    let output = Command::new(kernel_binary())
        .args(["new", "--json"])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(65));

    cleanup(&path);
}

#[test]
fn new_simple_layout_creates_a_single_worker() {
    let path = temp_db_path("new-simple");
    let output = Command::new(kernel_binary())
        .args([
            "new",
            "--json",
            "--no-start",
            "--title=Classic Mission",
            "--layout=simple",
        ])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();

    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mission_id = value["mission_id"].as_str().unwrap().to_string();

    let connection = Connection::open(&path).unwrap();
    let role_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM team_roles WHERE mission_id = ?1",
            [&mission_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(role_count, 1);

    let role: String = connection
        .query_row(
            "SELECT role FROM team_roles WHERE mission_id = ?1",
            [&mission_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(role, "worker");

    cleanup(&path);
}

#[test]
fn new_rejects_unknown_layout() {
    let path = temp_db_path("new-unknown-layout");
    let output = Command::new(kernel_binary())
        .args([
            "new",
            "--json",
            "--title=Classic Mission",
            "--layout=frontend",
        ])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(69));

    cleanup(&path);
}

#[test]
fn status_reads_back_new_mission() {
    let path = temp_db_path("status");
    let new_output = Command::new(kernel_binary())
        .args(["new", "--json", "--no-start", "--title=Status Mission"])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    let new_value: Value = serde_json::from_slice(&new_output.stdout).unwrap();
    let mission_id = new_value["mission_id"].as_str().unwrap().to_string();

    let status_output = Command::new(kernel_binary())
        .args(["status", "--json"])
        .arg(format!("--mission-id={mission_id}"))
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    assert!(status_output.status.success());
    let status: Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status["status"], "ok");
    assert_eq!(status["mission_id"], mission_id);
    assert_eq!(status["stage"], "preparing");
    assert_eq!(status["pending_assignments"], 0);
    assert_eq!(status["roles"]["pm"], "unknown");

    cleanup(&path);
}

#[test]
fn reconcile_reports_health_and_delivery_results_as_structured_json() {
    let path = temp_db_path("reconcile");
    let fake_herdr = path.with_extension("herdr");
    fs::write(
        &fake_herdr,
        "#!/bin/sh\nif [ \"$1\" = agent ] && [ \"$2\" = list ]; then\n  printf '%s\\n' '{\"result\":{\"agents\":[{\"name\":\"mission-pm\",\"pane_id\":\"w16:p1\",\"agent_status\":\"working\"}]}}'\n  exit 0\nfi\nexit 1\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_herdr).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_herdr, permissions).unwrap();

    let created = Command::new(kernel_binary())
        .args(["new", "--json", "--no-start", "--title=Reconcile Mission"])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    let created: Value = serde_json::from_slice(&created.stdout).unwrap();
    let mission_id = created["mission_id"].as_str().unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE team_roles
             SET pane_id = 'w16:p1', terminal_id = 'mission-pm', health = 'idle'
             WHERE mission_id = ?1 AND role = 'pm'",
            [mission_id],
        )
        .unwrap();
    drop(connection);

    let output = Command::new(kernel_binary())
        .args(["reconcile", "--json"])
        .arg(format!("--database={}", path.display()))
        .env("HERDR_BIN_PATH", &fake_herdr)
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["health"]["status"], "ok");
    assert_eq!(value["health"]["matched"], 1);
    assert_eq!(value["health"]["updated"], 1);
    assert_eq!(value["delivery"]["status"], "ok");

    let connection = Connection::open(&path).unwrap();
    let health: String = connection
        .query_row(
            "SELECT health FROM team_roles WHERE mission_id = ?1 AND role = 'pm'",
            [mission_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(health, "working");

    let text_output = Command::new(kernel_binary())
        .arg("reconcile")
        .arg(format!("--database={}", path.display()))
        .env("HERDR_BIN_PATH", &fake_herdr)
        .output()
        .unwrap();
    assert!(text_output.status.success());
    assert!(
        String::from_utf8_lossy(&text_output.stdout).contains(" peer="),
        "text reconcile output omitted peer report: {}",
        String::from_utf8_lossy(&text_output.stdout)
    );

    cleanup(&path);
    let _ = fs::remove_file(fake_herdr);
}

#[test]
fn join_rejects_agent_rename_failure_without_changing_the_role_binding() {
    let path = temp_db_path("join-rename-failure");
    let fake_herdr = path.with_extension("herdr");
    write_executable(
        &fake_herdr,
        "#!/bin/sh\nif [ \"$1\" = pane ] && [ \"$2\" = rename ]; then exit 0; fi\nif [ \"$1\" = agent ] && [ \"$2\" = rename ]; then printf '%s\\n' 'rename rejected' >&2; exit 1; fi\nexit 1\n",
    );
    let mission_id = create_cli_mission(&path, "Join rename failure");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE team_roles
             SET pane_id = 'w19:pC', terminal_id = 'Codex', health = 'missing'
             WHERE mission_id = ?1 AND role = 'pm'",
            [&mission_id],
        )
        .unwrap();
    let before: (String, String, String, String, i64) = connection
        .query_row(
            "SELECT pane_id, terminal_id, health, updated_at,
                    (SELECT COUNT(*) FROM messages WHERE mission_id = ?1)
             FROM team_roles WHERE mission_id = ?1 AND role = 'pm'",
            [&mission_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    drop(connection);

    let output = Command::new(kernel_binary())
        .args([
            "join",
            "--json",
            "--role=pm",
            "--pane=w19:pC",
            "--agent-name=mission-verified-pm",
        ])
        .arg(format!("--mission-id={mission_id}"))
        .arg(format!("--database={}", path.display()))
        .env("HERDR_BIN_PATH", &fake_herdr)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "role_agent_rename_failed");
    let connection = Connection::open(&path).unwrap();
    let after: (String, String, String, String, i64) = connection
        .query_row(
            "SELECT pane_id, terminal_id, health, updated_at,
                    (SELECT COUNT(*) FROM messages WHERE mission_id = ?1)
             FROM team_roles WHERE mission_id = ?1 AND role = 'pm'",
            [&mission_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(after, before);

    cleanup(&path);
    let _ = fs::remove_file(fake_herdr);
}

#[test]
fn join_requires_exact_live_identity_before_repairing_a_stale_binding() {
    let path = temp_db_path("join-live-identity");
    let fake_herdr = path.with_extension("herdr");
    write_executable(
        &fake_herdr,
        "#!/bin/sh\nif [ \"$1\" = pane ] && [ \"$2\" = rename ]; then exit 0; fi\nif [ \"$1\" = agent ] && [ \"$2\" = rename ]; then exit 0; fi\nif [ \"$1\" = agent ] && [ \"$2\" = list ]; then\n  printf '%s\\n' '{\"result\":{\"agents\":[{\"name\":\"another-agent\",\"pane_id\":\"w19:pC\",\"agent_status\":\"working\"}]}}'\n  exit 0\nfi\nexit 1\n",
    );
    let mission_id = create_cli_mission(&path, "Join identity mismatch");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE team_roles
             SET pane_id = 'w19:pC', terminal_id = 'Codex', health = 'missing'
             WHERE mission_id = ?1 AND role = 'pm'",
            [&mission_id],
        )
        .unwrap();
    let before: (String, String, String, String, i64) = connection
        .query_row(
            "SELECT pane_id, terminal_id, health, updated_at,
                    (SELECT COUNT(*) FROM messages WHERE mission_id = ?1)
             FROM team_roles WHERE mission_id = ?1 AND role = 'pm'",
            [&mission_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    drop(connection);

    let rejected = Command::new(kernel_binary())
        .args([
            "join",
            "--json",
            "--role=pm",
            "--pane=w19:pC",
            "--agent-name=mission-verified-pm",
        ])
        .arg(format!("--mission-id={mission_id}"))
        .arg(format!("--database={}", path.display()))
        .env("HERDR_BIN_PATH", &fake_herdr)
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&rejected.stdout).unwrap();
    assert_eq!(error["error"]["code"], "role_runtime_identity_unverified");
    let connection = Connection::open(&path).unwrap();
    let after_rejection: (String, String, String, String, i64) = connection
        .query_row(
            "SELECT pane_id, terminal_id, health, updated_at,
                    (SELECT COUNT(*) FROM messages WHERE mission_id = ?1)
             FROM team_roles WHERE mission_id = ?1 AND role = 'pm'",
            [&mission_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(after_rejection, before);
    drop(connection);

    write_executable(
        &fake_herdr,
        "#!/bin/sh\nif [ \"$1\" = pane ] && [ \"$2\" = rename ]; then exit 0; fi\nif [ \"$1\" = agent ] && [ \"$2\" = rename ]; then exit 0; fi\nif [ \"$1\" = agent ] && [ \"$2\" = list ]; then\n  printf '%s\\n' '{\"result\":{\"agents\":[{\"name\":\"mission-verified-pm\",\"pane_id\":\"w19:pC\",\"agent_status\":\"working\"}]}}'\n  exit 0\nfi\nexit 1\n",
    );
    let repaired = Command::new(kernel_binary())
        .args([
            "join",
            "--json",
            "--role=pm",
            "--pane=w19:pC",
            "--agent-name=mission-verified-pm",
        ])
        .arg(format!("--mission-id={mission_id}"))
        .arg(format!("--database={}", path.display()))
        .env("HERDR_BIN_PATH", &fake_herdr)
        .output()
        .unwrap();
    assert!(
        repaired.status.success(),
        "join failed: {}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    let connection = Connection::open(&path).unwrap();
    let repaired_binding: (String, String, String) = connection
        .query_row(
            "SELECT pane_id, terminal_id, health FROM team_roles
             WHERE mission_id = ?1 AND role = 'pm'",
            [&mission_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        repaired_binding,
        ("w19:pC".into(), "mission-verified-pm".into(), "idle".into())
    );

    cleanup(&path);
    let _ = fs::remove_file(fake_herdr);
}

#[test]
fn new_applies_per_role_overrides() {
    let path = temp_db_path("new-role");
    let output = Command::new(kernel_binary())
        .args([
            "new",
            "--json",
            "--no-start",
            "--title=Flexible Roles",
            "--role",
            "worker:provider=claude,model=claude-sonnet-5",
        ])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mission_id = value["mission_id"].as_str().unwrap().to_string();

    let connection = Connection::open(&path).unwrap();
    let worker: (String, String) = connection
        .query_row(
            "SELECT provider, model FROM team_roles WHERE mission_id = ?1 AND role = 'worker'",
            [&mission_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        worker,
        ("claude".to_string(), "claude-sonnet-5".to_string())
    );

    cleanup(&path);
}

#[test]
fn new_declares_additional_role_instance() {
    let path = temp_db_path("new-instance");
    let output = Command::new(kernel_binary())
        .args([
            "new",
            "--json",
            "--no-start",
            "--title=Instanced Roles",
            "--role",
            "scout-01:model=claude-sonnet-5",
        ])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mission_id = value["mission_id"].as_str().unwrap().to_string();

    let connection = Connection::open(&path).unwrap();
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM team_roles WHERE mission_id = ?1",
            [&mission_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 5);

    let instance: (String, String) = connection
        .query_row(
            "SELECT provider, model FROM team_roles WHERE mission_id = ?1 AND role = 'scout-01'",
            [&mission_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        instance,
        ("codex".to_string(), "claude-sonnet-5".to_string())
    );

    cleanup(&path);
}

#[test]
fn new_rejects_unknown_profile() {
    let path = temp_db_path("new-profile");
    let output = Command::new(kernel_binary())
        .args(["new", "--json", "--title=X", "--profile=nope"])
        .arg(format!("--database={}", path.display()))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(65));

    cleanup(&path);
}

#[test]
fn manifest_generates_writes_and_verifies_binary() {
    let root = std::env::temp_dir().join(format!(
        "herdr-mission-manifest-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let binary = root.join("fixture-bin");
    std::fs::write(&binary, b"fixture-bytes").unwrap();

    let generated = Command::new(kernel_binary())
        .args(["manifest", "--json"])
        .arg(format!("--binary={}", binary.display()))
        .output()
        .unwrap();
    assert!(
        generated.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let value: Value = serde_json::from_slice(&generated.stdout).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["manifest"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        value["manifest"]["sha256"],
        "c16a40a4584e5bccc84b45172fcdfa922f59ff1edebf3adba7b8266ea04eb39a"
    );

    let manifest_path = binary.with_file_name("fixture-bin.manifest.json");
    assert!(manifest_path.exists());

    let verified = Command::new(kernel_binary())
        .args(["manifest", "--verify", "--json"])
        .arg(format!("--binary={}", binary.display()))
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&verified.stderr)
    );
    let value: Value = serde_json::from_slice(&verified.stdout).unwrap();
    assert_eq!(value["status"], "ok");

    // Tampering with the binary makes verification fail closed.
    std::fs::write(&binary, b"tampered").unwrap();
    let failed = Command::new(kernel_binary())
        .args(["manifest", "--verify", "--json"])
        .arg(format!("--binary={}", binary.display()))
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(value["status"], "error");
    assert_eq!(value["error"]["code"], "binary_hash_mismatch");

    std::fs::remove_dir_all(&root).unwrap();
}
