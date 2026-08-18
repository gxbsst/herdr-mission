use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

use crate::adapters::RecordingAgentProviderAdapter;
use crate::store::{
    CoordinationStore, ReadOnlyDatabasePermit, StoreObservation, WritableDatabasePermit,
};
use crate::{
    AssignmentState, ClaimedEffect, DecisionContext, DriveExecutionMode, DriveReport, DriveRequest,
    EffectIntent, EffectIntentKind, EffectOutcome, EffectResult, ErrorCategory, Generation,
    HandleDisposition, HandleInput, HandleReceipt, InspectQuery, KernelError, KernelInput,
    MissionView, RoleKind, RoleState, RuntimeOwner, ToolJobOutputMetadata, ToolJobRequest,
    ToolJobTerminalState, ToolJobTransition,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriveContext {
    pub claim_owner: String,
    pub observed_at: String,
    pub claimed_at_ms: i64,
}

pub trait EffectExecutor {
    fn execute(&mut self, intent: &EffectIntent) -> EffectOutcome;
}

#[derive(Debug)]
pub struct MissionKernel {
    store: CoordinationStore,
}

#[derive(Debug, Clone)]
pub(crate) struct MemoryCoordinationStore {
    mission_id: String,
    revision: u64,
    processed_inputs: BTreeMap<String, ProcessedReceipt>,
    processed_state_sequences: BTreeMap<(String, String, u64), ProcessedReceipt>,
    processed_reply_identities: BTreeMap<String, ProcessedReceipt>,
    processed_context_revisions: BTreeMap<(String, u64), ProcessedReceipt>,
    assignments: BTreeMap<String, Assignment>,
    messages: BTreeMap<String, Message>,
    ledger: Vec<LedgerEntry>,
    effects: BTreeMap<String, PendingEffect>,
    claimed_effects: BTreeMap<String, String>,
    tool_jobs: BTreeMap<String, ToolJob>,
    review_revisions: BTreeSet<String>,
    role_generations: BTreeMap<String, Generation>,
    role_states: BTreeMap<String, (Generation, RoleState, String)>,
    max_review_rounds: u32,
}

#[derive(Debug, Clone)]
struct ProcessedReceipt {
    fingerprint: String,
    receipt: HandleReceipt,
}

#[derive(Debug, Clone)]
struct Assignment {
    target: crate::RoleRef,
    kind: String,
    state: AssignmentState,
    parent_id: Option<String>,
    review_round: u32,
    observed_at: String,
}

#[derive(Debug, Clone)]
struct PendingEffect {
    role: crate::RoleRef,
    assignment_id: Option<String>,
    generation: Generation,
    prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolJobState {
    Queued,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

impl ToolJobState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }

    const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::TimedOut | Self::Cancelled
        )
    }
}

impl From<ToolJobTerminalState> for ToolJobState {
    fn from(value: ToolJobTerminalState) -> Self {
        match value {
            ToolJobTerminalState::Succeeded => Self::Succeeded,
            ToolJobTerminalState::Failed => Self::Failed,
            ToolJobTerminalState::TimedOut => Self::TimedOut,
            ToolJobTerminalState::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Debug, Clone)]
struct ToolJob {
    request: ToolJobRequest,
    request_fingerprint: String,
    state: ToolJobState,
    pane_id: String,
    coordination_dir: String,
    request_path: String,
    stdout_path: String,
    stderr_path: String,
    result_path: String,
    output: Option<ToolJobOutputMetadata>,
    created_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    cancelled_at: Option<String>,
    updated_at: String,
}

#[derive(Debug, Clone)]
struct Message {
    id: String,
    assignment_id: Option<String>,
    source: crate::RoleRef,
    target: crate::RoleRef,
    kind: String,
    body: String,
    revision: u64,
    observed_at: String,
}

#[derive(Debug, Clone)]
struct LedgerEntry {
    revision: u64,
    assignment_id: Option<String>,
    source: crate::RoleRef,
    kind: String,
    body: String,
    observed_at: String,
}

#[derive(Debug, Clone)]
struct PlannedReviewNotice {
    message_id: String,
    outbox_id: String,
    source: crate::RoleRef,
    target: crate::RoleRef,
    generation: Generation,
    prompt: String,
}

impl MissionKernel {
    pub fn in_memory(mission_id: impl Into<String>) -> Self {
        Self::in_memory_with_review_limit(mission_id, 3)
    }

    pub fn in_memory_with_review_limit(
        mission_id: impl Into<String>,
        max_review_rounds: u32,
    ) -> Self {
        Self {
            store: CoordinationStore::in_memory(mission_id, max_review_rounds),
        }
    }

    pub fn inspect(&self, query: InspectQuery) -> Result<MissionView, KernelError> {
        self.store.inspect(query)
    }

    pub fn handle(&mut self, request: KernelInput) -> Result<HandleReceipt, KernelError> {
        self.store.handle(request)
    }

    pub fn drive(
        &mut self,
        request: DriveRequest,
        decision_context: DecisionContext,
    ) -> Result<DriveReport, KernelError> {
        let context = DriveContext {
            claim_owner: request
                .claim_owner
                .clone()
                .filter(|owner| !owner.is_empty())
                .unwrap_or_else(|| "mission-kernel-driver".into()),
            observed_at: decision_context.observed_at,
            claimed_at_ms: request.claimed_at_ms,
        };
        match request.execution_mode {
            DriveExecutionMode::Deferred => self.claim_effects(request, context),
            DriveExecutionMode::Recording => {
                let mut executor = RecordingAgentProviderAdapter::default();
                self.drive_with_executor(request, context, &mut executor)
            }
        }
    }

    /// Drive claimed effects with a caller-provided executor (for the real
    /// Herdr/Provider adapters), resolving each effect against the state machine.
    pub fn drive_with(
        &mut self,
        request: DriveRequest,
        decision_context: DecisionContext,
        executor: &mut dyn EffectExecutor,
    ) -> Result<DriveReport, KernelError> {
        let context = DriveContext {
            claim_owner: request
                .claim_owner
                .clone()
                .filter(|owner| !owner.is_empty())
                .unwrap_or_else(|| "mission-kernel-driver".into()),
            observed_at: decision_context.observed_at,
            claimed_at_ms: request.claimed_at_ms,
        };
        self.drive_with_executor(request, context, executor)
    }

    pub(crate) fn drive_with_executor(
        &mut self,
        request: DriveRequest,
        context: DriveContext,
        executor: &mut dyn EffectExecutor,
    ) -> Result<DriveReport, KernelError> {
        if !matches!(
            request.runtime_owner,
            RuntimeOwner::Rust | RuntimeOwner::Shadow
        ) {
            return Err(KernelError {
                category: ErrorCategory::Contract,
                code: "drive_write_authority_denied".into(),
                message: "runtime owner does not grant Rust effect-driving authority".into(),
                retryable: false,
                details: BTreeMap::from([("runtime_owner".into(), json!(request.runtime_owner))]),
            });
        }
        let mut report = DriveReport {
            claimed: 0,
            resolved: 0,
            pending: 0,
            retryable_failures: 0,
            terminal_failures: 0,
            effect_results: Vec::new(),
            claimed_effects: Vec::new(),
        };
        if request.effect_budget == 0 || request.time_budget_ms == 0 {
            return Ok(report);
        }
        let started = Instant::now();
        while report.claimed < request.effect_budget
            && started.elapsed().as_millis() < u128::from(request.time_budget_ms)
        {
            let attempt_owner = format!("{}:{}", context.claim_owner, report.claimed + 1);
            let Some(intent) = self.store.claim_effect(
                &attempt_owner,
                context.claimed_at_ms,
                &context.observed_at,
            )?
            else {
                break;
            };
            report.claimed += 1;
            report.claimed_effects.push(ClaimedEffect {
                intent: intent.clone(),
                claim_owner: attempt_owner.clone(),
            });
            let outcome = executor.execute(&intent);
            match &outcome {
                EffectOutcome::Succeeded { .. } => report.resolved += 1,
                EffectOutcome::Pending { .. } => report.pending += 1,
                EffectOutcome::RetryableFailure { .. } => report.retryable_failures += 1,
                EffectOutcome::TerminalFailure { .. } => report.terminal_failures += 1,
            }
            let result = EffectResult {
                effect_id: intent.effect_id.clone(),
                generation: intent.generation.clone(),
                claim_owner: attempt_owner,
                outcome,
            };
            let role = effect_role(&intent.intent)?;
            let role_key = role_identity(role)?;
            let receipt = self.handle(KernelInput {
                decision_context: crate::DecisionContext {
                    observed_at: context.observed_at.clone(),
                    allocated_ids: BTreeMap::new(),
                    generations: BTreeMap::from([(role_key, intent.generation.clone())]),
                },
                input: HandleInput::EffectResult {
                    result: result.clone(),
                },
            })?;
            if receipt.disposition == HandleDisposition::Rejected {
                return Err(receipt.error.unwrap_or_else(|| KernelError {
                    category: ErrorCategory::Domain,
                    code: "effect_resolve_rejected".into(),
                    message: "claimed effect result was rejected".into(),
                    retryable: false,
                    details: BTreeMap::from([("effect_id".into(), json!(intent.effect_id))]),
                }));
            }
            report.effect_results.push(result);
        }
        Ok(report)
    }

    pub(crate) fn claim_effects(
        &mut self,
        request: DriveRequest,
        context: DriveContext,
    ) -> Result<DriveReport, KernelError> {
        if !matches!(
            request.runtime_owner,
            RuntimeOwner::Rust | RuntimeOwner::Shadow
        ) {
            return Err(KernelError {
                category: ErrorCategory::Contract,
                code: "drive_write_authority_denied".into(),
                message: "runtime owner does not grant Rust effect-driving authority".into(),
                retryable: false,
                details: BTreeMap::from([("runtime_owner".into(), json!(request.runtime_owner))]),
            });
        }
        let mut report = DriveReport {
            claimed: 0,
            resolved: 0,
            pending: 0,
            retryable_failures: 0,
            terminal_failures: 0,
            effect_results: Vec::new(),
            claimed_effects: Vec::new(),
        };
        if request.effect_budget == 0 || request.time_budget_ms == 0 {
            return Ok(report);
        }
        let started = Instant::now();
        while report.claimed < request.effect_budget
            && started.elapsed().as_millis() < u128::from(request.time_budget_ms)
        {
            let attempt_owner = format!("{}:{}", context.claim_owner, report.claimed + 1);
            let Some(intent) = self.store.claim_effect(
                &attempt_owner,
                context.claimed_at_ms,
                &context.observed_at,
            )?
            else {
                break;
            };
            report.claimed += 1;
            report.claimed_effects.push(ClaimedEffect {
                intent,
                claim_owner: attempt_owner,
            });
        }
        Ok(report)
    }

    pub(crate) fn open_temporary_sqlite_v3(
        mission_id: impl Into<String>,
        permit: WritableDatabasePermit,
        busy_timeout: Duration,
    ) -> Result<Self, KernelError> {
        Ok(Self {
            store: CoordinationStore::open_temporary_sqlite_v3(mission_id, permit, busy_timeout)?,
        })
    }

