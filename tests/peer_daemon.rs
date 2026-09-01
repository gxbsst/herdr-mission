use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use herdr_mission::{
    configure_local_peer, create_mission, default_codex_team, kernel_dispatch_command,
    kernel_reconcile, kernel_reconcile_with_peer_transport, queue_peer_message, read_peer_inbox,
    read_role_context, reconcile_peer_relay, upsert_peer, upsert_peer_route, CreateMissionRequest,
    LaunchMode, PeerEnvelopeV1, PeerReceipt, PeerSendRequest, PeerTransport, ProcessOutput,
    ProcessRunner,
};
use rusqlite::Connection;
use serde_json::json;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "herdr-mission-peer-daemon-{label}-{}-{id}.sqlite3",
                std::process::id()
            )),
        }
    }

    fn add_mission(&self, mission_id: &str) {
        create_mission(
            &self.path,
            &CreateMissionRequest {
                mission_id: mission_id.into(),
                brief: format!("Peer daemon fixture for {mission_id}"),
                template: "general".into(),
                agent_profile_id: "codex-default-v1".into(),
                agent_profile_version: 1,
                launch_mode: LaunchMode::Manual,
                roles: default_codex_team(),
            },
        )
        .unwrap();
        Connection::open(&self.path)
            .unwrap()
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

    fn bind_role(&self, mission_id: &str, role: &str) {
        Connection::open(&self.path)
            .unwrap()
            .execute(
                "UPDATE team_roles
                 SET pane_id = ?1, terminal_id = ?2, launch_generation = ?3,
                     health = 'idle'
                 WHERE mission_id = ?4 AND role = ?5",
                rusqlite::params![
                    format!("pane-{mission_id}-{role}"),
                    format!("agent-{mission_id}-{role}"),
                    format!("generation-{mission_id}-{role}"),
                    mission_id,
                    role,
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

fn peer_send(message_id: &str, source: &str, target: &str) -> PeerSendRequest {
    PeerSendRequest {
        message_id: message_id.into(),
        source_mission_id: source.into(),
        target_mission_id: target.into(),
        source_role: "pm".into(),
        peer_id: Some("device-b".into()),
        kind: "delegate".into(),
        body: "Split the remote work into local Assignments.".into(),
        in_reply_to: None,
    }
}

#[derive(Default)]
struct NoopRunner;

impl ProcessRunner for NoopRunner {
    fn run(&self, _program: &str, _args: &[String]) -> std::io::Result<ProcessOutput> {
        panic!("peer reconcile unexpectedly invoked the process runner")
    }
}

#[derive(Default)]
struct TeamOnlyRunner {
    prompt_calls: AtomicU64,
}

impl ProcessRunner for TeamOnlyRunner {
    fn run(&self, _program: &str, args: &[String]) -> std::io::Result<ProcessOutput> {
        if args == ["agent", "list"] {
            return Ok(ProcessOutput {
                exit_code: 0,
                stdout: serde_json::to_string(&json!({"result": {"agents": []}})).unwrap(),
                stderr: String::new(),
            });
        }
        self.prompt_calls.fetch_add(1, Ordering::Relaxed);
        Ok(ProcessOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "unexpected peer prompt".into(),
        })
    }
}

#[test]
fn legacy_kernel_reconcile_does_not_drive_peer_notifications() {
    let database = TestDatabase::new("team-only-reconcile");
    database.add_mission("msn-source");
    database.add_mission("msn-target");
    configure_local_peer(&database.path, "device-a").unwrap();
    let mut request = peer_send("peer-team-only-1", "msn-source", "msn-target");
    request.peer_id = None;
    queue_peer_message(&database.path, &request).unwrap();
    let runner = TeamOnlyRunner::default();

    let report = kernel_reconcile(&database.path, &runner, "herdr");

    assert!(report.health.is_ok());
    assert!(report.delivery.is_ok());
    assert_eq!(runner.prompt_calls.load(Ordering::Relaxed), 0);
    let notify_attempts: i64 = Connection::open(&database.path)
        .unwrap()
        .query_row(
            "SELECT notify_attempts FROM mission_peer_messages
             WHERE message_id = 'peer-team-only-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(notify_attempts, 0);
}

struct RetryThenReceiptTransport {
    calls: Mutex<Vec<Vec<u8>>>,
}

impl RetryThenReceiptTransport {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl PeerTransport for RetryThenReceiptTransport {
    fn send(&self, _destination: &str, envelope: &[u8]) -> std::io::Result<ProcessOutput> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(envelope.to_vec());
        if calls.len() == 1 {
            return Ok(ProcessOutput {
                exit_code: 255,
                stdout: String::new(),
                stderr: "connection lost after an unknown remote commit".into(),
            });
        }
        let envelope: PeerEnvelopeV1 = serde_json::from_slice(envelope).unwrap();
        Ok(ProcessOutput {
            exit_code: 0,
            stdout: serde_json::to_string(&PeerReceipt {
                status: "duplicate".into(),
                message_id: envelope.payload.message_id,
                payload_sha256: envelope.payload_sha256,
            })
            .unwrap(),
            stderr: String::new(),
        })
    }
}

#[test]
fn peer_reconcile_durably_retries_the_same_outbound_after_transport_failure() {
    let database = TestDatabase::new("outbound-retry");
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
    let request = peer_send("peer-daemon-retry-1", "msn-source", "msn-remote-target");
    assert_eq!(
        queue_peer_message(&database.path, &request).unwrap().state,
        "queued"
    );

    let transport = RetryThenReceiptTransport::new();
    let first = reconcile_peer_relay(&database.path, &transport, &NoopRunner, "herdr").unwrap();
    assert_eq!(first.retried, 1);
    let durable = queue_peer_message(&database.path, &request).unwrap();
    assert!(durable.duplicate);
    assert_eq!(durable.state, "retry");

    let deadline = Instant::now() + Duration::from_secs(4);
    let final_report = loop {
        let report =
            reconcile_peer_relay(&database.path, &transport, &NoopRunner, "herdr").unwrap();
        if report.sent == 1 {
            break report;
        }
        assert!(
            Instant::now() < deadline,
            "durable peer retry never became due"
        );
        thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(final_report.retried, 0);

    let acknowledged = queue_peer_message(&database.path, &request).unwrap();
    assert!(acknowledged.duplicate);
    assert_eq!(acknowledged.state, "acknowledged");
    let calls = transport.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1]);
}

struct NoopTransport;

impl PeerTransport for NoopTransport {
    fn send(&self, _destination: &str, _envelope: &[u8]) -> std::io::Result<ProcessOutput> {
        panic!("local peer notification unexpectedly invoked transport")
    }
}

struct RetryWakeRunner {
    calls: AtomicU64,
    agent_name: String,
    pane_id: String,
}

impl ProcessRunner for RetryWakeRunner {
    fn run(&self, _program: &str, args: &[String]) -> std::io::Result<ProcessOutput> {
        assert_eq!(args.first().map(String::as_str), Some("agent"));
        assert_eq!(args.get(1).map(String::as_str), Some("prompt"));
        assert_eq!(args.get(2), Some(&self.agent_name));
        if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
            return Ok(ProcessOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "target pane temporarily unavailable".into(),
            });
        }
        Ok(ProcessOutput {
            exit_code: 0,
            stdout: serde_json::to_string(&json!({
                "result": {
                    "type": "agent_prompted",
                    "agent": {
                        "name": self.agent_name,
                        "pane_id": self.pane_id,
                        "state_change_seq": 12
                    }
                }
            }))
            .unwrap(),
            stderr: String::new(),
        })
    }
}

#[test]
fn peer_reconcile_keeps_inbox_after_wake_failure_and_retries_next_cycle() {
    let database = TestDatabase::new("wake-retry");
    database.add_mission("msn-source");
    database.add_mission("msn-target");
    configure_local_peer(&database.path, "device-a").unwrap();
    let mut request = peer_send("peer-daemon-wake-1", "msn-source", "msn-target");
    request.peer_id = None;
    assert_eq!(
        queue_peer_message(&database.path, &request).unwrap().state,
        "accepted"
    );

    let runner = RetryWakeRunner {
        calls: AtomicU64::new(0),
        agent_name: "agent-msn-target-pm".into(),
        pane_id: "pane-msn-target-pm".into(),
    };
    let first = reconcile_peer_relay(&database.path, &NoopTransport, &runner, "herdr").unwrap();
    assert_eq!(first.notify_failed, 1);
    assert_eq!(
        read_peer_inbox(&database.path, "msn-target", "pm")
            .unwrap()
            .len(),
        1
    );

    let second = reconcile_peer_relay(&database.path, &NoopTransport, &runner, "herdr").unwrap();
    assert_eq!(second.notified, 1);
    assert_eq!(second.notify_failed, 0);
    assert_eq!(
        read_peer_inbox(&database.path, "msn-target", "pm")
            .unwrap()
            .len(),
        1
    );
    assert_eq!(runner.calls.load(Ordering::Relaxed), 2);
}

struct FailingTransport {
    calls: AtomicU64,
}

impl PeerTransport for FailingTransport {
    fn send(&self, _destination: &str, _envelope: &[u8]) -> std::io::Result<ProcessOutput> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ProcessOutput {
            exit_code: 255,
            stdout: String::new(),
            stderr: "peer connection unavailable".into(),
        })
    }
}

