use std::collections::BTreeMap;

use herdr_mission::{
    AssignmentState, DecisionContext, DriveExecutionMode, DriveRequest, EffectOutcome,
    EffectResult, Generation, HandleDisposition, HandleInput, HandleReceipt, InspectQuery,
    KernelInput, MissionKernel, RoleKind, RoleRef, RoleState, RuntimeOwner,
};
use serde_json::json;

fn generation(value: &str) -> Generation {
    Generation::new(value).unwrap()
}

fn decision_context() -> DecisionContext {
    DecisionContext {
        observed_at: "2026-08-14T04:30:00Z".into(),
        allocated_ids: BTreeMap::from([
            ("assignment".into(), "asg-1786681800000-a1b2c3".into()),
            ("message".into(), "msg-1786681800000-d4e5f6".into()),
            ("outbox".into(), "out-1786681800000-0a1b2c".into()),
        ]),
        generations: BTreeMap::from([("worker".into(), generation("generation-worker"))]),
    }
}

#[test]
fn mission_kernel_exposes_the_public_drive_entrypoint() {
    let mut kernel = MissionKernel::in_memory("mission-public-drive");
    let assigned = kernel.handle(task_input()).unwrap();

    let report = kernel
        .drive(
            DriveRequest {
                runtime_owner: RuntimeOwner::Rust,
                effect_budget: 1,
                time_budget_ms: 10,
                execution_mode: DriveExecutionMode::Deferred,
                claim_owner: Some("public-driver".into()),
                claimed_at_ms: 1_786_681_800_000,
            },
            decision_context(),
        )
        .unwrap();

    assert_eq!(report.claimed, 1);
    assert_eq!(report.resolved, 0);
    let claimed = &report.claimed_effects[0];

    let forged = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T04:31:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([("worker".into(), claimed.intent.generation.clone())]),
            },
            input: HandleInput::EffectResult {
                result: EffectResult {
                    effect_id: claimed.intent.effect_id.clone(),
                    generation: claimed.intent.generation.clone(),
                    claim_owner: "forged-driver".into(),
                    outcome: EffectOutcome::Succeeded {
                        observation: json!({"delivered": true}),
                    },
                },
            },
        })
        .unwrap();
    assert_eq!(forged.disposition, HandleDisposition::Rejected);
    assert_eq!(forged.error.unwrap().code, "outbox_claim_owner_mismatch");

    let resolved = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T04:32:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([("worker".into(), claimed.intent.generation.clone())]),
            },
            input: HandleInput::EffectResult {
                result: EffectResult {
                    effect_id: claimed.intent.effect_id.clone(),
                    generation: claimed.intent.generation.clone(),
                    claim_owner: claimed.claim_owner.clone(),
                    outcome: EffectOutcome::Succeeded {
                        observation: json!({"delivered": true}),
                    },
                },
            },
        })
        .unwrap();
    assert_eq!(resolved.disposition, HandleDisposition::Applied);
    assert_eq!(
        resolved.created_ids["assignment"],
        assigned.created_ids["assignment"]
    );
}

fn task_input() -> KernelInput {
    KernelInput {
        decision_context: decision_context(),
        input: HandleInput::Command {
            command_id: "cmd-0123456789abcdef".into(),
            kind: "task".into(),
            source: RoleRef {
                role: RoleKind::Pm,
                instance: None,
            },
            target: Some(RoleRef {
                role: RoleKind::Worker,
                instance: None,
            }),
            body: json!({"text": "Implement the approved slice"}),
        },
    }
}

fn worker_completion_input(assignment_id: &str) -> KernelInput {
    KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T04:32:00Z".into(),
            allocated_ids: BTreeMap::from([
                ("message".into(), "msg-1786681920000-000001".into()),
                ("outbox".into(), "out-1786681920000-000002".into()),
                (
                    "follow_up_assignment".into(),
                    "asg-1786681920000-000003".into(),
                ),
                (
                    "follow_up_message".into(),
                    "msg-1786681920000-000004".into(),
                ),
                ("follow_up_outbox".into(), "out-1786681920000-000005".into()),
            ]),
            generations: BTreeMap::from([
                ("pm".into(), generation("generation-pm")),
                ("reviewer".into(), generation("generation-reviewer")),
            ]),
        },
        input: HandleInput::Command {
            command_id: "cmd-1234567890abcdef".into(),
            kind: "completed".into(),
            source: RoleRef {
                role: RoleKind::Worker,
                instance: None,
            },
            target: Some(RoleRef {
                role: RoleKind::Pm,
                instance: None,
            }),
            body: json!({
                "assignment_id": assignment_id,
                "text": "Implementation and tests are complete"
            }),
        },
    }
}

fn pm_assignment_input(
    command_id: &str,
    kind: &str,
    target: RoleRef,
    id_suffix: &str,
    generation_key: &str,
    generation_token: &str,
) -> KernelInput {
    KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T05:00:00Z".into(),
            allocated_ids: BTreeMap::from([
                ("assignment".into(), format!("asg-{id_suffix}")),
                ("message".into(), format!("msg-{id_suffix}")),
                ("outbox".into(), format!("out-{id_suffix}")),
            ]),
            generations: BTreeMap::from([(generation_key.into(), generation(generation_token))]),
        },
        input: HandleInput::Command {
            command_id: command_id.into(),
            kind: kind.into(),
            source: RoleRef {
                role: RoleKind::Pm,
                instance: None,
            },
            target: Some(target),
            body: json!({"text": format!("Run {kind} assignment")}),
        },
    }
}

fn succeeded_effect(
    effect_id: &str,
    generation_key: &str,
    generation_token: &Generation,
) -> KernelInput {
    KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T05:01:00Z".into(),
            allocated_ids: BTreeMap::new(),
            generations: BTreeMap::from([(generation_key.into(), generation_token.clone())]),
        },
        input: HandleInput::EffectResult {
            result: EffectResult {
                effect_id: effect_id.into(),
                generation: generation_token.clone(),
                claim_owner: "memory-driver".into(),
                outcome: EffectOutcome::Succeeded {
                    observation: json!({"delivered": true}),
                },
            },
        },
    }
}

fn activate(
    kernel: &mut MissionKernel,
    assigned: &HandleReceipt,
    generation_key: &str,
) -> HandleReceipt {
    let intent = assigned
        .effect_intents
        .first()
        .expect("assignment dispatch must emit one delivery effect");
    kernel
        .handle(succeeded_effect(
            &intent.effect_id,
            generation_key,
            &intent.generation,
        ))
        .unwrap()
}

#[test]
fn handle_applies_a_pm_task_with_injected_time_ids_and_generation() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");

    let receipt = kernel.handle(task_input()).unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Applied);
    assert_eq!(receipt.revision, Some(1));
    assert_eq!(
        receipt.created_ids,
        BTreeMap::from([
            ("assignment".into(), "asg-1786681800000-a1b2c3".into()),
            ("message".into(), "msg-1786681800000-d4e5f6".into()),
            ("outbox".into(), "out-1786681800000-0a1b2c".into()),
        ])
    );
    assert_eq!(receipt.effect_intents.len(), 1);
    assert_eq!(
        receipt.effect_intents[0].effect_id,
        "out-1786681800000-0a1b2c"
    );
    assert_eq!(
        receipt.effect_intents[0].generation,
        generation("generation-worker")
    );
}

#[test]
fn inspect_exposes_assignment_identity_state_relationships_and_observed_time() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();

    let queued = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(
        queued.data["assignments"][0]["id"],
        assigned.created_ids["assignment"]
    );
    assert_eq!(queued.data["assignments"][0]["target"]["role"], "worker");
    assert_eq!(queued.data["assignments"][0]["state"], "queued");
    assert_eq!(queued.data["assignments"][0]["parent_id"], json!(null));
    assert_eq!(queued.data["assignments"][0]["review_round"], 0);
    assert_eq!(
        queued.data["assignments"][0]["observed_at"],
        "2026-08-14T04:30:00Z"
    );

    activate(&mut kernel, &assigned, "worker");
    let active = kernel
        .inspect(InspectQuery::AssignmentThread {
            assignment_id: assigned.created_ids["assignment"].clone(),
        })
        .unwrap();
    assert_eq!(active.data["assignment"]["state"], "active");
    assert_eq!(
        active.data["assignment"]["observed_at"],
        "2026-08-14T05:01:00Z"
    );
    assert_eq!(
        active.data["messages"][0]["observed_at"],
        "2026-08-14T04:30:00Z"
    );
}

#[test]
fn handle_rejects_malformed_observed_time_before_mutating_state() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut input = task_input();
    input.decision_context.observed_at = "not-a-timestamp".into();

    let error = kernel.handle(input).unwrap_err();

    assert_eq!(error.code, "invalid_observed_at");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["revision"], 0);
    assert_eq!(status.data["assignment_count"], 0);
}

