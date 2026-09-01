use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use herdr_mission::{
    acknowledge_peer_message, bootstrap_database, configure_local_peer, create_mission,
    default_codex_team, deliver_peer_messages_with, notify_peer_inboxes, queue_peer_message,
    read_peer_inbox, receive_peer_envelope, reconcile_peer_relay, sha256_hex, upsert_peer,
    upsert_peer_route, CreateMissionRequest, DecisionContext, ErrorCategory, HandleDisposition,
    HandleInput, KernelInput, LaunchMode, MissionKernel, PeerEnvelopeV1, PeerPayloadV1,
    PeerReceipt, PeerSendRequest, PeerTransport, ProcessOutput, ProcessRunner, RoleKind, RoleRef,
};
use rusqlite::{types::Value, Connection};
use serde_json::json;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "herdr-mission-peer-{label}-{}-{id}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn add_mission(&self, mission_id: &str) {
        create_mission(&self.path, &mission_request(mission_id)).unwrap();
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
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(self.path.with_extension("sqlite3-shm"));
    }
}

fn mission_request(mission_id: &str) -> CreateMissionRequest {
    CreateMissionRequest {
        mission_id: mission_id.into(),
        brief: format!("Peer relay fixture for {mission_id}"),
        template: "general".into(),
        agent_profile_id: "codex-default-v1".into(),
        agent_profile_version: 1,
        launch_mode: LaunchMode::Manual,
        roles: default_codex_team(),
    }
}

