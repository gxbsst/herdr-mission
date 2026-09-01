//! Standalone Herdr Mission runtime kernel scaffold.

mod bootstrap;
pub mod cli;
mod config;
mod coordination;
mod creation;
mod daemon;
mod effects;
mod herdr;
mod installer;
mod keybinding;
mod log;
mod manifest;
mod peer;
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
    LaunchConfig, LaunchMode, LaunchSection, TabMode, TabsSection, ToolsSection,
    REVIEW_REGION_NAME, VERIFICATION_REGION_NAME, WORK_REGION_NAME,
};
pub use coordination::{
    kernel_deliver, kernel_dispatch_command, kernel_read_context, kernel_reconcile,
    kernel_reconcile_with_peer, kernel_reconcile_with_peer_transport, kernel_reply_command,
    read_role_context, DeliveryReport, InboxMessage, KernelDispatchOutcome, KernelReplyOutcome,
    PeerReconcileReport, PendingAssignment, ReconcileReport, RoleContext,
};
pub use creation::{
    agent_name_token, create_mission, default_codex_team, delete_mission, is_valid_role_identity,
    list_missions, make_mission_id, parse_role_ref, pi_quality_team, read_mission_launch_mode,
    read_mission_overviews, read_mission_state, read_mission_status, read_mission_title,
    read_role_runtime, reconcile_role_healths, record_role_pane, record_role_runtime,
    resolve_mission_id, resolve_roles, role_kind, set_mission_launch_mode, set_mission_stage,
    slugify, utc_timestamp, CreateMissionOutcome, CreateMissionRequest, DeleteMissionOutcome,
    MissionLayout, MissionOverview, MissionStatus, MissionSummary, Provider, RoleConfig,
    RoleHealthReconciliation, RoleOverride, RoleOverview, RoleRuntimeRow, DEFAULT_AGENT_PROFILE_ID,
    DEFAULT_AGENT_PROFILE_VERSION, PI_QUALITY_PROFILE_ID, TEAM_ROLES,
};
pub use daemon::{request_stop, run_daemon, DaemonLock};
pub use domain::{EffectExecutor, MissionKernel};
pub use effects::{
    agent_start_args, herdr_bin, launch_argv, resume_argv, source_cwd, AgentAdapter,
    AgentProviderAdapter, HerdrProcessAdapter, ProcessOutput, ProcessRunner, RoleRuntimeConfig,
    SystemProcessRunner,
};
pub use herdr::{
    agent_list_argv, agent_rename_argv, pane_get_argv, pane_list_argv, pane_move_to_tab_argv,
    pane_rename_argv, pane_run_argv, pane_split_argv, pane_split_in_argv, parse_agent_list,
    parse_pane_get, parse_pane_list, parse_pane_split, parse_tab_create, parse_tab_get,
    parse_tab_list, parse_workspace_create, parse_worktree_create, tab_create_argv, tab_get_argv,
    tab_list_argv, tab_rename_argv, workspace_close_argv, workspace_create_argv,
    worktree_create_argv, worktree_open_argv, AgentSnapshot, AgentStatus, PaneCreated, PaneInfo,
    PaneLocation, TabCreated, TabInfo, WorkspaceCreated, WorktreeCreated,
};
pub use log::{log_error, log_event, log_mission_error};
pub use manifest::{
    compute_manifest, install_atomic, manifest_path_for, read_manifest, sha256_hex, verify_binary,
    write_manifest, ReleaseManifest,
};
pub use peer::{
    acknowledge_peer_message, configure_local_peer, deliver_peer_messages_with,
    new_peer_message_id, notify_peer_inboxes, queue_peer_message, read_peer_inbox,
    receive_peer_envelope, reconcile_peer_relay, upsert_peer, upsert_peer_route, PeerEnvelopeV1,
    PeerInboxMessage, PeerPayloadV1, PeerReceipt, PeerRelayReport, PeerSendOutcome,
    PeerSendRequest, PeerTransport, SystemSshPeerTransport, MAX_PEER_ENVELOPE_BYTES,
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