struct TeamRunner {
    mission_id: String,
    prompted_roles: Mutex<Vec<String>>,
}

impl ProcessRunner for TeamRunner {
    fn run(&self, _program: &str, args: &[String]) -> std::io::Result<ProcessOutput> {
        if args == ["agent", "list"] {
            return Ok(ProcessOutput {
                exit_code: 0,
                stdout: serde_json::to_string(&json!({
                    "result": {
                        "agents": [
                            {
                                "name": format!("agent-{}-pm", self.mission_id),
                                "pane_id": format!("pane-{}-pm", self.mission_id),
                                "agent_status": "idle",
                                "state_change_seq": 20
                            },
                            {
                                "name": format!("agent-{}-worker", self.mission_id),
                                "pane_id": format!("pane-{}-worker", self.mission_id),
                                "agent_status": "idle",
                                "state_change_seq": 21
                            }
                        ]
                    }
                }))
                .unwrap(),
                stderr: String::new(),
            });
        }
        assert_eq!(args.first().map(String::as_str), Some("agent"));
        assert_eq!(args.get(1).map(String::as_str), Some("prompt"));
        let agent_name = args.get(2).cloned().unwrap();
        let role = agent_name.rsplit('-').next().unwrap().to_string();
        self.prompted_roles.lock().unwrap().push(role.clone());
        Ok(ProcessOutput {
            exit_code: 0,
            stdout: serde_json::to_string(&json!({
                "result": {
                    "type": "agent_prompted",
                    "agent": {
                        "name": agent_name,
                        "pane_id": format!("pane-{}-{role}", self.mission_id),
                        "state_change_seq": 22
                    }
                }
            }))
            .unwrap(),
            stderr: String::new(),
        })
    }
}

