//! Durable cross-Mission PM relay for local and SSH-connected Herdr devices.
//!
//! Peer messages deliberately live outside the coordination-v3 `messages`
//! tables: a remote PM is not a role inside the target Mission. SQLite is the
//! durable source of truth; Herdr prompts only wake the target PM.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    bootstrap_database, open_writable, utc_timestamp, ErrorCategory, KernelError, ProcessOutput,
    ProcessRunner, OWNER_IDENTITY,
};

const PEER_SCHEMA_VERSION: u32 = 1;
const PEER_PROTOCOL: &str = "herdr-mission-peer-v1";
const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_PEER_ENVELOPE_BYTES: usize = 256 * 1024;
const CLAIM_LEASE_MS: i64 = 60_000;
const SSH_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const SSH_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_PEER_PROCESS_OUTPUT_BYTES: u64 = 64 * 1024;

const PEER_SCHEMA_DDL: &str = r#"
CREATE TABLE mission_peer_schema_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE mission_peer_identity (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    local_peer_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE mission_peers (
    peer_id TEXT PRIMARY KEY,
    ssh_destination TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE mission_peer_routes (
    peer_id TEXT NOT NULL,
    local_mission_id TEXT NOT NULL,
    remote_mission_id TEXT NOT NULL,
    direction TEXT NOT NULL CHECK(direction IN ('inbound', 'outbound', 'bidirectional')),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (peer_id, local_mission_id, remote_mission_id),
    FOREIGN KEY (peer_id) REFERENCES mission_peers(peer_id) ON DELETE CASCADE,
    FOREIGN KEY (local_mission_id) REFERENCES team_missions(mission_id) ON DELETE CASCADE
);

CREATE TABLE mission_peer_messages (
    message_id TEXT PRIMARY KEY,
    direction TEXT NOT NULL CHECK(direction IN ('local', 'outbound', 'inbound')),
    source_peer_id TEXT NOT NULL,
    target_peer_id TEXT NOT NULL,
    source_mission_id TEXT NOT NULL,
    target_mission_id TEXT NOT NULL,
    source_pm_generation TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('delegate', 'context', 'result', 'blocked')),
    body TEXT NOT NULL,
    in_reply_to TEXT,
    payload_sha256 TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN (
        'queued', 'sending', 'retry', 'acknowledged', 'accepted', 'handled'
    )),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
    last_error TEXT NOT NULL DEFAULT '',
    receipt_json TEXT,
    claim_owner TEXT NOT NULL DEFAULT '',
    claimed_at INTEGER,
    next_attempt_at INTEGER,
    notify_attempts INTEGER NOT NULL DEFAULT 0 CHECK(notify_attempts >= 0),
    notify_last_error TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    received_at TEXT,
    notified_at TEXT,
    handled_at TEXT,
    CHECK(source_mission_id <> target_mission_id),
    CHECK(length(payload_sha256) = 64),
    CHECK(
        (direction = 'outbound' AND state IN ('queued', 'sending', 'retry', 'acknowledged'))
        OR (direction IN ('local', 'inbound') AND state IN ('accepted', 'handled'))
    )
);

CREATE INDEX mission_peer_outbox_lookup
    ON mission_peer_messages(direction, state, next_attempt_at, created_at, message_id);
CREATE INDEX mission_peer_inbox_lookup
    ON mission_peer_messages(target_mission_id, direction, state, created_at, message_id);