#[test]
fn observed_at_rejects_impossible_calendar_dates_without_mutating_state() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut input = task_input();
    input.decision_context.observed_at = "2026-02-31T04:30:00Z".into();

    let error = kernel.handle(input).unwrap_err();

    assert_eq!(error.code, "invalid_observed_at");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["revision"], 0);
    assert_eq!(status.data["assignment_count"], 0);
}

#[test]
fn observed_at_accepts_rfc3339_fractional_seconds() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut input = task_input();
    input.decision_context.observed_at = "2026-08-14T04:30:00.123Z".into();

    let receipt = kernel.handle(input).unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Applied);
}

#[test]
fn observed_at_rejects_non_leap_second_sixty_without_mutating_state() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut input = task_input();
    input.decision_context.observed_at = "2026-08-14T04:30:60Z".into();

    let error = kernel.handle(input).unwrap_err();

    assert_eq!(error.code, "invalid_observed_at");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["revision"], 0);
    assert_eq!(status.data["assignment_count"], 0);
}

#[test]
fn handle_replays_the_original_receipt_for_a_duplicate_command_id() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");

    let applied = kernel.handle(task_input()).unwrap();
    let duplicate = kernel.handle(task_input()).unwrap();

    assert_eq!(duplicate.disposition, HandleDisposition::Duplicate);
    assert_eq!(duplicate.revision, applied.revision);
    assert_eq!(duplicate.created_ids, applied.created_ids);
    assert_eq!(duplicate.effect_intents, applied.effect_intents);
}

#[test]
fn same_command_payload_is_duplicate_when_retry_decision_context_changes() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let applied = kernel.handle(task_input()).unwrap();
    let mut retry = task_input();
    retry.decision_context.observed_at = "2026-08-14T04:30:01Z".into();
    retry.decision_context.allocated_ids = BTreeMap::from([
        ("assignment".into(), "asg-1786681801000-a1b2c3".into()),
        ("message".into(), "msg-1786681801000-d4e5f6".into()),
        ("outbox".into(), "out-1786681801000-0a1b2c".into()),
    ]);
    retry.decision_context.generations =
        BTreeMap::from([("worker".into(), generation("generation-worker-retry"))]);

    let duplicate = kernel.handle(retry).unwrap();

    assert_eq!(duplicate.disposition, HandleDisposition::Duplicate);
    assert_eq!(duplicate.revision, applied.revision);
    assert_eq!(duplicate.created_ids, applied.created_ids);
    assert_eq!(duplicate.effect_intents, applied.effect_intents);
}

#[test]
fn handle_returns_a_structured_rejected_receipt_for_an_invalid_assignment_kind() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut input = task_input();
    let HandleInput::Command { kind, .. } = &mut input.input else {
        unreachable!();
    };
    *kind = "unknown".into();

    let receipt = kernel.handle(input).unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Rejected);
    assert_eq!(receipt.revision, None);
    assert!(receipt.created_ids.is_empty());
    assert!(receipt.effect_intents.is_empty());
    assert_eq!(receipt.error.unwrap().code, "invalid_assignment_kind");
}

#[test]
fn handle_rejects_cross_role_delivery_that_is_not_in_the_team_acl() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut input = task_input();
    let HandleInput::Command { source, target, .. } = &mut input.input else {
        unreachable!();
    };
    *source = RoleRef {
        role: RoleKind::Worker,
        instance: None,
    };
    *target = Some(RoleRef {
        role: RoleKind::Reviewer,
        instance: None,
    });

    let receipt = kernel.handle(input).unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Rejected);
    assert_eq!(receipt.error.unwrap().code, "acl_denied");
    assert!(receipt.created_ids.is_empty());
}

#[test]
fn handle_allows_pm_assignment_dispatch_to_worker_reviewer_and_dynamic_scout() {
    let scenarios = [
        ("task", RoleKind::Worker, None, "worker"),
        ("review", RoleKind::Reviewer, None, "reviewer"),
        ("task", RoleKind::Scout, Some("scout-07"), "scout-07"),
    ];

    for (kind_value, role_kind, instance_value, generation_key) in scenarios {
        let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
        let mut input = task_input();
        let HandleInput::Command { kind, target, .. } = &mut input.input else {
            unreachable!();
        };
        *kind = kind_value.into();
        *target = Some(RoleRef {
            role: role_kind,
            instance: instance_value.map(str::to_owned),
        });
        input.decision_context.generations =
            BTreeMap::from([(generation_key.into(), generation("generation-dispatch"))]);

        let receipt = kernel.handle(input).unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(
            receipt.effect_intents[0].generation,
            generation("generation-dispatch")
        );
        let herdr_mission::EffectIntentKind::DeliverPrompt { role, .. } =
            &receipt.effect_intents[0].intent
        else {
            panic!("assignment dispatch must create a deliver_prompt intent");
        };
        assert_eq!(role.role, role_kind);
        assert_eq!(role.instance.as_deref(), instance_value);
    }
}

#[test]
fn handle_requires_the_exact_dynamic_scout_identity_that_owns_the_assignment() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut assignment_input = task_input();
    let HandleInput::Command { target, .. } = &mut assignment_input.input else {
        unreachable!();
    };
    *target = Some(RoleRef {
        role: RoleKind::Scout,
        instance: Some("scout-07".into()),
    });
    assignment_input.decision_context.generations =
        BTreeMap::from([("scout-07".into(), generation("generation-scout-07"))]);
    let assignment = kernel.handle(assignment_input).unwrap();
    let activated = activate(&mut kernel, &assignment, "scout-07");
    assert_eq!(activated.assignment_state, Some(AssignmentState::Active));
    let assignment_id = assignment.created_ids["assignment"].clone();

    let reply = |command_id: &str, scout: &str| KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T04:31:00Z".into(),
            allocated_ids: BTreeMap::from([
                ("message".into(), "msg-1786681860000-aabbcc".into()),
                ("outbox".into(), "out-1786681860000-ddeeff".into()),
            ]),
            generations: BTreeMap::from([("pm".into(), generation("generation-pm"))]),
        },
        input: HandleInput::Command {
            command_id: command_id.into(),
            kind: "finding".into(),
            source: RoleRef {
                role: RoleKind::Scout,
                instance: Some(scout.into()),
            },
            target: Some(RoleRef {
                role: RoleKind::Pm,
                instance: None,
            }),
            body: json!({
                "assignment_id": assignment_id,
                "text": "The compatibility evidence is complete"
            }),
        },
    };

    let wrong_owner = kernel
        .handle(reply("cmd-fedcba9876543210", "scout-08"))
        .unwrap();
    assert_eq!(wrong_owner.disposition, HandleDisposition::Rejected);
    assert_eq!(wrong_owner.error.unwrap().code, "assignment_owner_mismatch");

    let accepted = kernel
        .handle(reply("cmd-0011223344556677", "scout-07"))
        .unwrap();
    assert_eq!(accepted.disposition, HandleDisposition::Applied);
    assert_eq!(accepted.revision, Some(2));
    assert_eq!(accepted.created_ids.len(), 2);
    assert_eq!(accepted.effect_intents.len(), 1);
}

#[test]
fn handle_turns_context_into_a_notice_without_creating_an_assignment() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut input = task_input();
    let HandleInput::Command { kind, body, .. } = &mut input.input else {
        unreachable!();
    };
    *kind = "context".into();
    *body = json!({"text": "Use the frozen compatibility corpus"});
    input.decision_context.allocated_ids.remove("assignment");

    let receipt = kernel.handle(input).unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Applied);
    assert_eq!(receipt.revision, Some(1));
    assert_eq!(
        receipt.created_ids.keys().cloned().collect::<Vec<_>>(),
        vec!["message".to_string(), "outbox".to_string()]
    );
    assert_eq!(receipt.effect_intents.len(), 1);
    let inbox = kernel
        .inspect(InspectQuery::Inbox {
            role: Some(RoleRef {
                role: RoleKind::Worker,
                instance: None,
            }),
        })
        .unwrap();
    assert_eq!(inbox.data["messages"].as_array().unwrap().len(), 1);
    assert_eq!(inbox.data["messages"][0]["kind"], "context");
    assert!(inbox.data["messages"][0]["assignment_id"].is_null());
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["assignment_count"], 0);
}

#[test]
fn succeeded_notice_delivery_resolves_without_activating_assignment_work() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut input = task_input();
    let HandleInput::Command { kind, body, .. } = &mut input.input else {
        unreachable!();
    };
    *kind = "context".into();
    *body = json!({"text": "Use the frozen compatibility corpus"});
    input.decision_context.allocated_ids.remove("assignment");
    let notice = kernel.handle(input).unwrap();

    let resolved = kernel
        .handle(succeeded_effect(
            &notice.created_ids["outbox"],
            "worker",
            &generation("generation-worker"),
        ))
        .unwrap();

    assert_eq!(resolved.disposition, HandleDisposition::Applied);
    assert_eq!(resolved.assignment_state, None);
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["assignment_count"], 0);
    assert_eq!(status.data["effect_count"], 0);
}