fn table_columns(connection: &Connection, table: &str) -> Vec<String> {
    connection
        .prepare(&format!("PRAGMA table_info('{table}')"))
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn row_count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DurableCounts {
    assignments: i64,
    messages: i64,
    outbox: i64,
    context_ledger: i64,
    peer_messages: i64,
}

fn durable_counts(path: &Path) -> DurableCounts {
    let connection = Connection::open(path).unwrap();
    DurableCounts {
        assignments: row_count(&connection, "assignments"),
        messages: row_count(&connection, "messages"),
        outbox: row_count(&connection, "outbox"),
        context_ledger: row_count(&connection, "context_ledger"),
        peer_messages: row_count(&connection, "mission_peer_messages"),
    }
}

fn peer_message_snapshot(path: &Path) -> Vec<Vec<Value>> {
    let connection = Connection::open(path).unwrap();
    let mut statement = connection
        .prepare("SELECT * FROM mission_peer_messages ORDER BY message_id")
        .unwrap();
    let column_count = statement.column_count();
    statement
        .query_map([], |row| {
            (0..column_count)
                .map(|column| row.get(column))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn local_send(message_id: &str, source: &str, target: &str) -> PeerSendRequest {
    PeerSendRequest {
        message_id: message_id.into(),
        source_mission_id: source.into(),
        target_mission_id: target.into(),
        source_role: "pm".into(),
        peer_id: None,
        kind: "delegate".into(),
        body: "Break this work into local Assignments.".into(),
        in_reply_to: None,
    }
}

fn outbound_send(message_id: &str, source: &str, target: &str) -> PeerSendRequest {
    PeerSendRequest {
        peer_id: Some("device-b".into()),
        ..local_send(message_id, source, target)
    }
}

fn remote_envelope(message_id: &str, body: &str) -> PeerEnvelopeV1 {
    let payload = PeerPayloadV1 {
        protocol: "herdr-mission-peer-v1".into(),
        message_id: message_id.into(),
        source_peer_id: "device-a".into(),
        target_peer_id: "device-b".into(),
        source_mission_id: "msn-remote-source".into(),
        target_mission_id: "msn-local-target".into(),
        source_pm_generation: "generation-remote-pm-7".into(),
        kind: "delegate".into(),
        body: body.into(),
        in_reply_to: None,
        created_at: "2026-09-01T02:00:00Z".into(),
    };
    let payload_sha256 = sha256_hex(&serde_json::to_vec(&payload).unwrap());
    PeerEnvelopeV1 {
        payload,
        payload_sha256,
    }
}

fn configure_inbound_receiver(database: &TestDatabase) {
    database.add_mission("msn-local-target");
    configure_local_peer(&database.path, "device-b").unwrap();
    upsert_peer(&database.path, "device-a", "relay-a@example.test").unwrap();
    upsert_peer_route(
        &database.path,
        "device-a",
        "msn-local-target",
        "msn-remote-source",
        "inbound",
    )
    .unwrap();
}

#[test]
fn bootstrap_adds_exact_peer_schema_idempotently_without_changing_coordination_v3() {
    let database = TestDatabase::new("schema");

    bootstrap_database(&database.path).unwrap();
    bootstrap_database(&database.path).unwrap();

    let connection = Connection::open(&database.path).unwrap();
    let peer_objects = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE name LIKE 'mission_peer_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .unwrap();
    assert_eq!(
        peer_objects,
        BTreeSet::from([
            "mission_peer_identity".into(),
            "mission_peer_inbox_lookup".into(),
            "mission_peer_messages".into(),
            "mission_peer_notification_lookup".into(),
            "mission_peer_outbox_lookup".into(),
            "mission_peers".into(),
            "mission_peer_routes".into(),
            "mission_peer_schema_meta".into(),
        ])
    );
    assert_eq!(
        table_columns(&connection, "mission_peer_messages"),
        [
            "message_id",
            "direction",
            "source_peer_id",
            "target_peer_id",
            "source_mission_id",
            "target_mission_id",
            "source_pm_generation",
            "kind",
            "body",
            "in_reply_to",
            "payload_sha256",
            "state",
            "attempts",
            "last_error",
            "receipt_json",
            "claim_owner",
            "claimed_at",
            "next_attempt_at",
            "notify_attempts",
            "notify_last_error",
            "created_at",
            "updated_at",
            "received_at",
            "notified_at",
            "handled_at",
        ]
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "3"
    );
    assert_eq!(
        table_columns(&connection, "messages"),
        [
            "id",
            "mission_id",
            "assignment_id",
            "source_role",
            "target_role",
            "kind",
            "body",
            "context_rev",
            "in_reply_to",
            "review_id",
            "created_at",
        ]
    );
}

#[test]
fn bootstrap_rejects_a_malformed_same_name_peer_schema() {
    let database = TestDatabase::new("schema-conflict");
    bootstrap_database(&database.path).unwrap();
    let connection = Connection::open(&database.path).unwrap();
    connection
        .execute_batch(
            "DROP TABLE mission_peer_messages;
             DROP TABLE mission_peer_routes;
             DROP TABLE mission_peers;
             DROP TABLE mission_peer_identity;
             DROP TABLE mission_peer_schema_meta;
             CREATE TABLE mission_peer_messages(message_id TEXT PRIMARY KEY);",
        )
        .unwrap();
    drop(connection);

    let error = bootstrap_database(&database.path).unwrap_err();

    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(error.code, "incompatible_peer_schema");
    let connection = Connection::open(&database.path).unwrap();
    assert_eq!(
        table_columns(&connection, "mission_peer_messages"),
        ["message_id"]
    );
}

#[test]
fn bootstrap_rejects_peer_schema_with_changed_string_literal_semantics() {
    let database = TestDatabase::new("schema-literal-conflict");
    bootstrap_database(&database.path).unwrap();
    let connection = Connection::open(&database.path).unwrap();
    connection
        .execute_batch(
            "PRAGMA writable_schema = ON;
             UPDATE sqlite_master
             SET sql = replace(sql, '''queued''', '''QUEUED''')
             WHERE type = 'table' AND name = 'mission_peer_messages';
             PRAGMA writable_schema = OFF;",
        )
        .unwrap();
    drop(connection);

    let error = bootstrap_database(&database.path).unwrap_err();

    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(error.code, "incompatible_peer_schema");
}

#[test]
fn ordinary_and_peer_self_pm_sends_remain_denied_without_writes() {
    let database = TestDatabase::new("self-acl");
    database.add_mission("msn-one");
    configure_local_peer(&database.path, "device-a").unwrap();
    let before = durable_counts(&database.path);

    let error = queue_peer_message(
        &database.path,
        &local_send("peer-self-message", "msn-one", "msn-one"),
    )
    .unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(error.code, "acl_denied");
    assert!(!error.retryable);
    assert_eq!(durable_counts(&database.path), before);

    let mut non_pm = local_send("peer-worker-message", "msn-one", "msn-two");
    non_pm.source_role = "worker".into();
    let error = queue_peer_message(&database.path, &non_pm).unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(durable_counts(&database.path), before);

    let mut kernel = MissionKernel::in_memory("msn-one");
    let receipt = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-09-01T02:00:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::new(),
            },
            input: HandleInput::Command {
                command_id: "cmd-peer-self-pm".into(),
                kind: "task".into(),
                source: RoleRef {
                    role: RoleKind::Pm,
                    instance: None,
                },
                target: Some(RoleRef {
                    role: RoleKind::Pm,
                    instance: None,
                }),
                body: json!({"text": "must remain denied"}),
            },
        })
        .unwrap();
    assert_eq!(receipt.disposition, HandleDisposition::Rejected);
    assert_eq!(receipt.error.unwrap().code, "acl_denied");
}

#[test]
fn local_cross_mission_send_is_durable_and_only_the_target_pm_can_acknowledge() {
    let database = TestDatabase::new("local");
    database.add_mission("msn-source");
    database.add_mission("msn-target");
    configure_local_peer(&database.path, "device-a").unwrap();
    let before = durable_counts(&database.path);

    let first = queue_peer_message(
        &database.path,
        &local_send("peer-local-1", "msn-source", "msn-target"),
    )
    .unwrap();
    let duplicate = queue_peer_message(
        &database.path,
        &local_send("peer-local-1", "msn-source", "msn-target"),
    )
    .unwrap();

    assert_eq!(first.state, "accepted");
    assert!(!first.duplicate);
    assert!(duplicate.duplicate);
    assert_eq!(first.payload_sha256, duplicate.payload_sha256);
    let inbox = read_peer_inbox(&database.path, "msn-target", "pm").unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].message_id, "peer-local-1");
    assert_eq!(inbox[0].source_mission_id, "msn-source");
    assert_eq!(inbox[0].target_mission_id, "msn-target");
    assert_eq!(inbox[0].source_peer_id, "device-a");
    assert_eq!(inbox[0].target_peer_id, "device-a");
    assert_eq!(inbox[0].source_pm_generation, "generation-msn-source-pm");
    assert_eq!(inbox[0].body, "Break this work into local Assignments.");
    assert!(read_peer_inbox(&database.path, "msn-source", "pm")
        .unwrap()
        .is_empty());

    let after_send = durable_counts(&database.path);
    assert_eq!(after_send.assignments, before.assignments);
    assert_eq!(after_send.messages, before.messages);
    assert_eq!(after_send.outbox, before.outbox);
    assert_eq!(after_send.context_ledger, before.context_ledger);
    assert_eq!(after_send.peer_messages, before.peer_messages + 1);

    let error =
        acknowledge_peer_message(&database.path, "msn-source", "pm", "peer-local-1").unwrap_err();
    assert_eq!(error.code, "peer_ack_denied");
    assert_eq!(durable_counts(&database.path), after_send);

    assert!(acknowledge_peer_message(&database.path, "msn-target", "pm", "peer-local-1").unwrap());
    assert!(read_peer_inbox(&database.path, "msn-target", "pm")
        .unwrap()
        .is_empty());
    assert!(!acknowledge_peer_message(&database.path, "msn-target", "pm", "peer-local-1").unwrap());
}

