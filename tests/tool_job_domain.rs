use std::collections::BTreeMap;

use herdr_mission::{
    DecisionContext, EffectOutcome, EffectResult, Generation, HandleDisposition, HandleInput,
    HandleReceipt, InspectQuery, KernelInput, MissionKernel, MissionView, RoleKind, RoleRef,
    ToolJobMode, ToolJobOutputMetadata, ToolJobRequest, ToolJobTerminalState, ToolJobTransition,
};
use serde_json::{json, Value};

fn generation(value: &str) -> Generation {
    Generation::new(value).unwrap()
}

fn role(role: RoleKind) -> RoleRef {
    RoleRef {
        role,
        instance: None,
    }
}

fn decision_context(observed_at: &str) -> DecisionContext {
    DecisionContext {
        observed_at: observed_at.into(),
        allocated_ids: BTreeMap::new(),
        generations: BTreeMap::new(),
    }
}

fn create_active_worker_assignment(kernel: &mut MissionKernel) -> String {
    let assigned = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T06:00:00Z".into(),
                allocated_ids: BTreeMap::from([
                    ("assignment".into(), "asg-tool-worker-001".into()),
                    ("message".into(), "msg-tool-worker-001".into()),
                    ("outbox".into(), "out-tool-worker-001".into()),
                ]),
                generations: BTreeMap::from([("worker".into(), generation("generation-worker"))]),
            },
            input: HandleInput::Command {
                command_id: "cmd-tool-worker-001".into(),
                kind: "task".into(),
                source: role(RoleKind::Pm),
                target: Some(role(RoleKind::Worker)),
                body: json!({"text": "Run the approved Rust verification"}),
            },
        })
        .unwrap();
    activate_assignment(kernel, &assigned);
    assigned.created_ids["assignment"].clone()
}

fn activate_assignment(kernel: &mut MissionKernel, assigned: &HandleReceipt) {
    let intent = assigned.effect_intents.first().unwrap();
    kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T06:01:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([("worker".into(), intent.generation.clone())]),
            },
            input: HandleInput::EffectResult {
                result: EffectResult {
                    effect_id: intent.effect_id.clone(),
                    generation: intent.generation.clone(),
                    claim_owner: "memory-driver".into(),
                    outcome: EffectOutcome::Succeeded {
                        observation: json!({"delivered": true}),
                    },
                },
            },
        })
        .unwrap();
}

fn tool_job_request(job_id: &str, assignment_id: &str) -> KernelInput {
    KernelInput {
        decision_context: decision_context("2026-08-14T06:02:00Z"),
        input: HandleInput::ToolJobRequest {
            request: ToolJobRequest {
                job_id: job_id.into(),
                assignment_id: assignment_id.into(),
                source: role(RoleKind::Worker),
                mode: ToolJobMode::Bounded,
                label: "Rust tests".into(),
                argv: vec!["cargo".into(), "test".into()],
                cwd: "/tmp/mission-worktree".into(),
                env: BTreeMap::from([("NO_COLOR".into(), "1".into()), ("CI".into(), "1".into())]),
                timeout_seconds: 30.0,
                parallel: false,
                max_output_bytes: 2 * 1024 * 1024,
            },
        },
    }
}

fn tool_job_transition(
    transition_id: &str,
    job_id: &str,
    owner: RoleKind,
    observed_at: &str,
    transition: ToolJobTransition,
) -> KernelInput {
    KernelInput {
        decision_context: decision_context(observed_at),
        input: HandleInput::ToolJobTransition {
            transition_id: transition_id.into(),
            job_id: job_id.into(),
            owner: role(owner),
            transition,
        },
    }
}

fn started_transition() -> ToolJobTransition {
    ToolJobTransition::Started {
        pane_id: "wteam:p3".into(),
        coordination_dir: "/tmp/coordination".into(),
        request_path: "/tmp/coordination/request.json".into(),
        stdout_path: "/tmp/coordination/stdout.log".into(),
        stderr_path: "/tmp/coordination/stderr.log".into(),
        result_path: "/tmp/coordination/result.json".into(),
    }
}

