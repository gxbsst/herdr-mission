//! Role coordination primitives: kernel-backed dispatch, reply, delivery, and
//! read-only context projections.
//!
//! All writes (`kernel_dispatch_command`, `kernel_reply_command`,
//! `kernel_deliver`) flow through the kernel state machine so assignment
//! transitions and effect intents stay consistent. The direct-SQL helpers that
//! predate the kernel wiring have been removed; `read_role_context` remains as
//! a read-only projection for tests and diagnostics.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::OptionalExtension;
use serde_json::json;

use crate::creation::reconcile_role_healths_with;
use crate::{
    agent_list_argv, open_writable, parse_agent_list, parse_role_ref, read_generation,
    utc_timestamp, AgentProviderAdapter, AssignmentState, DecisionContext, DriveExecutionMode,
    DriveRequest, ErrorCategory, Generation, HandleDisposition, HandleInput, InspectQuery,
    KernelError, KernelInput, MissionKernel, ProcessRunner, RoleHealthReconciliation,
    RoleRuntimeConfig, RuntimeOwner, OWNER_IDENTITY,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAssignment {
    pub id: String,
    pub source: String,
    pub kind: String,
    pub summary: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxMessage {
    pub id: String,
    pub assignment_id: Option<String>,
    pub source: String,
    pub kind: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleContext {
    pub mission_id: String,
    pub title: String,
    pub role: String,
    pub health: String,
    pub pending_assignments: Vec<PendingAssignment>,
    pub inbox: Vec<InboxMessage>,
    pub generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReport {
    pub delivered: u32,
    pub failed: u32,
}

/// Independent results from one event-driven health and delivery pass.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconcileReport {
    pub health: Result<RoleHealthReconciliation, KernelError>,
    pub delivery: Result<DeliveryReport, KernelError>,
}

/// Result of dispatching a command through the kernel state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelDispatchOutcome {
    pub assignment_id: Option<String>,
    pub message_id: String,
    pub outbox_id: String,
}

/// Result of replying to an assignment through the kernel state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelReplyOutcome {
    pub assignment_id: String,
    pub assignment_state: Option<String>,
    pub message_id: String,
}

/// Read the context a role needs on startup: mission identity, its health,
/// pending assignments, and inbox.
pub fn read_role_context(
    database: &Path,
    mission_id: &str,
    role: &str,
) -> Result<RoleContext, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;

    let title = connection
        .query_row(
            "SELECT brief FROM team_missions WHERE mission_id = ?1",
            [mission_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_mission_read_failed", "context", error))?
        .ok_or_else(|| mission_not_found(mission_id))?;

    let health = connection
        .query_row(
            "SELECT health FROM team_roles WHERE mission_id = ?1 AND role = ?2",
            [mission_id, role],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_role_read_failed", "context", error))?
        .unwrap_or_else(|| "unknown".to_string());

    let mut assignment_statement = connection
        .prepare(
            "SELECT id, source_role, kind, summary, state
             FROM assignments
             WHERE mission_id = ?1 AND target_role = ?2 AND state IN ('queued', 'active')
             ORDER BY created_at",
        )
        .map_err(|error| sqlite_error("sqlite_assignment_read_failed", "context", error))?;
    let pending_assignments = assignment_statement
        .query_map([mission_id, role], |row| {
            Ok(PendingAssignment {
                id: row.get(0)?,
                source: row.get(1)?,
                kind: row.get(2)?,
                summary: row.get(3)?,
                state: row.get(4)?,
            })
        })
        .map_err(|error| sqlite_error("sqlite_assignment_read_failed", "context", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sqlite_error("sqlite_assignment_read_failed", "context", error))?;

    let mut message_statement = connection
        .prepare(
            "SELECT id, assignment_id, source_role, kind, body
             FROM messages
             WHERE mission_id = ?1 AND target_role = ?2
             ORDER BY created_at",
        )
        .map_err(|error| sqlite_error("sqlite_message_read_failed", "context", error))?;
    let inbox = message_statement
        .query_map([mission_id, role], |row| {
            Ok(InboxMessage {
                id: row.get(0)?,
                assignment_id: row.get(1)?,
                source: row.get(2)?,
                kind: row.get(3)?,
                body: row.get(4)?,
            })
        })
        .map_err(|error| sqlite_error("sqlite_message_read_failed", "context", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sqlite_error("sqlite_message_read_failed", "context", error))?;

    let generation = read_generation(&connection)?;

    Ok(RoleContext {
        mission_id: mission_id.to_string(),
        title,
        role: role.to_string(),
        health,
        pending_assignments,
        inbox,
        generation,
    })
}

/// Dispatch a coordination command through the kernel state machine.
///
/// Parentless `review` commands are rejected before the database is opened;
/// Reviewer assignments are created by the Worker reply transition instead.
/// It builds a `HandleInput::Command` with freshly allocated ids and the target
/// role's current generation, then persists via `MissionKernel::handle`. The
/// resulting queued outbox rows are delivered later by `kernel_deliver`.
pub fn kernel_dispatch_command(
    database: &Path,
    mission_id: &str,
    source: &str,
    target: &str,
    kind: &str,
    text: &str,
) -> Result<KernelDispatchOutcome, KernelError> {
    if kind == "review" {
        return Err(KernelError {
            category: ErrorCategory::Contract,
            code: "review_parent_required".into(),
            message: "Reviewer Assignments must be created from a completed Worker Assignment"
                .into(),
            retryable: false,
            details: BTreeMap::from([
                ("mission_id".into(), json!(mission_id)),
                ("target".into(), json!(target)),
            ]),
        });
    }
    let mut kernel = MissionKernel::open_writable_sqlite_v3(
        mission_id,
        database,
        std::time::Duration::from_millis(25),
    )?;
    let source_ref = parse_role_ref(source)?;
    let target_ref = parse_role_ref(target)?;
    let generation = read_role_generation(database, mission_id, target)?;
    let generation = Generation::new(generation).map_err(|_| KernelError {
        category: ErrorCategory::Contract,
        code: "invalid_generation".into(),
        message: "role generation token is empty".into(),
        retryable: false,
        details: BTreeMap::new(),
    })?;

    let assignment_id = unique_id("asg");
    let message_id = unique_id("msg");
    let outbox_id = unique_id("out");
    let receipt = kernel.handle(KernelInput {
        decision_context: DecisionContext {
            observed_at: utc_timestamp(),
            allocated_ids: std::collections::BTreeMap::from([
                ("assignment".to_string(), assignment_id.clone()),
                ("message".to_string(), message_id.clone()),
                ("outbox".to_string(), outbox_id.clone()),
            ]),
            generations: std::collections::BTreeMap::from([(target.to_string(), generation)]),
        },
        input: HandleInput::Command {
            command_id: unique_id("cmd"),
            kind: kind.to_string(),
            source: source_ref,
            target: Some(target_ref),
            body: serde_json::json!({ "text": text }),
        },
    })?;

    if receipt.disposition == HandleDisposition::Rejected {
        return Err(receipt.error.unwrap_or_else(|| KernelError {
            category: ErrorCategory::Domain,
            code: "command_rejected".into(),
            message: "kernel rejected the coordination command".into(),
            retryable: false,
            details: BTreeMap::new(),
        }));
    }

    Ok(KernelDispatchOutcome {
        assignment_id: (kind == "task").then_some(assignment_id),
        message_id,
        outbox_id,
    })
}

/// Reply to an assignment through the kernel state machine.
pub fn kernel_reply_command(
    database: &Path,
    mission_id: &str,
    source: &str,
    assignment_id: &str,
    kind: &str,
    text: &str,
) -> Result<KernelReplyOutcome, KernelError> {
    let mut kernel = MissionKernel::open_writable_sqlite_v3(
        mission_id,
        database,
        std::time::Duration::from_millis(25),
    )?;
    let source_ref = parse_role_ref(source)?;
    let pm_ref = parse_role_ref("pm")?;
    let pm_generation = read_role_generation(database, mission_id, "pm")?;
    let pm_generation = Generation::new(pm_generation).map_err(|_| KernelError {
        category: ErrorCategory::Contract,
        code: "invalid_generation".into(),
        message: "role generation token is empty".into(),
        retryable: false,
        details: BTreeMap::new(),
    })?;
    let reviewer_generation = read_role_generation(database, mission_id, "reviewer")?;
    let reviewer_generation = Generation::new(reviewer_generation).map_err(|_| KernelError {
        category: ErrorCategory::Contract,
        code: "invalid_generation".into(),
        message: "role generation token is empty".into(),
        retryable: false,
        details: BTreeMap::new(),
    })?;
    let worker_generation = read_role_generation(database, mission_id, "worker")?;
    let worker_generation = Generation::new(worker_generation).map_err(|_| KernelError {
        category: ErrorCategory::Contract,
        code: "invalid_generation".into(),
        message: "role generation token is empty".into(),
        retryable: false,
        details: BTreeMap::new(),
    })?;

    let message_id = unique_id("msg");
    let outbox_id = unique_id("out");
    let receipt = kernel.handle(KernelInput {
        decision_context: DecisionContext {
            observed_at: utc_timestamp(),
            allocated_ids: BTreeMap::from([
                ("message".to_string(), message_id.clone()),
                ("outbox".to_string(), outbox_id.clone()),
                ("follow_up_assignment".to_string(), unique_id("asg")),
                ("follow_up_message".to_string(), unique_id("msg")),
                ("follow_up_outbox".to_string(), unique_id("out")),
                ("review_revision".to_string(), unique_id("rev")),
                ("review_pm_notice_message".to_string(), unique_id("msg-pm")),
                ("review_pm_notice_outbox".to_string(), unique_id("out-pm")),
                (
                    "review_worker_notice_message".to_string(),
                    unique_id("msg-wk"),
                ),
                (
                    "review_worker_notice_outbox".to_string(),
                    unique_id("out-wk"),
                ),
            ]),
            generations: BTreeMap::from([
                ("pm".to_string(), pm_generation),
                ("reviewer".to_string(), reviewer_generation),
                ("worker".to_string(), worker_generation),
            ]),
        },
        input: HandleInput::Command {
            command_id: unique_id("cmd"),
            kind: kind.to_string(),
            source: source_ref,
            target: Some(pm_ref),
            body: serde_json::json!({ "assignment_id": assignment_id, "text": text }),
        },
    })?;

    if receipt.disposition == HandleDisposition::Rejected {
        return Err(receipt.error.unwrap_or_else(|| KernelError {
            category: ErrorCategory::Domain,
            code: "reply_rejected".into(),
            message: "kernel rejected the reply command".into(),
            retryable: false,
            details: BTreeMap::new(),
        }));
    }

    Ok(KernelReplyOutcome {
        assignment_id: assignment_id.to_string(),
        assignment_state: receipt.assignment_state.map(assignment_state_name),
        message_id,
    })
}

fn assignment_state_name(state: AssignmentState) -> String {
    match state {
        AssignmentState::Queued => "queued".into(),
        AssignmentState::Active => "active".into(),
        AssignmentState::Completed => "completed".into(),
        AssignmentState::Approved => "approved".into(),
        AssignmentState::Rejected => "rejected".into(),
        AssignmentState::Blocked => "blocked".into(),
    }
}

/// Read a role's startup context through the kernel `inspect` projection.
pub fn kernel_read_context(
    database: &Path,
    mission_id: &str,
    role: &str,
) -> Result<RoleContext, KernelError> {
    let kernel = MissionKernel::open_writable_sqlite_v3(
        mission_id,
        database,
        std::time::Duration::from_millis(25),
    )?;
    let status = kernel.inspect(InspectQuery::Status)?;

    let title = status
        .data
        .get("mission")
        .and_then(|mission| mission.get("brief"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();

    let health = status
        .data
        .get("roles")
        .and_then(serde_json::Value::as_array)
        .and_then(|roles| {
            roles
                .iter()
                .find(|entry| entry.get("role").and_then(serde_json::Value::as_str) == Some(role))
        })
        .and_then(|entry| entry.get("health"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let pending_assignments = status
        .data
        .get("assignments")
        .and_then(serde_json::Value::as_array)
        .map(|assignments| {
            assignments
                .iter()
                .filter(|entry| {
                    entry.get("target_role").and_then(serde_json::Value::as_str) == Some(role)
                        && matches!(
                            entry.get("state").and_then(serde_json::Value::as_str),
                            Some("queued") | Some("active")
                        )
                })
                .map(|entry| PendingAssignment {
                    id: entry
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    source: entry
                        .get("source_role")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    kind: entry
                        .get("kind")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    summary: entry
                        .get("summary")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    state: entry
                        .get("state")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let role_ref = parse_role_ref(role)?;
    let inbox_view = kernel.inspect(InspectQuery::Inbox {
        role: Some(role_ref),
    })?;
    let inbox = inbox_view
        .data
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .map(|entry| InboxMessage {
                    id: entry
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    assignment_id: entry
                        .get("assignment_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    source: entry
                        .get("source_role")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    kind: entry
                        .get("kind")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    body: entry
                        .get("body")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let generation = read_generation(&open_writable(database, OWNER_IDENTITY)?)?;

    Ok(RoleContext {
        mission_id: mission_id.to_string(),
        title,
        role: role.to_string(),
        health,
        pending_assignments,
        inbox,
        generation,
    })
}

/// Drive queued outbox messages through the kernel state machine using the
/// real agent-provider adapter.
///
/// This replaces the direct-SQL `deliver_pending`: each mission's queued
/// outbox is claimed as a `DeliverPrompt` effect, executed via
/// `agent prompt`, then resolved against the kernel so the assignment advances
/// from `queued` to `active`.
pub fn kernel_deliver(
    database: &Path,
    runner: &dyn ProcessRunner,
    herdr: &str,
) -> Result<DeliveryReport, KernelError> {
    let mission_ids = queued_mission_ids(database)?;
    let mut report = DeliveryReport {
        delivered: 0,
        failed: 0,
    };
    for mission_id in mission_ids {
        let roles = read_runtime_roles(database, &mission_id)?;
        let mut adapter = AgentProviderAdapter::new(herdr, roles, runner);
        let mut kernel = MissionKernel::open_writable_sqlite_v3(
            &mission_id,
            database,
            std::time::Duration::from_millis(25),
        )?;
        let request = DriveRequest {
            runtime_owner: RuntimeOwner::Rust,
            effect_budget: 100,
            time_budget_ms: 60_000,
            execution_mode: DriveExecutionMode::Deferred,
            claim_owner: Some(format!("kernel-driver-{}", unique_id("drv"))),
            claimed_at_ms: now_ms(),
        };
        let decision = DecisionContext {
            observed_at: utc_timestamp(),
            allocated_ids: BTreeMap::new(),
            generations: BTreeMap::new(),
        };
        let drive = kernel.drive_with(request, decision, &mut adapter)?;
        report.delivered += drive.resolved;
        report.failed += drive.pending + drive.retryable_failures + drive.terminal_failures;
    }
    Ok(report)
}

/// Reconcile persisted role health from Herdr Core, then always attempt one
/// delivery pass even when the Core snapshot is unavailable or invalid.
pub fn kernel_reconcile(
    database: &Path,
    runner: &dyn ProcessRunner,
    herdr: &str,
) -> ReconcileReport {
    let health =
        reconcile_role_healths_with(database, || match runner.run(herdr, &agent_list_argv()) {
            Ok(output) if output.exit_code == 0 => parse_agent_list(&output.stdout),
            Ok(output) => Err(KernelError {
                category: ErrorCategory::Infrastructure,
                code: "herdr_agent_list_failed".into(),
                message: "herdr agent list exited non-zero".into(),
                retryable: true,
                details: BTreeMap::from([
                    ("exit_code".into(), json!(output.exit_code)),
                    ("stderr".into(), json!(output.stderr)),
                ]),
            }),
            Err(error) => Err(KernelError {
                category: ErrorCategory::Infrastructure,
                code: "herdr_agent_list_spawn_failed".into(),
                message: "failed to run herdr agent list".into(),
                retryable: true,
                details: BTreeMap::from([("reason".into(), json!(error.to_string()))]),
            }),
        });
    let delivery = kernel_deliver(database, runner, herdr);
    ReconcileReport { health, delivery }
}

fn queued_mission_ids(database: &Path) -> Result<Vec<String>, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT mission_id FROM outbox
             WHERE status IN ('queued', 'retry', 'sending')
             ORDER BY mission_id",
        )
        .map_err(|error| sqlite_error("sqlite_outbox_read_failed", "deliver", error))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| sqlite_error("sqlite_outbox_read_failed", "deliver", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sqlite_error("sqlite_outbox_read_failed", "deliver", error))?;
    Ok(rows)
}

fn read_runtime_roles(
    database: &Path,
    mission_id: &str,
) -> Result<BTreeMap<String, RoleRuntimeConfig>, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    let mut statement = connection
        .prepare("SELECT role, provider, terminal_id FROM team_roles WHERE mission_id = ?1")
        .map_err(|error| sqlite_error("sqlite_role_read_failed", "deliver", error))?;
    let rows = statement
        .query_map([mission_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| sqlite_error("sqlite_role_read_failed", "deliver", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sqlite_error("sqlite_role_read_failed", "deliver", error))?;
    let mut roles = BTreeMap::new();
    for (role, provider, agent_name) in rows {
        roles.insert(
            role,
            RoleRuntimeConfig {
                provider,
                agent_name,
                ..Default::default()
            },
        );
    }
    Ok(roles)
}

fn read_role_generation(
    database: &Path,
    mission_id: &str,
    role: &str,
) -> Result<String, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    let generation = connection
        .query_row(
            "SELECT launch_generation FROM team_roles WHERE mission_id = ?1 AND role = ?2",
            [mission_id, role],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_role_read_failed", "generation", error))?;
    Ok(generation
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "generation-1".to_string()))
}

fn mission_not_found(mission_id: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Domain,
        code: "mission_not_found".into(),
        message: "Mission is not present in the Rust-owned database".into(),
        retryable: false,
        details: BTreeMap::from([("mission_id".into(), json!(mission_id))]),
    }
}

static UNIQUE_ID_SEQ: AtomicU64 = AtomicU64::new(1);

fn unique_id(prefix: &str) -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seq = UNIQUE_ID_SEQ.fetch_add(1, Ordering::Relaxed);
    // Fold the process-local counter into the existing secs/nanos shape so
    // same-tick callers cannot collide, without changing the id grammar.
    let mixed = u64::from(duration.subsec_nanos()).wrapping_add(seq.wrapping_mul(1_000_003));
    format!("{prefix}-{:x}-{:08x}", duration.as_secs(), mixed as u32)
}

fn now_ms() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(0)
}

fn sqlite_error(code: &str, operation: &str, error: rusqlite::Error) -> KernelError {
    KernelError {
        category: ErrorCategory::Infrastructure,
        code: code.into(),
        message: "SQLite operation failed".into(),
        retryable: false,
        details: BTreeMap::from([
            ("operation".into(), json!(operation)),
            ("reason".into(), json!(error.to_string())),
        ]),
    }
}