#[test]
fn context_ids_cannot_overwrite_existing_notice_objects() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut first = task_input();
    let HandleInput::Command { kind, body, .. } = &mut first.input else {
        unreachable!();
    };
    *kind = "context".into();
    *body = json!({"text": "First context"});
    first.decision_context.allocated_ids.remove("assignment");
    kernel.handle(first.clone()).unwrap();

    let HandleInput::Command {
        command_id, body, ..
    } = &mut first.input
    else {
        unreachable!();
    };
    *command_id = "cmd-context-id-collision".into();
    *body = json!({"text": "Different context"});

    let error = kernel.handle(first).unwrap_err();

    assert_eq!(error.code, "allocated_id_conflict");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["message_count"], 1);
    assert_eq!(status.data["effect_count"], 1);
}

#[test]
fn raw_command_and_outbox_ids_do_not_collide_across_typed_input_namespaces() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut input = task_input();
    let outbox_id = input.decision_context.allocated_ids["outbox"].clone();
    let HandleInput::Command {
        command_id,
        kind,
        body,
        ..
    } = &mut input.input
    else {
        unreachable!();
    };
    *command_id = outbox_id;
    *kind = "context".into();
    *body = json!({"text": "This notice must remain resolvable"});
    input.decision_context.allocated_ids.remove("assignment");

    let notice = kernel.handle(input).unwrap();
    let resolved = kernel
        .handle(succeeded_effect(
            &notice.created_ids["outbox"],
            "worker",
            &generation("generation-worker"),
        ))
        .unwrap();

    assert_eq!(notice.disposition, HandleDisposition::Applied);
    assert_eq!(resolved.disposition, HandleDisposition::Applied);
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["revision"], 1);
    assert_eq!(status.data["message_count"], 1);
    assert_eq!(status.data["effect_count"], 0);
}

#[test]
fn resolved_outbox_identity_cannot_be_allocated_to_a_later_effect() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut first = task_input();
    let HandleInput::Command { kind, body, .. } = &mut first.input else {
        unreachable!();
    };
    *kind = "context".into();
    *body = json!({"text": "First context"});
    first.decision_context.allocated_ids.remove("assignment");
    let notice = kernel.handle(first).unwrap();
    kernel
        .handle(succeeded_effect(
            &notice.created_ids["outbox"],
            "worker",
            &generation("generation-worker"),
        ))
        .unwrap();
    let before = kernel.inspect(InspectQuery::Status).unwrap();

    let mut second = task_input();
    let HandleInput::Command {
        command_id,
        kind,
        body,
        ..
    } = &mut second.input
    else {
        unreachable!();
    };
    *command_id = "cmd-context-reuses-resolved-outbox".into();
    *kind = "context".into();
    *body = json!({"text": "Second context"});
    second.decision_context.allocated_ids.remove("assignment");
    second
        .decision_context
        .allocated_ids
        .insert("message".into(), "msg-context-after-resolve".into());
    second
        .decision_context
        .allocated_ids
        .insert("outbox".into(), notice.created_ids["outbox"].clone());
    second
        .decision_context
        .generations
        .insert("worker".into(), generation("generation-worker-other"));

    let error = kernel.handle(second).unwrap_err();

    assert_eq!(error.code, "allocated_id_conflict");
    let after = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(after.data["revision"], before.data["revision"]);
    assert_eq!(after.data["message_count"], 1);
    assert_eq!(after.data["effect_count"], 0);
}

#[test]
fn worker_completed_creates_one_reviewer_follow_up_in_the_same_receipt() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    let activated = activate(&mut kernel, &assigned, "worker");
    assert_eq!(activated.assignment_state, Some(AssignmentState::Active));
    let assignment_id = assigned.created_ids["assignment"].clone();
    let receipt = kernel
        .handle(worker_completion_input(&assignment_id))
        .unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Applied);
    assert_eq!(receipt.revision, Some(2));
    assert_eq!(
        receipt.created_ids["follow_up_assignment"],
        "asg-1786681920000-000003"
    );
    assert_eq!(receipt.effect_intents.len(), 2);
    assert_eq!(
        receipt.effect_intents[1].effect_id,
        "out-1786681920000-000005"
    );
    let herdr_mission::EffectIntentKind::DeliverPrompt {
        role,
        assignment_id,
        ..
    } = &receipt.effect_intents[1].intent
    else {
        panic!("follow-up must deliver a Reviewer assignment");
    };
    assert_eq!(role.role, RoleKind::Reviewer);
    assert_eq!(assignment_id.as_deref(), Some("asg-1786681920000-000003"));

    let worker_thread = kernel
        .inspect(InspectQuery::AssignmentThread {
            assignment_id: assigned.created_ids["assignment"].clone(),
        })
        .unwrap();
    let worker_messages = worker_thread.data["messages"].as_array().unwrap();
    assert_eq!(worker_messages.len(), 2);
    assert_eq!(worker_messages[0]["kind"], "task");
    assert_eq!(worker_messages[1]["kind"], "completed");

    let reviewer_inbox = kernel
        .inspect(InspectQuery::Inbox {
            role: Some(RoleRef {
                role: RoleKind::Reviewer,
                instance: None,
            }),
        })
        .unwrap();
    assert_eq!(reviewer_inbox.data["messages"].as_array().unwrap().len(), 1);
    assert_eq!(reviewer_inbox.data["messages"][0]["kind"], "review");
    assert_eq!(
        reviewer_inbox.data["messages"][0]["assignment_id"],
        "asg-1786681920000-000003"
    );

    let diagnostics = kernel.inspect(InspectQuery::Diagnostics).unwrap();
    assert_eq!(diagnostics.data["ledger_entries"], 1);
}

#[test]
fn worker_completion_cannot_emit_reviewer_follow_up_while_reviewer_capacity_is_held() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let review = kernel
        .handle(pm_assignment_input(
            "cmd-review-capacity-held",
            "review",
            RoleRef {
                role: RoleKind::Reviewer,
                instance: None,
            },
            "1786681900000-review-held",
            "reviewer",
            "generation-reviewer",
        ))
        .unwrap();
    activate(&mut kernel, &review, "reviewer");

    let worker = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &worker, "worker");
    let before = kernel.inspect(InspectQuery::Status).unwrap();

    let completion = kernel
        .handle(worker_completion_input(&worker.created_ids["assignment"]))
        .unwrap();

    assert_eq!(completion.disposition, HandleDisposition::Rejected);
    assert_eq!(completion.error.unwrap().code, "role_capacity_exhausted");
    assert!(completion.effect_intents.is_empty());
    let after = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(after.data["revision"], before.data["revision"]);
    assert_eq!(after.data["assignment_count"], 2);
    let worker_thread = kernel
        .inspect(InspectQuery::AssignmentThread {
            assignment_id: worker.created_ids["assignment"].clone(),
        })
        .unwrap();
    assert_eq!(worker_thread.data["assignment"]["state"], "active");
}

#[test]
fn worker_completion_queues_reviewer_follow_up_behind_a_queued_review() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    // A queued (not yet delivered) review holds no active capacity, so a later
    // worker completion must be accepted and queue a second review behind it.
    let review = kernel
        .handle(pm_assignment_input(
            "cmd-review-queued",
            "review",
            RoleRef {
                role: RoleKind::Reviewer,
                instance: None,
            },
            "1786681900000-review-queued",
            "reviewer",
            "generation-reviewer",
        ))
        .unwrap();
    assert_eq!(review.disposition, HandleDisposition::Applied);

    let worker = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &worker, "worker");

    let completion = kernel
        .handle(worker_completion_input(&worker.created_ids["assignment"]))
        .unwrap();

    assert_eq!(completion.disposition, HandleDisposition::Applied);
    assert_eq!(
        completion.created_ids["follow_up_assignment"],
        "asg-1786681920000-000003"
    );
    let follow_up = kernel
        .inspect(InspectQuery::AssignmentThread {
            assignment_id: "asg-1786681920000-000003".into(),
        })
        .unwrap();
    assert_eq!(follow_up.data["assignment"]["state"], "queued");
}

#[test]
fn succeeded_reply_delivery_to_pm_does_not_reactivate_the_settled_assignment() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &assigned, "worker");
    let completed = kernel
        .handle(worker_completion_input(&assigned.created_ids["assignment"]))
        .unwrap();
    let reply_effect = completed
        .effect_intents
        .iter()
        .find(|intent| intent.effect_id == completed.created_ids["outbox"])
        .expect("completion must deliver its reply to PM");

    let resolved = kernel
        .handle(succeeded_effect(
            &reply_effect.effect_id,
            "pm",
            &reply_effect.generation,
        ))
        .unwrap();

    assert_eq!(resolved.disposition, HandleDisposition::Applied);
    assert_eq!(resolved.assignment_state, None);
}