fn completed_transition(state: ToolJobTerminalState) -> ToolJobTransition {
    ToolJobTransition::Completed {
        output: ToolJobOutputMetadata {
            state,
            exit_code: Some(0),
            stdout_path: "/tmp/coordination/stdout.log".into(),
            stderr_path: "/tmp/coordination/stderr.log".into(),
            result_path: "/tmp/coordination/result.json".into(),
            stdout_bytes: 3,
            stderr_bytes: 0,
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_checksum: "dc51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22"
                .into(),
            stderr_checksum: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .into(),
            error: String::new(),
        },
    }
}

fn tool_job<'a>(view: &'a MissionView, job_id: &str) -> &'a Value {
    view.data["tool_jobs"]
        .as_array()
        .expect("status must expose typed Tool Jobs")
        .iter()
        .find(|job| job["job_id"] == job_id)
        .expect("requested Tool Job must be present")
}

#[test]
fn tool_job_request_fingerprint_makes_exact_replay_idempotent_and_conflicts_fail_closed() {
    let mut kernel = MissionKernel::in_memory("msn-tool-jobs");
    let assignment_id = create_active_worker_assignment(&mut kernel);
    let request = tool_job_request("job-fingerprint-001", &assignment_id);
    let mut semantic_replay = tool_job_request("job-fingerprint-001", &assignment_id);
    let HandleInput::ToolJobRequest {
        request: replay_request,
    } = &mut semantic_replay.input
    else {
        unreachable!();
    };
    replay_request.env =
        BTreeMap::from([("CI".into(), "1".into()), ("NO_COLOR".into(), "1".into())]);

    let first = kernel.handle(request.clone()).unwrap();
    let replay = kernel.handle(semantic_replay).unwrap();
    let mut conflicting = request;
    let HandleInput::ToolJobRequest { request } = &mut conflicting.input else {
        unreachable!();
    };
    request.argv.push("--all-targets".into());
    let conflict = kernel.handle(conflicting).unwrap();

    assert_eq!(first.disposition, HandleDisposition::Applied);
    assert_eq!(first.created_ids["tool_job"], "job-fingerprint-001");
    assert_eq!(replay.disposition, HandleDisposition::Duplicate);
    assert_eq!(conflict.disposition, HandleDisposition::Rejected);
    assert_eq!(conflict.error.unwrap().code, "input_id_conflict");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["tool_jobs"].as_array().unwrap().len(), 1);
}

#[test]
fn tool_job_lifecycle_persists_running_and_terminal_output_metadata() {
    let mut kernel = MissionKernel::in_memory("msn-tool-jobs");
    let assignment_id = create_active_worker_assignment(&mut kernel);
    kernel
        .handle(tool_job_request("job-lifecycle-001", &assignment_id))
        .unwrap();

    let started = kernel
        .handle(tool_job_transition(
            "job-transition-started-001",
            "job-lifecycle-001",
            RoleKind::Worker,
            "2026-08-14T06:03:00Z",
            started_transition(),
        ))
        .unwrap();
    assert_eq!(started.disposition, HandleDisposition::Applied);
    let running = kernel.inspect(InspectQuery::Status).unwrap();
    let running_job = tool_job(&running, "job-lifecycle-001");
    assert_eq!(running_job["state"], "running");
    assert_eq!(running_job["pane_id"], "wteam:p3");

    let completed_input = tool_job_transition(
        "job-transition-completed-001",
        "job-lifecycle-001",
        RoleKind::Worker,
        "2026-08-14T06:04:00Z",
        completed_transition(ToolJobTerminalState::Succeeded),
    );
    let completed = kernel.handle(completed_input.clone()).unwrap();
    let replay = kernel.handle(completed_input).unwrap();
    assert_eq!(completed.disposition, HandleDisposition::Applied);
    assert_eq!(replay.disposition, HandleDisposition::Duplicate);

    let terminal = kernel.inspect(InspectQuery::Status).unwrap();
    let terminal_job = tool_job(&terminal, "job-lifecycle-001");
    assert_eq!(terminal_job["state"], "succeeded");
    assert_eq!(terminal_job["exit_code"], 0);
    assert_eq!(terminal_job["stdout_bytes"], 3);
    assert_eq!(
        terminal_job["stdout_checksum"],
        "dc51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22"
    );

    let invalid = kernel
        .handle(tool_job_transition(
            "job-transition-restart-001",
            "job-lifecycle-001",
            RoleKind::Worker,
            "2026-08-14T06:05:00Z",
            started_transition(),
        ))
        .unwrap();
    assert_eq!(invalid.disposition, HandleDisposition::Rejected);
    assert_eq!(invalid.error.unwrap().code, "invalid_tool_job_transition");
}

