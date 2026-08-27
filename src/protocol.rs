use std::{collections::BTreeMap, fmt};

use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: &str = "herdr.mission.kernel.v1";
pub const BINARY_CONTRACT: &str = "herdr.mission.kernel.binary.v1";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Generation(String);

impl Generation {
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidGeneration> {
        let value = value.into();
        if value.is_empty() {
            return Err(InvalidGeneration);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Generation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidGeneration;

impl fmt::Display for InvalidGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("generation must be a non-empty opaque string")
    }
}

impl std::error::Error for InvalidGeneration {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelRequest {
    pub protocol: String,
    pub binary_contract: String,
    pub request_id: String,
    pub mission: MissionIdentity,
    pub database: DatabaseAccessIntent,
    pub decision_context: DecisionContext,
    pub operation: Operation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissionIdentity {
    pub mission_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseAccessIntent {
    pub path: String,
    pub access: DatabaseAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionContext {
    pub observed_at: String,
    #[serde(default)]
    pub allocated_ids: BTreeMap<String, String>,
    #[serde(default)]
    pub generations: BTreeMap<String, Generation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "request", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Operation {
    Handle(HandleRequest),
    Drive(DriveRequest),
    Inspect(InspectRequest),
}

impl Operation {
    pub const fn kind(&self) -> OperationKind {
        match self {
            Self::Handle(_) => OperationKind::Handle,
            Self::Drive(_) => OperationKind::Drive,
            Self::Inspect(_) => OperationKind::Inspect,
        }
    }
}

impl OperationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Handle => "handle",
            Self::Drive => "drive",
            Self::Inspect => "inspect",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "handle" => Some(Self::Handle),
            "drive" => Some(Self::Drive),
            "inspect" => Some(Self::Inspect),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Handle,
    Drive,
    Inspect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandleRequest {
    pub input: HandleInput,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelInput {
    pub decision_context: DecisionContext,
    pub input: HandleInput,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HandleInput {
    Command {
        command_id: String,
        kind: String,
        source: RoleRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<RoleRef>,
        body: Value,
    },
    TeamEvent {
        event_id: String,
        sequence: u64,
        name: String,
        body: Value,
    },
    RoleLaunchRequest {
        launch_id: String,
        role: RoleRef,
        generation: Generation,
        launch_owner: String,
        acquired_at: i64,
        expires_at: i64,
        attach_mode: RoleAttachMode,
    },
    RoleObservation {
        observation_id: String,
        role: RoleRef,
        generation: Generation,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_owner: Option<String>,
        state: RoleState,
        #[serde(default)]
        details: Value,
    },
    EffectResult {
        result: EffectResult,
    },
    ToolJobRequest {
        request: ToolJobRequest,
    },
    ToolJobTransition {
        transition_id: String,
        job_id: String,
        owner: RoleRef,
        transition: ToolJobTransition,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolJobRequest {
    pub job_id: String,
    pub assignment_id: String,
    pub source: RoleRef,
    pub mode: ToolJobMode,
    pub label: String,
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub timeout_seconds: f64,
    #[serde(default)]
    pub parallel: bool,
    pub max_output_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolJobMode {
    Bounded,
    Persistent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolJobTransition {
    Started {
        pane_id: String,
        coordination_dir: String,
        request_path: String,
        stdout_path: String,
        stderr_path: String,
        result_path: String,
    },
    CancelRequested,
    Completed {
        output: ToolJobOutputMetadata,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolJobOutputMetadata {
    pub state: ToolJobTerminalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub stdout_path: String,
    pub stderr_path: String,
    pub result_path: String,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub stdout_checksum: String,
    pub stderr_checksum: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolJobTerminalState {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleRef {
    pub role: RoleKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleKind {
    Pm,
    Worker,
    Scout,
    Reviewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleState {
    Pending,
    Starting,
    Ready,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleAttachMode {
    Managed,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriveRequest {
    pub runtime_owner: RuntimeOwner,
    pub effect_budget: u32,
    pub time_budget_ms: u64,
    #[serde(default)]
    pub execution_mode: DriveExecutionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_owner: Option<String>,
    #[serde(default)]
    pub claimed_at_ms: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveExecutionMode {
    #[default]
    Deferred,
    Recording,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOwner {
    Python,
    Shadow,
    RustRead,
    Rust,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub query: InspectQuery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InspectQuery {
    Mission,
    Status,
    Inbox {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<RoleRef>,
    },
    AssignmentThread {
        assignment_id: String,
    },
    Diagnostics,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandleReceipt {
    pub input_id: String,
    pub disposition: HandleDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default)]
    pub effect_intents: Vec<EffectIntent>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub created_ids: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub relationships: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_round: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_state: Option<AssignmentState>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub review_limit_reached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<KernelError>,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentState {
    Queued,
    Active,
    Completed,
    Approved,
    Rejected,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandleDisposition {
    Applied,
    Duplicate,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriveReport {
    pub claimed: u32,
    pub resolved: u32,
    pub pending: u32,
    pub retryable_failures: u32,
    pub terminal_failures: u32,
    #[serde(default)]
    pub effect_results: Vec<EffectResult>,
    #[serde(default)]
    pub claimed_effects: Vec<ClaimedEffect>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimedEffect {
    pub intent: EffectIntent,
    pub claim_owner: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissionView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectIntent {
    pub effect_id: String,
    pub generation: Generation,
    pub intent: EffectIntentKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectIntentKind {
    EnsureRoleReady {
        role: RoleRef,
        attach_mode: RoleAttachMode,
    },
    ObserveRole {
        role: RoleRef,
    },
    DeliverPrompt {
        role: RoleRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignment_id: Option<String>,
        prompt: String,
    },
    RefreshMissionMirror,
    RecordEvidence {
        kind: String,
        payload: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectResult {
    pub effect_id: String,
    pub generation: Generation,
    pub claim_owner: String,
    pub outcome: EffectOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectOutcome {
    Succeeded {
        #[serde(default)]
        observation: Value,
    },
    Pending {
        reason: String,
    },
    RetryableFailure {
        error: KernelError,
        retry_after_ms: u64,
    },
    TerminalFailure {
        error: KernelError,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelError {
    pub category: ErrorCategory,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default)]
    pub details: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Transport,
    Protocol,
    Contract,
    Operation,
    Domain,
    Infrastructure,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelOutcome {
    pub protocol: String,
    pub binary_contract: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub outcome: OutcomeBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OutcomeBody {
    Success {
        operation: OperationKind,
        result: OperationResult,
    },
    Error {
        error: KernelError,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum OperationResult {
    Handle(HandleReceipt),
    Drive(DriveReport),
    Inspect(MissionView),
}

pub fn parse_request(value: Value) -> Result<KernelRequest, serde_json::Error> {
    serde_json::from_value(value)
}
