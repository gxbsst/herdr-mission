use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use herdr_mission::{
    create_mission, default_codex_team, kernel_deliver, kernel_dispatch_command,
    kernel_read_context, kernel_reconcile, kernel_reply_command, read_role_context,
    reconcile_role_healths, record_role_runtime, AgentSnapshot, AgentStatus, CreateMissionRequest,
    DecisionContext, DriveExecutionMode, DriveRequest, InspectQuery, LaunchMode, MissionKernel,
    ProcessOutput, ProcessRunner, RuntimeOwner,
};
use rusqlite::Connection;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn temp_db(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "herdr-mission-kernel-store-{label}-{}-{id}.sqlite3",
        std::process::id()
    ))
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn request(mission_id: &str) -> CreateMissionRequest {
    CreateMissionRequest {
        mission_id: mission_id.to_string(),
        brief: "kernel store".into(),
        template: "general".into(),
        agent_profile_id: "codex-default-v1".into(),
        agent_profile_version: 1,
        launch_mode: LaunchMode::Manual,
        roles: default_codex_team(),
    }
}

#[test]
fn opens_production_database_and_inspects() {
    let path = temp_db("open");
    create_mission(&path, &request("msn-kernel-open")).unwrap();

    let kernel =
        MissionKernel::open_writable_sqlite_v3("msn-kernel-open", &path, Duration::from_millis(25))
            .unwrap();
    let view = kernel.inspect(InspectQuery::Status).unwrap();
    assert!(view.data.is_object());

    cleanup(&path);
}

#[test]
fn kernel_deliver_keeps_not_started_agent_recoverable_across_many_cycles() {
    let path = temp_db("deliver-not-started");
    create_mission(&path, &request("msn-kernel-deliver-not-started")).unwrap();

    // The worker role exists but its herdr agent has not started yet, so the
    // prompt resolves to agent_not_found on every attempt.
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE team_roles SET terminal_id = 'mission-x-worker'
             WHERE mission_id = 'msn-kernel-deliver-not-started' AND role = 'worker'",
            [],
        )
        .unwrap();
    drop(connection);

    kernel_dispatch_command(
        &path,
        "msn-kernel-deliver-not-started",
        "pm",
        "worker",
        "task",
        "wait for me",
    )
    .unwrap();

    // Run far more delivery cycles than MAX_DELIVERY_ATTEMPTS, resetting the
    // backoff timestamp between cycles to simulate the daemon waking later.
    // The outbox must never orphan: attempts stays zeroed and it stays queued.
    for _ in 0..8 {
        let runner = AgentNotFoundRunner {
            calls: RefCell::new(Vec::new()),
        };
        kernel_deliver(&path, &runner, "herdr").unwrap();

        let connection = Connection::open(&path).unwrap();
        let (status, attempts): (String, i64) = connection
            .query_row(
                "SELECT status, attempts FROM outbox WHERE mission_id = 'msn-kernel-deliver-not-started'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "queued");
        assert_eq!(attempts, 0);
        // Simulate the pending backoff elapsing so the next cycle re-claims.
        connection
            .execute(
                "UPDATE outbox SET claimed_at = NULL
                 WHERE mission_id = 'msn-kernel-deliver-not-started'",
                [],
            )
            .unwrap();
        drop(connection);
    }

    cleanup(&path);
}