#[test]
fn tampered_inbox_body_is_rejected_without_changing_the_database() {
    let database = TestDatabase::new("tampered-inbox-body");
    database.add_mission("msn-source");
    database.add_mission("msn-target");
    configure_local_peer(&database.path, "device-a").unwrap();
    queue_peer_message(
        &database.path,
        &local_send("peer-local-tampered", "msn-source", "msn-target"),
    )
    .unwrap();
    Connection::open(&database.path)
        .unwrap()
        .execute(
            "UPDATE mission_peer_messages SET body = 'Untrusted replacement body.'
             WHERE message_id = 'peer-local-tampered'",
            [],
        )
        .unwrap();
    let before = peer_message_snapshot(&database.path);

    let error = read_peer_inbox(&database.path, "msn-target", "pm").unwrap_err();

    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(error.code, "peer_payload_corrupt");
    assert!(!error.retryable);
    assert_eq!(peer_message_snapshot(&database.path), before);
}

#[test]
fn peer_identity_destination_and_route_are_explicit_and_fail_closed() {
    let database = TestDatabase::new("route");
    database.add_mission("msn-source");
    configure_local_peer(&database.path, "device-a").unwrap();
    configure_local_peer(&database.path, "device-a").unwrap();

    let error = upsert_peer(
        &database.path,
        "device-b",
        "-oProxyCommand=touch /tmp/peer-owned",
    )
    .unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert!(!error.retryable);
    let connection = Connection::open(&database.path).unwrap();
    assert_eq!(row_count(&connection, "mission_peers"), 0);
    drop(connection);

    upsert_peer(&database.path, "device-b", "relay-b@example.test").unwrap();
    upsert_peer_route(
        &database.path,
        "device-b",
        "msn-source",
        "msn-remote-target",
        "outbound",
    )
    .unwrap();

    let queued = queue_peer_message(
        &database.path,
        &outbound_send("peer-outbound-route", "msn-source", "msn-remote-target"),
    )
    .unwrap();
    assert_eq!(queued.state, "queued");

    let before_identity_change = durable_counts(&database.path);
    let error = configure_local_peer(&database.path, "device-c").unwrap_err();
    assert_eq!(error.code, "peer_identity_in_use");
    assert_eq!(durable_counts(&database.path), before_identity_change);

    let mut wrong_pair = outbound_send(
        "peer-outbound-wrong-route",
        "msn-source",
        "msn-other-remote-target",
    );
    wrong_pair.body = "This pair was never authorized.".into();
    let before = durable_counts(&database.path);
    let error = queue_peer_message(&database.path, &wrong_pair).unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(durable_counts(&database.path), before);
}