#[test]
fn peer_transport_failure_does_not_block_team_outbox_in_the_same_reconcile_cycle() {
    let database = TestDatabase::new("team-isolation");
    let mission_id = "msn-team-source";
    database.add_mission(mission_id);
    database.bind_role(mission_id, "worker");
    configure_local_peer(&database.path, "device-a").unwrap();
    upsert_peer(&database.path, "device-b", "relay-b@example.test").unwrap();
    upsert_peer_route(
        &database.path,
        "device-b",
        mission_id,
        "msn-remote-target",
        "outbound",
    )
    .unwrap();
    queue_peer_message(
        &database.path,
        &peer_send("peer-daemon-isolation-1", mission_id, "msn-remote-target"),
    )
    .unwrap();
    let assignment = kernel_dispatch_command(
        &database.path,
        mission_id,
        "pm",
        "worker",
        "task",
        "Deliver the ordinary Team Assignment.",
    )
    .unwrap();

    let runner = TeamRunner {
        mission_id: mission_id.into(),
        prompted_roles: Mutex::new(Vec::new()),
    };
    let transport = FailingTransport {
        calls: AtomicU64::new(0),
    };
    let report = kernel_reconcile_with_peer_transport(&database.path, &runner, "herdr", &transport);

    assert!(report.health.is_ok());
    let delivery = report.delivery.unwrap();
    assert_eq!(delivery.delivered, 1);
    assert_eq!(delivery.failed, 0);
    let peer = report.peer.unwrap();
    assert_eq!(peer.retried, 1);
    assert_eq!(transport.calls.load(Ordering::Relaxed), 1);
    assert_eq!(runner.prompted_roles.lock().unwrap().as_slice(), ["worker"]);

    let context = read_role_context(&database.path, mission_id, "worker").unwrap();
    let assignment_id = assignment.assignment_id.unwrap();
    assert!(context
        .pending_assignments
        .iter()
        .any(|pending| pending.id == assignment_id && pending.state == "active"));
}
