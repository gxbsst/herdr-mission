use std::collections::BTreeMap;

use herdr_mission::{
    parse_request, EffectIntent, EffectIntentKind, EffectOutcome, EffectResult, ErrorCategory,
    Generation, HandleDisposition, HandleInput, HandleReceipt, KernelError, Operation,
    OperationKind, RoleAttachMode, RoleKind, RoleRef, ToolJobMode, ToolJobTerminalState,
    ToolJobTransition,
};
use serde_json::{json, Value};

fn base_request(operation: Value) -> Value {
    json!({
        "protocol": "herdr.mission.kernel.v1",
        "binary_contract": "herdr.mission.kernel.binary.v1",
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
}

fn generation(value: &str) -> Generation {
    Generation::new(value).unwrap()
}

#[test]
fn versioned_requests_deserialize_into_the_three_public_operations() {
    let requests = [
        base_request(json!({
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
        })),
        base_request(json!({
            "type": "drive",
            "request": {
                "runtime_owner": "rust",
                "effect_budget": 1,
                "time_budget_ms": 100
            }
        })),
        base_request(json!({
            "type": "inspect",
            "request": {
                "query": { "type": "status" }
            }
        })),
    ];

    let kinds: Vec<_> = requests
        .iter()
        .map(|request| parse_request(request.clone()).unwrap().operation.kind())
        .collect();

    assert_eq!(
        kinds,
        [
            OperationKind::Handle,
            OperationKind::Drive,
            OperationKind::Inspect
        ]
    );
}

#[test]
fn decision_context_accepts_opaque_generation_tokens() {
    let mut request = base_request(json!({
        "type": "inspect",
        "request": {
            "query": { "type": "status" }
        }
    }));
    request["decision_context"]["generations"] = json!({
        "worker": "generation-worker"
    });

    let parsed = parse_request(request).unwrap();

    assert_eq!(
        serde_json::to_value(parsed.decision_context.generations).unwrap(),
        json!({ "worker": "generation-worker" })
    );
}

#[test]
fn role_observation_accepts_a_non_numeric_generation_token() {
    let mut request = base_request(json!({
        "type": "handle",
        "request": {
            "input": {
                "type": "role_observation",
                "observation_id": "obs-worker-001",
                "role": { "role": "worker" },
                "generation": "generation-worker",
                "state": "ready",
                "details": {}
            }
        }
    }));
    request["decision_context"]["generations"] = json!({
        "worker": "generation-worker"
    });

    let parsed = parse_request(request).unwrap();

    assert_eq!(
        serde_json::to_value(parsed.operation).unwrap()["request"]["input"]["generation"],
        "generation-worker"
    );
}

#[test]
fn role_launch_request_carries_opaque_generation_owner_window_and_attach_mode() {
    let mut request = base_request(json!({
        "type": "handle",
        "request": {
            "input": {
                "type": "role_launch_request",
                "launch_id": "launch-worker-001",
                "role": { "role": "worker" },
                "generation": "generation-worker/opaque:A",
                "launch_owner": "launch-driver-001",
                "acquired_at": 1786712400,
                "expires_at": 1786712460,
                "attach_mode": "manual"
            }
        }
    }));
    request["decision_context"]["generations"] = json!({
        "worker": "generation-worker/opaque:A"
    });

    let parsed = parse_request(request).unwrap();
    let Operation::Handle(handle) = parsed.operation else {
        panic!("expected handle operation");
    };
    let HandleInput::RoleLaunchRequest {
        launch_id,
        generation,
        launch_owner,
        acquired_at,
        expires_at,
        attach_mode,
        ..
    } = handle.input
    else {
        panic!("expected typed role launch request");
    };

    assert_eq!(launch_id, "launch-worker-001");
    assert_eq!(generation.as_str(), "generation-worker/opaque:A");
    assert_eq!(launch_owner, "launch-driver-001");
    assert_eq!((acquired_at, expires_at), (1786712400, 1786712460));
    assert_eq!(attach_mode, RoleAttachMode::Manual);
}

#[test]
fn empty_generation_tokens_are_rejected_at_the_protocol_boundary() {
    let mut request = base_request(json!({
        "type": "inspect",
        "request": {
            "query": { "type": "status" }
        }
    }));
    request["decision_context"]["generations"] = json!({ "worker": "" });

    let error = parse_request(request).unwrap_err();

    assert!(error
        .to_string()
        .contains("generation must be a non-empty opaque string"));
}

#[test]
fn receipt_error_and_effect_types_have_stable_json_tags() {
    let role = RoleRef {
        role: RoleKind::Worker,
        instance: None,
    };
    let error = KernelError {
        category: ErrorCategory::Infrastructure,
        code: "provider_unavailable".into(),
        message: "provider is unavailable".into(),
        retryable: true,
        details: BTreeMap::new(),
    };
    let intent = EffectIntent {
        effect_id: "eff-001".into(),
        generation: generation("generation-worker"),
        intent: EffectIntentKind::DeliverPrompt {
            role: role.clone(),
            assignment_id: Some("asg-001".into()),
            prompt: "fixture prompt".into(),
        },
    };
    let result = EffectResult {
        effect_id: "eff-001".into(),
        generation: generation("generation-worker"),
        claim_owner: "driver-001".into(),
        outcome: EffectOutcome::RetryableFailure {
            error: error.clone(),
            retry_after_ms: 250,
        },
    };
    let receipt = HandleReceipt {
        input_id: "cmd-001".into(),
        disposition: HandleDisposition::Applied,
        revision: Some(3),
        effect_intents: vec![intent],
        created_ids: BTreeMap::new(),
        relationships: BTreeMap::new(),
        review_round: None,
        assignment_state: None,
        review_limit_reached: false,
        error: None,
    };

    assert_eq!(
        serde_json::to_value((receipt, result, error)).unwrap(),
        json!([{
            "input_id": "cmd-001",
            "disposition": "applied",
            "revision": 3,
            "effect_intents": [{
                "effect_id": "eff-001",
                "generation": "generation-worker",
                "intent": {
                    "type": "deliver_prompt",
                    "role": { "role": "worker" },
                    "assignment_id": "asg-001",
                    "prompt": "fixture prompt"
                }
            }]
        }, {
            "effect_id": "eff-001",
            "generation": "generation-worker",
            "claim_owner": "driver-001",
            "outcome": {
                "type": "retryable_failure",
                "error": {
                    "category": "infrastructure",
                    "code": "provider_unavailable",
                    "message": "provider is unavailable",
                    "retryable": true,
                    "details": {}
                },
                "retry_after_ms": 250
            }
        }, {
            "category": "infrastructure",
            "code": "provider_unavailable",
            "message": "provider is unavailable",
            "retryable": true,
            "details": {}
        }])
    );
}

#[test]
fn notice_prompt_intents_omit_assignment_identity() {
    let intent = EffectIntent {
        effect_id: "eff-notice-001".into(),
        generation: generation("generation-worker"),
        intent: EffectIntentKind::DeliverPrompt {
            role: RoleRef {
                role: RoleKind::Worker,
                instance: None,
            },
            assignment_id: None,
            prompt: "Context only".into(),
        },
    };

    assert_eq!(
        serde_json::to_value(intent).unwrap(),
        json!({
            "effect_id": "eff-notice-001",
            "generation": "generation-worker",
            "intent": {
                "type": "deliver_prompt",
                "role": { "role": "worker" },
                "prompt": "Context only"
            }
        })
    );
}

#[test]
fn tool_job_requests_use_a_dedicated_typed_protocol_shape() {
    let parsed = parse_request(base_request(json!({
        "type": "handle",
        "request": {
            "input": {
                "type": "tool_job_request",
                "request": {
                    "job_id": "job-tests-001",
                    "assignment_id": "asg-worker-001",
                    "source": { "role": "worker" },
                    "mode": "bounded",
                    "label": "Rust tests",
                    "argv": ["cargo", "test"],
                    "cwd": "/tmp/worktree",
                    "env": {
                        "NO_COLOR": "1",
                        "CI": "1"
                    },
                    "timeout_seconds": 30,
                    "parallel": false,
                    "max_output_bytes": 2097152
                }
            }
        }
    })))
    .unwrap();

    let Operation::Handle(handle) = parsed.operation else {
        panic!("expected handle operation");
    };
    let HandleInput::ToolJobRequest { request } = handle.input else {
        panic!("expected typed Tool Job request");
    };

    assert_eq!(request.job_id, "job-tests-001");
    assert_eq!(request.assignment_id, "asg-worker-001");
    assert_eq!(request.source.role, RoleKind::Worker);
    assert_eq!(request.mode, ToolJobMode::Bounded);
    assert_eq!(request.argv, ["cargo", "test"]);
    assert_eq!(
        request.env.keys().cloned().collect::<Vec<_>>(),
        ["CI", "NO_COLOR"]
    );
}

#[test]
fn tool_job_completion_carries_typed_terminal_output_metadata() {
    let parsed = parse_request(base_request(json!({
        "type": "handle",
        "request": {
            "input": {
                "type": "tool_job_transition",
                "transition_id": "job-transition-001",
                "job_id": "job-tests-001",
                "owner": { "role": "worker" },
                "transition": {
                    "type": "completed",
                    "output": {
                        "state": "succeeded",
                        "exit_code": 0,
                        "stdout_path": "/tmp/stdout.log",
                        "stderr_path": "/tmp/stderr.log",
                        "result_path": "/tmp/result.json",
                        "stdout_bytes": 3,
                        "stderr_bytes": 0,
                        "stdout_truncated": false,
                        "stderr_truncated": false,
                        "stdout_checksum": "dc51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22",
                        "stderr_checksum": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                        "error": ""
                    }
                }
            }
        }
    })))
    .unwrap();

    let Operation::Handle(handle) = parsed.operation else {
        panic!("expected handle operation");
    };
    let HandleInput::ToolJobTransition { transition, .. } = handle.input else {
        panic!("expected typed Tool Job transition");
    };
    let ToolJobTransition::Completed { output } = transition else {
        panic!("expected typed Tool Job completion");
    };

    assert_eq!(output.state, ToolJobTerminalState::Succeeded);
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout_bytes, 3);
    assert!(!output.stdout_truncated);
    assert!(output.error.is_empty());
}