#[test]
fn remote_receive_commits_before_receipt_and_is_idempotent_by_id_and_digest() {
    let database = TestDatabase::new("receive");
    configure_inbound_receiver(&database);
    let envelope = remote_envelope("peer-inbound-1", "Please own the local slice.");
    let bytes = serde_json::to_vec(&envelope).unwrap();

    let accepted = receive_peer_envelope(&database.path, "device-a", &bytes).unwrap();
    assert_eq!(accepted.status, "accepted");
    assert_eq!(accepted.message_id, "peer-inbound-1");
    assert_eq!(accepted.payload_sha256, envelope.payload_sha256);
    assert_eq!(
        read_peer_inbox(&database.path, "msn-local-target", "pm")
            .unwrap()
            .len(),
        1
    );

    let duplicate = receive_peer_envelope(&database.path, "device-a", &bytes).unwrap();
    assert_eq!(duplicate.status, "duplicate");
    assert_eq!(durable_counts(&database.path).peer_messages, 1);

    let conflicting = remote_envelope("peer-inbound-1", "A different payload.");
    let before = durable_counts(&database.path);
    let error = receive_peer_envelope(
        &database.path,
        "device-a",
        &serde_json::to_vec(&conflicting).unwrap(),
    )
    .unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(error.code, "peer_message_id_conflict");
    assert_eq!(durable_counts(&database.path), before);
}

#[test]
fn receive_replay_rejects_tampered_created_at_without_changing_the_database() {
    let database = TestDatabase::new("tampered-receive-created-at");
    configure_inbound_receiver(&database);
    let envelope = remote_envelope(
        "peer-inbound-tampered-created-at",
        "Persist this exact envelope.",
    );
    let bytes = serde_json::to_vec(&envelope).unwrap();
    assert_eq!(
        receive_peer_envelope(&database.path, "device-a", &bytes)
            .unwrap()
            .status,
        "accepted"
    );
    Connection::open(&database.path)
        .unwrap()
        .execute(
            "UPDATE mission_peer_messages SET created_at = '2026-09-01T02:00:01Z'
             WHERE message_id = 'peer-inbound-tampered-created-at'",
            [],
        )
        .unwrap();
    let before = peer_message_snapshot(&database.path);

    let error = receive_peer_envelope(&database.path, "device-a", &bytes).unwrap_err();

    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(error.code, "peer_payload_corrupt");
    assert!(!error.retryable);
    assert_eq!(peer_message_snapshot(&database.path), before);
}

#[test]
fn exact_receive_replay_returns_duplicate_after_the_route_is_disabled() {
    let database = TestDatabase::new("receive-replay-after-route-change");
    configure_inbound_receiver(&database);
    let envelope = remote_envelope(
        "peer-inbound-route-replay",
        "Committed before route change.",
    );
    let bytes = serde_json::to_vec(&envelope).unwrap();
    assert_eq!(
        receive_peer_envelope(&database.path, "device-a", &bytes)
            .unwrap()
            .status,
        "accepted"
    );
    Connection::open(&database.path)
        .unwrap()
        .execute(
            "UPDATE mission_peer_routes SET enabled = 0 WHERE peer_id = 'device-a'",
            [],
        )
        .unwrap();

    let duplicate = receive_peer_envelope(&database.path, "device-a", &bytes).unwrap();

    assert_eq!(duplicate.status, "duplicate");
    assert_eq!(durable_counts(&database.path).peer_messages, 1);
}

