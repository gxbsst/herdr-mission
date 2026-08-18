use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use herdr_mission::{
    create_mission, default_codex_team, kernel_deliver, kernel_dispatch_command,
    kernel_read_context, kernel_reply_command, read_role_context, CreateMissionRequest,
    InspectQuery, MissionKernel, ProcessOutput, ProcessRunner,
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