CREATE INDEX mission_peer_notification_lookup
    ON mission_peer_messages(direction, state, notified_at, target_mission_id, message_id);
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSendRequest {
    pub message_id: String,
    pub source_mission_id: String,
    pub target_mission_id: String,
    pub source_role: String,
    pub peer_id: Option<String>,
    pub kind: String,
    pub body: String,
    pub in_reply_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerSendOutcome {
    pub message_id: String,
    pub payload_sha256: String,
    pub state: String,
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerInboxMessage {
    pub message_id: String,
    pub source_peer_id: String,
    pub target_peer_id: String,
    pub source_mission_id: String,
    pub target_mission_id: String,
    pub source_pm_generation: String,
    pub kind: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub payload_sha256: String,
    pub received_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerPayloadV1 {
    pub protocol: String,
    pub message_id: String,
    pub source_peer_id: String,
    pub target_peer_id: String,
    pub source_mission_id: String,
    pub target_mission_id: String,
    pub source_pm_generation: String,
    pub kind: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerEnvelopeV1 {
    pub payload: PeerPayloadV1,
    pub payload_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerReceipt {
    pub status: String,
    pub message_id: String,
    pub payload_sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PeerRelayReport {
    pub sent: u32,
    pub retried: u32,
    pub notified: u32,
    pub notify_failed: u32,
}

pub trait PeerTransport {
    fn send(&self, destination: &str, envelope: &[u8]) -> std::io::Result<ProcessOutput>;
}

pub struct SystemSshPeerTransport;

impl PeerTransport for SystemSshPeerTransport {
    fn send(&self, destination: &str, envelope: &[u8]) -> std::io::Result<ProcessOutput> {
        let child = Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=1",
                "--",
                destination,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        run_peer_child(child, envelope, SSH_TOTAL_TIMEOUT)
    }
}

fn run_peer_child(
    mut child: Child,
    envelope: &[u8],
    timeout: Duration,
) -> std::io::Result<ProcessOutput> {
    let mut stdin = child
        .stdin
        .take()
        .expect("piped peer stdin must be available");
    let stdout = child
        .stdout
        .take()
        .expect("piped peer stdout must be available");
    let stderr = child
        .stderr
        .take()
        .expect("piped peer stderr must be available");
    let input = envelope.to_vec();
    let stdin_task = thread::spawn(move || stdin.write_all(&input));
    let stdout_task = thread::spawn(move || read_bounded_process_output(stdout));
    let stderr_task = thread::spawn(move || read_bounded_process_output(stderr));

    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break (child.wait()?, true);
        }
        thread::sleep(SSH_POLL_INTERVAL);
    };

    let stdin_result = join_peer_io_task(stdin_task, "peer stdin writer")?;
    let stdout = join_peer_io_task(stdout_task, "peer stdout reader")??;
    let stderr = join_peer_io_task(stderr_task, "peer stderr reader")??;
    if timed_out {
        return Ok(ProcessOutput {
            exit_code: 124,
            stdout: String::new(),
            stderr: "peer transport timed out".into(),
        });
    }
    stdin_result?;
    Ok(ProcessOutput {
        exit_code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn read_bounded_process_output(reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader
        .take(MAX_PEER_PROCESS_OUTPUT_BYTES.saturating_add(1))
        .read_to_end(&mut output)?;
    Ok(output)
}

fn join_peer_io_task<T>(task: thread::JoinHandle<T>, name: &str) -> std::io::Result<T> {
    task.join()
        .map_err(|_| std::io::Error::other(format!("{name} panicked")))
}

pub(crate) fn migrate_peer_schema(connection: &Connection) -> Result<(), KernelError> {
    let actual = peer_schema_objects(connection)?;
    if actual.is_empty() {
        connection
            .execute_batch(PEER_SCHEMA_DDL)
            .map_err(|error| peer_sqlite_error("create_peer_schema", error))?;
        connection
            .execute(
                "INSERT INTO mission_peer_schema_meta(key, value) VALUES('schema_version', ?1)",
                [PEER_SCHEMA_VERSION.to_string()],
            )
            .map_err(|error| peer_sqlite_error("write_peer_schema_version", error))?;
        return validate_peer_schema(connection);
    }
    validate_peer_schema(connection)
}

fn validate_peer_schema(connection: &Connection) -> Result<(), KernelError> {
    let reference = Connection::open_in_memory()
        .map_err(|error| peer_sqlite_error("open_peer_reference_schema", error))?;
    reference
        .execute_batch(PEER_SCHEMA_DDL)
        .map_err(|error| peer_sqlite_error("create_peer_reference_schema", error))?;
    let expected = peer_schema_objects(&reference)?;
    let actual = peer_schema_objects(connection)?;
    if actual != expected {
        return Err(peer_contract_error(
            "incompatible_peer_schema",
            "peer schema objects do not match version 1",
            BTreeMap::from([
                ("actual".into(), json!(actual)),
                ("expected".into(), json!(expected)),
            ]),
        ));
    }
    let entries = connection
        .prepare("SELECT key, value FROM mission_peer_schema_meta ORDER BY key")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|error| peer_sqlite_error("read_peer_schema_version", error))?;
    if entries != vec![("schema_version".into(), PEER_SCHEMA_VERSION.to_string())] {
        return Err(peer_contract_error(
            "incompatible_peer_schema",
            "peer schema version marker is invalid",
            BTreeMap::from([("entries".into(), json!(entries))]),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct PeerSchemaObject {
    kind: String,
    name: String,
    table: String,
    sql: Option<String>,
}

fn peer_schema_objects(connection: &Connection) -> Result<BTreeSet<PeerSchemaObject>, KernelError> {
    connection
        .prepare(
            "SELECT type, name, tbl_name, sql
             FROM sqlite_master
             WHERE substr(name, 1, 13) COLLATE NOCASE = 'mission_peer_'
                OR substr(tbl_name, 1, 13) COLLATE NOCASE = 'mission_peer_'
             ORDER BY type, name",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok(PeerSchemaObject {
                        kind: row.get(0)?,
                        name: row.get(1)?,
                        table: row.get(2)?,
                        sql: row
                            .get::<_, Option<String>>(3)?
                            .map(|sql| normalize_sql(&sql)),
                    })
                })?
                .collect::<rusqlite::Result<BTreeSet<_>>>()
        })
        .map_err(|error| peer_sqlite_error("inspect_peer_schema", error))
}

fn normalize_sql(sql: &str) -> String {
    let mut chars = sql.chars().peekable();
    let mut tokens = Vec::new();

    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '-' && chars.peek() == Some(&'-') {
            chars.next();
            for comment in chars.by_ref() {
                if comment == '\n' {
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut previous = '\0';
            for comment in chars.by_ref() {
                if previous == '*' && comment == '/' {
                    break;
                }
                previous = comment;
            }
            continue;
        }
        if ch == '\'' {
            let mut token = String::from(ch);
            while let Some(quoted) = chars.next() {
                token.push(quoted);
                if quoted == '\'' {
                    if chars.peek() == Some(&'\'') {
                        token.push(chars.next().expect("peeked escaped quote"));
                    } else {
                        break;
                    }
                }
            }
            tokens.push(token);
            continue;
        }
        if ch == '"' || ch == '`' {
            let delimiter = ch;
            let mut identifier = String::new();
            while let Some(quoted) = chars.next() {
                if quoted == delimiter {
                    if chars.peek() == Some(&delimiter) {
                        identifier.push(chars.next().expect("peeked escaped identifier quote"));
                    } else {
                        break;
                    }
                } else {
                    identifier.push(quoted);
                }
            }
            tokens.push(identifier.to_ascii_uppercase());
            continue;
        }
        if ch == '[' {
            let mut identifier = String::new();
            for quoted in chars.by_ref() {
                if quoted == ']' {
                    break;
                }
                identifier.push(quoted);
            }
            tokens.push(identifier.to_ascii_uppercase());
            continue;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            let mut token = String::from(ch);
            while matches!(
                chars.peek(),
                Some(next) if next.is_ascii_alphanumeric() || *next == '_' || *next == '$'
            ) {
                token.push(chars.next().expect("peeked SQL token character"));
            }
            tokens.push(token.to_ascii_uppercase());
            continue;
        }
        tokens.push(ch.to_string());
    }

    tokens.join(" ")
}

pub fn configure_local_peer(database: &Path, local_peer_id: &str) -> Result<(), KernelError> {
    validate_identifier("local_peer_id", local_peer_id)?;
    bootstrap_database(database)?;
    let mut connection = open_writable(database, OWNER_IDENTITY)?;
    let transaction = begin_peer_transaction(&mut connection, "configure_local_peer")?;
    let current = transaction
        .query_row(
            "SELECT local_peer_id FROM mission_peer_identity WHERE singleton = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| peer_sqlite_error("read_local_peer_identity", error))?;
    match current {
        Some(current) if current == local_peer_id => {}
        Some(current) => {
            let messages: i64 = transaction
                .query_row("SELECT COUNT(*) FROM mission_peer_messages", [], |row| {
                    row.get(0)
                })
                .map_err(|error| peer_sqlite_error("count_peer_messages", error))?;
            if messages != 0 {
                return Err(peer_contract_error(
                    "peer_identity_in_use",
                    "local peer identity cannot change while durable peer messages exist",
                    BTreeMap::from([
                        ("current_peer_id".into(), json!(current)),
                        ("requested_peer_id".into(), json!(local_peer_id)),
                        ("message_count".into(), json!(messages)),
                    ]),
                ));
            }
            transaction
                .execute(
                    "UPDATE mission_peer_identity
                     SET local_peer_id = ?1, updated_at = ?2 WHERE singleton = 1",
                    params![local_peer_id, utc_timestamp()],
                )
                .map_err(|error| peer_sqlite_error("update_local_peer_identity", error))?;
        }
        None => {
            let now = utc_timestamp();
            transaction
                .execute(
                    "INSERT INTO mission_peer_identity(
                         singleton, local_peer_id, created_at, updated_at
                     ) VALUES(1, ?1, ?2, ?2)",
                    params![local_peer_id, now],
                )
                .map_err(|error| peer_sqlite_error("insert_local_peer_identity", error))?;
        }
    }
    commit_peer_transaction(transaction, "configure_local_peer")
}

pub fn upsert_peer(
    database: &Path,
    peer_id: &str,
    ssh_destination: &str,
) -> Result<(), KernelError> {
    validate_identifier("peer_id", peer_id)?;
    validate_ssh_destination(ssh_destination)?;
    bootstrap_database(database)?;
    let mut connection = open_writable(database, OWNER_IDENTITY)?;
    let transaction = begin_peer_transaction(&mut connection, "upsert_peer")?;
    let local = require_local_peer(&transaction)?;
    if local == peer_id {
        return Err(peer_contract_error(
            "peer_identity_conflict",
            "remote peer ID must differ from the local peer ID",
            BTreeMap::from([("peer_id".into(), json!(peer_id))]),
        ));
    }
    let now = utc_timestamp();
    transaction
        .execute(
            "INSERT INTO mission_peers(
                 peer_id, ssh_destination, enabled, created_at, updated_at
             ) VALUES(?1, ?2, 1, ?3, ?3)
             ON CONFLICT(peer_id) DO UPDATE SET
                 ssh_destination = excluded.ssh_destination,
                 enabled = 1,
                 updated_at = excluded.updated_at",
            params![peer_id, ssh_destination, now],
        )
        .map_err(|error| peer_sqlite_error("upsert_peer", error))?;
    commit_peer_transaction(transaction, "upsert_peer")
}

pub fn upsert_peer_route(
    database: &Path,
    peer_id: &str,
    local_mission_id: &str,
    remote_mission_id: &str,
    direction: &str,
) -> Result<(), KernelError> {
    validate_identifier("peer_id", peer_id)?;
    validate_identifier("local_mission_id", local_mission_id)?;
    validate_identifier("remote_mission_id", remote_mission_id)?;
    validate_route_direction(direction)?;
    if local_mission_id == remote_mission_id {
        return Err(acl_denied(local_mission_id, remote_mission_id));
    }
    bootstrap_database(database)?;
    let mut connection = open_writable(database, OWNER_IDENTITY)?;
    let transaction = begin_peer_transaction(&mut connection, "upsert_peer_route")?;
    require_local_peer(&transaction)?;
    require_mission_pm(&transaction, local_mission_id, false)?;
    let peer_exists = transaction
        .query_row(
            "SELECT enabled FROM mission_peers WHERE peer_id = ?1",
            [peer_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| peer_sqlite_error("read_peer", error))?;
    if peer_exists != Some(1) {
        return Err(peer_contract_error(
            "peer_not_found",
            "enabled peer configuration does not exist",
            BTreeMap::from([("peer_id".into(), json!(peer_id))]),
        ));
    }
    let now = utc_timestamp();
    transaction
        .execute(
            "INSERT INTO mission_peer_routes(
                 peer_id, local_mission_id, remote_mission_id, direction,
                 enabled, created_at, updated_at
             ) VALUES(?1, ?2, ?3, ?4, 1, ?5, ?5)
             ON CONFLICT(peer_id, local_mission_id, remote_mission_id) DO UPDATE SET
                 direction = excluded.direction,
                 enabled = 1,
                 updated_at = excluded.updated_at",
            params![peer_id, local_mission_id, remote_mission_id, direction, now],
        )
        .map_err(|error| peer_sqlite_error("upsert_peer_route", error))?;
    commit_peer_transaction(transaction, "upsert_peer_route")
}

pub fn new_peer_message_id() -> String {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "peer-{:x}-{:08x}",
        duration.as_secs(),
        duration.subsec_nanos().wrapping_add(sequence as u32)
    )
}

pub fn queue_peer_message(
    database: &Path,
    request: &PeerSendRequest,
) -> Result<PeerSendOutcome, KernelError> {
    validate_send_request(request)?;
    bootstrap_database(database)?;
    let mut connection = open_writable(database, OWNER_IDENTITY)?;
    let transaction = begin_peer_transaction(&mut connection, "queue_peer_message")?;
    let local_peer_id = require_local_peer(&transaction)?;
    let source_generation = require_mission_pm(&transaction, &request.source_mission_id, true)?;
    let (direction, target_peer_id, initial_state) = match request.peer_id.as_deref() {
        Some(peer_id) => {
            require_outbound_route(
                &transaction,
                peer_id,
                &request.source_mission_id,
                &request.target_mission_id,
            )?;
            ("outbound", peer_id.to_string(), "queued")
        }
        None => {
            require_mission_pm(&transaction, &request.target_mission_id, false)?;
            ("local", local_peer_id.clone(), "accepted")
        }
    };
    validate_reply_reference(
        &transaction,
        request.in_reply_to.as_deref(),
        &request.source_mission_id,
        &request.target_mission_id,
        &target_peer_id,
    )?;

    if let Some(existing) = read_existing_send(&transaction, &request.message_id)? {
        if existing.direction == direction
            && existing.source_peer_id == local_peer_id
            && existing.target_peer_id == target_peer_id
            && existing.source_mission_id == request.source_mission_id
            && existing.target_mission_id == request.target_mission_id
            && existing.source_pm_generation == source_generation
            && existing.kind == request.kind
            && existing.body == request.body
            && existing.in_reply_to == request.in_reply_to
        {
            return Ok(PeerSendOutcome {
                message_id: request.message_id.clone(),
                payload_sha256: existing.payload_sha256,
                state: existing.state,
                duplicate: true,
            });
        }
        return Err(peer_message_conflict(&request.message_id));
    }

    let now = utc_timestamp();
    let payload = PeerPayloadV1 {
        protocol: PEER_PROTOCOL.into(),
        message_id: request.message_id.clone(),
        source_peer_id: local_peer_id.clone(),
        target_peer_id: target_peer_id.clone(),
        source_mission_id: request.source_mission_id.clone(),
        target_mission_id: request.target_mission_id.clone(),
        source_pm_generation: source_generation.clone(),
        kind: request.kind.clone(),
        body: request.body.clone(),
        in_reply_to: request.in_reply_to.clone(),
        created_at: now.clone(),
    };
    let payload_sha256 = payload_sha256(&payload)?;
    transaction
        .execute(
            "INSERT INTO mission_peer_messages(
                 message_id, direction, source_peer_id, target_peer_id,
                 source_mission_id, target_mission_id, source_pm_generation,
                 kind, body, in_reply_to, payload_sha256, state,
                 created_at, updated_at, received_at
             ) VALUES(
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                 ?13, ?13, CASE WHEN ?2 = 'local' THEN ?13 ELSE NULL END
             )",
            params![
                request.message_id,
                direction,
                local_peer_id,
                target_peer_id,
                request.source_mission_id,
                request.target_mission_id,
                source_generation,
                request.kind,
                request.body,
                request.in_reply_to,
                payload_sha256,
                initial_state,
                now,
            ],
        )
        .map_err(|error| peer_sqlite_error("insert_peer_message", error))?;
    commit_peer_transaction(transaction, "queue_peer_message")?;
    Ok(PeerSendOutcome {
        message_id: request.message_id.clone(),
        payload_sha256,
        state: initial_state.into(),
        duplicate: false,
    })
}

pub fn receive_peer_envelope(
    database: &Path,
    forced_source_peer_id: &str,
    envelope_bytes: &[u8],
) -> Result<PeerReceipt, KernelError> {
    validate_identifier("forced_source_peer_id", forced_source_peer_id)?;
    if envelope_bytes.len() > MAX_PEER_ENVELOPE_BYTES {
        return Err(peer_contract_error(
            "peer_envelope_too_large",
            "peer envelope exceeds the 256 KiB limit",
            BTreeMap::from([("bytes".into(), json!(envelope_bytes.len()))]),
        ));
    }
    let envelope: PeerEnvelopeV1 = serde_json::from_slice(envelope_bytes).map_err(|error| {
        peer_contract_error(
            "peer_envelope_invalid",
            "peer envelope is not valid typed JSON",
            BTreeMap::from([("reason".into(), json!(error.to_string()))]),
        )
    })?;
    validate_received_envelope(&envelope, forced_source_peer_id)?;
    bootstrap_database(database)?;
    let mut connection = open_writable(database, OWNER_IDENTITY)?;
    let transaction = begin_peer_transaction(&mut connection, "receive_peer_envelope")?;
    let local_peer_id = require_local_peer(&transaction)?;
    if envelope.payload.target_peer_id != local_peer_id {
        return Err(peer_contract_error(
            "peer_target_identity_mismatch",
            "peer envelope targets a different local peer",
            BTreeMap::from([
                ("expected".into(), json!(local_peer_id)),
                ("actual".into(), json!(envelope.payload.target_peer_id)),
            ]),
        ));
    }

    if let Some(existing) = read_existing_send(&transaction, &envelope.payload.message_id)? {
        if existing.direction == "inbound"
            && existing.source_peer_id == envelope.payload.source_peer_id
            && existing.target_peer_id == envelope.payload.target_peer_id
            && existing.source_mission_id == envelope.payload.source_mission_id
            && existing.target_mission_id == envelope.payload.target_mission_id
            && existing.source_pm_generation == envelope.payload.source_pm_generation
            && existing.kind == envelope.payload.kind
            && existing.body == envelope.payload.body
            && existing.in_reply_to == envelope.payload.in_reply_to
            && existing.payload_sha256 == envelope.payload_sha256
        {
            return Ok(PeerReceipt {
                status: "duplicate".into(),
                message_id: envelope.payload.message_id,
                payload_sha256: envelope.payload_sha256,
            });
        }
        return Err(peer_message_conflict(&envelope.payload.message_id));
    }

    require_inbound_route(
        &transaction,
        forced_source_peer_id,
        &envelope.payload.target_mission_id,
        &envelope.payload.source_mission_id,
    )?;
    require_mission_pm(&transaction, &envelope.payload.target_mission_id, false)?;
    validate_reply_reference(
        &transaction,
        envelope.payload.in_reply_to.as_deref(),
        &envelope.payload.source_mission_id,
        &envelope.payload.target_mission_id,
        forced_source_peer_id,
    )?;

    let now = utc_timestamp();
    transaction
        .execute(
            "INSERT INTO mission_peer_messages(
                 message_id, direction, source_peer_id, target_peer_id,
                 source_mission_id, target_mission_id, source_pm_generation,
                 kind, body, in_reply_to, payload_sha256, state,
                 created_at, updated_at, received_at
             ) VALUES(?1, 'inbound', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                      'accepted', ?11, ?12, ?12)",
            params![
                envelope.payload.message_id,
                envelope.payload.source_peer_id,
                envelope.payload.target_peer_id,
                envelope.payload.source_mission_id,
                envelope.payload.target_mission_id,
                envelope.payload.source_pm_generation,
                envelope.payload.kind,
                envelope.payload.body,
                envelope.payload.in_reply_to,
                envelope.payload_sha256,
                envelope.payload.created_at,
                now,
            ],
        )
        .map_err(|error| peer_sqlite_error("insert_inbound_peer_message", error))?;
    commit_peer_transaction(transaction, "receive_peer_envelope")?;
    Ok(PeerReceipt {
        status: "accepted".into(),
        message_id: envelope.payload.message_id,
        payload_sha256: envelope.payload_sha256,
    })
}

pub fn read_peer_inbox(
    database: &Path,
    mission_id: &str,
    role: &str,
) -> Result<Vec<PeerInboxMessage>, KernelError> {
    require_pm_role(role)?;
    let connection = open_writable(database, OWNER_IDENTITY)?;
    if peer_schema_objects(&connection)?.is_empty() {
        return Ok(Vec::new());
    }
    validate_peer_schema(&connection)?;
    require_mission_pm(&connection, mission_id, false)?;
    let persisted = connection
        .prepare(
            "SELECT message_id, source_peer_id, target_peer_id, source_mission_id,
                    target_mission_id, source_pm_generation, kind, body, in_reply_to,
                    created_at, payload_sha256, COALESCE(received_at, created_at)
             FROM mission_peer_messages
             WHERE target_mission_id = ?1
               AND direction IN ('local', 'inbound')
               AND state = 'accepted'
             ORDER BY created_at, message_id",
        )
        .and_then(|mut statement| {
            statement
                .query_map([mission_id], |row| {
                    Ok((
                        PeerPayloadV1 {
                            protocol: PEER_PROTOCOL.into(),
                            message_id: row.get(0)?,
                            source_peer_id: row.get(1)?,
                            target_peer_id: row.get(2)?,
                            source_mission_id: row.get(3)?,
                            target_mission_id: row.get(4)?,
                            source_pm_generation: row.get(5)?,
                            kind: row.get(6)?,
                            body: row.get(7)?,
                            in_reply_to: row.get(8)?,
                            created_at: row.get(9)?,
                        },
                        row.get::<_, String>(10)?,
                        row.get::<_, String>(11)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|error| peer_sqlite_error("read_peer_inbox", error))?;
    persisted
        .into_iter()
        .map(|(payload, payload_sha256, received_at)| {
            validate_persisted_payload(&payload, &payload_sha256)?;
            Ok(PeerInboxMessage {
                message_id: payload.message_id,
                source_peer_id: payload.source_peer_id,
                target_peer_id: payload.target_peer_id,
                source_mission_id: payload.source_mission_id,
                target_mission_id: payload.target_mission_id,
                source_pm_generation: payload.source_pm_generation,
                kind: payload.kind,
                body: payload.body,
                in_reply_to: payload.in_reply_to,
                payload_sha256,
                received_at,
            })
        })
        .collect()
}

pub fn acknowledge_peer_message(
    database: &Path,
    mission_id: &str,
    role: &str,
    message_id: &str,
) -> Result<bool, KernelError> {
    require_pm_role(role)?;
    validate_identifier("mission_id", mission_id)?;
    validate_identifier("message_id", message_id)?;
    bootstrap_database(database)?;
    let mut connection = open_writable(database, OWNER_IDENTITY)?;
    let transaction = begin_peer_transaction(&mut connection, "acknowledge_peer_message")?;
    require_mission_pm(&transaction, mission_id, false)?;
    let row = transaction
        .query_row(
            "SELECT target_mission_id, direction, state
             FROM mission_peer_messages WHERE message_id = ?1",
            [message_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| peer_sqlite_error("read_peer_message_for_ack", error))?
        .ok_or_else(|| {
            peer_contract_error(
                "peer_message_not_found",
                "peer inbox message does not exist",
                BTreeMap::from([("message_id".into(), json!(message_id))]),
            )
        })?;
    if row.0 != mission_id || !matches!(row.1.as_str(), "local" | "inbound") {
        return Err(peer_contract_error(
            "peer_ack_denied",
            "only the target Mission PM can acknowledge a peer inbox message",
            BTreeMap::from([
                ("message_id".into(), json!(message_id)),
                ("target_mission_id".into(), json!(row.0)),
            ]),
        ));
    }
    if row.2 == "handled" {
        return Ok(false);
    }
    if row.2 != "accepted" {
        return Err(peer_contract_error(
            "peer_ack_state_invalid",
            "peer message is not an accepted inbox item",
            BTreeMap::from([("state".into(), json!(row.2))]),
        ));
    }
    let now = utc_timestamp();
    let updated = transaction
        .execute(
            "UPDATE mission_peer_messages
             SET state = 'handled', handled_at = ?1, updated_at = ?1
             WHERE message_id = ?2 AND target_mission_id = ?3 AND state = 'accepted'",
            params![now, message_id, mission_id],
        )
        .map_err(|error| peer_sqlite_error("acknowledge_peer_message", error))?;
    if updated != 1 {
        return Err(peer_contract_error(
            "peer_ack_conflict",
            "peer message changed concurrently while being acknowledged",
            BTreeMap::from([("message_id".into(), json!(message_id))]),
        ));
    }
    commit_peer_transaction(transaction, "acknowledge_peer_message")?;
    Ok(true)
}

pub fn deliver_peer_messages_with(
    database: &Path,
    transport: &dyn PeerTransport,
) -> Result<PeerRelayReport, KernelError> {
    bootstrap_database(database)?;
    let mut report = PeerRelayReport::default();
    while let Some(claim) = claim_next_outbound(database)? {
        let envelope = match claim.envelope() {
            Ok(envelope) => envelope,
            Err(_) => {
                release_peer_claim(database, &claim, "peer_payload_corrupt")?;
                report.retried = report.retried.saturating_add(1);
                continue;
            }
        };
        let wire = match serde_json::to_vec(&envelope) {
            Ok(wire) => wire,
            Err(_) => {
                release_peer_claim(database, &claim, "peer_envelope_serialize_failed")?;
                report.retried = report.retried.saturating_add(1);
                continue;
            }
        };
        if wire.len() > MAX_PEER_ENVELOPE_BYTES {
            release_peer_claim(
                database,
                &claim,
                "persisted peer envelope exceeds the protocol limit",
            )?;
            report.retried = report.retried.saturating_add(1);
            continue;
        }
        let output = match transport.send(&claim.ssh_destination, &wire) {
            Ok(output) => output,
            Err(error) => {
                release_peer_claim(database, &claim, &format!("ssh_spawn:{:?}", error.kind()))?;
                report.retried = report.retried.saturating_add(1);
                continue;
            }
        };
        if output.exit_code != 0 {
            release_peer_claim(database, &claim, &format!("ssh_exit:{}", output.exit_code))?;
            report.retried = report.retried.saturating_add(1);
            continue;
        }
        let receipt = serde_json::from_str::<PeerReceipt>(output.stdout.trim());
        let valid_receipt = receipt.as_ref().is_ok_and(|receipt| {
            matches!(receipt.status.as_str(), "accepted" | "duplicate")
                && receipt.message_id == claim.payload.message_id
                && receipt.payload_sha256 == claim.payload_sha256
        });
        if !valid_receipt {
            release_peer_claim(database, &claim, "peer_receipt_invalid")?;
            report.retried = report.retried.saturating_add(1);
            continue;
        }
        acknowledge_outbound_claim(database, &claim, receipt.expect("validated receipt"))?;
        report.sent = report.sent.saturating_add(1);
    }
    Ok(report)
}

pub fn notify_peer_inboxes(
    database: &Path,
    runner: &dyn ProcessRunner,
    herdr: &str,
) -> Result<PeerRelayReport, KernelError> {
    let pending = pending_notifications(database)?;
    let mut report = PeerRelayReport::default();
    for notification in pending {
        if notification.agent_name.is_empty() || notification.pane_id.is_empty() {
            record_notification_failure(
                database,
                &notification.message_ids,
                "target_pm_not_started",
            )?;
            report.notify_failed = report.notify_failed.saturating_add(1);
            continue;
        }
        let prompt = format!(
            "收到 {} 条跨 Mission PM 消息。请运行 herdr-mission init 读取 peer_inbox，\
             再按本 Mission 权限拆分本地 Assignment。",
            notification.message_ids.len()
        );
        let args = vec![
            "agent".to_string(),
            "prompt".to_string(),
            notification.agent_name.clone(),
            prompt,
        ];
        let result = runner.run(herdr, &args);
        let succeeded = match result {
            Ok(output) if output.exit_code == 0 => prompt_matches_binding(
                &output.stdout,
                &notification.agent_name,
                &notification.pane_id,
            ),
            _ => false,
        };
        if succeeded {
            record_notification_success(database, &notification.message_ids)?;
            report.notified = report.notified.saturating_add(1);
        } else {
            record_notification_failure(
                database,
                &notification.message_ids,
                "target_pm_wake_failed",
            )?;
            report.notify_failed = report.notify_failed.saturating_add(1);
        }
    }
    Ok(report)
}

pub fn reconcile_peer_relay(
    database: &Path,
    transport: &dyn PeerTransport,
    runner: &dyn ProcessRunner,
    herdr: &str,
) -> Result<PeerRelayReport, KernelError> {
    let mut report = deliver_peer_messages_with(database, transport)?;
    let notification = notify_peer_inboxes(database, runner, herdr)?;
    report.notified = notification.notified;
    report.notify_failed = notification.notify_failed;
    Ok(report)
}

#[derive(Debug, Clone)]
struct ExistingPeerMessage {
    direction: String,
    source_peer_id: String,
    target_peer_id: String,
    source_mission_id: String,
    target_mission_id: String,
    source_pm_generation: String,
    kind: String,
    body: String,
    in_reply_to: Option<String>,
    created_at: String,
    payload_sha256: String,
    state: String,
}

fn read_existing_send(
    connection: &Connection,
    message_id: &str,
) -> Result<Option<ExistingPeerMessage>, KernelError> {
    let existing = connection
        .query_row(
            "SELECT direction, source_peer_id, target_peer_id, source_mission_id,
                    target_mission_id, source_pm_generation, kind, body,
                    in_reply_to, created_at, payload_sha256, state
             FROM mission_peer_messages WHERE message_id = ?1",
            [message_id],
            |row| {
                Ok(ExistingPeerMessage {
                    direction: row.get(0)?,
                    source_peer_id: row.get(1)?,
                    target_peer_id: row.get(2)?,
                    source_mission_id: row.get(3)?,
                    target_mission_id: row.get(4)?,
                    source_pm_generation: row.get(5)?,
                    kind: row.get(6)?,
                    body: row.get(7)?,
                    in_reply_to: row.get(8)?,
                    created_at: row.get(9)?,
                    payload_sha256: row.get(10)?,
                    state: row.get(11)?,
                })
            },
        )
        .optional()
        .map_err(|error| peer_sqlite_error("read_peer_message", error))?;
    if let Some(existing) = existing.as_ref() {
        let payload = PeerPayloadV1 {
            protocol: PEER_PROTOCOL.into(),
            message_id: message_id.into(),
            source_peer_id: existing.source_peer_id.clone(),
            target_peer_id: existing.target_peer_id.clone(),
            source_mission_id: existing.source_mission_id.clone(),
            target_mission_id: existing.target_mission_id.clone(),
            source_pm_generation: existing.source_pm_generation.clone(),
            kind: existing.kind.clone(),
            body: existing.body.clone(),
            in_reply_to: existing.in_reply_to.clone(),
            created_at: existing.created_at.clone(),
        };
        validate_persisted_payload(&payload, &existing.payload_sha256)?;
    }
    Ok(existing)
}

#[derive(Debug, Clone)]
struct ClaimedOutbound {
    claim_owner: String,
    ssh_destination: String,
    payload: PeerPayloadV1,
    payload_sha256: String,
    attempts: i64,
}

impl ClaimedOutbound {
    fn envelope(&self) -> Result<PeerEnvelopeV1, KernelError> {
        validate_persisted_payload(&self.payload, &self.payload_sha256)?;
        Ok(PeerEnvelopeV1 {
            payload: self.payload.clone(),
            payload_sha256: self.payload_sha256.clone(),
        })
    }
}

fn claim_next_outbound(database: &Path) -> Result<Option<ClaimedOutbound>, KernelError> {
    let mut connection = open_writable(database, OWNER_IDENTITY)?;
    let transaction = begin_peer_transaction(&mut connection, "claim_peer_outbound")?;
    let now_ms = unix_millis();
    transaction
        .execute(
            "UPDATE mission_peer_messages
             SET state = 'retry', claim_owner = '', claimed_at = NULL,
                 next_attempt_at = ?1, updated_at = ?2,
                 last_error = 'stale_peer_claim'
             WHERE direction = 'outbound' AND state = 'sending'
               AND claimed_at IS NOT NULL AND claimed_at <= ?3",
            params![
                now_ms,
                utc_timestamp(),
                now_ms.saturating_sub(CLAIM_LEASE_MS)
            ],
        )
        .map_err(|error| peer_sqlite_error("recover_stale_peer_claim", error))?;
    let row = transaction
        .query_row(
            "SELECT m.message_id, m.source_peer_id, m.target_peer_id,
                    m.source_mission_id, m.target_mission_id, m.source_pm_generation,
                    m.kind, m.body, m.in_reply_to, m.created_at,
                    m.payload_sha256, m.attempts, p.ssh_destination
             FROM mission_peer_messages AS m
             JOIN mission_peers AS p
               ON p.peer_id = m.target_peer_id AND p.enabled = 1
             JOIN mission_peer_routes AS r
               ON r.peer_id = m.target_peer_id
              AND r.local_mission_id = m.source_mission_id
              AND r.remote_mission_id = m.target_mission_id
              AND r.enabled = 1
              AND r.direction IN ('outbound', 'bidirectional')
             WHERE m.direction = 'outbound' AND m.state IN ('queued', 'retry')
               AND (m.next_attempt_at IS NULL OR m.next_attempt_at <= ?1)
             ORDER BY m.created_at, m.message_id LIMIT 1",
            [now_ms],
            |row| {
                Ok((
                    PeerPayloadV1 {
                        protocol: PEER_PROTOCOL.into(),
                        message_id: row.get(0)?,
                        source_peer_id: row.get(1)?,
                        target_peer_id: row.get(2)?,
                        source_mission_id: row.get(3)?,
                        target_mission_id: row.get(4)?,
                        source_pm_generation: row.get(5)?,
                        kind: row.get(6)?,
                        body: row.get(7)?,
                        in_reply_to: row.get(8)?,
                        created_at: row.get(9)?,
                    },
                    row.get::<_, String>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, String>(12)?,
                ))
            },
        )
        .optional()
        .map_err(|error| peer_sqlite_error("select_peer_outbound", error))?;
    let Some((payload, digest, attempts, ssh_destination)) = row else {
        commit_peer_transaction(transaction, "claim_peer_outbound_empty")?;
        return Ok(None);
    };
    let claim_owner = new_claim_owner();
    let updated = transaction
        .execute(
            "UPDATE mission_peer_messages
             SET state = 'sending', claim_owner = ?1, claimed_at = ?2,
                 updated_at = ?3
             WHERE message_id = ?4 AND direction = 'outbound'
               AND state IN ('queued', 'retry')",
            params![claim_owner, now_ms, utc_timestamp(), payload.message_id],
        )
        .map_err(|error| peer_sqlite_error("claim_peer_outbound", error))?;
    if updated != 1 {
        return Err(peer_contract_error(
            "peer_claim_conflict",
            "peer outbound changed concurrently while being claimed",
            BTreeMap::from([("message_id".into(), json!(payload.message_id))]),
        ));
    }
    commit_peer_transaction(transaction, "claim_peer_outbound")?;
    Ok(Some(ClaimedOutbound {
        claim_owner,
        ssh_destination,
        payload,
        payload_sha256: digest,
        attempts,
    }))
}

fn release_peer_claim(
    database: &Path,
    claim: &ClaimedOutbound,
    error: &str,
) -> Result<(), KernelError> {
    let mut connection = open_writable(database, OWNER_IDENTITY)?;
    let transaction = begin_peer_transaction(&mut connection, "release_peer_claim")?;
    let attempts = claim.attempts.saturating_add(1);
    let exponent = u32::try_from(attempts.min(6)).unwrap_or(6);
    let delay_ms = 1_000_i64.saturating_mul(1_i64 << exponent).min(60_000);
    let updated = transaction
        .execute(
            "UPDATE mission_peer_messages
             SET state = 'retry', attempts = ?1, last_error = ?2,
                 claim_owner = '', claimed_at = NULL, next_attempt_at = ?3,
                 updated_at = ?4
             WHERE message_id = ?5 AND direction = 'outbound'
               AND state = 'sending' AND claim_owner = ?6",
            params![
                attempts,
                error,
                unix_millis().saturating_add(delay_ms),
                utc_timestamp(),
                claim.payload.message_id,
                claim.claim_owner,
            ],
        )
        .map_err(|error| peer_sqlite_error("release_peer_claim", error))?;
    if updated != 1 {
        return Err(peer_claim_lost(&claim.payload.message_id));
    }
    commit_peer_transaction(transaction, "release_peer_claim")
}

fn acknowledge_outbound_claim(
    database: &Path,
    claim: &ClaimedOutbound,
    receipt: PeerReceipt,
) -> Result<(), KernelError> {
    let receipt_json = serde_json::to_string(&receipt).map_err(|error| {
        peer_internal_error(
            "peer_receipt_serialize_failed",
            "failed to serialize peer receipt",
            BTreeMap::from([("reason".into(), json!(error.to_string()))]),
        )
    })?;
    let mut connection = open_writable(database, OWNER_IDENTITY)?;
    let transaction = begin_peer_transaction(&mut connection, "acknowledge_outbound_peer")?;
    let updated = transaction
        .execute(
            "UPDATE mission_peer_messages
             SET state = 'acknowledged', attempts = ?1, last_error = '',
                 receipt_json = ?2, claim_owner = '', claimed_at = NULL,
                 next_attempt_at = NULL, updated_at = ?3
             WHERE message_id = ?4 AND direction = 'outbound'
               AND state = 'sending' AND claim_owner = ?5
               AND payload_sha256 = ?6",
            params![
                claim.attempts.saturating_add(1),
                receipt_json,
                utc_timestamp(),
                claim.payload.message_id,
                claim.claim_owner,
                claim.payload_sha256,
            ],
        )
        .map_err(|error| peer_sqlite_error("acknowledge_outbound_peer", error))?;
    if updated != 1 {
        return Err(peer_claim_lost(&claim.payload.message_id));
    }
    commit_peer_transaction(transaction, "acknowledge_outbound_peer")
}

#[derive(Debug)]
struct PendingNotification {
    message_ids: Vec<String>,
    pane_id: String,
    agent_name: String,
}

fn pending_notifications(database: &Path) -> Result<Vec<PendingNotification>, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    if peer_schema_objects(&connection)?.is_empty() {
        return Ok(Vec::new());
    }
    validate_peer_schema(&connection)?;
    let mut statement = connection
        .prepare(
            "SELECT m.target_mission_id, r.pane_id, r.terminal_id, m.message_id
             FROM mission_peer_messages AS m
             JOIN team_roles AS r
               ON r.mission_id = m.target_mission_id AND r.role = 'pm'
             WHERE m.direction IN ('local', 'inbound')
               AND m.state = 'accepted' AND m.notified_at IS NULL
             ORDER BY m.target_mission_id, m.created_at, m.message_id",
        )
        .map_err(|error| peer_sqlite_error("read_pending_peer_notifications", error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| peer_sqlite_error("read_pending_peer_notifications", error))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| peer_sqlite_error("read_pending_peer_notifications", error))?;
    let mut grouped: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    for (mission_id, pane_id, agent_name, message_id) in rows {
        grouped
            .entry((mission_id, pane_id, agent_name))
            .or_default()
            .push(message_id);
    }
    Ok(grouped
        .into_iter()
        .map(
            |((_mission_id, pane_id, agent_name), message_ids)| PendingNotification {
                message_ids,
                pane_id,
                agent_name,
            },
        )
        .collect())
}

fn record_notification_success(database: &Path, message_ids: &[String]) -> Result<(), KernelError> {
    update_notification_rows(database, message_ids, true, "")
}

fn record_notification_failure(
    database: &Path,
    message_ids: &[String],
    error: &str,
) -> Result<(), KernelError> {
    update_notification_rows(database, message_ids, false, error)
}

fn update_notification_rows(
    database: &Path,
    message_ids: &[String],
    success: bool,
    failure: &str,
) -> Result<(), KernelError> {
    let mut connection = open_writable(database, OWNER_IDENTITY)?;
    let transaction = begin_peer_transaction(&mut connection, "update_peer_notification")?;
    let now = utc_timestamp();
    for message_id in message_ids {
        let changed = if success {
            transaction.execute(
                "UPDATE mission_peer_messages
                 SET notified_at = ?1, notify_last_error = '', updated_at = ?1
                 WHERE message_id = ?2 AND direction IN ('local', 'inbound')
                   AND state = 'accepted' AND notified_at IS NULL",
                params![now, message_id],
            )
        } else {
            transaction.execute(
                "UPDATE mission_peer_messages
                 SET notify_attempts = notify_attempts + 1,
                     notify_last_error = ?1, updated_at = ?2
                 WHERE message_id = ?3 AND direction IN ('local', 'inbound')
                   AND state = 'accepted' AND notified_at IS NULL",
                params![failure, now, message_id],
            )
        }
        .map_err(|error| peer_sqlite_error("update_peer_notification", error))?;
        if changed > 1 {
            return Err(peer_internal_error(
                "peer_notification_cardinality",
                "peer notification update changed multiple rows",
                BTreeMap::from([("message_id".into(), json!(message_id))]),
            ));
        }
    }
    commit_peer_transaction(transaction, "update_peer_notification")
}

fn prompt_matches_binding(response: &str, agent_name: &str, pane_id: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(response) else {
        return false;
    };
    let Some(result) = value.get("result") else {
        return false;
    };
    result.get("type").and_then(serde_json::Value::as_str) == Some("agent_prompted")
        && result
            .pointer("/agent/name")
            .and_then(serde_json::Value::as_str)
            == Some(agent_name)
        && result
            .pointer("/agent/pane_id")
            .and_then(serde_json::Value::as_str)
            == Some(pane_id)
        && result
            .pointer("/agent/state_change_seq")
            .and_then(serde_json::Value::as_u64)
            .is_some()
}

fn validate_send_request(request: &PeerSendRequest) -> Result<(), KernelError> {
    validate_identifier("message_id", &request.message_id)?;
    validate_identifier("source_mission_id", &request.source_mission_id)?;
    validate_identifier("target_mission_id", &request.target_mission_id)?;
    require_pm_role(&request.source_role)?;
    if request.source_mission_id == request.target_mission_id {
        return Err(acl_denied(
            &request.source_mission_id,
            &request.target_mission_id,
        ));
    }
    if let Some(peer_id) = request.peer_id.as_deref() {
        validate_identifier("peer_id", peer_id)?;
    }
    validate_kind(&request.kind)?;
    validate_body(&request.body)?;
    if let Some(message_id) = request.in_reply_to.as_deref() {
        validate_identifier("in_reply_to", message_id)?;
    }
    Ok(())
}

fn validate_received_envelope(
    envelope: &PeerEnvelopeV1,
    forced_source_peer_id: &str,
) -> Result<(), KernelError> {
    let payload = &envelope.payload;
    if payload.protocol != PEER_PROTOCOL {
        return Err(peer_contract_error(
            "peer_protocol_unsupported",
            "peer envelope protocol is not supported",
            BTreeMap::from([("protocol".into(), json!(payload.protocol))]),
        ));
    }
    for (field, value) in [
        ("message_id", payload.message_id.as_str()),
        ("source_peer_id", payload.source_peer_id.as_str()),
        ("target_peer_id", payload.target_peer_id.as_str()),
        ("source_mission_id", payload.source_mission_id.as_str()),
        ("target_mission_id", payload.target_mission_id.as_str()),
        (
            "source_pm_generation",
            payload.source_pm_generation.as_str(),
        ),
    ] {
        validate_identifier(field, value)?;
    }
    if payload.source_peer_id != forced_source_peer_id {
        return Err(peer_contract_error(
            "peer_source_identity_mismatch",
            "forced SSH peer identity does not match the envelope source",
            BTreeMap::from([
                ("forced_peer_id".into(), json!(forced_source_peer_id)),
                ("envelope_peer_id".into(), json!(payload.source_peer_id)),
            ]),
        ));
    }
    if payload.source_mission_id == payload.target_mission_id {
        return Err(acl_denied(
            &payload.source_mission_id,
            &payload.target_mission_id,
        ));
    }
    validate_kind(&payload.kind)?;
    validate_body(&payload.body)?;
    if let Some(message_id) = payload.in_reply_to.as_deref() {
        validate_identifier("in_reply_to", message_id)?;
    }
    if !is_utc_timestamp(&payload.created_at) {
        return Err(peer_contract_error(
            "peer_timestamp_invalid",
            "peer payload created_at must be canonical UTC",
            BTreeMap::from([("created_at".into(), json!(payload.created_at))]),
        ));
    }
    validate_digest(&envelope.payload_sha256)?;
    let actual = payload_sha256(payload)?;
    if actual != envelope.payload_sha256 {
        return Err(peer_contract_error(
            "peer_payload_digest_mismatch",
            "peer payload does not match its SHA-256",
            BTreeMap::from([
                ("expected".into(), json!(envelope.payload_sha256)),
                ("actual".into(), json!(actual)),
            ]),
        ));
    }
    Ok(())
}

fn validate_identifier(field: &str, value: &str) -> Result<(), KernelError> {
    let valid = !value.is_empty()
        && value.len() <= 160
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
        });
    if valid {
        Ok(())
    } else {
        Err(peer_contract_error(
            "peer_identifier_invalid",
            "peer identifiers must use 1-160 ASCII letters, digits, dot, colon, underscore or hyphen",
            BTreeMap::from([("field".into(), json!(field))]),
        ))
    }
}

fn validate_ssh_destination(destination: &str) -> Result<(), KernelError> {
    let valid = !destination.is_empty()
        && destination.len() <= 255
        && !destination.starts_with('-')
        && destination.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '-' | '_' | '@' | ':' | '[' | ']')
        });
    if valid {
        Ok(())
    } else {
        Err(peer_contract_error(
            "peer_ssh_destination_invalid",
            "SSH destination contains unsupported or option-like characters",
            BTreeMap::new(),
        ))
    }
}

fn validate_route_direction(direction: &str) -> Result<(), KernelError> {
    if matches!(direction, "inbound" | "outbound" | "bidirectional") {
        Ok(())
    } else {
        Err(peer_contract_error(
            "peer_route_direction_invalid",
            "peer route direction must be inbound, outbound or bidirectional",
            BTreeMap::from([("direction".into(), json!(direction))]),
        ))
    }
}

fn validate_kind(kind: &str) -> Result<(), KernelError> {
    if matches!(kind, "delegate" | "context" | "result" | "blocked") {
        Ok(())
    } else {
        Err(peer_contract_error(
            "peer_kind_invalid",
            "peer message kind is not permitted",
            BTreeMap::from([("kind".into(), json!(kind))]),
        ))
    }
}

fn validate_body(body: &str) -> Result<(), KernelError> {
    if body.trim().is_empty() || body.len() > MAX_BODY_BYTES {
        return Err(peer_contract_error(
            "peer_body_invalid",
            "peer message body must be non-empty and at most 64 KiB",
            BTreeMap::from([("bytes".into(), json!(body.len()))]),
        ));
    }
    Ok(())
}

fn validate_digest(digest: &str) -> Result<(), KernelError> {
    if digest.len() == 64
        && digest
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(peer_contract_error(
            "peer_digest_invalid",
            "peer payload SHA-256 must be 64 lowercase hexadecimal characters",
            BTreeMap::new(),
        ))
    }
}

fn payload_sha256(payload: &PeerPayloadV1) -> Result<String, KernelError> {
    serde_json::to_vec(payload)
        .map(|bytes| crate::manifest::sha256_hex(&bytes))
        .map_err(|error| {
            peer_internal_error(
                "peer_payload_serialize_failed",
                "failed to serialize typed peer payload",
                BTreeMap::from([("reason".into(), json!(error.to_string()))]),
            )
        })
}

fn validate_persisted_payload(payload: &PeerPayloadV1, expected: &str) -> Result<(), KernelError> {
    let actual = payload_sha256(payload)?;
    if actual == expected {
        return Ok(());
    }
    Err(peer_contract_error(
        "peer_payload_corrupt",
        "persisted peer payload does not match its SHA-256",
        BTreeMap::from([
            ("message_id".into(), json!(payload.message_id)),
            ("expected".into(), json!(expected)),
            ("actual".into(), json!(actual)),
        ]),
    ))
}

fn require_pm_role(role: &str) -> Result<(), KernelError> {
    if role == "pm" {
        Ok(())
    } else {
        Err(peer_contract_error(
            "acl_denied",
            "only a Mission PM can use the peer relay",
            BTreeMap::from([("role".into(), json!(role))]),
        ))
    }
}

fn require_local_peer(connection: &Connection) -> Result<String, KernelError> {
    connection
        .query_row(
            "SELECT local_peer_id FROM mission_peer_identity WHERE singleton = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| peer_sqlite_error("read_local_peer_identity", error))?
        .ok_or_else(|| {
            peer_contract_error(
                "local_peer_not_configured",
                "local peer identity is not configured",
                BTreeMap::new(),
            )
        })
}

fn require_mission_pm(
    connection: &Connection,
    mission_id: &str,
    require_runtime_identity: bool,
) -> Result<String, KernelError> {
    let row = connection
        .query_row(
            "SELECT terminal_id, launch_generation
             FROM team_roles WHERE mission_id = ?1 AND role = 'pm'",
            [mission_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|error| peer_sqlite_error("read_mission_pm", error))?
        .ok_or_else(|| {
            peer_contract_error(
                "mission_pm_not_found",
                "Mission or its PM role does not exist",
                BTreeMap::from([("mission_id".into(), json!(mission_id))]),
            )
        })?;
    if require_runtime_identity && (row.0.is_empty() || row.1.is_empty()) {
        return Err(peer_contract_error(
            "source_pm_identity_unavailable",
            "source Mission PM has no durable runtime identity",
            BTreeMap::from([("mission_id".into(), json!(mission_id))]),
        ));
    }
    Ok(row.1)
}

fn require_outbound_route(
    connection: &Connection,
    peer_id: &str,
    local_mission_id: &str,
    remote_mission_id: &str,
) -> Result<(), KernelError> {
    require_route(
        connection,
        peer_id,
        local_mission_id,
        remote_mission_id,
        "outbound",
    )
}

fn require_inbound_route(
    connection: &Connection,
    peer_id: &str,
    local_mission_id: &str,
    remote_mission_id: &str,
) -> Result<(), KernelError> {
    require_route(
        connection,
        peer_id,
        local_mission_id,
        remote_mission_id,
        "inbound",
    )
}

fn require_route(
    connection: &Connection,
    peer_id: &str,
    local_mission_id: &str,
    remote_mission_id: &str,
    required_direction: &str,
) -> Result<(), KernelError> {
    let direction = connection
        .query_row(
            "SELECT r.direction
             FROM mission_peer_routes AS r
             JOIN mission_peers AS p ON p.peer_id = r.peer_id AND p.enabled = 1
             WHERE r.peer_id = ?1 AND r.local_mission_id = ?2
               AND r.remote_mission_id = ?3 AND r.enabled = 1",
            params![peer_id, local_mission_id, remote_mission_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| peer_sqlite_error("read_peer_route", error))?;
    if direction
        .as_deref()
        .is_some_and(|direction| direction == required_direction || direction == "bidirectional")
    {
        Ok(())
    } else {
        Err(peer_contract_error(
            "peer_route_not_allowed",
            "no enabled peer route authorizes this Mission pair and direction",
            BTreeMap::from([
                ("peer_id".into(), json!(peer_id)),
                ("local_mission_id".into(), json!(local_mission_id)),
                ("remote_mission_id".into(), json!(remote_mission_id)),
                ("direction".into(), json!(required_direction)),
            ]),
        ))
    }
}

fn validate_reply_reference(
    connection: &Connection,
    in_reply_to: Option<&str>,
    source_mission_id: &str,
    target_mission_id: &str,
    peer_id: &str,
) -> Result<(), KernelError> {
    let Some(in_reply_to) = in_reply_to else {
        return Ok(());
    };
    let valid = connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM mission_peer_messages
                 WHERE message_id = ?1
                   AND source_mission_id = ?2 AND target_mission_id = ?3
                   AND (
                       (direction IN ('local', 'inbound') AND source_peer_id = ?4)
                       OR (direction = 'outbound' AND target_peer_id = ?4)
                   )
             )",
            params![in_reply_to, target_mission_id, source_mission_id, peer_id],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| peer_sqlite_error("read_peer_reply_reference", error))?;
    if valid {
        Ok(())
    } else {
        Err(peer_contract_error(
            "peer_reply_reference_invalid",
            "peer reply does not reference the reverse route's durable message",
            BTreeMap::from([("in_reply_to".into(), json!(in_reply_to))]),
        ))
    }
}

fn begin_peer_transaction<'a>(
    connection: &'a mut Connection,
    operation: &str,
) -> Result<Transaction<'a>, KernelError> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| peer_sqlite_error(operation, error))
}