#[test]
fn queued_and_running_cancellation_follow_distinct_typed_transitions() {
    let mut kernel = MissionKernel::in_memory("msn-tool-jobs");
    let assignment_id = create_active_worker_assignment(&mut kernel);

    kernel
        .handle(tool_job_request("job-cancel-queued", &assignment_id))
        .unwrap();
    kernel
        .handle(tool_job_transition(
            "job-transition-cancel-queued",
            "job-cancel-queued",
            RoleKind::Worker,
            "2026-08-14T06:03:00Z",
            ToolJobTransition::CancelRequested,
        ))
        .unwrap();

    kernel
        .handle(tool_job_request("job-cancel-running", &assignment_id))
        .unwrap();
    kernel
        .handle(tool_job_transition(
            "job-transition-start-running",
            "job-cancel-running",
            RoleKind::Worker,
            "2026-08-14T06:04:00Z",
            started_transition(),
        ))
        .unwrap();
    kernel
        .handle(tool_job_transition(
            "job-transition-cancel-running",
            "job-cancel-running",
            RoleKind::Worker,
            "2026-08-14T06:05:00Z",
            ToolJobTransition::CancelRequested,
        ))
        .unwrap();

    let cancelling = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(
        tool_job(&cancelling, "job-cancel-queued")["state"],
        "cancelled"
    );
    assert_eq!(
        tool_job(&cancelling, "job-cancel-running")["state"],
        "cancelling"
    );

    kernel
        .handle(tool_job_transition(
            "job-transition-result-running",
            "job-cancel-running",
            RoleKind::Worker,
            "2026-08-14T06:06:00Z",
            completed_transition(ToolJobTerminalState::Succeeded),
        ))
        .unwrap();
    let cancelled = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(
        tool_job(&cancelled, "job-cancel-running")["state"],
        "cancelled"
    );
}

#[test]
fn tool_job_request_and_transitions_are_fenced_by_assignment_owner() {
    let mut kernel = MissionKernel::in_memory("msn-tool-jobs");
    let assignment_id = create_active_worker_assignment(&mut kernel);
    let mut unauthorized_request = tool_job_request("job-owner-001", &assignment_id);
    let HandleInput::ToolJobRequest { request } = &mut unauthorized_request.input else {
        unreachable!();
    };
    request.source = role(RoleKind::Reviewer);

    let rejected_request = kernel.handle(unauthorized_request).unwrap();
    assert_eq!(rejected_request.disposition, HandleDisposition::Rejected);
    assert_eq!(
        rejected_request.error.unwrap().code,
        "tool_job_owner_mismatch"
    );

    kernel
        .handle(tool_job_request("job-owner-002", &assignment_id))
        .unwrap();
    let rejected_transition = kernel
        .handle(tool_job_transition(
            "job-transition-owner-001",
            "job-owner-002",
            RoleKind::Reviewer,
            "2026-08-14T06:03:00Z",
            started_transition(),
        ))
        .unwrap();
    assert_eq!(rejected_transition.disposition, HandleDisposition::Rejected);
    assert_eq!(
        rejected_transition.error.unwrap().code,
        "tool_job_owner_mismatch"
    );
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(tool_job(&status, "job-owner-002")["state"], "queued");
}
