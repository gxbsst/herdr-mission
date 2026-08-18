use std::{
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
    assert_eq!(value["manifest"]["version"], "0.1.0");
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