    /// Open a production Rust-owned database for writable kernel access.
    pub fn open_writable_sqlite_v3(
        mission_id: impl Into<String>,
        database: &std::path::Path,
        busy_timeout: Duration,
    ) -> Result<Self, KernelError> {
        let permit = WritableDatabasePermit::for_production(database)?;
        Self::open_temporary_sqlite_v3(mission_id, permit, busy_timeout)
    }

    pub(crate) fn open_read_only_sqlite_v3(
        mission_id: impl Into<String>,
        permit: ReadOnlyDatabasePermit,
    ) -> Result<Self, KernelError> {
        Ok(Self {
            store: CoordinationStore::open_read_only_sqlite_v3(mission_id, permit)?,
        })
    }

    pub(crate) fn observe_store(&self) -> Result<StoreObservation, KernelError> {
        self.store.observe()
    }
}

impl MemoryCoordinationStore {
    pub(crate) fn new(mission_id: impl Into<String>, max_review_rounds: u32) -> Self {
        Self::new_at_revision(mission_id, max_review_rounds, 0)
    }

    pub(crate) fn new_at_revision(
        mission_id: impl Into<String>,
        max_review_rounds: u32,
        revision: u64,
    ) -> Self {
        Self {
            mission_id: mission_id.into(),
            revision,
            processed_inputs: BTreeMap::new(),
            processed_state_sequences: BTreeMap::new(),
            processed_reply_identities: BTreeMap::new(),
            processed_context_revisions: BTreeMap::new(),
            assignments: BTreeMap::new(),
            messages: BTreeMap::new(),
            ledger: Vec::new(),
            effects: BTreeMap::new(),
            claimed_effects: BTreeMap::new(),
            tool_jobs: BTreeMap::new(),
            review_revisions: BTreeSet::new(),
            role_generations: BTreeMap::new(),
            role_states: BTreeMap::new(),
            max_review_rounds,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restore_assignment(
        &mut self,
        assignment_id: String,
        target: crate::RoleRef,
        kind: String,
        state: AssignmentState,
        parent_id: Option<String>,
        review_round: u32,
        observed_at: String,
    ) -> Result<(), KernelError> {
        self.ensure_id_available("assignment", &assignment_id)?;
        self.assignments.insert(
            assignment_id,
            Assignment {
                target,
                kind,
                state,
                parent_id,
                review_round,
                observed_at,
            },
        );
        Ok(())
    }

    pub(crate) fn restore_ledger_entry(
        &mut self,
        revision: u64,
        assignment_id: Option<String>,
        source: crate::RoleRef,
        kind: String,
        body: String,
        observed_at: String,
    ) {
        self.ledger.push(LedgerEntry {
            revision,
            assignment_id,
            source,
            kind,
            body,
            observed_at,
        });
    }

    pub(crate) fn restore_role_generation(
        &mut self,
        role_identity: String,
        generation: Generation,
    ) {
        self.role_generations.insert(role_identity, generation);
    }

    pub(crate) fn restore_role_state(
        &mut self,
        role_identity: String,
        generation: Generation,
        state: RoleState,
        observed_at: String,
    ) {
        self.role_states
            .insert(role_identity, (generation, state, observed_at));
    }

    pub(crate) fn restore_effect(
        &mut self,
        effect_id: String,
        role: crate::RoleRef,
        assignment_id: Option<String>,
        generation: Generation,
        prompt: String,
    ) -> Result<(), KernelError> {
        self.ensure_id_available("outbox", &effect_id)?;
        self.effects.insert(
            effect_id,
            PendingEffect {
                role,
                assignment_id,
                generation,
                prompt,
            },
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restore_tool_job(
        &mut self,
        request: ToolJobRequest,
        request_fingerprint: String,
        state: &str,
        pane_id: String,
        coordination_dir: String,
        request_path: String,
        stdout_path: String,
        stderr_path: String,
        result_path: String,
        output: Option<ToolJobOutputMetadata>,
        created_at: String,
        started_at: Option<String>,
        finished_at: Option<String>,
        cancelled_at: Option<String>,
        updated_at: String,
    ) -> Result<(), KernelError> {
        self.ensure_id_available("tool_job", &request.job_id)?;
        let state = match state {
            "queued" => ToolJobState::Queued,
            "running" => ToolJobState::Running,
            "cancelling" => ToolJobState::Cancelling,
            "succeeded" => ToolJobState::Succeeded,
            "failed" => ToolJobState::Failed,
            "timed_out" => ToolJobState::TimedOut,
            "cancelled" => ToolJobState::Cancelled,
            _ => {
                return Err(KernelError {
                    category: ErrorCategory::Contract,
                    code: "invalid_persisted_tool_job_state".into(),
                    message: "persisted Tool Job state is outside the typed lifecycle".into(),
                    retryable: false,
                    details: BTreeMap::from([("state".into(), json!(state))]),
                });
            }
        };
        self.tool_jobs.insert(
            request.job_id.clone(),
            ToolJob {
                request,
                request_fingerprint,
                state,
                pane_id,
                coordination_dir,
                request_path,
                stdout_path,
                stderr_path,
                result_path,
                output,
                created_at,
                started_at,
                finished_at,
                cancelled_at,
                updated_at,
            },
        );
        Ok(())
    }

    pub(crate) fn inspect(&self, query: InspectQuery) -> Result<MissionView, KernelError> {
        let data = match query {
            InspectQuery::Mission | InspectQuery::Status => {
                let roles = self
                    .role_states
                    .iter()
                    .map(|(role, (generation, state, observed_at))| {
                        (
                            role.clone(),
                            json!({
                                "generation": generation,
                                "state": state,
                                "observed_at": observed_at,
                            }),
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                let assignments = self
                    .assignments
                    .iter()
                    .map(|(assignment_id, assignment)| {
                        assignment_projection(assignment_id, assignment)
                    })
                    .collect::<Vec<_>>();
                let tool_jobs = self
                    .tool_jobs
                    .values()
                    .map(tool_job_projection)
                    .collect::<Vec<_>>();
                json!({
                    "mission_id": self.mission_id,
                    "revision": self.revision,
                    "assignment_count": self.assignments.len(),
                    "message_count": self.messages.len(),
                    "effect_count": self.effects.len(),
                    "tool_job_count": self.tool_jobs.len(),
                    "roles": roles,
                    "assignments": assignments,
                    "tool_jobs": tool_jobs,
                })
            }
            InspectQuery::Inbox { role } => {
                let messages = self
                    .messages
                    .values()
                    .filter(|message| role.as_ref().is_none_or(|role| &message.target == role))
                    .map(message_projection)
                    .collect::<Vec<_>>();
                json!({"messages": messages})
            }
            InspectQuery::AssignmentThread { assignment_id } => {
                let assignment = self
                    .assignments
                    .get(&assignment_id)
                    .map(|assignment| assignment_projection(&assignment_id, assignment));
                let messages = self
                    .messages
                    .values()
                    .filter(|message| message.assignment_id.as_deref() == Some(&assignment_id))
                    .map(message_projection)
                    .collect::<Vec<_>>();
                json!({
                    "assignment_id": assignment_id,
                    "assignment": assignment,
                    "messages": messages,
                })
            }
            InspectQuery::Diagnostics => json!({
                "processed_inputs": self.processed_inputs.len(),
                "processed_state_sequences": self.processed_state_sequences.len(),
                "processed_reply_identities": self.processed_reply_identities.len(),
                "processed_context_revisions": self.processed_context_revisions.len(),
                "ledger_entries": self.ledger.len(),
                "tool_jobs": self.tool_jobs.len(),
                "ledger": self.ledger.iter().map(ledger_projection).collect::<Vec<_>>(),
            }),
        };
        Ok(MissionView {
            revision: Some(self.revision),
            data,
        })
    }

    pub(crate) fn handle(&mut self, request: KernelInput) -> Result<HandleReceipt, KernelError> {
        let mut transaction = self.clone();
        let result = transaction.apply_handle(request);
        if result
            .as_ref()
            .is_ok_and(|receipt| receipt.disposition != HandleDisposition::Rejected)
        {
            *self = transaction;
        }
        result
    }

    pub(crate) fn observe(&self) -> StoreObservation {
        StoreObservation {
            schema_version: "memory".into(),
            mission_id: self.mission_id.clone(),
            assignment_count: self.assignments.len() as u64,
            message_count: self.messages.len() as u64,
            outbox_count: self.effects.len() as u64,
            revision: self.revision,
        }
    }

    pub(crate) fn claim_effect(
        &mut self,
        claim_owner: &str,
        _claimed_at_ms: i64,
        _observed_at: &str,
    ) -> Result<Option<EffectIntent>, KernelError> {
        if claim_owner.trim().is_empty() {
            return Err(KernelError {
                category: ErrorCategory::Contract,
                code: "empty_claim_owner".into(),
                message: "drive requires a non-empty durable claim owner".into(),
                retryable: false,
                details: BTreeMap::new(),
            });
        }
        let candidate = self
            .effects
            .iter()
            .find(|(effect_id, _)| !self.claimed_effects.contains_key(*effect_id))
            .map(|(effect_id, effect)| (effect_id.clone(), effect.clone()));
        let Some((effect_id, effect)) = candidate else {
            return Ok(None);
        };
        self.claimed_effects
            .insert(effect_id.clone(), claim_owner.to_owned());
        Ok(Some(EffectIntent {
            effect_id,
            generation: effect.generation,
            intent: EffectIntentKind::DeliverPrompt {
                role: effect.role,
                assignment_id: effect.assignment_id,
                prompt: effect.prompt,
            },
        }))
    }

    fn apply_handle(&mut self, request: KernelInput) -> Result<HandleReceipt, KernelError> {
        validate_observed_at(&request.decision_context.observed_at)?;
        validate_input_roles(&request.input)?;
        let input_fingerprint = semantic_fingerprint(&request.input)?;
        let state_change_identity = match &request.input {
            HandleInput::TeamEvent {
                sequence,
                name,
                body,
                ..
            } => Some((
                (name.clone(), event_scope(body), *sequence),
                semantic_fingerprint(&json!({"name": name, "body": body}))?,
            )),
            _ => None,
        };
        let input_id = match &request.input {
            HandleInput::Command { command_id, .. } => command_id,
            HandleInput::TeamEvent { event_id, .. } => event_id,
            HandleInput::RoleLaunchRequest { launch_id, .. } => launch_id,
            HandleInput::RoleObservation { observation_id, .. } => observation_id,
            HandleInput::EffectResult { result } => &result.effect_id,
            HandleInput::ToolJobRequest { request } => &request.job_id,
            HandleInput::ToolJobTransition { transition_id, .. } => transition_id,
        };
        let processed_input_key = processed_input_key(&request.input);
        if let Some(original) = self.processed_inputs.get(&processed_input_key) {
            if original.fingerprint != input_fingerprint {
                return Ok(rejected_receipt(
                    input_id.clone(),
                    "input_id_conflict",
                    "input ID was already committed with different semantics",
                ));
            }
            let mut duplicate = original.receipt.clone();
            duplicate.disposition = HandleDisposition::Duplicate;
            return Ok(duplicate);
        }
        if let (
            HandleInput::TeamEvent { event_id, .. },
            Some((state_change_key, state_change_fingerprint)),
        ) = (&request.input, &state_change_identity)
        {
            if let Some(original) = self.processed_state_sequences.get(state_change_key) {
                if original.fingerprint != *state_change_fingerprint {
                    return Ok(rejected_receipt(
                        event_id.clone(),
                        "state_change_sequence_conflict",
                        "state-change sequence was already committed with different semantics",
                    ));
                }
                let mut duplicate = original.receipt.clone();
                duplicate.input_id = event_id.clone();
                duplicate.disposition = HandleDisposition::Duplicate;
                return Ok(duplicate);
            }
        }
        if let HandleInput::Command {
            command_id,
            source,
            target: Some(target),
            ..
        } = &request.input
        {
            if !send_allowed(source.role, target.role) {
                return Ok(rejected_receipt(
                    command_id.clone(),
                    "acl_denied",
                    "source role is not allowed to deliver to target role",
                ));
            }
        }

        match request.input {
            HandleInput::Command {
                command_id,
                kind,
                source,
                target: Some(target),
                body,
            } if kind == "context" && source.role == RoleKind::Pm => {
                let generation_key = role_identity(&target)?;
                let prompt = body
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let context_revision = body.get("context_revision").and_then(Value::as_u64);
                let context_fingerprint = semantic_fingerprint(&json!({
                    "target": generation_key,
                    "context_revision": context_revision,
                    "text": prompt,
                }))?;
                if let Some(context_revision) = context_revision {
                    if let Some(original) = self
                        .processed_context_revisions
                        .get(&(generation_key.clone(), context_revision))
                    {
                        if original.fingerprint != context_fingerprint {
                            return Ok(rejected_receipt(
                                command_id,
                                "context_revision_conflict",
                                "context revision was already committed with different text",
                            ));
                        }
                        let mut duplicate = original.receipt.clone();
                        duplicate.input_id = command_id.clone();
                        duplicate.disposition = HandleDisposition::Duplicate;
                        self.processed_inputs.insert(
                            processed_input_key.clone(),
                            ProcessedReceipt {
                                fingerprint: input_fingerprint.clone(),
                                receipt: duplicate.clone(),
                            },
                        );
                        return Ok(duplicate);
                    }
                }
                let message_id = required_id(&request.decision_context.allocated_ids, "message")?;
                let outbox_id = required_id(&request.decision_context.allocated_ids, "outbox")?;
                self.ensure_id_available("message", &message_id)?;
                self.ensure_id_available("outbox", &outbox_id)?;
                let generation = request
                    .decision_context
                    .generations
                    .get(&generation_key)
                    .cloned()
                    .ok_or_else(|| missing_decision_value("generation", &generation_key))?;
                if self.generation_is_stale(&generation_key, &generation) {
                    return Ok(rejected_receipt(
                        command_id,
                        "stale_generation",
                        "command generation cannot replace a newer role generation",
                    ));
                }
                self.role_generations
                    .insert(generation_key.clone(), generation.clone());
                self.effects.insert(
                    outbox_id.clone(),
                    PendingEffect {
                        role: target.clone(),
                        assignment_id: None,
                        generation: generation.clone(),
                        prompt: prompt.clone(),
                    },
                );

                self.revision += 1;
                self.messages.insert(
                    message_id.clone(),
                    Message {
                        id: message_id.clone(),
                        assignment_id: None,
                        source,
                        target: target.clone(),
                        kind: "context".into(),
                        body: prompt.clone(),
                        revision: self.revision,
                        observed_at: request.decision_context.observed_at.clone(),
                    },
                );
                self.ledger.push(LedgerEntry {
                    revision: self.revision,
                    assignment_id: None,
                    source: self
                        .messages
                        .get(&message_id)
                        .expect("context message was inserted above")
                        .source
                        .clone(),
                    kind: "context".into(),
                    body: prompt.clone(),
                    observed_at: request.decision_context.observed_at.clone(),
                });
                let receipt = HandleReceipt {
                    input_id: command_id,
                    disposition: HandleDisposition::Applied,
                    revision: Some(self.revision),
                    effect_intents: vec![EffectIntent {
                        effect_id: outbox_id.clone(),
                        generation,
                        intent: EffectIntentKind::DeliverPrompt {
                            role: target,
                            assignment_id: None,
                            prompt,
                        },
                    }],
                    created_ids: BTreeMap::from([
                        ("message".into(), message_id),
                        ("outbox".into(), outbox_id),
                    ]),
                    relationships: BTreeMap::new(),
                    review_round: None,
                    assignment_state: None,
                    review_limit_reached: false,
                    error: None,
                };
                self.processed_inputs.insert(
                    processed_input_key.clone(),
                    ProcessedReceipt {
                        fingerprint: input_fingerprint.clone(),
                        receipt: receipt.clone(),
                    },
                );
                if let Some(context_revision) = context_revision {
                    self.processed_context_revisions.insert(
                        (generation_key, context_revision),
                        ProcessedReceipt {
                            fingerprint: context_fingerprint,
                            receipt: receipt.clone(),
                        },
                    );
                }
                Ok(receipt)
            }
            HandleInput::Command {
                command_id,
                kind,
                source,
                ..
            } if kind == "context" && source.role != RoleKind::Pm => Ok(rejected_receipt(
                command_id,
                "invalid_context_source",
                "context notices may only be sent by PM",
            )),
            HandleInput::Command {
                command_id,
                kind,
                source,
                target,
                body,
            } if source.role == RoleKind::Pm
                && target
                    .as_ref()
                    .is_some_and(|role| assignment_kind_allowed(role.role, &kind)) =>
            {
                let target = target.expect("guard requires a target");
                if self.role_capacity_held(&target) {
                    return Ok(rejected_receipt(
                        command_id,
                        "role_capacity_exhausted",
                        "another assignment already holds the target role capacity",
                    ));
                }
                let assignment_id =
                    required_id(&request.decision_context.allocated_ids, "assignment")?;
                let message_id = required_id(&request.decision_context.allocated_ids, "message")?;
                let outbox_id = required_id(&request.decision_context.allocated_ids, "outbox")?;
                self.ensure_id_available("assignment", &assignment_id)?;
                self.ensure_id_available("message", &message_id)?;
                self.ensure_id_available("outbox", &outbox_id)?;
                let generation_key = role_identity(&target)?;
                let generation = request
                    .decision_context
                    .generations
                    .get(&generation_key)
                    .cloned()
                    .ok_or_else(|| missing_decision_value("generation", &generation_key))?;
                if self.generation_is_stale(&generation_key, &generation) {
                    return Ok(rejected_receipt(
                        command_id,
                        "stale_generation",
                        "command generation cannot replace a newer role generation",
                    ));
                }
                let prompt = body
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.role_generations
                    .insert(generation_key, generation.clone());

                self.revision += 1;
                self.assignments.insert(
                    assignment_id.clone(),
                    Assignment {
                        target: target.clone(),
                        kind: kind.clone(),
                        state: AssignmentState::Queued,
                        parent_id: None,
                        review_round: 0,
                        observed_at: request.decision_context.observed_at.clone(),
                    },
                );
                self.effects.insert(
                    outbox_id.clone(),
                    PendingEffect {
                        role: target.clone(),
                        assignment_id: Some(assignment_id.clone()),
                        generation: generation.clone(),
                        prompt: prompt.clone(),
                    },
                );
                self.messages.insert(
                    message_id.clone(),
                    Message {
                        id: message_id.clone(),
                        assignment_id: Some(assignment_id.clone()),
                        source,
                        target: target.clone(),
                        kind,
                        body: prompt.clone(),
                        revision: self.revision,
                        observed_at: request.decision_context.observed_at.clone(),
                    },
                );
                let receipt = HandleReceipt {
                    input_id: command_id,
                    disposition: HandleDisposition::Applied,
                    revision: Some(self.revision),
                    effect_intents: vec![EffectIntent {
                        effect_id: outbox_id.clone(),
                        generation,
                        intent: EffectIntentKind::DeliverPrompt {
                            role: target,
                            assignment_id: Some(assignment_id.clone()),
                            prompt,
                        },
                    }],
                    created_ids: BTreeMap::from([
                        ("assignment".into(), assignment_id),
                        ("message".into(), message_id),
                        ("outbox".into(), outbox_id),
                    ]),
                    relationships: BTreeMap::new(),
                    review_round: None,
                    assignment_state: Some(AssignmentState::Queued),
                    review_limit_reached: false,
                    error: None,
                };
                self.processed_inputs.insert(
                    processed_input_key.clone(),
                    ProcessedReceipt {
                        fingerprint: input_fingerprint.clone(),
                        receipt: receipt.clone(),
                    },
                );
                Ok(receipt)
            }
            HandleInput::Command {
                command_id,
                kind,
                source,
                target: Some(target),
                body,
            } if source.role != RoleKind::Pm && target.role == RoleKind::Pm => {
                let Some(assignment_id) = body.get("assignment_id").and_then(Value::as_str) else {
                    return Ok(rejected_receipt(
                        command_id,
                        "missing_assignment_id",
                        "reply commands require body.assignment_id",
                    ));
                };
                let Some(assignment) = self.assignments.get(assignment_id).cloned() else {
                    return Ok(rejected_receipt(
                        command_id,
                        "assignment_not_found",
                        "reply assignment does not exist",
                    ));
                };
                if assignment.target != source {
                    return Ok(rejected_receipt(
                        command_id,
                        "assignment_owner_mismatch",
                        "reply source does not own the assignment",
                    ));
                }
                if !reply_kind_allowed(source.role, &assignment.kind, &kind) {
                    return Ok(rejected_receipt(
                        command_id,
                        "invalid_reply_kind",
                        "reply kind is not allowed for the assignment owner",
                    ));
                }
                let settled_state = match kind.as_str() {
                    "approved" => AssignmentState::Approved,
                    "rejected" => AssignmentState::Rejected,
                    "blocked" => AssignmentState::Blocked,
                    "completed" | "finding" => AssignmentState::Completed,
                    _ => unreachable!("reply kind was validated above"),
                };

                let prompt = body
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let source_identity = role_identity(&source)?;
                let reply_identity = body
                    .get("reply_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!("{assignment_id}:{source_identity}:{kind}:{prompt}")
                    });
                let reply_fingerprint = semantic_fingerprint(&json!({
                    "assignment_id": assignment_id,
                    "source": source_identity,
                    "kind": kind,
                    "text": prompt,
                }))?;
                if let Some(original) = self.processed_reply_identities.get(&reply_identity) {
                    if original.fingerprint != reply_fingerprint {
                        return Ok(rejected_receipt(
                            command_id,
                            "reply_identity_conflict",
                            "reply identity was already committed with different semantics",
                        ));
                    }
                    let mut duplicate = original.receipt.clone();
                    duplicate.input_id = command_id.clone();
                    duplicate.disposition = HandleDisposition::Duplicate;
                    self.processed_inputs.insert(
                        processed_input_key.clone(),
                        ProcessedReceipt {
                            fingerprint: input_fingerprint.clone(),
                            receipt: duplicate.clone(),
                        },
                    );
                    return Ok(duplicate);
                }
                let prior_reply = self.ledger.iter().rev().find(|entry| {
                    entry.assignment_id.as_deref() == Some(assignment_id) && entry.source == source
                });
                if !matches!(
                    assignment.state,
                    AssignmentState::Active | AssignmentState::Blocked
                ) {
                    if let Some(entry) = prior_reply {
                        let receipt = HandleReceipt {
                            input_id: command_id.clone(),
                            disposition: HandleDisposition::Duplicate,
                            revision: Some(entry.revision),
                            effect_intents: Vec::new(),
                            created_ids: BTreeMap::new(),
                            relationships: BTreeMap::new(),
                            review_round: Some(assignment.review_round),
                            assignment_state: Some(assignment.state),
                            review_limit_reached: false,
                            error: None,
                        };
                        let processed = ProcessedReceipt {
                            fingerprint: input_fingerprint.clone(),
                            receipt: receipt.clone(),
                        };
                        self.processed_inputs
                            .insert(processed_input_key.clone(), processed.clone());
                        self.processed_reply_identities
                            .insert(reply_identity, processed);
                        return Ok(receipt);
                    }
                    return Ok(rejected_receipt(
                        command_id,
                        "assignment_not_active",
                        "reply assignment must be active",
                    ));
                }
                if assignment.state == AssignmentState::Blocked
                    && prior_reply.is_some_and(|entry| entry.kind == kind)
                {
                    return Ok(rejected_receipt(
                        command_id,
                        "assignment_not_active",
                        "reply assignment must be active",
                    ));
                }

                let message_id = required_id(&request.decision_context.allocated_ids, "message")?;
                let outbox_id = required_id(&request.decision_context.allocated_ids, "outbox")?;
                self.ensure_id_available("message", &message_id)?;
                self.ensure_id_available("outbox", &outbox_id)?;
                let generation = request
                    .decision_context
                    .generations
                    .get("pm")
                    .cloned()
                    .ok_or_else(|| missing_decision_value("generation", "pm"))?;
                if self.generation_is_stale("pm", &generation) {
                    return Ok(rejected_receipt(
                        command_id,
                        "stale_generation",
                        "reply generation cannot replace a newer PM generation",
                    ));
                }
                let mut effect_intents = vec![EffectIntent {
                    effect_id: outbox_id.clone(),
                    generation: generation.clone(),
                    intent: EffectIntentKind::DeliverPrompt {
                        role: target.clone(),
                        assignment_id: None,
                        prompt: prompt.clone(),
                    },
                }];
                let mut created_ids = BTreeMap::from([
                    ("message".into(), message_id.clone()),
                    ("outbox".into(), outbox_id.clone()),
                ]);
                let mut relationships = BTreeMap::new();
                let mut review_round = None;
                let mut review_limit_reached = false;
                let mut review_revision_id = None;
                let mut review_notice_records = Vec::new();
                let mut follow_up_message_record: Option<(
                    String,
                    String,
                    crate::RoleRef,
                    String,
                    String,
                )> = None;

                if source.role == RoleKind::Reviewer
                    && matches!(kind.as_str(), "approved" | "rejected")
                {
                    let review_revision =
                        required_id(&request.decision_context.allocated_ids, "review_revision")?;
                    self.ensure_id_available("review_revision", &review_revision)?;
                    let acknowledge_review = match body.get("acknowledge_review") {
                        None => false,
                        Some(Value::Bool(value)) => *value,
                        Some(_) => {
                            return Ok(rejected_receipt(
                                command_id,
                                "invalid_review_acknowledgement",
                                "acknowledge_review must be a boolean",
                            ));
                        }
                    };
                    if acknowledge_review && kind != "approved" {
                        return Ok(rejected_receipt(
                            command_id,
                            "invalid_review_acknowledgement",
                            "only an approved review can be acknowledged immediately",
                        ));
                    }
                    let pm_notice_message = required_id(
                        &request.decision_context.allocated_ids,
                        "review_pm_notice_message",
                    )?;
                    let pm_notice_outbox = required_id(
                        &request.decision_context.allocated_ids,
                        "review_pm_notice_outbox",
                    )?;
                    let worker_notice_message = required_id(
                        &request.decision_context.allocated_ids,
                        "review_worker_notice_message",
                    )?;
                    let worker_notice_outbox = required_id(
                        &request.decision_context.allocated_ids,
                        "review_worker_notice_outbox",
                    )?;
                    self.ensure_ids_available(&[
                        ("message", &pm_notice_message),
                        ("outbox", &pm_notice_outbox),
                        ("message", &worker_notice_message),
                        ("outbox", &worker_notice_outbox),
                    ])?;
                    let worker_generation = request
                        .decision_context
                        .generations
                        .get("worker")
                        .cloned()
                        .ok_or_else(|| missing_decision_value("generation", "worker"))?;
                    if self.generation_is_stale("worker", &worker_generation) {
                        return Ok(rejected_receipt(
                            command_id,
                            "stale_generation",
                            "review notice generation is older than the current Worker generation",
                        ));
                    }
                    let notice_prompt =
                        format!("Reviewer {kind}：{prompt}（review_id={review_revision}）");
                    let pm = crate::RoleRef {
                        role: RoleKind::Pm,
                        instance: None,
                    };
                    let worker = crate::RoleRef {
                        role: RoleKind::Worker,
                        instance: None,
                    };
                    review_notice_records.extend([
                        PlannedReviewNotice {
                            message_id: pm_notice_message.clone(),
                            outbox_id: pm_notice_outbox.clone(),
                            source: source.clone(),
                            target: pm,
                            generation: generation.clone(),
                            prompt: notice_prompt.clone(),
                        },
                        PlannedReviewNotice {
                            message_id: worker_notice_message.clone(),
                            outbox_id: worker_notice_outbox.clone(),
                            source: crate::RoleRef {
                                role: RoleKind::Pm,
                                instance: None,
                            },
                            target: worker,
                            generation: worker_generation,
                            prompt: notice_prompt,
                        },
                    ]);
                    created_ids.insert("review_revision".into(), review_revision.clone());
                    created_ids.extend([
                        ("review_pm_notice_message".into(), pm_notice_message),
                        ("review_pm_notice_outbox".into(), pm_notice_outbox),
                        ("review_worker_notice_message".into(), worker_notice_message),
                        ("review_worker_notice_outbox".into(), worker_notice_outbox),
                    ]);
                    review_revision_id = Some(review_revision);
                }

                if source.role == RoleKind::Worker && kind == "completed" {
                    let follow_up_assignment = required_id(
                        &request.decision_context.allocated_ids,
                        "follow_up_assignment",
                    )?;
                    let follow_up_message =
                        required_id(&request.decision_context.allocated_ids, "follow_up_message")?;
                    let follow_up_outbox =
                        required_id(&request.decision_context.allocated_ids, "follow_up_outbox")?;
                    self.ensure_ids_available(&[
                        ("message", &message_id),
                        ("outbox", &outbox_id),
                        ("follow_up_assignment", &follow_up_assignment),
                        ("follow_up_message", &follow_up_message),
                        ("follow_up_outbox", &follow_up_outbox),
                    ])?;
                    let reviewer_generation = request
                        .decision_context
                        .generations
                        .get("reviewer")
                        .cloned()
                        .ok_or_else(|| missing_decision_value("generation", "reviewer"))?;
                    if self.generation_is_stale("reviewer", &reviewer_generation) {
                        return Ok(rejected_receipt(
                            command_id,
                            "stale_generation",
                            "Reviewer follow-up generation is older than the current role generation",
                        ));
                    }
                    let reviewer = crate::RoleRef {
                        role: RoleKind::Reviewer,
                        instance: None,
                    };
                    if self.role_active_capacity_held(&reviewer) {
                        return Ok(rejected_receipt(
                            command_id,
                            "role_capacity_exhausted",
                            "another assignment already holds the target role capacity",
                        ));
                    }
                    let parent_assignment = assignment
                        .parent_id
                        .clone()
                        .unwrap_or_else(|| assignment_id.to_owned());
                    let follow_up_prompt =
                        format!("审查 Worker 完成的 assignment {assignment_id}：{prompt}");
                    self.assignments.insert(
                        follow_up_assignment.clone(),
                        Assignment {
                            target: reviewer.clone(),
                            kind: "review".into(),
                            state: AssignmentState::Queued,
                            parent_id: Some(parent_assignment.clone()),
                            review_round: assignment.review_round,
                            observed_at: request.decision_context.observed_at.clone(),
                        },
                    );
                    self.role_generations
                        .insert("reviewer".into(), reviewer_generation.clone());
                    self.effects.insert(
                        follow_up_outbox.clone(),
                        PendingEffect {
                            role: reviewer.clone(),
                            assignment_id: Some(follow_up_assignment.clone()),
                            generation: reviewer_generation.clone(),
                            prompt: follow_up_prompt.clone(),
                        },
                    );
                    effect_intents.push(EffectIntent {
                        effect_id: follow_up_outbox.clone(),
                        generation: reviewer_generation,
                        intent: EffectIntentKind::DeliverPrompt {
                            role: reviewer,
                            assignment_id: Some(follow_up_assignment.clone()),
                            prompt: follow_up_prompt.clone(),
                        },
                    });
                    follow_up_message_record = Some((
                        follow_up_message.clone(),
                        follow_up_assignment.clone(),
                        crate::RoleRef {
                            role: RoleKind::Reviewer,
                            instance: None,
                        },
                        "review".into(),
                        follow_up_prompt,
                    ));
                    created_ids.extend([
                        ("follow_up_assignment".into(), follow_up_assignment),
                        ("follow_up_message".into(), follow_up_message),
                        ("follow_up_outbox".into(), follow_up_outbox),
                    ]);
                    relationships.insert("parent_assignment".into(), parent_assignment);
                    review_round = Some(assignment.review_round);
                }

                if source.role == RoleKind::Reviewer && kind == "rejected" {
                    let parent_assignment = assignment
                        .parent_id
                        .clone()
                        .unwrap_or_else(|| assignment_id.to_owned());
                    let next_review_round = assignment.review_round.saturating_add(1);
                    relationships.insert("parent_assignment".into(), parent_assignment);
                    review_round = Some(next_review_round);
                    review_limit_reached = next_review_round > self.max_review_rounds;
                    if !review_limit_reached {
                        let follow_up_assignment = required_id(
                            &request.decision_context.allocated_ids,
                            "follow_up_assignment",
                        )?;
                        let follow_up_message = required_id(
                            &request.decision_context.allocated_ids,
                            "follow_up_message",
                        )?;
                        let follow_up_outbox = required_id(
                            &request.decision_context.allocated_ids,
                            "follow_up_outbox",
                        )?;
                        self.ensure_ids_available(&[
                            ("message", &message_id),
                            ("outbox", &outbox_id),
                            ("follow_up_assignment", &follow_up_assignment),
                            ("follow_up_message", &follow_up_message),
                            ("follow_up_outbox", &follow_up_outbox),
                        ])?;
                        let worker_generation = request
                            .decision_context
                            .generations
                            .get("worker")
                            .cloned()
                            .ok_or_else(|| missing_decision_value("generation", "worker"))?;
                        if self.generation_is_stale("worker", &worker_generation) {
                            return Ok(rejected_receipt(
                                command_id,
                                "stale_generation",
                                "Worker fix generation is older than the current role generation",
                            ));
                        }
                        let worker = crate::RoleRef {
                            role: RoleKind::Worker,
                            instance: None,
                        };
                        if self.role_capacity_held(&worker) {
                            return Ok(rejected_receipt(
                                command_id,
                                "role_capacity_exhausted",
                                "another assignment already holds the target role capacity",
                            ));
                        }
                        let parent_assignment = relationships["parent_assignment"].clone();
                        let follow_up_prompt = format!("修复 Reviewer 指出的问题：{prompt}");
                        self.assignments.insert(
                            follow_up_assignment.clone(),
                            Assignment {
                                target: worker.clone(),
                                kind: "fix".into(),
                                state: AssignmentState::Queued,
                                parent_id: Some(parent_assignment),
                                review_round: next_review_round,
                                observed_at: request.decision_context.observed_at.clone(),
                            },
                        );
                        self.role_generations
                            .insert("worker".into(), worker_generation.clone());
                        self.effects.insert(
                            follow_up_outbox.clone(),
                            PendingEffect {
                                role: worker.clone(),
                                assignment_id: Some(follow_up_assignment.clone()),
                                generation: worker_generation.clone(),
                                prompt: follow_up_prompt.clone(),
                            },
                        );
                        effect_intents.push(EffectIntent {
                            effect_id: follow_up_outbox.clone(),
                            generation: worker_generation,
                            intent: EffectIntentKind::DeliverPrompt {
                                role: worker,
                                assignment_id: Some(follow_up_assignment.clone()),
                                prompt: follow_up_prompt.clone(),
                            },
                        });
                        follow_up_message_record = Some((
                            follow_up_message.clone(),
                            follow_up_assignment.clone(),
                            crate::RoleRef {
                                role: RoleKind::Worker,
                                instance: None,
                            },
                            "fix".into(),
                            follow_up_prompt,
                        ));
                        created_ids.extend([
                            ("follow_up_assignment".into(), follow_up_assignment),
                            ("follow_up_message".into(), follow_up_message),
                            ("follow_up_outbox".into(), follow_up_outbox),
                        ]);
                    }
                }

                for notice in &review_notice_records {
                    effect_intents.push(EffectIntent {
                        effect_id: notice.outbox_id.clone(),
                        generation: notice.generation.clone(),
                        intent: EffectIntentKind::DeliverPrompt {
                            role: notice.target.clone(),
                            assignment_id: Some(assignment_id.to_owned()),
                            prompt: notice.prompt.clone(),
                        },
                    });
                }

                self.role_generations
                    .insert("pm".into(), generation.clone());
                self.effects.insert(
                    outbox_id.clone(),
                    PendingEffect {
                        role: target.clone(),
                        assignment_id: None,
                        generation: generation.clone(),
                        prompt: prompt.clone(),
                    },
                );
                for notice in &review_notice_records {
                    self.role_generations
                        .insert(role_identity(&notice.target)?, notice.generation.clone());
                    self.effects.insert(
                        notice.outbox_id.clone(),
                        PendingEffect {
                            role: notice.target.clone(),
                            assignment_id: Some(assignment_id.to_owned()),
                            generation: notice.generation.clone(),
                            prompt: notice.prompt.clone(),
                        },
                    );
                }
                self.revision += 1;
                self.assignments
                    .get_mut(assignment_id)
                    .expect("assignment was validated above")
                    .state = settled_state;
                self.assignments
                    .get_mut(assignment_id)
                    .expect("assignment was validated above")
                    .observed_at = request.decision_context.observed_at.clone();
                self.messages.insert(
                    message_id.clone(),
                    Message {
                        id: message_id,
                        assignment_id: Some(assignment_id.to_owned()),
                        source: source.clone(),
                        target: target.clone(),
                        kind: kind.clone(),
                        body: prompt.clone(),
                        revision: self.revision,
                        observed_at: request.decision_context.observed_at.clone(),
                    },
                );
                self.ledger.push(LedgerEntry {
                    revision: self.revision,
                    assignment_id: Some(assignment_id.to_owned()),
                    source,
                    kind,
                    body: prompt,
                    observed_at: request.decision_context.observed_at.clone(),
                });
                if let Some((id, follow_up_assignment, follow_up_target, follow_up_kind, body)) =
                    follow_up_message_record
                {
                    self.messages.insert(
                        id.clone(),
                        Message {
                            id,
                            assignment_id: Some(follow_up_assignment),
                            source: crate::RoleRef {
                                role: RoleKind::Pm,
                                instance: None,
                            },
                            target: follow_up_target,
                            kind: follow_up_kind,
                            body,
                            revision: self.revision,
                            observed_at: request.decision_context.observed_at.clone(),
                        },
                    );
                }
                for notice in review_notice_records {
                    self.messages.insert(
                        notice.message_id.clone(),
                        Message {
                            id: notice.message_id,
                            assignment_id: Some(assignment_id.to_owned()),
                            source: notice.source,
                            target: notice.target,
                            kind: "context".into(),
                            body: notice.prompt,
                            revision: self.revision,
                            observed_at: request.decision_context.observed_at.clone(),
                        },
                    );
                }
                if let Some(review_revision_id) = review_revision_id {
                    self.review_revisions.insert(review_revision_id);
                }
                let receipt = HandleReceipt {
                    input_id: command_id,
                    disposition: HandleDisposition::Applied,
                    revision: Some(self.revision),
                    effect_intents,
                    created_ids,
                    relationships,
                    review_round,
                    assignment_state: Some(settled_state),
                    review_limit_reached,
                    error: None,
                };
                self.processed_inputs.insert(
                    processed_input_key.clone(),
                    ProcessedReceipt {
                        fingerprint: input_fingerprint.clone(),
                        receipt: receipt.clone(),
                    },
                );
                self.processed_reply_identities.insert(
                    reply_identity,
                    ProcessedReceipt {
                        fingerprint: reply_fingerprint,
                        receipt: receipt.clone(),
                    },
                );
                Ok(receipt)
            }
            HandleInput::EffectResult { result } => {
                let Some(effect) = self.effects.get(&result.effect_id).cloned() else {
                    return Ok(rejected_receipt(
                        result.effect_id,
                        "effect_not_found",
                        "effect result does not match a pending effect",
                    ));
                };
                let role_key = role_identity(&effect.role)?;
                let current_generation = self
                    .role_generations
                    .get(&role_key)
                    .cloned()
                    .unwrap_or_else(|| effect.generation.clone());
                if result.generation != effect.generation || result.generation != current_generation
                {
                    return Ok(rejected_receipt(
                        result.effect_id,
                        "stale_generation",
                        "effect result generation no longer owns the role",
                    ));
                }
                if self
                    .claimed_effects
                    .get(&result.effect_id)
                    .is_some_and(|claim_owner| {
                        result.claim_owner.trim().is_empty() || result.claim_owner != *claim_owner
                    })
                {
                    return Ok(rejected_receipt(
                        result.effect_id,
                        "outbox_claim_owner_mismatch",
                        "effect result does not own the current claim",
                    ));
                }
                self.claimed_effects.remove(&result.effect_id);
                if !matches!(result.outcome, EffectOutcome::Succeeded { .. }) {
                    if matches!(result.outcome, EffectOutcome::TerminalFailure { .. }) {
                        self.effects.remove(&result.effect_id);
                    }
                    let receipt = HandleReceipt {
                        input_id: result.effect_id,
                        disposition: HandleDisposition::Applied,
                        revision: Some(self.revision),
                        effect_intents: Vec::new(),
                        created_ids: BTreeMap::new(),
                        relationships: BTreeMap::new(),
                        review_round: None,
                        assignment_state: None,
                        review_limit_reached: false,
                        error: None,
                    };
                    self.processed_inputs.insert(
                        processed_input_key.clone(),
                        ProcessedReceipt {
                            fingerprint: input_fingerprint.clone(),
                            receipt: receipt.clone(),
                        },
                    );
                    return Ok(receipt);
                }
                let Some(assignment_id) = effect.assignment_id else {
                    self.effects.remove(&result.effect_id);
                    let receipt = HandleReceipt {
                        input_id: result.effect_id,
                        disposition: HandleDisposition::Applied,
                        revision: Some(self.revision),
                        effect_intents: Vec::new(),
                        created_ids: BTreeMap::new(),
                        relationships: BTreeMap::new(),
                        review_round: None,
                        assignment_state: None,
                        review_limit_reached: false,
                        error: None,
                    };
                    self.processed_inputs.insert(
                        processed_input_key.clone(),
                        ProcessedReceipt {
                            fingerprint: input_fingerprint.clone(),
                            receipt: receipt.clone(),
                        },
                    );
                    return Ok(receipt);
                };
                let Some(pending_assignment) = self.assignments.get(&assignment_id).cloned() else {
                    return Ok(rejected_receipt(
                        result.effect_id,
                        "assignment_not_found",
                        "effect assignment does not exist",
                    ));
                };
                let capacity_held = self.assignments.iter().any(|(candidate_id, candidate)| {
                    candidate_id != &assignment_id
                        && candidate.target == pending_assignment.target
                        && candidate.state == AssignmentState::Active
                });
                if capacity_held {
                    return Ok(rejected_receipt(
                        result.effect_id,
                        "role_capacity_exhausted",
                        "another assignment already holds the target role capacity",
                    ));
                }
                let assignment = self
                    .assignments
                    .get_mut(&assignment_id)
                    .expect("assignment existence was validated above");
                assignment.state = AssignmentState::Active;
                assignment.observed_at = request.decision_context.observed_at.clone();
                let receipt = HandleReceipt {
                    input_id: result.effect_id,
                    disposition: HandleDisposition::Applied,
                    revision: Some(self.revision),
                    effect_intents: Vec::new(),
                    created_ids: BTreeMap::from([("assignment".into(), assignment_id)]),
                    relationships: BTreeMap::new(),
                    review_round: Some(assignment.review_round),
                    assignment_state: Some(AssignmentState::Active),
                    review_limit_reached: false,
                    error: None,
                };
                self.processed_inputs.insert(
                    processed_input_key.clone(),
                    ProcessedReceipt {
                        fingerprint: input_fingerprint.clone(),
                        receipt: receipt.clone(),
                    },
                );
                Ok(receipt)
            }
            HandleInput::RoleLaunchRequest {
                launch_id,
                role,
                generation,
                launch_owner,
                acquired_at,
                expires_at,
                attach_mode,
            } => {
                let role_key = role_identity(&role)?;
                if launch_owner.trim().is_empty() {
                    return Ok(rejected_receipt(
                        launch_id,
                        "missing_launch_owner",
                        "role launch requires a non-empty launch owner",
                    ));
                }
                if acquired_at < 0 || expires_at <= acquired_at {
                    return Ok(rejected_receipt(
                        launch_id,
                        "invalid_launch_lease_window",
                        "role launch lease expiry must be later than acquisition",
                    ));
                }
                if request
                    .decision_context
                    .generations
                    .get(&role_key)
                    .is_some_and(|expected| *expected != generation)
                {
                    return Ok(rejected_receipt(
                        launch_id,
                        "generation_context_mismatch",
                        "role launch generation differs from decision context",
                    ));
                }
                if self
                    .role_generations
                    .get(&role_key)
                    .is_some_and(|current| generation != *current)
                    && !self
                        .role_states
                        .get(&role_key)
                        .is_some_and(|(_, state, _)| {
                            matches!(state, RoleState::Stopped | RoleState::Failed)
                        })
                {
                    return Ok(rejected_receipt(
                        launch_id,
                        "stale_generation",
                        "role launch cannot overwrite an authoritative generation",
                    ));
                }
                self.role_generations
                    .insert(role_key.clone(), generation.clone());
                self.role_states.insert(
                    role_key.clone(),
                    (
                        generation.clone(),
                        RoleState::Starting,
                        request.decision_context.observed_at.clone(),
                    ),
                );
                self.revision += 1;
                let receipt = HandleReceipt {
                    input_id: launch_id.clone(),
                    disposition: HandleDisposition::Applied,
                    revision: Some(self.revision),
                    effect_intents: vec![EffectIntent {
                        effect_id: launch_id,
                        generation,
                        intent: EffectIntentKind::EnsureRoleReady { role, attach_mode },
                    }],
                    created_ids: BTreeMap::new(),
                    relationships: BTreeMap::from([("role".into(), role_key)]),
                    review_round: None,
                    assignment_state: None,
                    review_limit_reached: false,
                    error: None,
                };
                self.processed_inputs.insert(
                    processed_input_key.clone(),
                    ProcessedReceipt {
                        fingerprint: input_fingerprint.clone(),
                        receipt: receipt.clone(),
                    },
                );
                Ok(receipt)
            }
            HandleInput::RoleObservation {
                observation_id,
                role,
                generation,
                state,
                ..
            } => {
                let role_key = role_identity(&role)?;
                if request
                    .decision_context
                    .generations
                    .get(&role_key)
                    .is_some_and(|expected| *expected != generation)
                {
                    return Ok(rejected_receipt(
                        observation_id,
                        "generation_context_mismatch",
                        "role observation generation differs from decision context",
                    ));
                }
                if self
                    .role_generations
                    .get(&role_key)
                    .is_some_and(|current| generation != *current)
                {
                    return Ok(rejected_receipt(
                        observation_id,
                        "stale_generation",
                        "role observation cannot overwrite a newer generation",
                    ));
                }
                self.role_generations
                    .insert(role_key.clone(), generation.clone());
                self.role_states.insert(
                    role_key.clone(),
                    (
                        generation.clone(),
                        state,
                        request.decision_context.observed_at.clone(),
                    ),
                );
                self.revision += 1;
                let receipt = HandleReceipt {
                    input_id: observation_id,
                    disposition: HandleDisposition::Applied,
                    revision: Some(self.revision),
                    effect_intents: Vec::new(),
                    created_ids: BTreeMap::new(),
                    relationships: BTreeMap::from([("role".into(), role_key)]),
                    review_round: None,
                    assignment_state: None,
                    review_limit_reached: false,
                    error: None,
                };
                self.processed_inputs.insert(
                    processed_input_key.clone(),
                    ProcessedReceipt {
                        fingerprint: input_fingerprint.clone(),
                        receipt: receipt.clone(),
                    },
                );
                Ok(receipt)
            }
            HandleInput::TeamEvent {
                event_id,
                sequence,
                name,
                body,
            } if name == "assignment_settled" => {
                let Some(role_name) = body.get("role").and_then(Value::as_str) else {
                    return Ok(rejected_receipt(
                        event_id,
                        "missing_role",
                        "settled recovery requires body.role",
                    ));
                };
                let role = role_from_identity(role_name)?;
                let Some(expected_assignment_id) =
                    body.get("expected_assignment_id").and_then(Value::as_str)
                else {
                    return Ok(rejected_receipt(
                        event_id,
                        "missing_assignment_id",
                        "settled recovery requires body.expected_assignment_id",
                    ));
                };
                let Some(assignment) = self.assignments.get(expected_assignment_id).cloned() else {
                    return Ok(rejected_receipt(
                        event_id,
                        "assignment_not_found",
                        "settled recovery assignment does not exist",
                    ));
                };
                if assignment.target != role {
                    return Ok(rejected_receipt(
                        event_id,
                        "assignment_owner_mismatch",
                        "settled role does not own the expected assignment",
                    ));
                }
                if assignment.state != AssignmentState::Active {
                    return Ok(rejected_receipt(
                        event_id,
                        "assignment_not_active",
                        "settled recovery only resumes active work",
                    ));
                }
                let safe_to_resume = body
                    .get("safe_to_resume")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let recovered_state = if safe_to_resume {
                    AssignmentState::Active
                } else {
                    AssignmentState::Blocked
                };

                let effect_intents = if safe_to_resume {
                    let role_key = role_identity(&role)?;
                    let recovery_generation = request
                        .decision_context
                        .generations
                        .get(&role_key)
                        .cloned()
                        .ok_or_else(|| missing_decision_value("generation", &role_key))?;
                    if self.generation_is_stale(&role_key, &recovery_generation) {
                        return Ok(rejected_receipt(
                            event_id,
                            "stale_generation",
                            "settled recovery generation is older than the current role generation",
                        ));
                    }
                    let Some((effect_id, effect)) =
                        self.effects.iter().find_map(|(effect_id, effect)| {
                            (effect.assignment_id.as_deref() == Some(expected_assignment_id))
                                .then(|| (effect_id.clone(), effect.clone()))
                        })
                    else {
                        return Ok(rejected_receipt(
                            event_id,
                            "effect_not_found",
                            "settled recovery requires the original delivery effect",
                        ));
                    };
                    if recovery_generation == effect.generation {
                        return Ok(rejected_receipt(
                            event_id,
                            "stale_generation",
                            "settled recovery requires a newer role generation",
                        ));
                    }
                    self.role_generations
                        .insert(role_key, recovery_generation.clone());
                    self.effects
                        .get_mut(&effect_id)
                        .expect("recovery effect existence was validated above")
                        .generation = recovery_generation.clone();
                    vec![EffectIntent {
                        effect_id,
                        generation: recovery_generation,
                        intent: EffectIntentKind::DeliverPrompt {
                            role: effect.role,
                            assignment_id: Some(expected_assignment_id.to_owned()),
                            prompt: effect.prompt,
                        },
                    }]
                } else {
                    Vec::new()
                };
                self.assignments
                    .get_mut(expected_assignment_id)
                    .expect("assignment existence was validated above")
                    .state = recovered_state;
                self.assignments
                    .get_mut(expected_assignment_id)
                    .expect("assignment existence was validated above")
                    .observed_at = request.decision_context.observed_at.clone();
                self.revision += 1;
                let receipt = HandleReceipt {
                    input_id: event_id,
                    disposition: HandleDisposition::Applied,
                    revision: Some(self.revision),
                    effect_intents,
                    created_ids: BTreeMap::from([(
                        "assignment".into(),
                        expected_assignment_id.to_owned(),
                    )]),
                    relationships: BTreeMap::new(),
                    review_round: Some(assignment.review_round),
                    assignment_state: Some(recovered_state),
                    review_limit_reached: false,
                    error: None,
                };
                let (state_change_key, state_change_fingerprint) = state_change_identity
                    .expect("TeamEvent input must define a state-change identity");
                debug_assert_eq!(state_change_key.0, name);
                debug_assert_eq!(state_change_key.2, sequence);
                self.processed_state_sequences.insert(
                    state_change_key,
                    ProcessedReceipt {
                        fingerprint: state_change_fingerprint,
                        receipt: receipt.clone(),
                    },
                );
                self.processed_inputs.insert(
                    processed_input_key,
                    ProcessedReceipt {
                        fingerprint: input_fingerprint,
                        receipt: receipt.clone(),
                    },
                );
                Ok(receipt)
            }
            HandleInput::ToolJobRequest { request: tool_job } => self.apply_tool_job_request(
                tool_job,
                request.decision_context.observed_at,
                processed_input_key,
                input_fingerprint,
            ),
            HandleInput::ToolJobTransition {
                transition_id,
                job_id,
                owner,
                transition,
            } => self.apply_tool_job_transition(
                transition_id,
                job_id,
                owner,
                transition,
                request.decision_context.observed_at,
                processed_input_key,
                input_fingerprint,
            ),
            HandleInput::Command {
                command_id,
                source,
                target,
                ..
            } if source.role == RoleKind::Pm && target.is_some() => Ok(rejected_receipt(
                command_id,
                "invalid_assignment_kind",
                "Worker assignments must use task or fix",
            )),
            HandleInput::Command { command_id, .. } => Ok(rejected_receipt(
                command_id,
                "unsupported_transition",
                "the pure domain scaffold does not support this transition",
            )),
            _ => Err(KernelError {
                category: ErrorCategory::Domain,
                code: "unsupported_input".into(),
                message: "the pure domain scaffold does not support this input".into(),
                retryable: false,
                details: BTreeMap::new(),
            }),
        }
    }

    fn apply_tool_job_request(
        &mut self,
        request: ToolJobRequest,
        observed_at: String,
        processed_input_key: String,
        input_fingerprint: String,
    ) -> Result<HandleReceipt, KernelError> {
        let request_fingerprint = tool_job_request_fingerprint(&request)?;
        if let Some(existing) = self.tool_jobs.get(&request.job_id) {
            if existing.request_fingerprint != request_fingerprint {
                return Ok(rejected_receipt(
                    request.job_id,
                    "input_id_conflict",
                    "Tool Job ID was already committed with different semantics",
                ));
            }
            return Ok(HandleReceipt {
                input_id: request.job_id.clone(),
                disposition: HandleDisposition::Duplicate,
                revision: Some(self.revision),
                effect_intents: Vec::new(),
                created_ids: BTreeMap::from([("tool_job".into(), request.job_id)]),
                relationships: BTreeMap::from([(
                    "assignment".into(),
                    existing.request.assignment_id.clone(),
                )]),
                review_round: None,
                assignment_state: None,
                review_limit_reached: false,
                error: None,
            });
        }

        let assignment = self.assignments.get(&request.assignment_id);
        let owns_assignment = request.source.role == RoleKind::Worker
            && assignment.is_some_and(|assignment| {
                assignment.target == request.source
                    && matches!(
                        assignment.state,
                        AssignmentState::Queued
                            | AssignmentState::Active
                            | AssignmentState::Blocked
                    )
            });
        if !owns_assignment {
            return Ok(rejected_receipt(
                request.job_id,
                "tool_job_owner_mismatch",
                "Tool Job must be owned by the authoritative Worker Assignment",
            ));
        }

        let job_id = request.job_id.clone();
        let assignment_id = request.assignment_id.clone();
        self.tool_jobs.insert(
            job_id.clone(),
            ToolJob {
                request,
                request_fingerprint,
                state: ToolJobState::Queued,
                pane_id: String::new(),
                coordination_dir: String::new(),
                request_path: String::new(),
                stdout_path: String::new(),
                stderr_path: String::new(),
                result_path: String::new(),
                output: None,
                created_at: observed_at.clone(),
                started_at: None,
                finished_at: None,
                cancelled_at: None,
                updated_at: observed_at,
            },
        );
        self.revision += 1;
        let receipt = HandleReceipt {
            input_id: job_id.clone(),
            disposition: HandleDisposition::Applied,
            revision: Some(self.revision),
            effect_intents: Vec::new(),
            created_ids: BTreeMap::from([("tool_job".into(), job_id)]),
            relationships: BTreeMap::from([("assignment".into(), assignment_id)]),
            review_round: None,
            assignment_state: None,
            review_limit_reached: false,
            error: None,
        };
        self.processed_inputs.insert(
            processed_input_key,
            ProcessedReceipt {
                fingerprint: input_fingerprint,
                receipt: receipt.clone(),
            },
        );
        Ok(receipt)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_tool_job_transition(
        &mut self,
        transition_id: String,
        job_id: String,
        owner: crate::RoleRef,
        transition: ToolJobTransition,
        observed_at: String,
        processed_input_key: String,
        input_fingerprint: String,
    ) -> Result<HandleReceipt, KernelError> {
        let Some(existing) = self.tool_jobs.get(&job_id) else {
            return Ok(rejected_receipt(
                transition_id,
                "tool_job_not_found",
                "Tool Job does not exist",
            ));
        };
        let owns_assignment = existing.request.source == owner
            && self
                .assignments
                .get(&existing.request.assignment_id)
                .is_some_and(|assignment| assignment.target == owner);
        if !owns_assignment {
            return Ok(rejected_receipt(
                transition_id,
                "tool_job_owner_mismatch",
                "Tool Job transition owner does not match its authoritative Assignment",
            ));
        }
        let current_state = existing.state;
        let assignment_id = existing.request.assignment_id.clone();
        let job = self
            .tool_jobs
            .get_mut(&job_id)
            .expect("Tool Job existence was validated above");
        match transition {
            ToolJobTransition::Started {
                pane_id,
                coordination_dir,
                request_path,
                stdout_path,
                stderr_path,
                result_path,
            } if current_state == ToolJobState::Queued => {
                job.state = ToolJobState::Running;
                job.pane_id = pane_id;
                job.coordination_dir = coordination_dir;
                job.request_path = request_path;
                job.stdout_path = stdout_path;
                job.stderr_path = stderr_path;
                job.result_path = result_path;
                job.started_at = Some(observed_at.clone());
                job.updated_at = observed_at;
            }
            ToolJobTransition::CancelRequested if current_state == ToolJobState::Queued => {
                job.state = ToolJobState::Cancelled;
                job.cancelled_at = Some(observed_at.clone());
                job.finished_at = Some(observed_at.clone());
                job.updated_at = observed_at;
            }
            ToolJobTransition::CancelRequested if current_state == ToolJobState::Running => {
                job.state = ToolJobState::Cancelling;
                job.cancelled_at = Some(observed_at.clone());
                job.updated_at = observed_at;
            }
            ToolJobTransition::Completed { output }
                if matches!(
                    current_state,
                    ToolJobState::Running | ToolJobState::Cancelling
                ) =>
            {
                job.state = if current_state == ToolJobState::Cancelling {
                    ToolJobState::Cancelled
                } else {
                    output.state.into()
                };
                job.stdout_path = output.stdout_path.clone();
                job.stderr_path = output.stderr_path.clone();
                job.result_path = output.result_path.clone();
                job.output = Some(output);
                job.finished_at = Some(observed_at.clone());
                job.updated_at = observed_at;
            }
            _ => {
                return Ok(rejected_receipt(
                    transition_id,
                    "invalid_tool_job_transition",
                    "Tool Job state does not permit this transition",
                ));
            }
        }

        self.revision += 1;
        let receipt = HandleReceipt {
            input_id: transition_id,
            disposition: HandleDisposition::Applied,
            revision: Some(self.revision),
            effect_intents: Vec::new(),
            created_ids: BTreeMap::from([("tool_job".into(), job_id)]),
            relationships: BTreeMap::from([("assignment".into(), assignment_id)]),
            review_round: None,
            assignment_state: None,
            review_limit_reached: false,
            error: None,
        };
        self.processed_inputs.insert(
            processed_input_key,
            ProcessedReceipt {
                fingerprint: input_fingerprint,
                receipt: receipt.clone(),
            },
        );
        Ok(receipt)
    }

    fn role_capacity_held(&self, target: &crate::RoleRef) -> bool {
        if target.role == RoleKind::Scout {
            return false;
        }
        self.assignments.values().any(|assignment| {
            assignment.target.role == target.role
                && matches!(
                    assignment.state,
                    AssignmentState::Queued | AssignmentState::Active
                )
        })
    }

    /// True when the target role already has an *active* assignment (one that
    /// has been delivered and is being worked). A merely `queued` assignment
    /// does not block a follow-up: it is waiting for delivery, so a second
    /// queued follow-up may line up behind it without being rejected.
    fn role_active_capacity_held(&self, target: &crate::RoleRef) -> bool {
        if target.role == RoleKind::Scout {
            return false;
        }
        self.assignments.values().any(|assignment| {
            assignment.target.role == target.role
                && assignment.state == AssignmentState::Active
        })
    }

    fn generation_is_stale(&self, role_identity: &str, generation: &Generation) -> bool {
        self.role_generations
            .get(role_identity)
            .is_some_and(|current| generation != current)
    }

    fn ensure_id_available(&self, kind: &str, id: &str) -> Result<(), KernelError> {
        let exists = match kind {
            "assignment" | "follow_up_assignment" => self.assignments.contains_key(id),
            "message" | "follow_up_message" => self.messages.contains_key(id),
            "outbox" | "follow_up_outbox" => {
                let processed_effect_prefix = format!("effect:{id}:");
                self.effects.contains_key(id)
                    || self
                        .processed_inputs
                        .keys()
                        .any(|key| key.starts_with(&processed_effect_prefix))
            }
            "review_revision" => self.review_revisions.contains(id),
            "tool_job" => self.tool_jobs.contains_key(id),
            _ => false,
        };
        if exists {
            return Err(allocated_id_conflict(kind, id));
        }
        Ok(())
    }

    fn ensure_ids_available(&self, ids: &[(&str, &str)]) -> Result<(), KernelError> {
        let mut seen = BTreeSet::new();
        for (kind, id) in ids {
            self.ensure_id_available(kind, id)?;
            let namespace = if kind.ends_with("assignment") {
                "assignment"
            } else if kind.ends_with("message") {
                "message"
            } else if kind.ends_with("outbox") {
                "outbox"
            } else {
                kind
            };
            if !seen.insert((namespace, *id)) {
                return Err(KernelError {
                    category: ErrorCategory::Contract,
                    code: "allocated_id_conflict".into(),
                    message: "allocated ID is reused within the same command".into(),
                    retryable: false,
                    details: BTreeMap::from([
                        ("kind".into(), json!(kind)),
                        ("id".into(), json!(id)),
                    ]),
                });
            }
        }
        Ok(())
    }
}

fn allocated_id_conflict(kind: &str, id: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Contract,
        code: "allocated_id_conflict".into(),
        message: "allocated ID already names an input or coordination object".into(),
        retryable: false,
        details: BTreeMap::from([("kind".into(), json!(kind)), ("id".into(), json!(id))]),
    }
}

pub(crate) fn rejected_receipt(input_id: String, code: &str, message: &str) -> HandleReceipt {
    HandleReceipt {
        input_id,
        disposition: HandleDisposition::Rejected,
        revision: None,
        effect_intents: Vec::new(),
        created_ids: BTreeMap::new(),
        relationships: BTreeMap::new(),
        review_round: None,
        assignment_state: None,
        review_limit_reached: false,
        error: Some(KernelError {
            category: ErrorCategory::Domain,
            code: code.into(),
            message: message.into(),
            retryable: false,
            details: BTreeMap::new(),
        }),
    }
}

fn send_allowed(source: RoleKind, target: RoleKind) -> bool {
    match source {
        RoleKind::Pm => matches!(
            target,
            RoleKind::Worker | RoleKind::Scout | RoleKind::Reviewer
        ),
        RoleKind::Worker | RoleKind::Scout | RoleKind::Reviewer => target == RoleKind::Pm,
    }
}

fn validate_input_roles(input: &HandleInput) -> Result<(), KernelError> {
    match input {
        HandleInput::Command { source, target, .. } => {
            role_identity(source)?;
            if let Some(target) = target {
                role_identity(target)?;
            }
        }
        HandleInput::RoleObservation { role, .. } => {
            role_identity(role)?;
        }
        HandleInput::RoleLaunchRequest { role, .. } => {
            role_identity(role)?;
        }
        HandleInput::ToolJobRequest { request } => {
            role_identity(&request.source)?;
        }
        HandleInput::ToolJobTransition { owner, .. } => {
            role_identity(owner)?;
        }
        HandleInput::TeamEvent { .. } | HandleInput::EffectResult { .. } => {}
    }
    Ok(())
}

fn validate_observed_at(value: &str) -> Result<(), KernelError> {
    let bytes = value.as_bytes();
    let fixed_shape = bytes.len() >= 20
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes
            .get(10)
            .is_some_and(|byte| matches!(byte, b'T' | b't'))
        && bytes.get(13) == Some(&b':')
        && bytes.get(16) == Some(&b':')
        && [0..4, 5..7, 8..10, 11..13, 14..16, 17..19]
            .into_iter()
            .all(|range| bytes[range].iter().all(u8::is_ascii_digit));
    let calendar_values_valid = fixed_shape
        && value[0..4]
            .parse::<u32>()
            .ok()
            .zip(value[5..7].parse::<u8>().ok())
            .zip(value[8..10].parse::<u8>().ok())
            .is_some_and(|((year, month), day)| {
                (1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day)
            })
        && value[11..13].parse::<u8>().is_ok_and(|hour| hour <= 23)
        && value[14..16].parse::<u8>().is_ok_and(|minute| minute <= 59)
        && value[17..19].parse::<u8>().is_ok_and(|second| second <= 59);
    let timezone_valid = bytes.get(19..).is_some_and(rfc3339_suffix_is_valid);
    if calendar_values_valid && timezone_valid {
        return Ok(());
    }
    Err(KernelError {
        category: ErrorCategory::Contract,
        code: "invalid_observed_at".into(),
        message: "decision context observed_at must be an RFC 3339 timestamp".into(),
        retryable: false,
        details: BTreeMap::from([("observed_at".into(), json!(value))]),
    })
}

fn days_in_month(year: u32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        _ => 0,
    }
}

fn rfc3339_suffix_is_valid(suffix: &[u8]) -> bool {
    let timezone = match suffix {
        [b'Z' | b'z'] => return true,
        [b'.', rest @ ..] => {
            let timezone_start = rest
                .iter()
                .position(|byte| matches!(byte, b'Z' | b'z' | b'+' | b'-'));
            let Some(timezone_start) = timezone_start else {
                return false;
            };
            if timezone_start == 0 || !rest[..timezone_start].iter().all(u8::is_ascii_digit) {
                return false;
            }
            &rest[timezone_start..]
        }
        timezone => timezone,
    };
    if matches!(timezone, [b'Z' | b'z']) {
        return true;
    }
    timezone.len() == 6
        && matches!(timezone[0], b'+' | b'-')
        && timezone[1..3].iter().all(u8::is_ascii_digit)
        && timezone[3] == b':'
        && timezone[4..6].iter().all(u8::is_ascii_digit)
        && std::str::from_utf8(&timezone[1..3])
            .ok()
            .and_then(|hour| hour.parse::<u8>().ok())
            .is_some_and(|hour| hour <= 23)
        && std::str::from_utf8(&timezone[4..6])
            .ok()
            .and_then(|minute| minute.parse::<u8>().ok())
            .is_some_and(|minute| minute <= 59)
}

fn assignment_kind_allowed(target: RoleKind, kind: &str) -> bool {
    match target {
        RoleKind::Worker => matches!(kind, "task" | "fix"),
        RoleKind::Scout => kind == "task",
        RoleKind::Reviewer => kind == "review",
        RoleKind::Pm => false,
    }
}

fn reply_kind_allowed(source: RoleKind, assignment_kind: &str, reply_kind: &str) -> bool {
    match source {
        RoleKind::Worker => {
            matches!(assignment_kind, "task" | "fix")
                && matches!(reply_kind, "completed" | "blocked")
        }
        RoleKind::Scout => assignment_kind == "task" && matches!(reply_kind, "finding" | "blocked"),
        RoleKind::Reviewer => {
            assignment_kind == "review" && matches!(reply_kind, "approved" | "rejected" | "blocked")
        }
        RoleKind::Pm => false,
    }
}

fn role_identity(role: &crate::RoleRef) -> Result<String, KernelError> {
    match role.role {
        RoleKind::Pm if role.instance.is_none() => Ok("pm".into()),
        RoleKind::Worker if role.instance.is_none() => Ok("worker".into()),
        RoleKind::Reviewer if role.instance.is_none() => Ok("reviewer".into()),
        RoleKind::Scout => match &role.instance {
            // Canonical single-slot Scout (the default Team layout).
            None => Ok("scout".into()),
            Some(instance) if instance.starts_with("scout-") && instance.len() > "scout-".len() => {
                Ok(instance.clone())
            }
            Some(_) => Err(invalid_role_identity(role)),
        },
        _ => Err(invalid_role_identity(role)),
    }
}

fn invalid_role_identity(role: &crate::RoleRef) -> KernelError {
    KernelError {
        category: ErrorCategory::Contract,
        code: "invalid_role_identity".into(),
        message: "role reference does not use the canonical Team Mission identity".into(),
        retryable: false,
        details: BTreeMap::from([
            ("role".into(), json!(role.role)),
            ("instance".into(), json!(role.instance)),
        ]),
    }
}

fn role_from_identity(identity: &str) -> Result<crate::RoleRef, KernelError> {
    match identity {
        "pm" => Ok(crate::RoleRef {
            role: RoleKind::Pm,
            instance: None,
        }),
        "worker" => Ok(crate::RoleRef {
            role: RoleKind::Worker,
            instance: None,
        }),
        "reviewer" => Ok(crate::RoleRef {
            role: RoleKind::Reviewer,
            instance: None,
        }),
        "scout" => Ok(crate::RoleRef {
            role: RoleKind::Scout,
            instance: None,
        }),
        value if value.starts_with("scout-") => Ok(crate::RoleRef {
            role: RoleKind::Scout,
            instance: Some(value.into()),
        }),
        _ => Err(KernelError {
            category: ErrorCategory::Contract,
            code: "invalid_role_identity".into(),
            message: "role identity is not part of the Team Mission ACL".into(),
            retryable: false,
            details: BTreeMap::from([("role".into(), json!(identity))]),
        }),
    }
}

fn required_id(ids: &BTreeMap<String, String>, key: &str) -> Result<String, KernelError> {
    let value = ids
        .get(key)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| missing_decision_value("allocated_id", key))?;
    let expected_prefix = if key.ends_with("assignment") {
        "asg-"
    } else if key.ends_with("message") {
        "msg-"
    } else if key.ends_with("outbox") {
        "out-"
    } else if key == "review_revision" {
        "rev-"
    } else {
        return Err(missing_decision_value("allocated_id_kind", key));
    };
    if !value.starts_with(expected_prefix) || value.len() == expected_prefix.len() {
        return Err(KernelError {
            category: ErrorCategory::Contract,
            code: "invalid_allocated_id".into(),
            message: "allocated ID does not match its stable identity kind".into(),
            retryable: false,
            details: BTreeMap::from([
                ("kind".into(), json!(key)),
                ("id".into(), json!(value)),
                ("expected_prefix".into(), json!(expected_prefix)),
            ]),
        });
    }
    Ok(value)
}

fn semantic_fingerprint<T: serde::Serialize>(value: &T) -> Result<String, KernelError> {
    serde_json::to_string(value).map_err(|error| KernelError {
        category: ErrorCategory::Internal,
        code: "semantic_fingerprint_failed".into(),
        message: "kernel input could not be serialized for idempotency".into(),
        retryable: false,
        details: BTreeMap::from([("reason".into(), json!(error.to_string()))]),
    })
}

pub(crate) fn tool_job_request_fingerprint(
    request: &ToolJobRequest,
) -> Result<String, KernelError> {
    semantic_fingerprint(request)
}

pub(crate) fn processed_input_key(input: &HandleInput) -> String {
    match input {
        HandleInput::Command { command_id, .. } => format!("command:{command_id}"),
        HandleInput::TeamEvent { event_id, .. } => format!("event:{event_id}"),
        HandleInput::RoleLaunchRequest { launch_id, .. } => {
            format!("role-launch:{launch_id}")
        }
        HandleInput::RoleObservation { observation_id, .. } => {
            format!("observation:{observation_id}")
        }
        HandleInput::EffectResult { result } => {
            format!(
                "effect:{}:{}:{}",
                result.effect_id, result.generation, result.claim_owner
            )
        }
        HandleInput::ToolJobRequest { request } => format!("tool-job:{}", request.job_id),
        HandleInput::ToolJobTransition { transition_id, .. } => {
            format!("tool-job-transition:{transition_id}")
        }
    }
}

fn effect_role(intent: &EffectIntentKind) -> Result<&crate::RoleRef, KernelError> {
    match intent {
        EffectIntentKind::EnsureRoleReady { role, .. }
        | EffectIntentKind::ObserveRole { role }
        | EffectIntentKind::DeliverPrompt { role, .. } => Ok(role),
        EffectIntentKind::RefreshMissionMirror | EffectIntentKind::RecordEvidence { .. } => {
            Err(KernelError {
                category: ErrorCategory::Operation,
                code: "effect_role_unavailable".into(),
                message: "current durable Outbox drive requires a role-scoped effect".into(),
                retryable: false,
                details: BTreeMap::new(),
            })
        }
    }
}

fn event_scope(body: &Value) -> String {
    body.get("role")
        .and_then(Value::as_str)
        .unwrap_or("mission")
        .to_owned()
}

fn missing_decision_value(kind: &str, key: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Contract,
        code: "missing_decision_context".into(),
        message: format!("decision context is missing {kind} {key}"),
        retryable: false,
        details: BTreeMap::from([("kind".into(), json!(kind)), ("key".into(), json!(key))]),
    }
}

fn message_projection(message: &Message) -> Value {
    json!({
        "id": message.id,
        "assignment_id": message.assignment_id,
        "source": message.source,
        "target": message.target,
        "kind": message.kind,
        "body": message.body,
        "revision": message.revision,
        "observed_at": message.observed_at,
    })
}

fn assignment_projection(assignment_id: &str, assignment: &Assignment) -> Value {
    json!({
        "id": assignment_id,
        "target": assignment.target,
        "kind": assignment.kind,
        "state": assignment.state,
        "parent_id": assignment.parent_id,
        "review_round": assignment.review_round,
        "observed_at": assignment.observed_at,
    })
}

fn tool_job_projection(job: &ToolJob) -> Value {
    let output = job.output.as_ref();
    json!({
        "job_id": job.request.job_id,
        "assignment_id": job.request.assignment_id,
        "source": job.request.source,
        "mode": job.request.mode,
        "label": job.request.label,
        "argv": job.request.argv,
        "cwd": job.request.cwd,
        "env": job.request.env,
        "timeout_seconds": job.request.timeout_seconds,
        "parallel": job.request.parallel,
        "max_output_bytes": job.request.max_output_bytes,
        "state": job.state.as_str(),
        "pane_id": job.pane_id,
        "coordination_dir": job.coordination_dir,
        "request_path": job.request_path,
        "stdout_path": job.stdout_path,
        "stderr_path": job.stderr_path,
        "result_path": job.result_path,
        "stdout_bytes": output.map_or(0, |output| output.stdout_bytes),
        "stderr_bytes": output.map_or(0, |output| output.stderr_bytes),
        "stdout_truncated": output.is_some_and(|output| output.stdout_truncated),
        "stderr_truncated": output.is_some_and(|output| output.stderr_truncated),
        "stdout_checksum": output.map_or("", |output| output.stdout_checksum.as_str()),
        "stderr_checksum": output.map_or("", |output| output.stderr_checksum.as_str()),
        "exit_code": output.and_then(|output| output.exit_code),
        "error": output.map_or("", |output| output.error.as_str()),
        "created_at": job.created_at,
        "started_at": job.started_at,
        "finished_at": job.finished_at,
        "cancelled_at": job.cancelled_at,
        "updated_at": job.updated_at,
        "terminal": job.state.is_terminal(),
    })
}

fn ledger_projection(entry: &LedgerEntry) -> Value {
    json!({
        "revision": entry.revision,
        "assignment_id": entry.assignment_id,
        "source": entry.source,
        "kind": entry.kind,
        "body": entry.body,
        "observed_at": entry.observed_at,
    })
}