#[test]
fn reply_notice_to_pm_delivers_without_reactivating_assignment() {
    let path = temp_db("reply-notice");
    create_mission(&path, &request("msn-kernel-reply-notice")).unwrap();

    // Simulate launched PM and worker so delivery can address both.
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE team_roles SET terminal_id = 'mission-x-pm'
             WHERE mission_id = 'msn-kernel-reply-notice' AND role = 'pm'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE team_roles SET terminal_id = 'mission-x-worker'
             WHERE mission_id = 'msn-kernel-reply-notice' AND role = 'worker'",
            [],
        )
        .unwrap();
    drop(connection);

    let outcome = kernel_dispatch_command(
        &path,
        "msn-kernel-reply-notice",
        "pm",
        "worker",
        "task",
        "do it",
    )
    .unwrap();
    let assignment_id = outcome.assignment_id.unwrap();

    let runner = FakeRunner {
        calls: RefCell::new(Vec::new()),
    };
    // First delivery activates the worker assignment.
    kernel_deliver(&path, &runner, "herdr").unwrap();

    kernel_reply_command(
        &path,
        "msn-kernel-reply-notice",
        "worker",
        &assignment_id,
        "completed",
        "done",
    )
    .unwrap();

    // Second delivery delivers the PM notice and must NOT try to re-activate
    // the already-completed assignment.
    kernel_deliver(&path, &runner, "herdr").unwrap();

    let connection = Connection::open(&path).unwrap();
    let state: String = connection
        .query_row(
            "SELECT state FROM assignments WHERE id = ?1",
            [&assignment_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "completed");

    let pm_outbox_state: String = connection
        .query_row(
            "SELECT status FROM outbox WHERE mission_id = 'msn-kernel-reply-notice' AND target_role = 'pm'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pm_outbox_state, "delivered");

    cleanup(&path);
}

#[test]
fn rejects_database_without_owner_marker() {
    let path = temp_db("foreign");
    // A schema-v3 database that was never bootstrapped by herdr-mission has no
    // plugin_owner marker and must be rejected.
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(include_str!("fixtures/schema-v3.sql"))
        .unwrap();
    drop(connection);

    let error = MissionKernel::open_writable_sqlite_v3(
        "msn-kernel-foreign",
        &path,
        Duration::from_millis(25),
    )
    .unwrap_err();
    assert!(error.code.contains("owner"));

    cleanup(&path);
}

#[test]
fn kernel_dispatch_writes_assignment_and_is_readable() {
    let path = temp_db("dispatch");
    create_mission(&path, &request("msn-kernel-dispatch")).unwrap();

    let outcome = kernel_dispatch_command(
        &path,
        "msn-kernel-dispatch",
        "pm",
        "worker",
        "task",
        "fix via kernel",
    )
    .unwrap();
    assert!(outcome.assignment_id.is_some());

    // The kernel wrote through the same tables, so the direct read sees it.
    let context = read_role_context(&path, "msn-kernel-dispatch", "worker").unwrap();
    assert_eq!(context.pending_assignments.len(), 1);
    assert_eq!(context.pending_assignments[0].summary, "fix via kernel");
    assert_eq!(context.pending_assignments[0].state, "queued");
    assert_eq!(context.inbox.len(), 1);

    cleanup(&path);
}

#[test]
fn kernel_read_context_sees_kernel_dispatched_assignment() {
    let path = temp_db("read-context");
    create_mission(&path, &request("msn-kernel-read")).unwrap();
    kernel_dispatch_command(
        &path,
        "msn-kernel-read",
        "pm",
        "worker",
        "task",
        "readable via inspect",
    )
    .unwrap();

    let context = kernel_read_context(&path, "msn-kernel-read", "worker").unwrap();
    assert_eq!(context.pending_assignments.len(), 1);
    assert_eq!(
        context.pending_assignments[0].summary,
        "readable via inspect"
    );
    assert_eq!(context.inbox.len(), 1);

    cleanup(&path);
}

struct FakeRunner {
    calls: RefCell<Vec<(String, Vec<String>)>>,
}

impl ProcessRunner for FakeRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<ProcessOutput> {
        self.calls
            .borrow_mut()
            .push((program.to_string(), args.to_vec()));
        Ok(ProcessOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// A runner whose `herdr agent prompt` always reports the target agent has not
/// started yet. Mirrors a role that was queued before its pane/agent existed.
struct AgentNotFoundRunner {
    calls: RefCell<Vec<(String, Vec<String>)>>,
}

impl ProcessRunner for AgentNotFoundRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<ProcessOutput> {
        self.calls
            .borrow_mut()
            .push((program.to_string(), args.to_vec()));
        Ok(ProcessOutput {
            exit_code: 1,
            stdout: r#"{"error":{"code":"agent_not_found","message":"agent target not found"}}"#
                .into(),
            stderr: String::new(),
        })
    }
}

#[test]
fn role_health_reconciliation_uses_exact_bindings_and_marks_missing_roles() {
    let path = temp_db("role-health-reconcile");
    create_mission(&path, &request("msn-role-health-reconcile")).unwrap();
    let connection = Connection::open(&path).unwrap();
    for (role, pane, name, health) in [
        ("pm", "w16:p1", "mission-pm", "idle"),
        ("scout", "w16:p3", "mission-scout", "working"),
        ("reviewer", "w16:p6", "mission-reviewer", "idle"),
    ] {
        connection
            .execute(
                "UPDATE team_roles SET pane_id = ?1, terminal_id = ?2, health = ?3
                 WHERE mission_id = 'msn-role-health-reconcile' AND role = ?4",
                rusqlite::params![pane, name, health, role],
            )
            .unwrap();
    }
    drop(connection);

    let report = reconcile_role_healths(
        &path,
        &[
            AgentSnapshot {
                name: Some("mission-pm".into()),
                pane_id: "w16:p1".into(),
                status: AgentStatus::Working,
            },
            AgentSnapshot {
                name: Some("mission-scout".into()),
                pane_id: "w16:p3".into(),
                status: AgentStatus::Done,
            },
            // Same name but a different pane must not be adopted.
            AgentSnapshot {
                name: Some("mission-reviewer".into()),
                pane_id: "w16:p9".into(),
                status: AgentStatus::Working,
            },
            // An unnamed Core agent cannot match a persisted role binding.
            AgentSnapshot {
                name: None,
                pane_id: "w16:p7".into(),
                status: AgentStatus::Idle,
            },
        ],
    )
    .unwrap();

    assert_eq!(report.matched, 2);
    assert_eq!(report.missing, 1);
    assert_eq!(report.updated, 3);
    let connection = Connection::open(&path).unwrap();
    let health = |role: &str| -> String {
        connection
            .query_row(
                "SELECT health FROM team_roles
                 WHERE mission_id = 'msn-role-health-reconcile' AND role = ?1",
                [role],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(health("pm"), "working");
    assert_eq!(health("scout"), "done");
    assert_eq!(health("reviewer"), "missing");
    assert_eq!(health("worker"), "unknown");

    cleanup(&path);
}

struct MalformedListRunner {
    calls: RefCell<Vec<Vec<String>>>,
}

struct BlockingSnapshotRunner {
    started: mpsc::Sender<()>,
    release: Option<mpsc::Receiver<()>>,
    status: &'static str,
}

impl ProcessRunner for BlockingSnapshotRunner {
    fn run(&self, _program: &str, args: &[String]) -> std::io::Result<ProcessOutput> {
        if args == ["agent", "list"] {
            self.started.send(()).unwrap();
            if let Some(release) = &self.release {
                release.recv().unwrap();
            }
            return Ok(ProcessOutput {
                exit_code: 0,
                stdout: format!(
                    r#"{{"result":{{"agents":[{{"name":"mission-reviewer","pane_id":"w16:p6","agent_status":"{}"}}]}}}}"#,
                    self.status
                ),
                stderr: String::new(),
            });
        }
        Ok(ProcessOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

#[test]
fn concurrent_reconciles_fetch_and_apply_snapshots_in_commit_order() {
    let path = temp_db("reconcile-serialization");
    create_mission(&path, &request("msn-reconcile-serialization")).unwrap();
    record_role_runtime(
        &path,
        "msn-reconcile-serialization",
        "reviewer",
        "w16:p6",
        "mission-reviewer",
    )
    .unwrap();

    let (old_started_tx, old_started_rx) = mpsc::channel();
    let (old_release_tx, old_release_rx) = mpsc::channel();
    let old_path = path.clone();
    let old = thread::spawn(move || {
        let runner = BlockingSnapshotRunner {
            started: old_started_tx,
            release: Some(old_release_rx),
            status: "working",
        };
        kernel_reconcile(&old_path, &runner, "herdr")
    });
    old_started_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let (new_started_tx, new_started_rx) = mpsc::channel();
    let new_path = path.clone();
    let new = thread::spawn(move || {
        let runner = BlockingSnapshotRunner {
            started: new_started_tx,
            release: None,
            status: "idle",
        };
        kernel_reconcile(&new_path, &runner, "herdr")
    });
    assert!(matches!(
        new_started_rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    old_release_tx.send(()).unwrap();
    old.join().unwrap().health.unwrap();
    new_started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    new.join().unwrap().health.unwrap();

    let connection = Connection::open(&path).unwrap();
    let health: String = connection
        .query_row(
            "SELECT health FROM team_roles
             WHERE mission_id = 'msn-reconcile-serialization' AND role = 'reviewer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(health, "idle");

    cleanup(&path);
}

#[test]
fn role_rebind_waits_for_snapshot_and_is_not_marked_missing() {
    let path = temp_db("reconcile-rebind");
    create_mission(&path, &request("msn-reconcile-rebind")).unwrap();
    record_role_runtime(
        &path,
        "msn-reconcile-rebind",
        "reviewer",
        "w16:p6",
        "mission-reviewer",
    )
    .unwrap();

    let (snapshot_started_tx, snapshot_started_rx) = mpsc::channel();
    let (snapshot_release_tx, snapshot_release_rx) = mpsc::channel();
    let reconcile_path = path.clone();
    let reconcile = thread::spawn(move || {
        let runner = BlockingSnapshotRunner {
            started: snapshot_started_tx,
            release: Some(snapshot_release_rx),
            status: "working",
        };
        kernel_reconcile(&reconcile_path, &runner, "herdr")
    });
    snapshot_started_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let (rebind_done_tx, rebind_done_rx) = mpsc::channel();
    let rebind_path = path.clone();
    let rebind = thread::spawn(move || {
        let result = record_role_runtime(
            &rebind_path,
            "msn-reconcile-rebind",
            "reviewer",
            "w16:p9",
            "mission-reviewer-new",
        );
        rebind_done_tx.send(result).unwrap();
    });
    assert!(matches!(
        rebind_done_rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    snapshot_release_tx.send(()).unwrap();
    reconcile.join().unwrap().health.unwrap();
    rebind_done_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    rebind.join().unwrap();

    let connection = Connection::open(&path).unwrap();
    let (pane_id, agent_name, health): (String, String, String) = connection
        .query_row(
            "SELECT pane_id, terminal_id, health FROM team_roles
             WHERE mission_id = 'msn-reconcile-rebind' AND role = 'reviewer'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(pane_id, "w16:p9");
    assert_eq!(agent_name, "mission-reviewer-new");
    assert_eq!(health, "idle");

    cleanup(&path);
}

impl ProcessRunner for MalformedListRunner {
    fn run(&self, _program: &str, args: &[String]) -> std::io::Result<ProcessOutput> {
        self.calls.borrow_mut().push(args.to_vec());
        if args == ["agent", "list"] {
            return Ok(ProcessOutput {
                exit_code: 0,
                stdout: "not json".into(),
                stderr: String::new(),
            });
        }
        Ok(ProcessOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

#[test]
fn reconciliation_failure_preserves_health_and_still_delivers_outbox() {
    let path = temp_db("reconcile-failure-delivery");
    create_mission(&path, &request("msn-reconcile-failure-delivery")).unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE team_roles
             SET pane_id = 'w16:p2', terminal_id = 'mission-worker', health = 'idle'
             WHERE mission_id = 'msn-reconcile-failure-delivery' AND role = 'worker'",
            [],
        )
        .unwrap();
    drop(connection);
    kernel_dispatch_command(
        &path,
        "msn-reconcile-failure-delivery",
        "pm",
        "worker",
        "task",
        "deliver despite failed health sync",
    )
    .unwrap();

    let runner = MalformedListRunner {
        calls: RefCell::new(Vec::new()),
    };
    let report = kernel_reconcile(&path, &runner, "herdr");
    assert_eq!(report.health.unwrap_err().code, "herdr_response_malformed");
    assert_eq!(report.delivery.unwrap().delivered, 1);

    let connection = Connection::open(&path).unwrap();
    let health: String = connection
        .query_row(
            "SELECT health FROM team_roles
             WHERE mission_id = 'msn-reconcile-failure-delivery' AND role = 'worker'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let assignment_state: String = connection
        .query_row(
            "SELECT state FROM assignments
             WHERE mission_id = 'msn-reconcile-failure-delivery'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(health, "idle");
    assert_eq!(assignment_state, "active");
    assert_eq!(runner.calls.borrow()[0], ["agent", "list"]);
    assert!(runner
        .calls
        .borrow()
        .iter()
        .any(|args| args.first().map(String::as_str) == Some("agent")
            && args.get(1).map(String::as_str) == Some("prompt")));

    cleanup(&path);
}

#[test]
fn kernel_deliver_advances_assignment_to_active() {
    let path = temp_db("deliver");
    create_mission(&path, &request("msn-kernel-deliver")).unwrap();

    // Simulate a launched worker by recording its live agent name.
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE team_roles SET terminal_id = 'mission-x-worker' WHERE mission_id = 'msn-kernel-deliver' AND role = 'worker'",
            [],
        )
        .unwrap();
    drop(connection);

    kernel_dispatch_command(
        &path,
        "msn-kernel-deliver",
        "pm",
        "worker",
        "task",
        "deliver me",
    )
    .unwrap();

    let runner = FakeRunner {
        calls: RefCell::new(Vec::new()),
    };
    let report = kernel_deliver(&path, &runner, "herdr").unwrap();
    assert_eq!(report.delivered, 1);

    // The assignment advanced from queued to active.
    let connection = Connection::open(&path).unwrap();
    let state: String = connection
        .query_row(
            "SELECT state FROM assignments WHERE mission_id = 'msn-kernel-deliver'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "active");

    cleanup(&path);
}

#[test]
fn kernel_deliver_backs_off_when_agent_is_not_assigned() {
    let path = temp_db("deliver-pending");
    create_mission(&path, &request("msn-kernel-deliver-pending")).unwrap();

    kernel_dispatch_command(
        &path,
        "msn-kernel-deliver-pending",
        "pm",
        "worker",
        "task",
        "wait for agent",
    )
    .unwrap();

    let runner = FakeRunner {
        calls: RefCell::new(Vec::new()),
    };
    let report = kernel_deliver(&path, &runner, "herdr").unwrap();
    // The agent is not launched, so delivery parks the outbox back in `queued`
    // with a zeroed retry budget instead of burning a retry and eventually
    // orphaning it at MAX_DELIVERY_ATTEMPTS.
    assert_eq!(report.delivered, 0);
    assert_eq!(report.failed, 1);
    assert!(runner.calls.borrow().is_empty());

    let connection = Connection::open(&path).unwrap();
    let (status, attempts): (String, i64) = connection
        .query_row(
            "SELECT status, attempts FROM outbox WHERE mission_id = 'msn-kernel-deliver-pending'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "queued");
    assert_eq!(attempts, 0);

    cleanup(&path);
}

#[test]
fn kernel_reply_completes_active_assignment() {
    let path = temp_db("reply");
    create_mission(&path, &request("msn-kernel-reply")).unwrap();

    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE team_roles SET terminal_id = 'mission-x-worker' WHERE mission_id = 'msn-kernel-reply' AND role = 'worker'",
            [],
        )
        .unwrap();
    drop(connection);

    let outcome =
        kernel_dispatch_command(&path, "msn-kernel-reply", "pm", "worker", "task", "do it")
            .unwrap();
    let assignment_id = outcome.assignment_id.unwrap();

    let runner = FakeRunner {
        calls: RefCell::new(Vec::new()),
    };
    kernel_deliver(&path, &runner, "herdr").unwrap();

    let reply = kernel_reply_command(
        &path,
        "msn-kernel-reply",
        "worker",
        &assignment_id,
        "completed",
        "done",
    )
    .unwrap();
    assert_eq!(reply.assignment_state.as_deref(), Some("completed"));

    let connection = Connection::open(&path).unwrap();
    let state: String = connection
        .query_row(
            "SELECT state FROM assignments WHERE id = ?1",
            [&assignment_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "completed");

    cleanup(&path);
}

#[test]
fn queued_reviewer_follow_up_waits_for_active_capacity_before_delivery() {
    let path = temp_db("reviewer-follow-up-capacity");
    let mission_id = "msn-kernel-reviewer-follow-up-capacity";
    create_mission(&path, &request(mission_id)).unwrap();

    let connection = Connection::open(&path).unwrap();
    for (role, terminal_id) in [
        ("pm", "mission-x-pm"),
        ("worker", "mission-x-worker"),
        ("reviewer", "mission-x-reviewer"),
    ] {
        connection
            .execute(
                "UPDATE team_roles SET terminal_id = ?1 WHERE mission_id = ?2 AND role = ?3",
                rusqlite::params![terminal_id, mission_id, role],
            )
            .unwrap();
    }
    drop(connection);

    kernel_dispatch_command(
        &path,
        mission_id,
        "pm",
        "reviewer",
        "review",
        "review the other work",
    )
    .unwrap();
    let held_review = kernel_read_context(&path, mission_id, "reviewer")
        .unwrap()
        .pending_assignments[0]
        .id
        .clone();
    let runner = FakeRunner {
        calls: RefCell::new(Vec::new()),
    };
    kernel_deliver(&path, &runner, "herdr").unwrap();

    let worker = kernel_dispatch_command(
        &path,
        mission_id,
        "pm",
        "worker",
        "task",
        "complete while reviewer is busy",
    )
    .unwrap()
    .assignment_id
    .unwrap();
    kernel_deliver(&path, &runner, "herdr").unwrap();
    runner.calls.borrow_mut().clear();

    let completed = kernel_reply_command(
        &path,
        mission_id,
        "worker",
        &worker,
        "completed",
        "ready for review",
    )
    .unwrap();
    assert_eq!(completed.assignment_state.as_deref(), Some("completed"));
    let reviewer_context = kernel_read_context(&path, mission_id, "reviewer").unwrap();
    let queued_review = reviewer_context
        .pending_assignments
        .iter()
        .find(|assignment| assignment.state == "queued")
        .expect("automatic reviewer follow-up must be persisted as queued")
        .id
        .clone();

    let waiting_report = kernel_deliver(&path, &runner, "herdr").unwrap();
    assert_eq!(waiting_report.delivered, 1);
    assert_eq!(runner.calls.borrow().len(), 1);
    assert!(!runner.calls.borrow()[0]
        .1
        .iter()
        .any(|argument| argument == "mission-x-reviewer"));
    let connection = Connection::open(&path).unwrap();
    let waiting_outbox: (String, i64) = connection
        .query_row(
            "SELECT outbox.status, outbox.attempts
             FROM outbox JOIN messages ON messages.id = outbox.message_id
             WHERE messages.assignment_id = ?1 AND messages.kind = 'review'",
            [&queued_review],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(waiting_outbox, ("queued".into(), 0));
    drop(connection);

    kernel_reply_command(
        &path,
        mission_id,
        "reviewer",
        &held_review,
        "blocked",
        "capacity released",
    )
    .unwrap();
    runner.calls.borrow_mut().clear();

    let resumed_report = kernel_deliver(&path, &runner, "herdr").unwrap();
    assert_eq!(resumed_report.delivered, 2);
    assert_eq!(runner.calls.borrow().len(), 2);
    let reviewer_context = kernel_read_context(&path, mission_id, "reviewer").unwrap();
    let resumed_review = reviewer_context
        .pending_assignments
        .iter()
        .find(|assignment| assignment.id == queued_review)
        .expect("queued reviewer follow-up must survive reopen and resume");
    assert_eq!(resumed_review.state, "active");

    cleanup(&path);
}

#[test]
fn queued_worker_fix_waits_for_active_capacity_before_delivery() {
    let path = temp_db("worker-fix-capacity");
    let mission_id = "msn-kernel-worker-fix-capacity";
    create_mission(&path, &request(mission_id)).unwrap();

    let connection = Connection::open(&path).unwrap();
    for (role, terminal_id) in [
        ("pm", "mission-x-pm"),
        ("worker", "mission-x-worker"),
        ("reviewer", "mission-x-reviewer"),
    ] {
        connection
            .execute(
                "UPDATE team_roles SET terminal_id = ?1 WHERE mission_id = ?2 AND role = ?3",
                rusqlite::params![terminal_id, mission_id, role],
            )
            .unwrap();
    }
    drop(connection);

    let original_work = kernel_dispatch_command(
        &path,
        mission_id,
        "pm",
        "worker",
        "task",
        "work that will need review",
    )
    .unwrap()
    .assignment_id
    .unwrap();
    let runner = FakeRunner {
        calls: RefCell::new(Vec::new()),
    };
    kernel_deliver(&path, &runner, "herdr").unwrap();
    kernel_reply_command(
        &path,
        mission_id,
        "worker",
        &original_work,
        "completed",
        "ready for review",
    )
    .unwrap();
    kernel_deliver(&path, &runner, "herdr").unwrap();
    let review_assignment = kernel_read_context(&path, mission_id, "reviewer")
        .unwrap()
        .pending_assignments
        .into_iter()
        .find(|assignment| assignment.state == "active")
        .expect("automatic review must be active")
        .id;

    let competing_work = kernel_dispatch_command(
        &path,
        mission_id,
        "pm",
        "worker",
        "task",
        "work that holds worker capacity",
    )
    .unwrap()
    .assignment_id
    .unwrap();
    kernel_deliver(&path, &runner, "herdr").unwrap();
    runner.calls.borrow_mut().clear();

    let rejected = kernel_reply_command(
        &path,
        mission_id,
        "reviewer",
        &review_assignment,
        "rejected",
        "one focused correction is required",
    )
    .unwrap();
    assert_eq!(rejected.assignment_state.as_deref(), Some("rejected"));
    let worker_context = kernel_read_context(&path, mission_id, "worker").unwrap();
    let queued_fix = worker_context
        .pending_assignments
        .iter()
        .find(|assignment| assignment.state == "queued")
        .expect("automatic Worker fix must be persisted as queued")
        .id
        .clone();

    let waiting_report = kernel_deliver(&path, &runner, "herdr").unwrap();
    assert_eq!(waiting_report.delivered, 3);
    assert_eq!(runner.calls.borrow().len(), 3);
    assert!(runner.calls.borrow().iter().all(|(_, arguments)| {
        arguments
            .iter()
            .all(|argument| !argument.contains("修复 Reviewer 指出的问题"))
    }));
    let connection = Connection::open(&path).unwrap();
    let waiting_outbox: (String, i64) = connection
        .query_row(
            "SELECT outbox.status, outbox.attempts
             FROM outbox JOIN messages ON messages.id = outbox.message_id
             WHERE messages.assignment_id = ?1 AND messages.kind = 'fix'",
            [&queued_fix],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(waiting_outbox, ("queued".into(), 0));
    drop(connection);

    kernel_reply_command(
        &path,
        mission_id,
        "worker",
        &competing_work,
        "blocked",
        "capacity released",
    )
    .unwrap();
    runner.calls.borrow_mut().clear();

    let resumed_report = kernel_deliver(&path, &runner, "herdr").unwrap();
    assert_eq!(resumed_report.delivered, 2);
    assert_eq!(runner.calls.borrow().len(), 2);
    let worker_context = kernel_read_context(&path, mission_id, "worker").unwrap();
    let resumed_fix = worker_context
        .pending_assignments
        .iter()
        .find(|assignment| assignment.id == queued_fix)
        .expect("queued Worker fix must survive reopen and resume");
    assert_eq!(resumed_fix.state, "active");

    cleanup(&path);
}

#[test]
fn deferred_drive_claims_only_one_queued_singleton_assignment() {
    let path = temp_db("queued-singleton-serialization");
    let mission_id = "msn-kernel-queued-singleton-serialization";
    create_mission(&path, &request(mission_id)).unwrap();

    let connection = Connection::open(&path).unwrap();
    for (role, terminal_id) in [
        ("worker", "mission-x-worker"),
        ("reviewer", "mission-x-reviewer"),
    ] {
        connection
            .execute(
                "UPDATE team_roles SET terminal_id = ?1 WHERE mission_id = ?2 AND role = ?3",
                rusqlite::params![terminal_id, mission_id, role],
            )
            .unwrap();
    }
    drop(connection);

    let worker = kernel_dispatch_command(
        &path,
        mission_id,
        "pm",
        "worker",
        "task",
        "complete after a review is already queued",
    )
    .unwrap()
    .assignment_id
    .unwrap();
    let runner = FakeRunner {
        calls: RefCell::new(Vec::new()),
    };
    kernel_deliver(&path, &runner, "herdr").unwrap();
    kernel_dispatch_command(
        &path,
        mission_id,
        "pm",
        "reviewer",
        "review",
        "first queued review",
    )
    .unwrap();
    kernel_reply_command(
        &path,
        mission_id,
        "worker",
        &worker,
        "completed",
        "second queued review",
    )
    .unwrap();

    let mut kernel =
        MissionKernel::open_writable_sqlite_v3(mission_id, &path, Duration::from_millis(25))
            .unwrap();
    let drive = kernel
        .drive(
            DriveRequest {
                runtime_owner: RuntimeOwner::Rust,
                effect_budget: 10,
                time_budget_ms: 10,
                execution_mode: DriveExecutionMode::Deferred,
                claim_owner: Some("queued-singleton-serialization-test".into()),
                claimed_at_ms: i64::MAX,
            },
            DecisionContext {
                observed_at: "2026-08-28T00:00:00Z".into(),
                allocated_ids: Default::default(),
                generations: Default::default(),
            },
        )
        .unwrap();
    let claimed_reviews = drive
        .claimed_effects
        .iter()
        .filter(|effect| {
            matches!(
                &effect.intent.intent,
                herdr_mission::EffectIntentKind::DeliverPrompt {
                    role,
                    assignment_id: Some(_),
                    ..
                } if role.role == herdr_mission::RoleKind::Reviewer
            )
        })
        .count();
    assert_eq!(claimed_reviews, 1);

    cleanup(&path);
}

#[test]
fn bare_scout_round_trips_dispatch_and_reply_through_the_kernel() {
    let path = temp_db("scout-roundtrip");
    create_mission(&path, &request("msn-kernel-scout-roundtrip")).unwrap();

    // Simulate a launched scout by recording its live agent name.
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE team_roles SET terminal_id = 'mission-x-scout' WHERE mission_id = 'msn-kernel-scout-roundtrip' AND role = 'scout'",
            [],
        )
        .unwrap();
    drop(connection);

    let outcome = kernel_dispatch_command(
        &path,
        "msn-kernel-scout-roundtrip",
        "pm",
        "scout",
        "task",
        "read-only survey",
    )
    .unwrap();
    let assignment_id = outcome.assignment_id.unwrap();

    let context = kernel_read_context(&path, "msn-kernel-scout-roundtrip", "scout").unwrap();
    assert_eq!(context.pending_assignments.len(), 1);
    assert_eq!(context.pending_assignments[0].id, assignment_id);

    let runner = FakeRunner {
        calls: RefCell::new(Vec::new()),
    };
    kernel_deliver(&path, &runner, "herdr").unwrap();

    let reply = kernel_reply_command(
        &path,
        "msn-kernel-scout-roundtrip",
        "scout",
        &assignment_id,
        "finding",
        "evidence found",
    )
    .unwrap();
    assert_eq!(reply.assignment_state.as_deref(), Some("completed"));

    let pm = kernel_read_context(&path, "msn-kernel-scout-roundtrip", "pm").unwrap();
    assert!(pm
        .inbox
        .iter()
        .any(|message| message.body == "evidence found"));

    cleanup(&path);
}

#[test]
fn kernel_reply_reviewer_approved_twice_without_allocated_id_conflict() {
    let path = temp_db("reviewer-approved-twice");
    let mission_id = "msn-kernel-reviewer-approved-twice";
    create_mission(&path, &request(mission_id)).unwrap();

    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE team_roles SET terminal_id = 'mission-x-worker' WHERE mission_id = ?1 AND role = 'worker'",
            [mission_id],
        )
        .unwrap();
    drop(connection);

    for round in 1..=2 {
        let outcome = kernel_dispatch_command(
            &path,
            mission_id,
            "pm",
            "worker",
            "task",
            &format!("round {round}"),
        )
        .unwrap();
        let worker_assignment = outcome.assignment_id.expect("worker assignment");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE assignments SET state = 'active' WHERE id = ?1 AND state = 'queued'",
                [&worker_assignment],
            )
            .unwrap();
        drop(connection);

        let completed = kernel_reply_command(
            &path,
            mission_id,
            "worker",
            &worker_assignment,
            "completed",
            &format!("done {round}"),
        )
        .unwrap_or_else(|error| {
            panic!(
                "worker completed round {round} failed: {} ({})",
                error.message, error.code
            )
        });
        assert_eq!(completed.assignment_state.as_deref(), Some("completed"));

        let connection = Connection::open(&path).unwrap();
        let review_id: String = connection
            .query_row(
                "SELECT id FROM assignments
                 WHERE mission_id = ?1 AND target_role = 'reviewer' AND parent_id = ?2
                 ORDER BY created_at DESC LIMIT 1",
                rusqlite::params![mission_id, worker_assignment],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| {
                panic!("missing reviewer assignment in round {round}: {error}")
            });
        connection
            .execute(
                "UPDATE assignments SET state = 'active' WHERE id = ?1 AND state = 'queued'",
                [&review_id],
            )
            .unwrap();
        drop(connection);

        let approved = kernel_reply_command(
            &path,
            mission_id,
            "reviewer",
            &review_id,
            "approved",
            &format!("ok {round}"),
        )
        .unwrap_or_else(|error| {
            panic!(
                "reviewer approved round {round} failed: {} ({})",
                error.message, error.code
            )
        });
        assert_eq!(approved.assignment_state.as_deref(), Some("approved"));
        assert_ne!(
            approved.assignment_state.as_deref(),
            Some("allocated_id_conflict")
        );
    }

    cleanup(&path);
}