#[test]
fn reply_ids_cannot_overwrite_existing_coordination_objects() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &assigned, "worker");
    let mut completion = worker_completion_input(&assigned.created_ids["assignment"]);
    completion
        .decision_context
        .allocated_ids
        .insert("message".into(), assigned.created_ids["message"].clone());
    completion
        .decision_context
        .allocated_ids
        .insert("outbox".into(), assigned.created_ids["outbox"].clone());

    let error = kernel.handle(completion).unwrap_err();

    assert_eq!(error.code, "allocated_id_conflict");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["assignment_count"], 1);
    assert_eq!(status.data["message_count"], 1);
    assert_eq!(status.data["effect_count"], 1);
}

#[test]
fn follow_up_ids_cannot_reuse_ids_allocated_earlier_in_the_same_command() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &assigned, "worker");
    let mut completion = worker_completion_input(&assigned.created_ids["assignment"]);
    let reply_message_id = completion.decision_context.allocated_ids["message"].clone();
    completion
        .decision_context
        .allocated_ids
        .insert("follow_up_message".into(), reply_message_id);

    let error = kernel.handle(completion).unwrap_err();

    assert_eq!(error.code, "allocated_id_conflict");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["assignment_count"], 1);
    assert_eq!(status.data["message_count"], 1);
    assert_eq!(status.data["effect_count"], 1);
}

#[test]
fn reviewer_rejected_creates_a_worker_fix_linking_back_to_the_original_work() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    let activated = activate(&mut kernel, &assigned, "worker");
    assert_eq!(activated.assignment_state, Some(AssignmentState::Active));
    let root_assignment = assigned.created_ids["assignment"].clone();
    let completed = kernel
        .handle(worker_completion_input(&root_assignment))
        .unwrap();
    let review_assignment = completed.created_ids["follow_up_assignment"].clone();
    let review_effect = completed
        .effect_intents
        .get(1)
        .expect("Worker completion must emit Reviewer follow-up delivery");
    let review_activation = kernel
        .handle(succeeded_effect(
            &review_effect.effect_id,
            "reviewer",
            &review_effect.generation,
        ))
        .unwrap();
    assert_eq!(
        review_activation.assignment_state,
        Some(AssignmentState::Active)
    );
    let rejected = KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T04:33:00Z".into(),
            allocated_ids: BTreeMap::from([
                ("message".into(), "msg-1786681980000-100001".into()),
                ("outbox".into(), "out-1786681980000-100002".into()),
                ("review_revision".into(), "rev-1786681980000-100006".into()),
                (
                    "review_pm_notice_message".into(),
                    "msg-1786681980000-100007".into(),
                ),
                (
                    "review_pm_notice_outbox".into(),
                    "out-1786681980000-100008".into(),
                ),
                (
                    "review_worker_notice_message".into(),
                    "msg-1786681980000-100009".into(),
                ),
                (
                    "review_worker_notice_outbox".into(),
                    "out-1786681980000-100010".into(),
                ),
                (
                    "follow_up_assignment".into(),
                    "asg-1786681980000-100003".into(),
                ),
                (
                    "follow_up_message".into(),
                    "msg-1786681980000-100004".into(),
                ),
                ("follow_up_outbox".into(), "out-1786681980000-100005".into()),
            ]),
            generations: BTreeMap::from([
                ("pm".into(), generation("generation-pm")),
                ("worker".into(), generation("generation-worker")),
            ]),
        },
        input: HandleInput::Command {
            command_id: "cmd-abcdef0123456789".into(),
            kind: "rejected".into(),
            source: RoleRef {
                role: RoleKind::Reviewer,
                instance: None,
            },
            target: Some(RoleRef {
                role: RoleKind::Pm,
                instance: None,
            }),
            body: json!({
                "assignment_id": review_assignment,
                "text": "The implementation needs one focused correction"
            }),
        },
    };

    let receipt = kernel.handle(rejected).unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Applied);
    assert_eq!(
        receipt.created_ids["review_revision"],
        "rev-1786681980000-100006"
    );
    assert_eq!(
        receipt.created_ids["follow_up_assignment"],
        "asg-1786681980000-100003"
    );
    assert_eq!(receipt.relationships["parent_assignment"], root_assignment);
    assert_eq!(receipt.review_round, Some(1));
    let herdr_mission::EffectIntentKind::DeliverPrompt { role, .. } =
        &receipt.effect_intents[1].intent
    else {
        panic!("rejection follow-up must deliver a Worker fix");
    };
    assert_eq!(role.role, RoleKind::Worker);
}

#[test]
fn reviewer_rejection_cannot_emit_worker_fix_while_worker_capacity_is_held() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let original_work = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &original_work, "worker");
    let completed = kernel
        .handle(worker_completion_input(
            &original_work.created_ids["assignment"],
        ))
        .unwrap();
    let review_assignment = completed.created_ids["follow_up_assignment"].clone();
    let review_effect = completed
        .effect_intents
        .get(1)
        .expect("Worker completion must emit Reviewer follow-up delivery");
    kernel
        .handle(succeeded_effect(
            &review_effect.effect_id,
            "reviewer",
            &review_effect.generation,
        ))
        .unwrap();

    let competing_work = kernel
        .handle(pm_assignment_input(
            "cmd-worker-capacity-held",
            "task",
            RoleRef {
                role: RoleKind::Worker,
                instance: None,
            },
            "1786681970000-worker-held",
            "worker",
            "generation-worker",
        ))
        .unwrap();
    activate(&mut kernel, &competing_work, "worker");
    let before = kernel.inspect(InspectQuery::Status).unwrap();

    let rejection = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T04:33:00Z".into(),
                allocated_ids: BTreeMap::from([
                    ("message".into(), "msg-1786681980000-200001".into()),
                    ("outbox".into(), "out-1786681980000-200002".into()),
                    ("review_revision".into(), "rev-1786681980000-200006".into()),
                    (
                        "review_pm_notice_message".into(),
                        "msg-1786681980000-200007".into(),
                    ),
                    (
                        "review_pm_notice_outbox".into(),
                        "out-1786681980000-200008".into(),
                    ),
                    (
                        "review_worker_notice_message".into(),
                        "msg-1786681980000-200009".into(),
                    ),
                    (
                        "review_worker_notice_outbox".into(),
                        "out-1786681980000-200010".into(),
                    ),
                    (
                        "follow_up_assignment".into(),
                        "asg-1786681980000-200003".into(),
                    ),
                    (
                        "follow_up_message".into(),
                        "msg-1786681980000-200004".into(),
                    ),
                    ("follow_up_outbox".into(), "out-1786681980000-200005".into()),
                ]),
                generations: BTreeMap::from([
                    ("pm".into(), generation("generation-pm")),
                    ("worker".into(), generation("generation-worker")),
                ]),
            },
            input: HandleInput::Command {
                command_id: "cmd-reviewer-capacity-held".into(),
                kind: "rejected".into(),
                source: RoleRef {
                    role: RoleKind::Reviewer,
                    instance: None,
                },
                target: Some(RoleRef {
                    role: RoleKind::Pm,
                    instance: None,
                }),
                body: json!({
                    "assignment_id": review_assignment,
                    "text": "The implementation needs one focused correction"
                }),
            },
        })
        .unwrap();

    assert_eq!(rejection.disposition, HandleDisposition::Rejected);
    assert_eq!(rejection.error.unwrap().code, "role_capacity_exhausted");
    assert!(rejection.effect_intents.is_empty());
    let after = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(after.data["revision"], before.data["revision"]);
    assert_eq!(after.data["assignment_count"], 3);
    let review_thread = kernel
        .inspect(InspectQuery::AssignmentThread {
            assignment_id: completed.created_ids["follow_up_assignment"].clone(),
        })
        .unwrap();
    assert_eq!(review_thread.data["assignment"]["state"], "active");
}

#[test]
fn reviewer_rejection_stops_creating_fixes_after_the_configured_review_limit() {
    let mut kernel =
        MissionKernel::in_memory_with_review_limit("msn-20260814-123000-rust-kernel-abcd", 0);
    let assigned = kernel.handle(task_input()).unwrap();
    let activated = activate(&mut kernel, &assigned, "worker");
    assert_eq!(activated.assignment_state, Some(AssignmentState::Active));
    let completed = kernel
        .handle(worker_completion_input(&assigned.created_ids["assignment"]))
        .unwrap();
    let review_assignment = completed.created_ids["follow_up_assignment"].clone();
    let review_effect = completed
        .effect_intents
        .get(1)
        .expect("Worker completion must emit Reviewer follow-up delivery");
    let review_activation = kernel
        .handle(succeeded_effect(
            &review_effect.effect_id,
            "reviewer",
            &review_effect.generation,
        ))
        .unwrap();
    assert_eq!(
        review_activation.assignment_state,
        Some(AssignmentState::Active)
    );
    let receipt = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T04:34:00Z".into(),
                allocated_ids: BTreeMap::from([
                    ("message".into(), "msg-1786682040000-110001".into()),
                    ("outbox".into(), "out-1786682040000-110002".into()),
                    ("review_revision".into(), "rev-1786682040000-110003".into()),
                    (
                        "review_pm_notice_message".into(),
                        "msg-1786682040000-110004".into(),
                    ),
                    (
                        "review_pm_notice_outbox".into(),
                        "out-1786682040000-110005".into(),
                    ),
                    (
                        "review_worker_notice_message".into(),
                        "msg-1786682040000-110006".into(),
                    ),
                    (
                        "review_worker_notice_outbox".into(),
                        "out-1786682040000-110007".into(),
                    ),
                ]),
                generations: BTreeMap::from([
                    ("pm".into(), generation("generation-pm")),
                    ("worker".into(), generation("generation-worker")),
                ]),
            },
            input: HandleInput::Command {
                command_id: "cmd-review-limit-0001".into(),
                kind: "rejected".into(),
                source: RoleRef {
                    role: RoleKind::Reviewer,
                    instance: None,
                },
                target: Some(RoleRef {
                    role: RoleKind::Pm,
                    instance: None,
                }),
                body: json!({
                    "assignment_id": review_assignment,
                    "text": "Do not create another fix after the configured limit"
                }),
            },
        })
        .unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Applied);
    assert_eq!(receipt.assignment_state, Some(AssignmentState::Rejected));
    assert!(receipt.review_limit_reached);
    assert!(!receipt.created_ids.contains_key("follow_up_assignment"));
    assert_eq!(receipt.effect_intents.len(), 3);
}

