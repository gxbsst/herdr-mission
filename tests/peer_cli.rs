use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use herdr_mission::{sha256_hex, PeerEnvelopeV1, PeerPayloadV1, MAX_PEER_ENVELOPE_BYTES};
use rusqlite::Connection;
use serde_json::{json, Value};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn mission_binary() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-mission")
}

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "herdr-mission-peer-cli-{label}-{}-{id}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn create_mission(&self, title: &str) -> String {
        let output = run_cli(
            &self.path,
            &["new", "--no-start", &format!("--title={title}")],
        );
        let value = expect_success_json(output);
        value["mission_id"].as_str().unwrap().to_string()
    }

    fn bind_pm(&self, mission_id: &str) {
        let connection = Connection::open(&self.path).unwrap();
        connection
            .execute(
                "UPDATE team_roles
                 SET pane_id = ?1, terminal_id = ?2, launch_generation = ?3,
                     health = 'idle'
                 WHERE mission_id = ?4 AND role = 'pm'",
                rusqlite::params![
                    format!("pane-{mission_id}-pm"),
                    format!("agent-{mission_id}-pm"),
                    format!("generation-{mission_id}-pm"),
                    mission_id,
                ],
            )
            .unwrap();
    }

    fn configure_identity(&self, peer_id: &str) -> Value {
        expect_success_json(run_cli(
            &self.path,
            &["peer", "identity", &format!("--local-peer={peer_id}")],
        ))
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(self.path.with_extension("sqlite3-shm"));
    }
}

fn run_cli(database: &Path, args: &[&str]) -> Output {
    Command::new(mission_binary())
        .args(args)
        .arg("--json")
        .arg(format!("--database={}", database.display()))
        .output()
        .unwrap()
}