#[test]
fn inbound_reply_can_reference_the_reverse_outbound_peer_message() {
    let database = TestDatabase::new("inbound-reply");
    configure_inbound_receiver(&database);
    upsert_peer_route(
        &database.path,
        "device-a",
        "msn-local-target",
        "msn-remote-source",
        "bidirectional",
    )
    .unwrap();
    let outbound = PeerSendRequest {
        message_id: "peer-outbound-question".into(),
        source_mission_id: "msn-local-target".into(),
        target_mission_id: "msn-remote-source".into(),
        source_role: "pm".into(),
        peer_id: Some("device-a".into()),
        kind: "delegate".into(),
        body: "Please take the remote slice.".into(),
        in_reply_to: None,
    };
    queue_peer_message(&database.path, &outbound).unwrap();

    let mut reply = remote_envelope("peer-inbound-answer", "The remote slice is complete.");
    reply.payload.kind = "result".into();
    reply.payload.in_reply_to = Some(outbound.message_id);
    reply.payload_sha256 = sha256_hex(&serde_json::to_vec(&reply.payload).unwrap());

    let receipt = receive_peer_envelope(
        &database.path,
        "device-a",
        &serde_json::to_vec(&reply).unwrap(),
    )
    .unwrap();

    assert_eq!(receipt.status, "accepted");
    let inbox = read_peer_inbox(&database.path, "msn-local-target", "pm").unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(
        inbox[0].in_reply_to.as_deref(),
        Some("peer-outbound-question")
    );
}

#[test]
fn remote_receive_rejects_identity_route_digest_shape_and_size_errors_without_writes() {
    let database = TestDatabase::new("receive-reject");
    configure_inbound_receiver(&database);
    let envelope = remote_envelope("peer-invalid-1", "Untrusted relay input.");
    let before = durable_counts(&database.path);

    let error = receive_peer_envelope(
        &database.path,
        "device-c",
        &serde_json::to_vec(&envelope).unwrap(),
    )
    .unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(durable_counts(&database.path), before);

    let mut digest_mismatch = envelope.clone();
    digest_mismatch.payload_sha256 = "0".repeat(64);
    let error = receive_peer_envelope(
        &database.path,
        "device-a",
        &serde_json::to_vec(&digest_mismatch).unwrap(),
    )
    .unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(durable_counts(&database.path), before);

    let mut unknown_field = serde_json::to_value(&envelope).unwrap();
    unknown_field["unexpected"] = json!(true);
    let error = receive_peer_envelope(
        &database.path,
        "device-a",
        &serde_json::to_vec(&unknown_field).unwrap(),
    )
    .unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(durable_counts(&database.path), before);

    let mut oversized_body = remote_envelope("peer-invalid-body", &"x".repeat(64 * 1024 + 1));
    oversized_body.payload_sha256 =
        sha256_hex(&serde_json::to_vec(&oversized_body.payload).unwrap());
    let error = receive_peer_envelope(
        &database.path,
        "device-a",
        &serde_json::to_vec(&oversized_body).unwrap(),
    )
    .unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(durable_counts(&database.path), before);

    let error =
        receive_peer_envelope(&database.path, "device-a", &vec![b' '; 256 * 1024 + 1]).unwrap_err();
    assert_eq!(error.code, "peer_envelope_too_large");
    assert_eq!(durable_counts(&database.path), before);

    let connection = Connection::open(&database.path).unwrap();
    connection
        .execute(
            "UPDATE mission_peer_routes SET enabled = 0 WHERE peer_id = 'device-a'",
            [],
        )
        .unwrap();
    drop(connection);
    let error = receive_peer_envelope(
        &database.path,
        "device-a",
        &serde_json::to_vec(&envelope).unwrap(),
    )
    .unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(durable_counts(&database.path), before);
}