#[test]
fn succeeded_delivery_activates_the_existing_assignment_identity() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    let assignment_id = assigned.created_ids["assignment"].clone();
    let intent = &assigned.effect_intents[0];

    let receipt = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T04:31:00Z".into(),
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

    assert_eq!(receipt.disposition, HandleDisposition::Applied);
    assert_eq!(receipt.created_ids["assignment"], assignment_id);
    assert_eq!(receipt.assignment_state, Some(AssignmentState::Active));
    assert!(receipt.effect_intents.is_empty());
}

#[test]
fn a_second_worker_assignment_cannot_activate_while_worker_capacity_is_held() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let first = kernel.handle(task_input()).unwrap();
    let first_intent = first.effect_intents[0].clone();
    kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T04:31:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([("worker".into(), first_intent.generation.clone())]),
            },
            input: HandleInput::EffectResult {
                result: EffectResult {
                    effect_id: first_intent.effect_id,
                    generation: first_intent.generation.clone(),
                    claim_owner: "memory-driver".into(),
                    outcome: EffectOutcome::Succeeded {
                        observation: json!({"delivered": true}),
                    },
                },
            },
        })
        .unwrap();

    let mut second_input = task_input();
    second_input.decision_context.observed_at = "2026-08-14T04:32:00Z".into();
    second_input.decision_context.allocated_ids = BTreeMap::from([
        ("assignment".into(), "asg-1786681920000-200001".into()),
        ("message".into(), "msg-1786681920000-200002".into()),
        ("outbox".into(), "out-1786681920000-200003".into()),
    ]);
    let HandleInput::Command { command_id, .. } = &mut second_input.input else {
        unreachable!();
    };
    *command_id = "cmd-2222222222222222".into();
    let second = kernel.handle(second_input).unwrap();

    assert_eq!(second.disposition, HandleDisposition::Rejected);
    assert_eq!(second.error.unwrap().code, "role_capacity_exhausted");
    assert!(second.effect_intents.is_empty());
}

#[test]
fn blocked_is_terminal_and_releases_worker_capacity_without_creating_follow_up_work() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let first = kernel.handle(task_input()).unwrap();
    let first_assignment = first.created_ids["assignment"].clone();
    let first_intent = first.effect_intents[0].clone();
    kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T04:31:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([("worker".into(), first_intent.generation.clone())]),
            },
            input: HandleInput::EffectResult {
                result: EffectResult {
                    effect_id: first_intent.effect_id,
                    generation: first_intent.generation.clone(),
                    claim_owner: "memory-driver".into(),
                    outcome: EffectOutcome::Succeeded {
                        observation: json!({"delivered": true}),
                    },
                },
            },
        })
        .unwrap();

    let mut blocked_input = worker_completion_input(&first_assignment);
    let HandleInput::Command { kind, body, .. } = &mut blocked_input.input else {
        unreachable!();
    };
    *kind = "blocked".into();
    *body = json!({
        "assignment_id": first_assignment,
        "text": "Waiting for an external prerequisite"
    });
    let blocked = kernel.handle(blocked_input).unwrap();

    assert_eq!(blocked.assignment_state, Some(AssignmentState::Blocked));
    assert!(!blocked.created_ids.contains_key("follow_up_assignment"));

    let mut second_input = task_input();
    second_input.decision_context.observed_at = "2026-08-14T04:34:00Z".into();
    second_input.decision_context.allocated_ids = BTreeMap::from([
        ("assignment".into(), "asg-1786682040000-300001".into()),
        ("message".into(), "msg-1786682040000-300002".into()),
        ("outbox".into(), "out-1786682040000-300003".into()),
    ]);
    let HandleInput::Command { command_id, .. } = &mut second_input.input else {
        unreachable!();
    };
    *command_id = "cmd-3333333333333333".into();
    let second = kernel.handle(second_input).unwrap();
    let second_intent = &second.effect_intents[0];
    let activated = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T04:35:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([("worker".into(), second_intent.generation.clone())]),
            },
            input: HandleInput::EffectResult {
                result: EffectResult {
                    effect_id: second_intent.effect_id.clone(),
                    generation: second_intent.generation.clone(),
                    claim_owner: "memory-driver".into(),
                    outcome: EffectOutcome::Succeeded {
                        observation: json!({"delivered": true}),
                    },
                },
            },
        })
        .unwrap();

    assert_eq!(activated.disposition, HandleDisposition::Applied);
    assert_eq!(activated.assignment_state, Some(AssignmentState::Active));
}

#[test]
fn independent_dynamic_scout_instances_can_activate_in_parallel() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let first = kernel
        .handle(pm_assignment_input(
            "cmd-scout-00000001",
            "task",
            RoleRef {
                role: RoleKind::Scout,
                instance: Some("scout-01".into()),
            },
            "1786683600000-400001",
            "scout-01",
            "generation-scout-01",
        ))
        .unwrap();
    let second = kernel
        .handle(pm_assignment_input(
            "cmd-scout-00000002",
            "task",
            RoleRef {
                role: RoleKind::Scout,
                instance: Some("scout-02".into()),
            },
            "1786683600000-400002",
            "scout-02",
            "generation-scout-02",
        ))
        .unwrap();

    let first_active = kernel
        .handle(succeeded_effect(
            &first.effect_intents[0].effect_id,
            "scout-01",
            &generation("generation-scout-01"),
        ))
        .unwrap();
    let second_active = kernel
        .handle(succeeded_effect(
            &second.effect_intents[0].effect_id,
            "scout-02",
            &generation("generation-scout-02"),
        ))
        .unwrap();

    assert_eq!(first_active.assignment_state, Some(AssignmentState::Active));
    assert_eq!(
        second_active.assignment_state,
        Some(AssignmentState::Active)
    );
}

#[test]
fn reviewer_review_assignments_are_singleton_serial() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let first = kernel
        .handle(pm_assignment_input(
            "cmd-review-0000001",
            "review",
            RoleRef {
                role: RoleKind::Reviewer,
                instance: None,
            },
            "1786683600000-500001",
            "reviewer",
            "generation-reviewer",
        ))
        .unwrap();
    let second = kernel
        .handle(pm_assignment_input(
            "cmd-review-0000002",
            "review",
            RoleRef {
                role: RoleKind::Reviewer,
                instance: None,
            },
            "1786683600000-500002",
            "reviewer",
            "generation-reviewer",
        ))
        .unwrap();

    assert_eq!(second.disposition, HandleDisposition::Rejected);
    assert_eq!(
        second.error.as_ref().unwrap().code,
        "role_capacity_exhausted"
    );
    assert!(second.effect_intents.is_empty());

    assert_eq!(
        kernel
            .handle(succeeded_effect(
                &first.effect_intents[0].effect_id,
                "reviewer",
                &generation("generation-reviewer"),
            ))
            .unwrap()
            .assignment_state,
        Some(AssignmentState::Active)
    );
}