fn commit_peer_transaction(
    transaction: Transaction<'_>,
    operation: &str,
) -> Result<(), KernelError> {
    transaction
        .commit()
        .map_err(|error| peer_sqlite_error(operation, error))
}

fn new_claim_owner() -> String {
    static NEXT_CLAIM: AtomicU64 = AtomicU64::new(1);
    let sequence = NEXT_CLAIM.fetch_add(1, Ordering::Relaxed);
    format!(
        "peer-claim-{}-{}-{sequence}",
        std::process::id(),
        unix_millis().unsigned_abs()
    )
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn is_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        })
}

fn acl_denied(source_mission_id: &str, target_mission_id: &str) -> KernelError {
    peer_contract_error(
        "acl_denied",
        "PM peer relay requires two different Missions",
        BTreeMap::from([
            ("source_mission_id".into(), json!(source_mission_id)),
            ("target_mission_id".into(), json!(target_mission_id)),
        ]),
    )
}

fn peer_message_conflict(message_id: &str) -> KernelError {
    peer_contract_error(
        "peer_message_id_conflict",
        "peer message ID already exists with different semantics",
        BTreeMap::from([("message_id".into(), json!(message_id))]),
    )
}

fn peer_claim_lost(message_id: &str) -> KernelError {
    peer_contract_error(
        "peer_claim_lost",
        "peer outbound claim no longer owns the durable message",
        BTreeMap::from([("message_id".into(), json!(message_id))]),
    )
}

