//! Standalone Herdr Mission runtime kernel scaffold.

mod bootstrap;
pub mod cli;
mod config;
mod coordination;
mod creation;
mod daemon;
mod effects;
mod herdr;
mod log;
mod manifest;
mod prompt;
mod runtime;

#[cfg_attr(not(test), allow(dead_code))]
mod adapters;
// The temporary schema-v3 adapter is exercised through in-module fixture tests until the
// standalone kernel exposes its mutation bootstrap in a later OpenSpec task.
#[cfg_attr(not(test), allow(dead_code))]
mod domain;
mod protocol;
mod scaffold_cli;
#[cfg_attr(not(test), allow(dead_code))]
mod store;
mod tui;
mod workspace;

pub use bootstrap::{
    bootstrap_database, bump_generation, open_writable, read_generation, BootstrapOutcome,
    KEY_GENERATION, KEY_OWNER, KEY_SCHEMA_VERSION, OWNER_IDENTITY, SCHEMA_VERSION,
};
pub use config::{
    LaunchConfig, LaunchMode, LaunchSection, TabMode, TabsNames, TabsSection, ToolsSection,
};
pub use coordination::{
    kernel_deliver, kernel_dispatch_command, kernel_read_context, kernel_reply_command,
    read_role_context, DeliveryReport, InboxMessage, KernelDispatchOutcome, KernelReplyOutcome,
    PendingAssignment, RoleContext,
};
pub use creation::{
    agent_name_token, create_mission, default_codex_team, delete_mission, is_valid_role_identity,
    list_missions, make_mission_id, parse_role_ref, pi_quality_team, read_mission_overviews,
    read_mission_state, read_mission_status, read_mission_title, read_role_runtime,
    record_role_runtime, resolve_mission_id, resolve_roles, role_kind, set_mission_stage, slugify,
    utc_timestamp, CreateMissionOutcome, CreateMissionRequest, DeleteMissionOutcome, MissionLayout,
    MissionOverview, MissionStatus, MissionSummary, Provider, RoleConfig, RoleOverride,
    RoleOverview, RoleRuntimeRow, DEFAULT_AGENT_PROFILE_ID, DEFAULT_AGENT_PROFILE_VERSION,
    PI_QUALITY_PROFILE_ID, TEAM_ROLES,
};
pub use daemon::{request_stop, run_daemon, DaemonLock};
pub use domain::{EffectExecutor, MissionKernel};
pub use effects::{
    agent_start_args, herdr_bin, launch_argv, resume_argv, source_cwd, AgentAdapter,
    AgentProviderAdapter, HerdrProcessAdapter, ProcessOutput, ProcessRunner, RoleRuntimeConfig,
    SystemProcessRunner,
};
pub use herdr::{
    agent_rename_argv, pane_rename_argv, pane_run_argv, pane_split_argv, pane_split_in_argv,
    parse_pane_split, parse_tab_create, parse_workspace_create, parse_worktree_create,
    tab_create_argv, workspace_close_argv, workspace_create_argv, worktree_create_argv,
    worktree_open_argv, PaneCreated, TabCreated, WorkspaceCreated, WorktreeCreated,
};
pub use log::{log_error, log_event, log_mission_error};
pub use manifest::{
    compute_manifest, install_atomic, manifest_path_for, read_manifest, sha256_hex, verify_binary,
    write_manifest, ReleaseManifest,
};
pub use prompt::role_init_prompt;
pub use protocol::{
    parse_request, AssignmentState, ClaimedEffect, DatabaseAccess, DatabaseAccessIntent,
    DecisionContext, DriveExecutionMode, DriveReport, DriveRequest, EffectIntent, EffectIntentKind,
    EffectOutcome, EffectResult, ErrorCategory, Generation, HandleDisposition, HandleInput,
    HandleReceipt, HandleRequest, InspectQuery, InspectRequest, InvalidGeneration, KernelError,
    KernelInput, KernelOutcome, KernelRequest, MissionIdentity, MissionView, Operation,
    OperationKind, OperationResult, OutcomeBody, RoleAttachMode, RoleKind, RoleRef, RoleState,
    RuntimeOwner, ToolJobMode, ToolJobOutputMetadata, ToolJobRequest, ToolJobTerminalState,
    ToolJobTransition, BINARY_CONTRACT, PROTOCOL_VERSION,
};
pub use runtime::{launch_mission, start_role, LaunchOptions, LaunchOutcome, LaunchedRole};
pub use scaffold_cli::{
    process_fixture_request, process_read_only_canary_request, process_temporary_fixture_request,
    CliResponse,
};
pub use tui::run_tui;
pub use workspace::{
    git_head, git_root, primary_worktree, read_workspace, run_git, upsert_workspace,
    workspace_label, MissionWorkspace, WorkspaceSource,
};