#[test]
fn settled_recovery_rejects_an_untrusted_opaque_generation_without_mutating_state() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    let assignment_id = assigned.created_ids["assignment"].clone();
    let original_effect = assigned.effect_intents[0].clone();
    kernel
        .handle(succeeded_effect(
            &original_effect.effect_id,
            "worker",
            &original_effect.generation,
        ))
        .unwrap();
    let observation = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T05:09:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([(
                    "worker".into(),
                    generation("generation-worker-recovery"),
                )]),
            },
            input: HandleInput::RoleObservation {
                observation_id: "obs-worker-recovery-generation-8".into(),
                role: RoleRef {
                    role: RoleKind::Worker,
                    instance: None,
                },
                generation: generation("generation-worker-recovery"),
                launch_owner: None,
                state: RoleState::Ready,
                details: json!({}),
            },
        })
        .unwrap();
    assert_eq!(observation.disposition, HandleDisposition::Rejected);
    assert_eq!(observation.error.unwrap().code, "stale_generation");

    let before = kernel.inspect(InspectQuery::Status).unwrap();

    let recovery_event = KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T05:10:00Z".into(),
            allocated_ids: BTreeMap::new(),
            generations: BTreeMap::from([(
                "worker".into(),
                generation("generation-worker-recovery"),
            )]),
        },
        input: HandleInput::TeamEvent {
            event_id: "evt-settled-untrusted-generation".into(),
            sequence: 23,
            name: "assignment_settled".into(),
            body: json!({
                "role": "worker",
                "expected_assignment_id": assignment_id,
                "safe_to_resume": true
            }),
        },
    };

    let rejected = kernel.handle(recovery_event).unwrap();

    assert_eq!(rejected.disposition, HandleDisposition::Rejected);
    assert_eq!(rejected.error.unwrap().code, "stale_generation");
    assert!(rejected.effect_intents.is_empty());
    let after = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(after.data["revision"], before.data["revision"]);
    let thread = kernel
        .inspect(InspectQuery::AssignmentThread { assignment_id })
        .unwrap();
    assert_eq!(thread.data["assignment"]["state"], "active");
}

#[test]
fn safe_settled_recovery_requires_an_explicit_generation_without_mutating_state() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &assigned, "worker");
    let before = kernel.inspect(InspectQuery::Status).unwrap();

    let error = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T05:10:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::new(),
            },
            input: HandleInput::TeamEvent {
                event_id: "evt-settled-missing-generation".into(),
                sequence: 24,
                name: "assignment_settled".into(),
                body: json!({
                    "role": "worker",
                    "expected_assignment_id": assigned.created_ids["assignment"],
                    "safe_to_resume": true
                }),
            },
        })
        .unwrap_err();

    assert_eq!(error.code, "missing_decision_context");
    let after = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(after.data["revision"], before.data["revision"]);
    let thread = kernel
        .inspect(InspectQuery::AssignmentThread {
            assignment_id: assigned.created_ids["assignment"].clone(),
        })
        .unwrap();
    assert_eq!(thread.data["assignment"]["state"], "active");
}

#[test]
fn safe_settled_recovery_rejects_the_existing_effect_generation_without_mutating_state() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &assigned, "worker");
    let before = kernel.inspect(InspectQuery::Status).unwrap();

    let receipt = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T05:10:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([("worker".into(), generation("generation-worker"))]),
            },
            input: HandleInput::TeamEvent {
                event_id: "evt-settled-stale-generation".into(),
                sequence: 25,
                name: "assignment_settled".into(),
                body: json!({
                    "role": "worker",
                    "expected_assignment_id": assigned.created_ids["assignment"],
                    "safe_to_resume": true
                }),
            },
        })
        .unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Rejected);
    assert_eq!(receipt.error.unwrap().code, "stale_generation");
    assert!(receipt.effect_intents.is_empty());
    let after = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(after.data["revision"], before.data["revision"]);
    let thread = kernel
        .inspect(InspectQuery::AssignmentThread {
            assignment_id: assigned.created_ids["assignment"].clone(),
        })
        .unwrap();
    assert_eq!(thread.data["assignment"]["state"], "active");
}

#[test]
fn unsafe_settled_recovery_blocks_the_existing_assignment_without_redelivery() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &assigned, "worker");
    let before = kernel.inspect(InspectQuery::Status).unwrap();

    let receipt = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T05:10:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::new(),
            },
            input: HandleInput::TeamEvent {
                event_id: "evt-settled-blocked".into(),
                sequence: 26,
                name: "assignment_settled".into(),
                body: json!({
                    "role": "worker",
                    "expected_assignment_id": assigned.created_ids["assignment"],
                    "safe_to_resume": false
                }),
            },
        })
        .unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Applied);
    assert_eq!(receipt.assignment_state, Some(AssignmentState::Blocked));
    assert!(receipt.effect_intents.is_empty());
    let after = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(
        after.data["revision"],
        before.data["revision"].as_u64().unwrap() + 1
    );
    assert_eq!(after.data["effect_count"], before.data["effect_count"]);
    let thread = kernel
        .inspect(InspectQuery::AssignmentThread {
            assignment_id: assigned.created_ids["assignment"].clone(),
        })
        .unwrap();
    assert_eq!(thread.data["assignment"]["state"], "blocked");
}

#[test]
fn duplicate_reply_identity_does_not_create_a_second_message_or_follow_up() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel
        .handle(pm_assignment_input(
            "cmd-scout-reply-01",
            "task",
            RoleRef {
                role: RoleKind::Scout,
                instance: Some("scout-07".into()),
            },
            "1786684200000-600001",
            "scout-07",
            "generation-scout-07",
        ))
        .unwrap();
    let activated = activate(&mut kernel, &assigned, "scout-07");
    assert_eq!(activated.assignment_state, Some(AssignmentState::Active));
    let assignment_id = assigned.created_ids["assignment"].clone();
    let reply = |command_id: &str, message_id: &str, outbox_id: &str| KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T05:11:00Z".into(),
            allocated_ids: BTreeMap::from([
                ("message".into(), message_id.into()),
                ("outbox".into(), outbox_id.into()),
            ]),
            generations: BTreeMap::from([("pm".into(), generation("generation-pm"))]),
        },
        input: HandleInput::Command {
            command_id: command_id.into(),
            kind: "finding".into(),
            source: RoleRef {
                role: RoleKind::Scout,
                instance: Some("scout-07".into()),
            },
            target: Some(RoleRef {
                role: RoleKind::Pm,
                instance: None,
            }),
            body: json!({
                "assignment_id": assignment_id,
                "reply_id": "reply-scout-07-finding-1",
                "text": "The schema evidence is frozen"
            }),
        },
    };

    let first = kernel
        .handle(reply(
            "cmd-scout-reply-02",
            "msg-1786684260000-600002",
            "out-1786684260000-600003",
        ))
        .unwrap();
    let replay = kernel
        .handle(reply(
            "cmd-scout-reply-03",
            "msg-1786684320000-600004",
            "out-1786684320000-600005",
        ))
        .unwrap();

    assert_eq!(first.disposition, HandleDisposition::Applied);
    assert_eq!(replay.disposition, HandleDisposition::Duplicate);
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.created_ids, first.created_ids);
    assert_eq!(replay.effect_intents, first.effect_intents);
}

#[test]
fn duplicate_context_revision_does_not_create_a_second_notice() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let notice = |command_id: &str, message_id: &str, outbox_id: &str| KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T05:12:00Z".into(),
            allocated_ids: BTreeMap::from([
                ("message".into(), message_id.into()),
                ("outbox".into(), outbox_id.into()),
            ]),
            generations: BTreeMap::from([("worker".into(), generation("generation-worker"))]),
        },
        input: HandleInput::Command {
            command_id: command_id.into(),
            kind: "context".into(),
            source: RoleRef {
                role: RoleKind::Pm,
                instance: None,
            },
            target: Some(RoleRef {
                role: RoleKind::Worker,
                instance: None,
            }),
            body: json!({
                "context_revision": 12,
                "text": "Use the frozen decision context"
            }),
        },
    };

    let first = kernel
        .handle(notice(
            "cmd-context-000001",
            "msg-1786684380000-700001",
            "out-1786684380000-700002",
        ))
        .unwrap();
    let replay = kernel
        .handle(notice(
            "cmd-context-000002",
            "msg-1786684440000-700003",
            "out-1786684440000-700004",
        ))
        .unwrap();

    assert_eq!(first.disposition, HandleDisposition::Applied);
    assert_eq!(replay.disposition, HandleDisposition::Duplicate);
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.created_ids, first.created_ids);
    assert_eq!(replay.effect_intents, first.effect_intents);
}

#[test]
fn duplicate_effect_id_replays_the_original_activation_receipt() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    let intent = assigned.effect_intents[0].clone();
    let result = succeeded_effect(&intent.effect_id, "worker", &intent.generation);

    let first = kernel.handle(result.clone()).unwrap();
    let replay = kernel.handle(result).unwrap();

    assert_eq!(first.disposition, HandleDisposition::Applied);
    assert_eq!(replay.disposition, HandleDisposition::Duplicate);
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.created_ids, first.created_ids);
    assert_eq!(replay.assignment_state, Some(AssignmentState::Active));
}

#[test]
fn same_effect_result_is_duplicate_when_only_observed_at_changes() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    let intent = assigned.effect_intents[0].clone();
    let first = kernel
        .handle(succeeded_effect(
            &intent.effect_id,
            "worker",
            &intent.generation,
        ))
        .unwrap();
    let mut retry = succeeded_effect(&intent.effect_id, "worker", &intent.generation);
    retry.decision_context.observed_at = "2026-08-14T05:01:01Z".into();

    let duplicate = kernel.handle(retry).unwrap();

    assert_eq!(duplicate.disposition, HandleDisposition::Duplicate);
    assert_eq!(duplicate.revision, first.revision);
    assert_eq!(duplicate.created_ids, first.created_ids);
}