fn run_cli_with_stdin(database: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(mission_binary())
        .args(args)
        .arg("--json")
        .arg(format!("--database={}", database.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn parse_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn expect_success_json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "CLI failed: stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_json(&output)
}

fn expect_error_json(output: Output, code: &str) -> Value {
    assert!(
        !output.status.success(),
        "CLI unexpectedly succeeded: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value = parse_json(&output);
    assert_eq!(value["status"], "error");
    assert_eq!(value["error"]["code"], code);
    assert_eq!(value["error"]["retryable"], false);
    value
}

fn remote_envelope(message_id: &str) -> PeerEnvelopeV1 {
    let payload = PeerPayloadV1 {
        protocol: "herdr-mission-peer-v1".into(),
        message_id: message_id.into(),
        source_peer_id: "device-a".into(),
        target_peer_id: "device-b".into(),
        source_mission_id: "msn-remote-source".into(),
        target_mission_id: String::new(),
        source_pm_generation: "generation-remote-pm-1".into(),
        kind: "delegate".into(),
        body: "Please decompose this into local Assignments.".into(),
        in_reply_to: None,
        created_at: "2026-09-01T03:00:00Z".into(),
    };
    let payload_sha256 = sha256_hex(&serde_json::to_vec(&payload).unwrap());
    PeerEnvelopeV1 {
        payload,
        payload_sha256,
    }
}

#[test]
fn peer_identity_add_and_link_are_idempotent_cli_configuration() {
    let database = TestDatabase::new("configure");
    let local_mission = database.create_mission("peer-cli-config-source");

    let first = database.configure_identity("device-a");
    let duplicate = database.configure_identity("device-a");
    assert_eq!(first["local_peer_id"], "device-a");
    assert_eq!(duplicate["local_peer_id"], "device-a");

    for _ in 0..2 {
        let added = expect_success_json(run_cli(
            &database.path,
            &[
                "peer",
                "add",
                "--peer=device-b",
                "--ssh=relay-b@example.test",
            ],
        ));
        assert_eq!(added["peer_id"], "device-b");
        assert_eq!(added["ssh_destination"], "relay-b@example.test");

        let linked = expect_success_json(run_cli(
            &database.path,
            &[
                "peer",
                "link",
                "--peer=device-b",
                &format!("--local-mission={local_mission}"),
                "--remote-mission=msn-remote-target",
                "--direction=outbound",
            ],
        ));
        assert_eq!(linked["direction"], "outbound");
    }

    expect_error_json(
        run_cli(
            &database.path,
            &[
                "peer",
                "add",
                "--peer=device-c",
                "--ssh=-oProxyCommand=touch /tmp/peer-cli-owned",
            ],
        ),
        "peer_ssh_destination_invalid",
    );

    let connection = Connection::open(&database.path).unwrap();
    let peers: i64 = connection
        .query_row("SELECT COUNT(*) FROM mission_peers", [], |row| row.get(0))
        .unwrap();
    let routes: i64 = connection
        .query_row("SELECT COUNT(*) FROM mission_peer_routes", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!((peers, routes), (1, 1));
}

#[test]
fn peer_send_is_visible_in_init_and_acknowledgement_survives_reopen() {
    let database = TestDatabase::new("local-send");
    let source = database.create_mission("peer-cli-local-source");
    let target = database.create_mission("peer-cli-local-target");
    database.bind_pm(&source);
    database.configure_identity("device-a");

    let sent = expect_success_json(run_cli(
        &database.path,
        &[
            "peer",
            "send",
            &format!("--mission-id={source}"),
            &format!("--target-mission={target}"),
            "--kind=delegate",
            "--body=Split this into a local Worker Assignment.",
            "--message-id=peer-cli-local-1",
        ],
    ));
    assert_eq!(sent["message_id"], "peer-cli-local-1");
    assert_eq!(sent["state"], "accepted");
    assert_eq!(sent["duplicate"], false);
    assert_eq!(sent["delivery"]["notify_failed"], 1);

    let initialized = expect_success_json(run_cli(
        &database.path,
        &["init", &format!("--mission-id={target}"), "--role=pm"],
    ));
    let inbox = initialized["peer_inbox"].as_array().unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0]["message_id"], "peer-cli-local-1");
    assert_eq!(inbox[0]["source_peer_id"], "device-a");
    assert_eq!(inbox[0]["target_peer_id"], "device-a");
    assert_eq!(inbox[0]["source_mission_id"], source);
    assert_eq!(inbox[0]["target_mission_id"], target);
    assert_eq!(
        inbox[0]["source_pm_generation"],
        format!(
            "generation-{}-pm",
            inbox[0]["source_mission_id"].as_str().unwrap()
        )
    );
    assert_eq!(initialized["pending_assignments"], json!([]));
    assert_eq!(initialized["inbox"], json!([]));

    let acknowledged = expect_success_json(run_cli(
        &database.path,
        &[
            "peer",
            "ack",
            &format!("--mission-id={target}"),
            "--role=pm",
            "--message-id=peer-cli-local-1",
        ],
    ));
    assert_eq!(acknowledged["acknowledged"], true);
    assert_eq!(acknowledged["changed"], true);

    let reopened = expect_success_json(run_cli(
        &database.path,
        &["init", &format!("--mission-id={target}"), "--role=pm"],
    ));
    assert_eq!(reopened["peer_inbox"], json!([]));
    let duplicate_ack = expect_success_json(run_cli(
        &database.path,
        &[
            "peer",
            "ack",
            &format!("--mission-id={target}"),
            "--role=pm",
            "--message-id=peer-cli-local-1",
        ],
    ));
    assert_eq!(duplicate_ack["changed"], false);
}

#[test]
fn send_target_mission_uses_peer_relay_while_ordinary_pm_send_remains_denied() {
    let database = TestDatabase::new("send-shortcut");
    let source = database.create_mission("peer-cli-shortcut-source");
    let target = database.create_mission("peer-cli-shortcut-target");
    database.bind_pm(&source);
    database.configure_identity("device-a");

    let sent = expect_success_json(run_cli(
        &database.path,
        &[
            "send",
            &format!("--mission-id={source}"),
            "--role=pm",
            "--target=pm",
            &format!("--target-mission={target}"),
            "--kind=context",
            "--body=Cross-Mission context only.",
            "--message-id=peer-cli-shortcut-1",
        ],
    ));
    assert_eq!(sent["message_id"], "peer-cli-shortcut-1");
    assert_eq!(sent["state"], "accepted");

    let before = expect_success_json(run_cli(
        &database.path,
        &["init", &format!("--mission-id={target}"), "--role=pm"],
    ));
    assert_eq!(before["peer_inbox"].as_array().unwrap().len(), 1);

    expect_error_json(
        run_cli(
            &database.path,
            &[
                "send",
                &format!("--mission-id={source}"),
                "--role=pm",
                "--target=pm",
                "--kind=context",
                "--body=This must not become a self-addressed Team message.",
            ],
        ),
        "acl_denied",
    );

    let after = expect_success_json(run_cli(
        &database.path,
        &["init", &format!("--mission-id={target}"), "--role=pm"],
    ));
    assert_eq!(after["peer_inbox"].as_array().unwrap().len(), 1);
    let connection = Connection::open(&database.path).unwrap();
    let team_writes: (i64, i64, i64, i64) = connection
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM assignments),
                (SELECT COUNT(*) FROM messages),
                (SELECT COUNT(*) FROM outbox),
                (SELECT COUNT(*) FROM context_ledger)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(team_writes, (0, 0, 0, 0));
}

#[test]
fn peer_receive_reads_bounded_typed_stdin_and_returns_durable_receipts() {
    let database = TestDatabase::new("receive");
    let target = database.create_mission("peer-cli-receive-target");
    database.configure_identity("device-b");
    expect_success_json(run_cli(
        &database.path,
        &[
            "peer",
            "add",
            "--peer=device-a",
            "--ssh=relay-a@example.test",
        ],
    ));
    expect_success_json(run_cli(
        &database.path,
        &[
            "peer",
            "link",
            "--peer=device-a",
            &format!("--local-mission={target}"),
            "--remote-mission=msn-remote-source",
            "--direction=inbound",
        ],
    ));

    let mut envelope = remote_envelope("peer-cli-inbound-1");
    envelope.payload.target_mission_id = target.clone();
    envelope.payload_sha256 = sha256_hex(&serde_json::to_vec(&envelope.payload).unwrap());
    let bytes = serde_json::to_vec(&envelope).unwrap();

    let accepted = expect_success_json(run_cli_with_stdin(
        &database.path,
        &["peer", "receive", "--peer=device-a"],
        &bytes,
    ));
    assert_eq!(accepted["status"], "accepted");
    assert_eq!(accepted["message_id"], "peer-cli-inbound-1");
    assert_eq!(accepted["payload_sha256"], envelope.payload_sha256);

    let duplicate = expect_success_json(run_cli_with_stdin(
        &database.path,
        &["peer", "receive", "--peer=device-a"],
        &bytes,
    ));
    assert_eq!(duplicate["status"], "duplicate");

    let mut unknown = serde_json::to_value(&envelope).unwrap();
    unknown["unexpected"] = json!(true);
    expect_error_json(
        run_cli_with_stdin(
            &database.path,
            &["peer", "receive", "--peer=device-a"],
            &serde_json::to_vec(&unknown).unwrap(),
        ),
        "peer_envelope_invalid",
    );
    expect_error_json(
        run_cli_with_stdin(
            &database.path,
            &["peer", "receive", "--peer=device-a"],
            &vec![b' '; MAX_PEER_ENVELOPE_BYTES + 1],
        ),
        "peer_envelope_too_large",
    );

    let inbox = expect_success_json(run_cli(
        &database.path,
        &["init", &format!("--mission-id={target}"), "--role=pm"],
    ));
    assert_eq!(inbox["peer_inbox"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["peer_inbox"][0]["message_id"], "peer-cli-inbound-1");
    assert_eq!(inbox["peer_inbox"][0]["target_peer_id"], "device-b");
}