fn peer_contract_error(
    code: &str,
    message: &str,
    details: BTreeMap<String, serde_json::Value>,
) -> KernelError {
    KernelError {
        category: ErrorCategory::Contract,
        code: code.into(),
        message: message.into(),
        retryable: false,
        details,
    }
}

fn peer_internal_error(
    code: &str,
    message: &str,
    details: BTreeMap<String, serde_json::Value>,
) -> KernelError {
    KernelError {
        category: ErrorCategory::Internal,
        code: code.into(),
        message: message.into(),
        retryable: false,
        details,
    }
}

fn peer_sqlite_error(operation: &str, error: rusqlite::Error) -> KernelError {
    KernelError {
        category: ErrorCategory::Infrastructure,
        code: if matches!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ) {
            "sqlite_busy"
        } else {
            "peer_sqlite_failed"
        }
        .into(),
        message: "SQLite peer relay operation failed".into(),
        retryable: matches!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ),
        details: BTreeMap::from([
            ("operation".into(), json!(operation)),
            ("reason".into(), json!(error.to_string())),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_child_total_timeout_finishes_before_the_claim_lease() {
        let child = Command::new("sh")
            .args(["-c", "sleep 5"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();

        let output = run_peer_child(child, b"{}", Duration::from_millis(50)).unwrap();

        assert_eq!(output.exit_code, 124);
        assert_eq!(output.stderr, "peer transport timed out");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
