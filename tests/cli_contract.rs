use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn kernel_binary() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-mission")
}

fn run_cli(command: &str, input: &str) -> Output {
    let mut child = Command::new(kernel_binary())
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn run_temporary_cli(command: &str, input: &str, root: &PathBuf) -> Output {
    let mut child = Command::new(kernel_binary())
        .arg(command)
        .env("HERDR_MISSION_TEMP_ROOT", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn run_read_only_cli(command: &str, input: &str, database: &PathBuf) -> Output {
    let mut child = Command::new(kernel_binary())
        .arg(command)
        .env("HERDR_MISSION_READ_ONLY_DATABASE", database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn temporary_database() -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "herdr-mission-kernel-cli-{}-{}",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("mission.sqlite3");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(include_str!("fixtures/schema-v3.sql"))
        .unwrap();
    connection
        .execute(
            "INSERT INTO team_missions(mission_id, created_at, updated_at)
             VALUES('mission-cli', '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO team_roles(
                mission_id, role, provider, launch_generation, health, updated_at
             ) VALUES(
                'mission-cli', 'worker', 'recording', 'generation-worker', 'ready',
                '2026-08-14T00:00:00Z'
             )",
            [],
        )
        .unwrap();
    drop(connection);
    (root, path)
}

fn base_request(protocol: &str, binary_contract: &str, operation: Value) -> String {
    json!({
        "protocol": protocol,
        "binary_contract": binary_contract,
        "request_id": "req-001",
        "mission": { "mission_id": "mission-001" },
        "database": {
            "path": "/tmp/mission.sqlite",
            "access": "read_write"
        },
        "decision_context": {
            "observed_at": "2026-08-14T12:00:00Z",
            "allocated_ids": {},
            "generations": {}
        },
        "operation": operation
    })
    .to_string()
}

fn assert_error(output: Output, exit_code: i32, category: &str, code: &str) {
    assert_eq!(output.status.code(), Some(exit_code));
    assert!(!output.stderr.is_empty());
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["outcome"]["status"], "error");
    assert_eq!(outcome["outcome"]["error"]["category"], category);
    assert_eq!(outcome["outcome"]["error"]["code"], code);
}

#[test]
fn version_discovery_is_machine_readable_and_protocol_explicit() {
    let output = Command::new(kernel_binary())
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        json!({
            "binary": "herdr-mission",
            "binary_version": env!("CARGO_PKG_VERSION"),
            "binary_contract": "herdr.mission.kernel.binary.v1",
            "protocol": "herdr.mission.kernel.v1",
            "operations": ["handle", "drive", "inspect"]
        })
    );
}

#[test]
fn malformed_json_fails_closed_with_one_structured_outcome() {
    assert_error(
        run_cli("handle", "{\"protocol\":"),
        65,
        "transport",
        "malformed_json",
    );
}

#[test]
fn unknown_protocol_fails_closed_before_operation_decode() {
    let input = base_request(
        "herdr.mission.kernel.v2",
        "herdr.mission.kernel.binary.v1",
        json!({ "type": "handle", "request": { "input": {} } }),
    );

    assert_error(
        run_cli("handle", &input),
        66,
        "protocol",
        "unknown_protocol",
    );
}

#[test]
fn incompatible_binary_contract_fails_closed() {
    let input = base_request(
        "herdr.mission.kernel.v1",
        "herdr.mission.kernel.binary.v2",
        json!({ "type": "handle", "request": { "input": {} } }),
    );

    assert_error(
        run_cli("handle", &input),
        67,
        "contract",
        "incompatible_binary_contract",
    );
}

#[test]
fn unsupported_operation_tag_fails_closed() {
    let input = base_request(
        "herdr.mission.kernel.v1",
        "herdr.mission.kernel.binary.v1",
        json!({ "type": "future_operation", "request": {} }),
    );

    assert_error(
        run_cli("handle", &input),
        64,
        "operation",
        "unsupported_operation",
    );
}

#[test]
fn recognized_request_uses_fixture_only_harness_without_executing_the_kernel() {
    let input = base_request(
        "herdr.mission.kernel.v1",
        "herdr.mission.kernel.binary.v1",
        json!({
            "type": "handle",
            "request": {
                "input": {
                    "type": "command",
                    "command_id": "cmd-001",
                    "kind": "context",
                    "source": { "role": "pm" },
                    "target": { "role": "worker" },
                    "body": { "text": "fixture" }
                }
            }
        }),
    );

    assert_error(
        run_cli("handle", &input),
        70,
        "internal",
        "standalone_scaffold_only",
    );
}

#[test]
fn temporary_fixture_mode_executes_handle_and_read_only_inspect_through_the_binary() {
    let (root, database) = temporary_database();
    let handle = json!({
        "protocol": "herdr.mission.kernel.v1",
        "binary_contract": "herdr.mission.kernel.binary.v1",
        "request_id": "req-cli-handle",
        "mission": {"mission_id": "mission-cli"},
        "database": {"path": database, "access": "read_write"},
        "decision_context": {
            "observed_at": "2026-08-14T12:00:00Z",
            "allocated_ids": {
                "assignment": "asg-cli",
                "message": "msg-cli",
                "outbox": "out-cli"
            },
            "generations": {"worker": "generation-worker"}
        },
        "operation": {
            "type": "handle",
            "request": {"input": {
                "type": "command",
                "command_id": "cmd-cli",
                "kind": "task",
                "source": {"role": "pm"},
                "target": {"role": "worker"},
                "body": {"text": "execute fixture"}
            }}
        }
    })
    .to_string();
    let output = run_temporary_cli("handle", &handle, &root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["outcome"]["status"], "success");
    assert_eq!(outcome["outcome"]["result"]["type"], "handle");

    let inspect = json!({
        "protocol": "herdr.mission.kernel.v1",
        "binary_contract": "herdr.mission.kernel.binary.v1",
        "request_id": "req-cli-inspect",
        "mission": {"mission_id": "mission-cli"},
        "database": {"path": database, "access": "read_only"},
        "decision_context": {"observed_at": "2026-08-14T12:01:00Z"},
        "operation": {"type": "inspect", "request": {"query": {"type": "status"}}}
    })
    .to_string();
    let output = run_temporary_cli("inspect", &inspect, &root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["outcome"]["result"]["type"], "inspect");
    assert_eq!(
        outcome["outcome"]["result"]["value"]["data"]["assignments"][0]["id"],
        "asg-cli"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn read_only_canary_requires_exact_path_and_never_opens_mutation_operations() {
    let (root, database) = temporary_database();
    let before = fs::read(&database).unwrap();
    let inspect = json!({
        "protocol": "herdr.mission.kernel.v1",
        "binary_contract": "herdr.mission.kernel.binary.v1",
        "request_id": "req-read-only",
        "mission": {"mission_id": "mission-cli"},
        "database": {"path": database, "access": "read_only"},
        "decision_context": {"observed_at": "2026-08-14T12:01:00Z"},
        "operation": {"type": "inspect", "request": {"query": {"type": "status"}}}
    })
    .to_string();
    let output = run_read_only_cli("inspect", &inspect, &database);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["outcome"]["result"]["type"], "inspect");
    assert_eq!(fs::read(&database).unwrap(), before);

    let other_database = root.join("other.sqlite3");
    fs::copy(&database, &other_database).unwrap();
    let wrong_path = inspect.replace(database.to_str().unwrap(), other_database.to_str().unwrap());
    assert_error(
        run_read_only_cli("inspect", &wrong_path, &database),
        70,
        "contract",
        "read_only_path_mismatch",
    );

    let mutation = inspect
        .replace("\"type\":\"inspect\"", "\"type\":\"handle\"")
        .replace("\"access\":\"read_only\"", "\"access\":\"read_write\"")
        .replace(
            "\"query\":{\"type\":\"status\"}",
            "\"input\":{\"type\":\"command\",\"command_id\":\"cmd-denied\",\"kind\":\"context\",\"source\":{\"role\":\"pm\"},\"target\":{\"role\":\"worker\"},\"body\":{\"text\":\"denied\"}}",
        );
    assert_error(
        run_read_only_cli("handle", &mutation, &database),
        70,
        "contract",
        "read_only_operation_required",
    );
    assert_eq!(fs::read(&database).unwrap(), before);
    fs::remove_dir_all(root).unwrap();
}