#[test]
fn role_observation_with_a_different_opaque_generation_is_rejected() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let old = kernel
        .handle(pm_assignment_input(
            "cmd-generation-0001",
            "task",
            RoleRef {
                role: RoleKind::Worker,
                instance: None,
            },
            "1786684500000-800001",
            "worker",
            "generation-worker-old",
        ))
        .unwrap();
    let observed = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T05:19:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([(
                    "worker".into(),
                    generation("generation-worker-current"),
                )]),
            },
            input: HandleInput::RoleObservation {
                observation_id: "obs-worker-generation-2".into(),
                role: RoleRef {
                    role: RoleKind::Worker,
                    instance: None,
                },
                generation: generation("generation-worker-current"),
                launch_owner: None,
                state: RoleState::Ready,
                details: json!({}),
            },
        })
        .unwrap();
    assert_eq!(observed.disposition, HandleDisposition::Rejected);
    assert_eq!(observed.error.unwrap().code, "stale_generation");

    let activated = kernel
        .handle(succeeded_effect(
            &old.effect_intents[0].effect_id,
            "worker",
            &generation("generation-worker-old"),
        ))
        .unwrap();
    assert_eq!(activated.disposition, HandleDisposition::Applied);
    assert_eq!(activated.assignment_state, Some(AssignmentState::Active));
}

#[test]
fn stale_role_observation_cannot_overwrite_a_newer_generation() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let observation =
        |observation_id: &str, generation_token: &str, state: RoleState| -> KernelInput {
            let role_generation = generation(generation_token);
            KernelInput {
                decision_context: DecisionContext {
                    observed_at: "2026-08-14T05:20:00Z".into(),
                    allocated_ids: BTreeMap::new(),
                    generations: BTreeMap::from([("worker".into(), role_generation.clone())]),
                },
                input: HandleInput::RoleObservation {
                    observation_id: observation_id.into(),
                    role: RoleRef {
                        role: RoleKind::Worker,
                        instance: None,
                    },
                    generation: role_generation,
                    launch_owner: None,
                    state,
                    details: json!({}),
                },
            }
        };

    let first = kernel
        .handle(observation(
            "obs-worker-0001",
            "generation-worker-current",
            RoleState::Ready,
        ))
        .unwrap();
    let current = kernel
        .handle(observation(
            "obs-worker-0002",
            "generation-worker-other",
            RoleState::Starting,
        ))
        .unwrap();
    let stale = kernel
        .handle(observation(
            "obs-worker-0003",
            "generation-worker-current",
            RoleState::Stopped,
        ))
        .unwrap();

    assert_eq!(first.disposition, HandleDisposition::Applied);
    assert_eq!(current.disposition, HandleDisposition::Rejected);
    assert_eq!(current.error.unwrap().code, "stale_generation");
    assert_eq!(stale.disposition, HandleDisposition::Applied);
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(
        status.data["roles"]["worker"]["generation"],
        "generation-worker-current"
    );
    assert_eq!(status.data["roles"]["worker"]["state"], "stopped");
}

#[test]
fn queued_assignment_cannot_be_replied_to_before_delivery_activates_it() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel.handle(task_input()).unwrap();
    let assignment_id = assigned.created_ids["assignment"].clone();

    let receipt = kernel
        .handle(worker_completion_input(&assignment_id))
        .unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Rejected);
    assert_eq!(receipt.error.unwrap().code, "assignment_not_active");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["assignment_count"], 1);
    assert_eq!(status.data["message_count"], 1);
}

#[test]
fn singleton_worker_capacity_rejects_a_second_dispatch_before_emitting_prompt_effect() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let first = kernel.handle(task_input()).unwrap();
    assert_eq!(first.disposition, HandleDisposition::Applied);

    let second = kernel
        .handle(pm_assignment_input(
            "cmd-worker-capacity-0002",
            "fix",
            RoleRef {
                role: RoleKind::Worker,
                instance: None,
            },
            "1786685100000-cap002",
            "worker",
            "generation-worker",
        ))
        .unwrap();

    assert_eq!(second.disposition, HandleDisposition::Rejected);
    assert_eq!(second.error.unwrap().code, "role_capacity_exhausted");
    assert!(second.effect_intents.is_empty());
    assert!(second.created_ids.is_empty());
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["assignment_count"], 1);
    assert_eq!(status.data["message_count"], 1);
    assert_eq!(status.data["effect_count"], 1);
}

#[test]
fn role_references_must_use_canonical_singleton_and_scout_identities() {
    let invalid_roles = [
        RoleRef {
            role: RoleKind::Worker,
            instance: Some("worker-a".into()),
        },
        RoleRef {
            role: RoleKind::Reviewer,
            instance: Some("reviewer-a".into()),
        },
        RoleRef {
            role: RoleKind::Scout,
            instance: Some("worker".into()),
        },
    ];

    for (index, target) in invalid_roles.into_iter().enumerate() {
        let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
        let input = pm_assignment_input(
            &format!("cmd-invalid-role-{index}"),
            if target.role == RoleKind::Reviewer {
                "review"
            } else {
                "task"
            },
            target,
            &format!("1786685200000-invalid{index}"),
            "worker",
            "generation-invalid-role",
        );

        let error = kernel.handle(input).unwrap_err();
        assert_eq!(error.code, "invalid_role_identity");
        let status = kernel.inspect(InspectQuery::Status).unwrap();
        assert_eq!(status.data["assignment_count"], 0);
        assert_eq!(status.data["message_count"], 0);
        assert_eq!(status.data["effect_count"], 0);
    }
}

#[test]
fn every_ingress_role_reference_must_use_a_canonical_identity() {
    let mut invalid_pm_source = task_input();
    let HandleInput::Command { source, .. } = &mut invalid_pm_source.input else {
        unreachable!();
    };
    source.instance = Some("pm-shadow".into());
    let mut kernel = MissionKernel::in_memory("msn-invalid-pm-source");
    assert_eq!(
        kernel.handle(invalid_pm_source).unwrap_err().code,
        "invalid_role_identity"
    );

    let mut kernel = MissionKernel::in_memory("msn-invalid-reply-target");
    let assigned = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &assigned, "worker");
    let mut invalid_reply_target = worker_completion_input(&assigned.created_ids["assignment"]);
    let HandleInput::Command { target, .. } = &mut invalid_reply_target.input else {
        unreachable!();
    };
    target.as_mut().unwrap().instance = Some("pm-shadow".into());
    assert_eq!(
        kernel.handle(invalid_reply_target).unwrap_err().code,
        "invalid_role_identity"
    );

    let mut kernel = MissionKernel::in_memory("msn-invalid-observation-role");
    let invalid_observation = KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T06:10:00Z".into(),
            allocated_ids: BTreeMap::new(),
            generations: BTreeMap::from([("worker".into(), generation("generation-worker"))]),
        },
        input: HandleInput::RoleObservation {
            observation_id: "obs-invalid-worker-instance".into(),
            role: RoleRef {
                role: RoleKind::Worker,
                instance: Some("worker-shadow".into()),
            },
            generation: generation("generation-worker"),
            launch_owner: None,
            state: RoleState::Ready,
            details: json!({}),
        },
    };
    assert_eq!(
        kernel.handle(invalid_observation).unwrap_err().code,
        "invalid_role_identity"
    );
}

#[test]
fn delayed_old_command_cannot_downgrade_role_generation() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let current = kernel
        .handle(pm_assignment_input(
            "cmd-generation-current",
            "task",
            RoleRef {
                role: RoleKind::Scout,
                instance: Some("scout-09".into()),
            },
            "1786685300000-current",
            "scout-09",
            "generation-current",
        ))
        .unwrap();
    assert_eq!(current.disposition, HandleDisposition::Applied);

    let delayed = kernel
        .handle(pm_assignment_input(
            "cmd-generation-delayed",
            "task",
            RoleRef {
                role: RoleKind::Scout,
                instance: Some("scout-09".into()),
            },
            "1786685300000-delayed",
            "scout-09",
            "generation-delayed",
        ))
        .unwrap();

    assert_eq!(delayed.disposition, HandleDisposition::Rejected);
    assert_eq!(delayed.error.unwrap().code, "stale_generation");
    assert!(delayed.effect_intents.is_empty());
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["assignment_count"], 1);
    assert_eq!(status.data["roles"]["scout-09"], json!(null));
}

#[test]
fn reused_command_id_with_different_semantics_is_rejected_as_a_conflict() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let first = kernel.handle(task_input()).unwrap();
    assert_eq!(first.disposition, HandleDisposition::Applied);

    let mut conflicting = task_input();
    let HandleInput::Command { body, .. } = &mut conflicting.input else {
        unreachable!();
    };
    *body = json!({"text": "A different operation under the same command ID"});

    let receipt = kernel.handle(conflicting).unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Rejected);
    assert_eq!(receipt.error.unwrap().code, "input_id_conflict");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["assignment_count"], 1);
    assert_eq!(status.data["message_count"], 1);
    assert_eq!(status.data["effect_count"], 1);
}