enum TransportStep {
    ExitFailure,
    Receipt(&'static str),
}

struct FakePeerTransport {
    database: PathBuf,
    steps: Mutex<VecDeque<TransportStep>>,
    calls: Mutex<Vec<(String, Vec<u8>)>>,
}

impl FakePeerTransport {
    fn new(database: &Path, steps: impl IntoIterator<Item = TransportStep>) -> Self {
        Self {
            database: database.into(),
            steps: Mutex::new(steps.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl PeerTransport for FakePeerTransport {
    fn send(&self, destination: &str, envelope: &[u8]) -> std::io::Result<ProcessOutput> {
        let persisted: (String, String) = Connection::open(&self.database)
            .unwrap()
            .query_row(
                "SELECT direction, state FROM mission_peer_messages
                 WHERE message_id = 'peer-transport-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(persisted.0, "outbound");
        assert!(matches!(
            persisted.1.as_str(),
            "queued" | "sending" | "retry"
        ));
        self.calls
            .lock()
            .unwrap()
            .push((destination.into(), envelope.to_vec()));

        match self.steps.lock().unwrap().pop_front().unwrap() {
            TransportStep::ExitFailure => Ok(ProcessOutput {
                exit_code: 255,
                stdout: String::new(),
                stderr: "connection reset after remote commit".into(),
            }),
            TransportStep::Receipt(status) => {
                let envelope: PeerEnvelopeV1 = serde_json::from_slice(envelope).unwrap();
                Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: serde_json::to_string(&PeerReceipt {
                        status: status.into(),
                        message_id: envelope.payload.message_id,
                        payload_sha256: envelope.payload_sha256,
                    })
                    .unwrap(),
                    stderr: String::new(),
                })
            }
        }
    }
}

#[test]
fn outbound_is_durable_before_network_and_receipt_loss_retries_the_same_envelope() {
    let database = TestDatabase::new("transport");
    database.add_mission("msn-source");
    configure_local_peer(&database.path, "device-a").unwrap();
    upsert_peer(&database.path, "device-b", "relay-b@example.test").unwrap();
    upsert_peer_route(
        &database.path,
        "device-b",
        "msn-source",
        "msn-remote-target",
        "outbound",
    )
    .unwrap();
    let queued = queue_peer_message(
        &database.path,
        &outbound_send("peer-transport-1", "msn-source", "msn-remote-target"),
    )
    .unwrap();
    assert_eq!(queued.state, "queued");

    let transport = FakePeerTransport::new(
        &database.path,
        [
            TransportStep::ExitFailure,
            TransportStep::Receipt("duplicate"),
        ],
    );
    let first = deliver_peer_messages_with(&database.path, &transport).unwrap();
    assert_eq!(first.retried, 1);
    let connection = Connection::open(&database.path).unwrap();
    let state: String = connection
        .query_row(
            "SELECT state FROM mission_peer_messages WHERE message_id = 'peer-transport-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "retry");
    connection
        .execute(
            "UPDATE mission_peer_messages SET next_attempt_at = 0
             WHERE message_id = 'peer-transport-1'",
            [],
        )
        .unwrap();
    drop(connection);

    let second = deliver_peer_messages_with(&database.path, &transport).unwrap();
    assert_eq!(second.sent, 1);
    let calls = transport.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "relay-b@example.test");
    assert_eq!(calls[0].0, calls[1].0);
    assert_eq!(calls[0].1, calls[1].1);
    assert!(!calls[0].0.contains("Break this work"));
    let envelope: PeerEnvelopeV1 = serde_json::from_slice(&calls[0].1).unwrap();
    assert_eq!(
        envelope.payload.body,
        "Break this work into local Assignments."
    );
    drop(calls);

    let connection = Connection::open(&database.path).unwrap();
    let acknowledged: (String, i64, String) = connection
        .query_row(
            "SELECT state, attempts, receipt_json FROM mission_peer_messages
             WHERE message_id = 'peer-transport-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(acknowledged.0, "acknowledged");
    assert_eq!(acknowledged.1, 2);
    assert!(acknowledged.2.contains("duplicate"));
}

struct DestinationAwareTransport {
    calls: Mutex<Vec<String>>,
}

impl PeerTransport for DestinationAwareTransport {
    fn send(&self, destination: &str, envelope: &[u8]) -> std::io::Result<ProcessOutput> {
        self.calls.lock().unwrap().push(destination.into());
        if destination == "offline@example.test" {
            return Ok(ProcessOutput {
                exit_code: 255,
                stdout: String::new(),
                stderr: "peer unavailable".into(),
            });
        }
        let envelope: PeerEnvelopeV1 = serde_json::from_slice(envelope).unwrap();
        Ok(ProcessOutput {
            exit_code: 0,
            stdout: serde_json::to_string(&PeerReceipt {
                status: "accepted".into(),
                message_id: envelope.payload.message_id,
                payload_sha256: envelope.payload_sha256,
            })
            .unwrap(),
            stderr: String::new(),
        })
    }
}

#[test]
fn one_unreachable_peer_does_not_block_an_independent_peer_delivery() {
    let database = TestDatabase::new("independent-peers");
    database.add_mission("msn-source");
    configure_local_peer(&database.path, "device-a").unwrap();
    for (peer_id, destination, target) in [
        (
            "device-offline",
            "offline@example.test",
            "msn-remote-offline",
        ),
        ("device-online", "online@example.test", "msn-remote-online"),
    ] {
        upsert_peer(&database.path, peer_id, destination).unwrap();
        upsert_peer_route(&database.path, peer_id, "msn-source", target, "outbound").unwrap();
    }
    for (message_id, peer_id, target) in [
        (
            "peer-multi-1-offline",
            "device-offline",
            "msn-remote-offline",
        ),
        ("peer-multi-2-online", "device-online", "msn-remote-online"),
    ] {
        let mut request = local_send(message_id, "msn-source", target);
        request.peer_id = Some(peer_id.into());
        queue_peer_message(&database.path, &request).unwrap();
    }

    let transport = DestinationAwareTransport {
        calls: Mutex::new(Vec::new()),
    };
    let report = deliver_peer_messages_with(&database.path, &transport).unwrap();

    assert_eq!((report.sent, report.retried), (1, 1));
    assert_eq!(
        transport.calls.lock().unwrap().as_slice(),
        ["offline@example.test", "online@example.test"]
    );
    let connection = Connection::open(&database.path).unwrap();
    let states = connection
        .prepare(
            "SELECT message_id, state FROM mission_peer_messages
             ORDER BY message_id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        states,
        [
            ("peer-multi-1-offline".into(), "retry".into()),
            ("peer-multi-2-online".into(), "acknowledged".into()),
        ]
    );
}

struct MismatchedReceiptTransport;

impl PeerTransport for MismatchedReceiptTransport {
    fn send(&self, _destination: &str, envelope: &[u8]) -> std::io::Result<ProcessOutput> {
        let envelope: PeerEnvelopeV1 = serde_json::from_slice(envelope).unwrap();
        Ok(ProcessOutput {
            exit_code: 0,
            stdout: serde_json::to_string(&PeerReceipt {
                status: "accepted".into(),
                message_id: "peer-wrong-receipt".into(),
                payload_sha256: envelope.payload_sha256,
            })
            .unwrap(),
            stderr: String::new(),
        })
    }
}

#[test]
fn receipt_mismatch_keeps_the_outbound_durably_retryable() {
    let database = TestDatabase::new("receipt-mismatch");
    database.add_mission("msn-source");
    configure_local_peer(&database.path, "device-a").unwrap();
    upsert_peer(&database.path, "device-b", "relay-b@example.test").unwrap();
    upsert_peer_route(
        &database.path,
        "device-b",
        "msn-source",
        "msn-remote-target",
        "outbound",
    )
    .unwrap();
    queue_peer_message(
        &database.path,
        &outbound_send("peer-receipt-mismatch", "msn-source", "msn-remote-target"),
    )
    .unwrap();

    let report = deliver_peer_messages_with(&database.path, &MismatchedReceiptTransport).unwrap();

    assert_eq!((report.sent, report.retried), (0, 1));
    let connection = Connection::open(&database.path).unwrap();
    let persisted: (String, String, Option<String>) = connection
        .query_row(
            "SELECT state, last_error, receipt_json FROM mission_peer_messages
             WHERE message_id = 'peer-receipt-mismatch'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        persisted,
        ("retry".into(), "peer_receipt_invalid".into(), None)
    );
}

#[test]
fn corrupt_outbound_does_not_block_later_delivery_or_inbox_wake() {
    let database = TestDatabase::new("corrupt-outbound-isolation");
    database.add_mission("msn-source");
    database.add_mission("msn-local-target");
    configure_local_peer(&database.path, "device-a").unwrap();
    upsert_peer(&database.path, "device-online", "online@example.test").unwrap();
    upsert_peer_route(
        &database.path,
        "device-online",
        "msn-source",
        "msn-remote-target",
        "outbound",
    )
    .unwrap();
    for message_id in ["peer-corrupt-1", "peer-corrupt-2"] {
        let mut request = local_send(message_id, "msn-source", "msn-remote-target");
        request.peer_id = Some("device-online".into());
        queue_peer_message(&database.path, &request).unwrap();
    }
    queue_peer_message(
        &database.path,
        &local_send("peer-corrupt-wake", "msn-source", "msn-local-target"),
    )
    .unwrap();
    Connection::open(&database.path)
        .unwrap()
        .execute(
            "UPDATE mission_peer_messages SET payload_sha256 = ?1
             WHERE message_id = 'peer-corrupt-1'",
            ["0".repeat(64)],
        )
        .unwrap();
    let transport = DestinationAwareTransport {
        calls: Mutex::new(Vec::new()),
    };
    let runner = FakeWakeRunner {
        output: ProcessOutput {
            exit_code: 0,
            stdout: serde_json::to_string(&json!({
                "result": {
                    "type": "agent_prompted",
                    "agent": {
                        "name": "agent-msn-local-target-pm",
                        "pane_id": "pane-msn-local-target-pm",
                        "state_change_seq": 1
                    }
                }
            }))
            .unwrap(),
            stderr: String::new(),
        },
        calls: Mutex::new(Vec::new()),
    };

    let report = reconcile_peer_relay(&database.path, &transport, &runner, "herdr").unwrap();

    assert_eq!((report.sent, report.retried, report.notified), (1, 1, 1));
    assert_eq!(
        transport.calls.lock().unwrap().as_slice(),
        ["online@example.test"]
    );
    let connection = Connection::open(&database.path).unwrap();
    let states = connection
        .prepare(
            "SELECT message_id, state FROM mission_peer_messages
             WHERE message_id IN ('peer-corrupt-1', 'peer-corrupt-2')
             ORDER BY message_id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        states,
        [
            ("peer-corrupt-1".into(), "retry".into()),
            ("peer-corrupt-2".into(), "acknowledged".into()),
        ]
    );
}

struct FakeWakeRunner {
    output: ProcessOutput,
    calls: Mutex<Vec<(String, Vec<String>)>>,
}

impl ProcessRunner for FakeWakeRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<ProcessOutput> {
        self.calls
            .lock()
            .unwrap()
            .push((program.into(), args.to_vec()));
        Ok(self.output.clone())
    }
}

#[test]
fn wake_failure_preserves_the_unhandled_inbox_for_retry_and_reopen() {
    let database = TestDatabase::new("wake");
    database.add_mission("msn-source");
    database.add_mission("msn-target");
    configure_local_peer(&database.path, "device-a").unwrap();
    queue_peer_message(
        &database.path,
        &local_send("peer-wake-1", "msn-source", "msn-target"),
    )
    .unwrap();

    let failing = FakeWakeRunner {
        output: ProcessOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "target pane not found".into(),
        },
        calls: Mutex::new(Vec::new()),
    };
    let report = notify_peer_inboxes(&database.path, &failing, "herdr").unwrap();
    assert_eq!(report.notify_failed, 1);
    assert_eq!(
        read_peer_inbox(&database.path, "msn-target", "pm")
            .unwrap()
            .len(),
        1
    );
    let connection = Connection::open(&database.path).unwrap();
    let failed: (String, i64, Option<String>) = connection
        .query_row(
            "SELECT state, notify_attempts, notified_at FROM mission_peer_messages
             WHERE message_id = 'peer-wake-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(failed, ("accepted".into(), 1, None));
    drop(connection);

    let succeeding = FakeWakeRunner {
        output: ProcessOutput {
            exit_code: 0,
            stdout: serde_json::to_string(&json!({
                "result": {
                    "type": "agent_prompted",
                    "agent": {
                        "name": "agent-msn-target-pm",
                        "pane_id": "pane-msn-target-pm",
                        "state_change_seq": 1
                    }
                }
            }))
            .unwrap(),
            stderr: String::new(),
        },
        calls: Mutex::new(Vec::new()),
    };
    let report = notify_peer_inboxes(&database.path, &succeeding, "herdr").unwrap();
    assert_eq!(report.notified, 1);
    assert_eq!(
        read_peer_inbox(&database.path, "msn-target", "pm")
            .unwrap()
            .len(),
        1
    );
    let connection = Connection::open(&database.path).unwrap();
    let notified_at: Option<String> = connection
        .query_row(
            "SELECT notified_at FROM mission_peer_messages WHERE message_id = 'peer-wake-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(notified_at.is_some());
}