#[test]
fn reused_context_revision_with_different_text_is_rejected_as_a_conflict() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let notice = |command_id: &str, suffix: &str, text: &str| KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T06:00:00Z".into(),
            allocated_ids: BTreeMap::from([
                ("message".into(), format!("msg-{suffix}")),
                ("outbox".into(), format!("out-{suffix}")),
            ]),
            generations: BTreeMap::from([("worker".into(), generation("generation-worker"))]),
        },
        input: HandleInput::Command {
            command_id: command_id.into(),
            kind: "context".into(),
            source: RoleRef {
                role: RoleKind::Pm,
                instance: None,
            },
            target: Some(RoleRef {
                role: RoleKind::Worker,
                instance: None,
            }),
            body: json!({"context_revision": 42, "text": text}),
        },
    };

    let first = kernel
        .handle(notice(
            "cmd-context-conflict-1",
            "1786687200000-context1",
            "Use the original context",
        ))
        .unwrap();
    assert_eq!(first.disposition, HandleDisposition::Applied);

    let conflict = kernel
        .handle(notice(
            "cmd-context-conflict-2",
            "1786687200000-context2",
            "Replace it with different context",
        ))
        .unwrap();

    assert_eq!(conflict.disposition, HandleDisposition::Rejected);
    assert_eq!(conflict.error.unwrap().code, "context_revision_conflict");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["message_count"], 1);
    assert_eq!(status.data["effect_count"], 1);
}

#[test]
fn reused_reply_identity_with_different_semantics_is_rejected_as_a_conflict() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let assigned = kernel
        .handle(pm_assignment_input(
            "cmd-reply-conflict-dispatch",
            "task",
            RoleRef {
                role: RoleKind::Scout,
                instance: Some("scout-11".into()),
            },
            "1786687300000-dispatch",
            "scout-11",
            "generation-scout-11",
        ))
        .unwrap();
    activate(&mut kernel, &assigned, "scout-11");
    let assignment_id = assigned.created_ids["assignment"].clone();
    let reply = |command_id: &str, suffix: &str, text: &str| KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T06:01:00Z".into(),
            allocated_ids: BTreeMap::from([
                ("message".into(), format!("msg-{suffix}")),
                ("outbox".into(), format!("out-{suffix}")),
            ]),
            generations: BTreeMap::from([("pm".into(), generation("generation-pm"))]),
        },
        input: HandleInput::Command {
            command_id: command_id.into(),
            kind: "finding".into(),
            source: RoleRef {
                role: RoleKind::Scout,
                instance: Some("scout-11".into()),
            },
            target: Some(RoleRef {
                role: RoleKind::Pm,
                instance: None,
            }),
            body: json!({
                "assignment_id": assignment_id,
                "reply_id": "reply-scout-11-1",
                "text": text,
            }),
        },
    };

    let first = kernel
        .handle(reply(
            "cmd-reply-conflict-1",
            "1786687300000-reply1",
            "Original finding",
        ))
        .unwrap();
    assert_eq!(first.disposition, HandleDisposition::Applied);

    let conflict = kernel
        .handle(reply(
            "cmd-reply-conflict-2",
            "1786687300000-reply2",
            "Different finding",
        ))
        .unwrap();

    assert_eq!(conflict.disposition, HandleDisposition::Rejected);
    assert_eq!(conflict.error.unwrap().code, "reply_identity_conflict");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["message_count"], 2);
}

#[test]
fn context_is_only_a_pm_to_role_notice_and_cannot_bypass_reply_ownership() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut input = task_input();
    input.decision_context.allocated_ids.remove("assignment");
    input.decision_context.generations =
        BTreeMap::from([("pm".into(), generation("generation-pm"))]);
    let HandleInput::Command {
        command_id,
        kind,
        source,
        target,
        body,
    } = &mut input.input
    else {
        unreachable!();
    };
    *command_id = "cmd-role-context-to-pm".into();
    *kind = "context".into();
    *source = RoleRef {
        role: RoleKind::Worker,
        instance: None,
    };
    *target = Some(RoleRef {
        role: RoleKind::Pm,
        instance: None,
    });
    *body = json!({"context_revision": 1, "text": "Bypass ownership"});

    let receipt = kernel.handle(input).unwrap();

    assert_eq!(receipt.disposition, HandleDisposition::Rejected);
    assert_eq!(receipt.error.unwrap().code, "invalid_context_source");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["message_count"], 0);
    assert_eq!(status.data["effect_count"], 0);
}

#[test]
fn state_change_sequence_is_scoped_to_the_authoritative_role_identity() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let worker = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &worker, "worker");
    let scout = kernel
        .handle(pm_assignment_input(
            "cmd-scoped-sequence-scout",
            "task",
            RoleRef {
                role: RoleKind::Scout,
                instance: Some("scout-12".into()),
            },
            "1786687400000-scout",
            "scout-12",
            "generation-scout-12",
        ))
        .unwrap();
    activate(&mut kernel, &scout, "scout-12");

    let settled = |event_id: &str, role: &str, assignment_id: &str| KernelInput {
        decision_context: DecisionContext {
            observed_at: "2026-08-14T06:02:00Z".into(),
            allocated_ids: BTreeMap::new(),
            generations: BTreeMap::new(),
        },
        input: HandleInput::TeamEvent {
            event_id: event_id.into(),
            sequence: 77,
            name: "assignment_settled".into(),
            body: json!({
                "role": role,
                "expected_assignment_id": assignment_id,
                "safe_to_resume": false,
            }),
        },
    };

    let worker_recovery = kernel
        .handle(settled(
            "evt-scoped-sequence-worker",
            "worker",
            &worker.created_ids["assignment"],
        ))
        .unwrap();
    let scout_recovery = kernel
        .handle(settled(
            "evt-scoped-sequence-scout",
            "scout-12",
            &scout.created_ids["assignment"],
        ))
        .unwrap();

    assert_eq!(worker_recovery.disposition, HandleDisposition::Applied);
    assert_eq!(scout_recovery.disposition, HandleDisposition::Applied);
    assert_ne!(
        worker_recovery.created_ids["assignment"],
        scout_recovery.created_ids["assignment"]
    );
}

#[test]
fn worker_completion_cannot_create_a_reviewer_follow_up_on_a_stale_generation() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let observed = kernel
        .handle(KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T06:03:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([(
                    "reviewer".into(),
                    generation("generation-reviewer-current"),
                )]),
            },
            input: HandleInput::RoleObservation {
                observation_id: "obs-reviewer-generation-5".into(),
                role: RoleRef {
                    role: RoleKind::Reviewer,
                    instance: None,
                },
                generation: generation("generation-reviewer-current"),
                launch_owner: None,
                state: RoleState::Ready,
                details: json!({}),
            },
        })
        .unwrap();
    assert_eq!(observed.disposition, HandleDisposition::Applied);

    let assigned = kernel.handle(task_input()).unwrap();
    activate(&mut kernel, &assigned, "worker");
    let completion = kernel
        .handle(worker_completion_input(&assigned.created_ids["assignment"]))
        .unwrap();

    assert_eq!(completion.disposition, HandleDisposition::Rejected);
    assert_eq!(completion.error.unwrap().code, "stale_generation");
    assert!(completion.effect_intents.is_empty());
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["assignment_count"], 1);
}

#[test]
fn allocated_ids_must_match_their_stable_identity_kind() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let mut input = task_input();
    input
        .decision_context
        .allocated_ids
        .insert("assignment".into(), "msg-not-an-assignment".into());

    let error = kernel.handle(input).unwrap_err();

    assert_eq!(error.code, "invalid_allocated_id");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["assignment_count"], 0);
}

#[test]
fn allocated_ids_cannot_overwrite_existing_coordination_objects() {
    let mut kernel = MissionKernel::in_memory("msn-20260814-123000-rust-kernel-abcd");
    let first = kernel
        .handle(pm_assignment_input(
            "cmd-id-collision-1",
            "task",
            RoleRef {
                role: RoleKind::Scout,
                instance: Some("scout-21".into()),
            },
            "1786687500000-shared",
            "scout-21",
            "generation-scout-21",
        ))
        .unwrap();
    assert_eq!(first.disposition, HandleDisposition::Applied);

    let mut second = pm_assignment_input(
        "cmd-id-collision-2",
        "task",
        RoleRef {
            role: RoleKind::Scout,
            instance: Some("scout-22".into()),
        },
        "1786687500000-second",
        "scout-22",
        "generation-scout-22",
    );
    second
        .decision_context
        .allocated_ids
        .insert("assignment".into(), first.created_ids["assignment"].clone());

    let error = kernel.handle(second).unwrap_err();

    assert_eq!(error.code, "allocated_id_conflict");
    let status = kernel.inspect(InspectQuery::Status).unwrap();
    assert_eq!(status.data["assignment_count"], 1);
    assert_eq!(status.data["message_count"], 1);
    assert_eq!(status.data["effect_count"], 1);
}
