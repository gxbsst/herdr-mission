use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde_json::{json, Value};

use crate::domain::{
    processed_input_key, rejected_receipt, tool_job_request_fingerprint, MemoryCoordinationStore,
};
use crate::{
    AssignmentState, EffectIntent, EffectIntentKind, EffectOutcome, ErrorCategory,
    HandleDisposition, HandleInput, HandleReceipt, InspectQuery, KernelError, KernelInput,
    MissionView, RoleKind, RoleRef, RoleState, ToolJobMode, ToolJobOutputMetadata, ToolJobRequest,
    ToolJobTerminalState, ToolJobTransition,
};

const SQLITE_SCHEMA_VERSION: &str = "3";
const SQLITE_HANDLE_ATTEMPTS: usize = 8;
const SQLITE_HANDLE_RETRY_DELAY: Duration = Duration::from_millis(1);
const OUTBOX_CLAIM_LEASE_MS: i64 = 60_000;
const MAX_DELIVERY_ATTEMPTS: i64 = 5;
const RETRY_BACKOFF_MS: i64 = 1_000;
/// Backoff for `Pending` delivery effects (e.g. the target agent has not
/// started yet). Longer than `RETRY_BACKOFF_MS` because the waiting condition
/// typically resolves on a human/agent-start timescale, not immediately.
const PENDING_BACKOFF_MS: i64 = 5_000;
const REQUIRED_TABLES: &[(&str, &[&str])] = &[
    ("schema_meta", &["key", "value"]),
    (
        "team_missions",
        &[
            "mission_id",
            "brief",
            "template",
            "agent_profile_id",
            "agent_profile_version",
            "context_rev",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "team_roles",
        &[
            "mission_id",
            "role",
            "provider",
            "model",
            "thinking",
            "permission_policy",
            "profile_id",
            "profile_version",
            "config_digest",
            "pane_id",
            "terminal_id",
            "session_json",
            "launch_generation",
            "health",
            "last_seen_rev",
            "updated_at",
        ],
    ),
    (
        "assignments",
        &[
            "id",
            "mission_id",
            "source_role",
            "target_role",
            "kind",
            "summary",
            "state",
            "parent_id",
            "review_round",
            "skills_json",
            "replace_skills",
            "review_id",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "messages",
        &[
            "id",
            "mission_id",
            "assignment_id",
            "source_role",
            "target_role",
            "kind",
            "body",
            "context_rev",
            "in_reply_to",
            "review_id",
            "created_at",
        ],
    ),
    (
        "outbox",
        &[
            "id",
            "message_id",
            "mission_id",
            "target_role",
            "status",
            "attempts",
            "last_error",
            "claimed_by",
            "claimed_at",
            "created_at",
            "updated_at",
            "delivered_at",
        ],
    ),
    (
        "context_ledger",
        &[
            "mission_id",
            "revision",
            "kind",
            "source_role",
            "summary",
            "refs_json",
            "assignment_id",
            "created_at",
        ],
    ),
    (
        "processed_events",
        &["event_key", "mission_id", "created_at"],
    ),
    (
        "expert_instances",
        &[
            "mission_id",
            "role",
            "provider",
            "model",
            "thinking",
            "permission_policy",
            "profile_id",
            "profile_version",
            "config_digest",
            "pane_id",
            "terminal_id",
            "session_json",
            "launch_generation",
            "state",
            "current_assignment_id",
            "close_policy",
            "role_skill",
            "capability_skills_json",
            "skill_hash",
            "prompt_path",
            "last_active_at",
            "updated_at",
        ],
    ),
    (
        "role_launch_leases",
        &[
            "mission_id",
            "role",
            "owner",
            "generation",
            "acquired_at",
            "expires_at",
        ],
    ),
    (
        "review_revisions",
        &[
            "id",
            "mission_id",
            "reviewer_assignment_id",
            "worker_assignment_id",
            "verdict",
            "summary",
            "refs_json",
            "context_rev",
            "acknowledged_by_pm",
            "created_at",
            "acknowledged_at",
        ],
    ),
    (
        "tool_jobs",
        &[
            "job_id",
            "mission_id",
            "assignment_id",
            "source_role",
            "mode",
            "label",
            "argv_json",
            "cwd",
            "env_json",
            "timeout_seconds",
            "parallel",
            "max_output_bytes",
            "request_json",
            "state",
            "pane_id",
            "coordination_dir",
            "request_path",
            "stdout_path",
            "stderr_path",
            "result_path",
            "stdout_bytes",
            "stderr_bytes",
            "stdout_truncated",
            "stderr_truncated",
            "stdout_checksum",
            "stderr_checksum",
            "exit_code",
            "error",
            "result_notified",
            "created_at",
            "started_at",
            "finished_at",
            "cancelled_at",
            "updated_at",
        ],
    ),
];
const REQUIRED_INDEXES: &[(&str, &str, &[&str])] = &[
    (
        "outbox_target_status",
        "outbox",
        &["mission_id", "target_role", "status", "created_at"],
    ),
    (
        "assignments_target_state",
        "assignments",
        &["mission_id", "target_role", "state", "created_at"],
    ),
    (
        "ledger_mission_revision",
        "context_ledger",
        &["mission_id", "revision"],
    ),
    (
        "expert_instance_state",
        "expert_instances",
        &["mission_id", "state", "role"],
    ),
    (
        "review_pending",
        "review_revisions",
        &["mission_id", "acknowledged_by_pm"],
    ),
    (
        "tool_jobs_mission_state",
        "tool_jobs",
        &["mission_id", "state", "created_at"],
    ),
    (
        "tool_jobs_role_state",
        "tool_jobs",
        &["mission_id", "source_role", "state", "created_at"],
    ),
];

#[derive(Debug, Clone)]
pub(crate) struct WritableDatabasePermit {
    canonical_root: PathBuf,
    canonical_database: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ReadOnlyDatabasePermit {
    canonical_database: PathBuf,
}

impl ReadOnlyDatabasePermit {
    pub(crate) fn for_exact_path(
        permitted_database: &Path,
        requested_database: &Path,
    ) -> Result<Self, KernelError> {
        let canonical_permitted = fs::canonicalize(permitted_database).map_err(|error| {
            path_policy_error(
                "read_only_database_unavailable",
                permitted_database,
                error.to_string(),
            )
        })?;
        let canonical_requested = fs::canonicalize(requested_database).map_err(|error| {
            path_policy_error(
                "read_only_database_unavailable",
                requested_database,
                error.to_string(),
            )
        })?;
        if canonical_permitted != canonical_requested {
            return Err(path_policy_error(
                "read_only_path_mismatch",
                requested_database,
                "request database does not match the exact read-only canary permit",
            ));
        }
        if !fs::metadata(&canonical_requested)
            .map_err(|error| {
                path_policy_error(
                    "read_only_database_unavailable",
                    &canonical_requested,
                    error.to_string(),
                )
            })?
            .is_file()
        {
            return Err(path_policy_error(
                "read_only_database_not_file",
                &canonical_requested,
                "read-only database path is not a regular file",
            ));
        }
        Ok(Self {
            canonical_database: canonical_requested,
        })
    }
}

impl WritableDatabasePermit {
    pub(crate) fn for_temporary_fixture(root: &Path, database: &Path) -> Result<Self, KernelError> {
        let canonical_root = fs::canonicalize(root).map_err(|error| {
            path_policy_error("temporary_root_unavailable", root, error.to_string())
        })?;
        let canonical_system_temp = fs::canonicalize(std::env::temp_dir()).map_err(|error| {
            path_policy_error(
                "temporary_root_unavailable",
                &std::env::temp_dir(),
                error.to_string(),
            )
        })?;
        if !canonical_root.starts_with(&canonical_system_temp) {
            return Err(path_policy_error(
                "production_path_forbidden",
                root,
                "writable adapter root is outside the operating-system temporary directory",
            ));
        }
        let metadata = fs::metadata(database).map_err(|error| {
            path_policy_error(
                "temporary_database_unavailable",
                database,
                error.to_string(),
            )
        })?;
        if !metadata.is_file() {
            return Err(path_policy_error(
                "temporary_database_not_file",
                database,
                "database path is not a regular file",
            ));
        }
        let canonical_database = fs::canonicalize(database).map_err(|error| {
            path_policy_error(
                "temporary_database_unavailable",
                database,
                error.to_string(),
            )
        })?;
        if !canonical_database.starts_with(&canonical_root) {
            return Err(path_policy_error(
                "production_path_forbidden",
                database,
                "database escapes the permitted temporary root",
            ));
        }
        Ok(Self {
            canonical_root,
            canonical_database,
        })
    }

    /// Open a production Rust-owned database for writable kernel access.
    ///
    /// Unlike `for_temporary_fixture`, this does not require the path to live
    /// under the operating-system temp directory; instead it verifies the file
    /// exists and its `plugin_owner` marker is `herdr-mission`, so a foreign
    /// database is never opened by the kernel store.
    pub(crate) fn for_production(database: &Path) -> Result<Self, KernelError> {
        let metadata = fs::metadata(database).map_err(|error| {
            path_policy_error(
                "production_database_unavailable",
                database,
                error.to_string(),
            )
        })?;
        if !metadata.is_file() {
            return Err(path_policy_error(
                "production_database_not_file",
                database,
                "database path is not a regular file",
            ));
        }
        let canonical_database = fs::canonicalize(database).map_err(|error| {
            path_policy_error(
                "production_database_unavailable",
                database,
                error.to_string(),
            )
        })?;
        // Verify the owner marker before handing the path to the kernel store.
        let _owner = crate::open_writable(&canonical_database, crate::OWNER_IDENTITY)?;
        let canonical_root = canonical_database
            .parent()
            .unwrap_or(&canonical_database)
            .to_path_buf();
        Ok(Self {
            canonical_root,
            canonical_database,
        })
    }

    #[cfg(test)]
    fn for_test(root: &Path, database: &Path) -> Result<Self, KernelError> {
        Self::for_temporary_fixture(root, database)
    }
}

#[derive(Debug)]
pub(crate) struct SqliteV3CoordinationStore {
    connection: Connection,
    _canonical_root: PathBuf,
    _canonical_database: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoreObservation {
    pub(crate) schema_version: String,
    pub(crate) mission_id: String,
    pub(crate) assignment_count: u64,
    pub(crate) message_count: u64,
    pub(crate) outbox_count: u64,
    pub(crate) revision: u64,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum CoordinationStore {
    Memory(MemoryCoordinationStore),
    TemporarySqliteV3 {
        mission_id: String,
        store: SqliteV3CoordinationStore,
    },
    ReadOnlySqliteV3 {
        mission_id: String,
        database: PathBuf,
    },
}

impl CoordinationStore {
    pub(crate) fn in_memory(mission_id: impl Into<String>, max_review_rounds: u32) -> Self {
        Self::Memory(MemoryCoordinationStore::new(mission_id, max_review_rounds))
    }

    pub(crate) fn open_temporary_sqlite_v3(
        mission_id: impl Into<String>,
        permit: WritableDatabasePermit,
        busy_timeout: Duration,
    ) -> Result<Self, KernelError> {
        let mission_id = mission_id.into();
        let store = SqliteV3CoordinationStore::open(permit, busy_timeout)?;
        store.observe_mission(&mission_id)?;
        Ok(Self::TemporarySqliteV3 { mission_id, store })
    }

    pub(crate) fn open_read_only_sqlite_v3(
        mission_id: impl Into<String>,
        permit: ReadOnlyDatabasePermit,
    ) -> Result<Self, KernelError> {
        let mission_id = mission_id.into();
        observe_sqlite_database(&permit.canonical_database, &mission_id)?;
        Ok(Self::ReadOnlySqliteV3 {
            mission_id,
            database: permit.canonical_database,
        })
    }

    pub(crate) fn observe(&self) -> Result<StoreObservation, KernelError> {
        match self {
            Self::Memory(store) => Ok(store.observe()),
            Self::TemporarySqliteV3 { mission_id, store } => store.observe_mission(mission_id),
            Self::ReadOnlySqliteV3 {
                mission_id,
                database,
            } => observe_sqlite_database(database, mission_id),
        }
    }

    pub(crate) fn handle(&mut self, request: KernelInput) -> Result<HandleReceipt, KernelError> {
        match self {
            Self::Memory(store) => store.handle(request),
            Self::TemporarySqliteV3 { mission_id, store } => {
                store.handle_mission(mission_id, request)
            }
            Self::ReadOnlySqliteV3 { .. } => Err(sqlite_capability_unavailable("read_only_handle")),
        }
    }

    pub(crate) fn inspect(&self, query: InspectQuery) -> Result<MissionView, KernelError> {
        match self {
            Self::Memory(store) => store.inspect(query),
            Self::TemporarySqliteV3 { mission_id, store } => {
                store.inspect_mission(mission_id, query)
            }
            Self::ReadOnlySqliteV3 {
                mission_id,
                database,
            } => inspect_sqlite_database(database, mission_id, query),
        }
    }

    pub(crate) fn claim_effect(
        &mut self,
        claim_owner: &str,
        claimed_at_ms: i64,
        observed_at: &str,
    ) -> Result<Option<EffectIntent>, KernelError> {
        match self {
            Self::Memory(store) => store.claim_effect(claim_owner, claimed_at_ms, observed_at),
            Self::TemporarySqliteV3 { mission_id, store } => {
                store.claim_effect(mission_id, claim_owner, claimed_at_ms, observed_at)
            }
            Self::ReadOnlySqliteV3 { .. } => Err(sqlite_capability_unavailable("read_only_drive")),
        }
    }
}

impl SqliteV3CoordinationStore {
    pub(crate) fn open(
        permit: WritableDatabasePermit,
        busy_timeout: Duration,
    ) -> Result<Self, KernelError> {
        let open_database = fs::canonicalize(&permit.canonical_database).map_err(|error| {
            path_policy_error(
                "temporary_database_unavailable",
                &permit.canonical_database,
                error.to_string(),
            )
        })?;
        if open_database != permit.canonical_database
            || !open_database.starts_with(&permit.canonical_root)
        {
            return Err(path_policy_error(
                "production_path_forbidden",
                &open_database,
                "database target changed or escapes the permitted temporary root",
            ));
        }
        let connection = Connection::open_with_flags(
            &open_database,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| sqlite_error("sqlite_open_failed", "open", error))?;
        connection
            .busy_timeout(busy_timeout)
            .map_err(|error| sqlite_error("sqlite_config_failed", "busy_timeout", error))?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|error| sqlite_error("sqlite_config_failed", "foreign_keys", error))?;

        let version = connection
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| incompatible_schema_error(None, error.to_string()))?;
        if version != SQLITE_SCHEMA_VERSION {
            return Err(incompatible_schema_error(
                Some(&version),
                "unsupported schema version",
            ));
        }
        verify_schema(&connection)?;

        Ok(Self {
            connection,
            _canonical_root: permit.canonical_root,
            _canonical_database: open_database,
        })
    }

    pub(crate) fn observe_mission(
        &self,
        mission_id: &str,
    ) -> Result<StoreObservation, KernelError> {
        let revision = self
            .connection
            .query_row(
                "SELECT context_rev FROM team_missions WHERE mission_id = ?1",
                [mission_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| sqlite_error("sqlite_observation_failed", "mission", error))?
            .ok_or_else(|| mission_not_found(mission_id))?;
        Ok(StoreObservation {
            schema_version: SQLITE_SCHEMA_VERSION.into(),
            mission_id: mission_id.into(),
            assignment_count: query_mission_count(&self.connection, "assignments", mission_id)?,
            message_count: query_mission_count(&self.connection, "messages", mission_id)?,
            outbox_count: query_mission_count(&self.connection, "outbox", mission_id)?,
            revision: nonnegative_observation(revision, "context_rev")?,
        })
    }

    pub(crate) fn inspect_mission(
        &self,
        mission_id: &str,
        query: InspectQuery,
    ) -> Result<MissionView, KernelError> {
        inspect_sqlite_database(&self._canonical_database, mission_id, query)
    }

    pub(crate) fn claim_effect(
        &mut self,
        mission_id: &str,
        claim_owner: &str,
        claimed_at_ms: i64,
        observed_at: &str,
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
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("sqlite_drive_failed", "begin_claim", error))?;
        let stale_before = claimed_at_ms.saturating_sub(OUTBOX_CLAIM_LEASE_MS);
        transaction
            .execute(
                "UPDATE outbox
                 SET status = 'retry', claimed_by = '', claimed_at = NULL,
                     updated_at = ?1
                 WHERE mission_id = ?2 AND status = 'sending'
                   AND claimed_at IS NOT NULL AND claimed_at < ?3",
                rusqlite::params![observed_at, mission_id, stale_before],
            )
            .map_err(|error| {
                sqlite_error("sqlite_drive_failed", "reclaim_expired_claims", error)
            })?;
        let candidate = transaction
            .query_row(
                "SELECT o.id, m.assignment_id, o.target_role,
                        r.launch_generation, m.body
                 FROM outbox o
                 JOIN messages m ON m.id = o.message_id
                 JOIN team_roles r
                   ON r.mission_id = o.mission_id AND r.role = o.target_role
                 WHERE o.mission_id = ?1 AND o.status IN ('queued', 'retry')
                   AND o.claimed_by = '' AND o.attempts < ?2
                   AND (o.claimed_at IS NULL OR o.claimed_at <= ?3)
                 ORDER BY o.created_at, o.id LIMIT 1",
                rusqlite::params![mission_id, MAX_DELIVERY_ATTEMPTS, claimed_at_ms],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| sqlite_error("sqlite_drive_failed", "select_claim", error))?;
        let Some((effect_id, assignment_id, target_role, generation, prompt)) = candidate else {
            transaction.commit().map_err(|error| {
                sqlite_error("sqlite_drive_failed", "commit_empty_claim", error)
            })?;
            return Ok(None);
        };
        let updated = transaction
            .execute(
                "UPDATE outbox
                 SET status = 'sending', attempts = attempts + 1,
                     claimed_by = ?1, claimed_at = ?2
                 WHERE mission_id = ?3 AND id = ?4
                   AND status IN ('queued', 'retry') AND claimed_by = ''
                   AND attempts < ?5
                   AND (claimed_at IS NULL OR claimed_at <= ?6)",
                rusqlite::params![
                    claim_owner,
                    claimed_at_ms,
                    mission_id,
                    effect_id,
                    MAX_DELIVERY_ATTEMPTS,
                    claimed_at_ms
                ],
            )
            .map_err(|error| sqlite_error("sqlite_drive_failed", "claim_outbox", error))?;
        if updated != 1 {
            return Err(KernelError {
                category: ErrorCategory::Infrastructure,
                code: "claim_race".into(),
                message: "Outbox claim was concurrently changed".into(),
                retryable: true,
                details: BTreeMap::from([("effect_id".into(), json!(effect_id))]),
            });
        }
        transaction
            .commit()
            .map_err(|error| sqlite_error("sqlite_drive_failed", "commit_claim", error))?;
        let generation = crate::Generation::new(generation.clone())
            .map_err(|_| invalid_persisted_state("launch generation", generation))?;
        Ok(Some(EffectIntent {
            effect_id,
            generation,
            intent: EffectIntentKind::DeliverPrompt {
                role: parse_role_storage_identity(&target_role)?,
                assignment_id,
                prompt,
            },
        }))
    }

    pub(crate) fn handle_mission(
        &mut self,
        mission_id: &str,
        request: KernelInput,
    ) -> Result<HandleReceipt, KernelError> {
        for attempt in 0..SQLITE_HANDLE_ATTEMPTS {
            match self.handle_mission_once(mission_id, request.clone()) {
                Err(error) if error.code == "sqlite_busy" => {
                    if attempt + 1 == SQLITE_HANDLE_ATTEMPTS {
                        return Err(error);
                    }
                    thread::sleep(SQLITE_HANDLE_RETRY_DELAY);
                }
                outcome => return outcome,
            }
        }
        unreachable!("bounded SQLite handle attempts always return an outcome")
    }

    fn handle_mission_once(
        &mut self,
        mission_id: &str,
        request: KernelInput,
    ) -> Result<HandleReceipt, KernelError> {
        let input_id = handle_input_id(&request.input).to_owned();
        let event_key = processed_input_key(&request.input);
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| sqlite_error("sqlite_transaction_failed", "begin", error))?;
        let revision = transaction
            .query_row(
                "SELECT context_rev FROM team_missions WHERE mission_id = ?1",
                [mission_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| sqlite_error("sqlite_handle_failed", "mission", error))?
            .ok_or_else(|| mission_not_found(mission_id))?;
        let revision = nonnegative_observation(revision, "context_rev")?;
        if let HandleInput::ToolJobRequest { request: tool_job } = &request.input {
            let request_fingerprint = tool_job_request_fingerprint(tool_job)?;
            let existing_fingerprint = transaction
                .query_row(
                    "SELECT request_json FROM tool_jobs
                     WHERE job_id = ?1 AND mission_id = ?2",
                    rusqlite::params![tool_job.job_id, mission_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| {
                    sqlite_error("sqlite_handle_failed", "load_tool_job_fingerprint", error)
                })?;
            if let Some(existing_fingerprint) = existing_fingerprint {
                if existing_fingerprint != request_fingerprint {
                    return Ok(rejected_receipt(
                        tool_job.job_id.clone(),
                        "input_id_conflict",
                        "Tool Job ID was already committed with different semantics",
                    ));
                }
                return Ok(duplicate_receipt(tool_job.job_id.clone(), revision));
            }
        }
        let already_processed = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM processed_events WHERE event_key = ?1)",
                [&event_key],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| sqlite_error("sqlite_handle_failed", "deduplicate", error))?;
        if already_processed {
            if let HandleInput::RoleLaunchRequest {
                role,
                generation,
                launch_owner,
                acquired_at,
                attach_mode,
                ..
            } = &request.input
            {
                return duplicate_role_launch_receipt(
                    &transaction,
                    mission_id,
                    input_id,
                    revision,
                    role,
                    generation,
                    launch_owner,
                    *acquired_at,
                    *attach_mode,
                );
            }
            return Ok(duplicate_receipt(input_id, revision));
        }
        if let HandleInput::TeamEvent {
            event_id,
            sequence,
            name,
            body,
        } = &request.input
        {
            if name == "assignment_settled" {
                let receipt = persist_assignment_settled_event(
                    &transaction,
                    mission_id,
                    &event_key,
                    event_id,
                    *sequence,
                    name,
                    body,
                    &request,
                    revision,
                )?;
                if receipt.disposition != HandleDisposition::Rejected {
                    transaction
                        .commit()
                        .map_err(|error| sqlite_error("sqlite_commit_failed", "commit", error))?;
                }
                return Ok(receipt);
            }
        }
        if let Some(rejection) = role_observation_authority_rejection(
            &transaction,
            mission_id,
            &request.input,
            input_id.clone(),
            &request.decision_context.observed_at,
        )? {
            return Ok(rejection);
        }
        if let Some(rejection) =
            effect_result_authority_rejection(&transaction, mission_id, &request.input, input_id)?
        {
            return Ok(rejection);
        }

        let mut reducer = MemoryCoordinationStore::new_at_revision(mission_id, 3, revision);
        restore_assignments(&transaction, mission_id, &mut reducer)?;
        restore_context_ledger(&transaction, mission_id, &mut reducer)?;
        restore_role_generations(&transaction, mission_id, &mut reducer)?;
        restore_pending_effects(&transaction, mission_id, &mut reducer)?;
        restore_tool_jobs(&transaction, mission_id, &mut reducer)?;
        let receipt = reducer.handle(request.clone())?;
        if receipt.disposition == HandleDisposition::Duplicate {
            transaction
                .execute(
                    "INSERT INTO processed_events(event_key, mission_id, created_at)
                     VALUES(?1, ?2, ?3)",
                    rusqlite::params![event_key, mission_id, request.decision_context.observed_at],
                )
                .map_err(|error| {
                    sqlite_error("sqlite_handle_failed", "insert_duplicate_input", error)
                })?;
            transaction
                .commit()
                .map_err(|error| sqlite_error("sqlite_commit_failed", "commit", error))?;
            return Ok(receipt);
        }
        if receipt.disposition != HandleDisposition::Applied {
            return Ok(receipt);
        }
        ensure_schema_v3_effects(&receipt, &request.input)?;

        match &request.input {
            HandleInput::Command {
                kind,
                source,
                target: Some(target),
                body,
                ..
            } if source.role == RoleKind::Pm && kind == "context" => {
                persist_context_command(
                    &transaction,
                    mission_id,
                    &event_key,
                    &request,
                    &receipt,
                    source,
                    target,
                    body,
                )?;
            }
            HandleInput::Command {
                kind,
                source,
                target: Some(target),
                body,
                ..
            } if source.role == RoleKind::Pm
                && matches!(kind.as_str(), "task" | "fix" | "review") =>
            {
                persist_assignment_command(
                    &transaction,
                    mission_id,
                    revision,
                    &event_key,
                    &request,
                    &receipt,
                    source,
                    target,
                    kind,
                    body,
                )?;
            }
            HandleInput::Command {
                kind,
                source,
                target: Some(target),
                body,
                ..
            } if source.role != RoleKind::Pm && target.role == RoleKind::Pm => {
                persist_reply_command(
                    &transaction,
                    mission_id,
                    &event_key,
                    &request,
                    &receipt,
                    source,
                    target,
                    kind,
                    body,
                )?;
            }
            HandleInput::RoleObservation {
                role,
                generation,
                launch_owner,
                state,
                details,
                ..
            } => {
                persist_role_observation(
                    &transaction,
                    mission_id,
                    &event_key,
                    &request,
                    &receipt,
                    role,
                    generation,
                    launch_owner.as_deref(),
                    *state,
                    details,
                )?;
            }
            HandleInput::RoleLaunchRequest {
                launch_id,
                role,
                generation,
                launch_owner,
                acquired_at,
                expires_at,
                ..
            } => {
                persist_role_launch_request(
                    &transaction,
                    mission_id,
                    &event_key,
                    &request,
                    &receipt,
                    launch_id,
                    role,
                    generation,
                    launch_owner,
                    *acquired_at,
                    *expires_at,
                )?;
            }
            HandleInput::EffectResult { result } => {
                persist_effect_result(
                    &transaction,
                    mission_id,
                    &event_key,
                    &request,
                    &receipt,
                    result,
                )?;
            }
            HandleInput::ToolJobRequest { request: tool_job } => {
                persist_tool_job_request(
                    &transaction,
                    mission_id,
                    &event_key,
                    &request,
                    &receipt,
                    tool_job,
                )?;
            }
            HandleInput::ToolJobTransition {
                job_id, transition, ..
            } => {
                persist_tool_job_transition(
                    &transaction,
                    mission_id,
                    &event_key,
                    &request,
                    &receipt,
                    job_id,
                    transition,
                )?;
            }
            _ => return Err(sqlite_capability_unavailable("handle_input")),
        }

        transaction
            .commit()
            .map_err(|error| sqlite_error("sqlite_commit_failed", "commit", error))?;
        Ok(receipt)
    }
}

#[allow(clippy::too_many_arguments)]
fn persist_assignment_settled_event(
    transaction: &Transaction<'_>,
    mission_id: &str,
    event_key: &str,
    event_id: &str,
    sequence: u64,
    name: &str,
    body: &Value,
    request: &KernelInput,
    current_revision: u64,
) -> Result<HandleReceipt, KernelError> {
    let Some(role_value) = body.get("role").and_then(Value::as_str) else {
        return Ok(rejected_receipt(
            event_id.to_owned(),
            "missing_role",
            "settled recovery requires body.role",
        ));
    };
    let role = role_storage_identity(&parse_role_storage_identity(role_value)?)?;
    let Some(expected_assignment_id) = body
        .get("expected_assignment_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Ok(rejected_receipt(
            event_id.to_owned(),
            "missing_assignment_id",
            "settled recovery requires body.expected_assignment_id",
        ));
    };
    let safe_to_resume = body
        .get("safe_to_resume")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let observed_at = &request.decision_context.observed_at;
    let sequence_key = format!("state-sequence:{name}:{role}:{sequence}");
    let sequence_processed = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM processed_events WHERE event_key = ?1)",
            [&sequence_key],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "deduplicate_sequence", error))?;
    if sequence_processed {
        insert_processed_event(transaction, event_key, mission_id, observed_at)?;
        return Ok(duplicate_receipt(event_id.to_owned(), current_revision));
    }

    let assignment = transaction
        .query_row(
            "SELECT target_role, kind, state, review_round
             FROM assignments
             WHERE mission_id = ?1 AND id = ?2",
            rusqlite::params![mission_id, expected_assignment_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_settled_assignment", error))?;
    let active_assignment = assignment.filter(|(target_role, kind, state, _)| {
        target_role == &role
            && matches!(kind.as_str(), "task" | "fix" | "review")
            && state == "active"
    });
    let Some((_, _, _, review_round)) = active_assignment else {
        insert_processed_event(transaction, event_key, mission_id, observed_at)?;
        insert_processed_event(transaction, &sequence_key, mission_id, observed_at)?;
        return Ok(applied_settled_receipt(
            event_id,
            current_revision,
            None,
            None,
        ));
    };
    let review_round = u32::try_from(review_round)
        .map_err(|_| invalid_persisted_state("assignment review_round", review_round))?;
    let already_replied = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM messages
                 WHERE mission_id = ?1 AND assignment_id = ?2 AND source_role = ?3
             )",
            rusqlite::params![mission_id, expected_assignment_id, role],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_settled_reply", error))?;
    if already_replied {
        insert_processed_event(transaction, event_key, mission_id, observed_at)?;
        insert_processed_event(transaction, &sequence_key, mission_id, observed_at)?;
        return Ok(applied_settled_receipt(
            event_id,
            current_revision,
            Some(AssignmentState::Active),
            Some(review_round),
        ));
    }

    let delivery = transaction
        .query_row(
            "SELECT outbox.id, outbox.status, outbox.attempts
             FROM messages
             JOIN outbox ON outbox.message_id = messages.id
             WHERE messages.assignment_id = ?1
               AND messages.target_role = ?2
               AND messages.in_reply_to IS NULL
             ORDER BY messages.created_at, messages.id
             LIMIT 1",
            rusqlite::params![expected_assignment_id, role],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_settled_delivery", error))?;
    if delivery
        .as_ref()
        .is_some_and(|(_, status, _)| matches!(status.as_str(), "queued" | "retry" | "sending"))
    {
        insert_processed_event(transaction, event_key, mission_id, observed_at)?;
        insert_processed_event(transaction, &sequence_key, mission_id, observed_at)?;
        return Ok(applied_settled_receipt(
            event_id,
            current_revision,
            Some(AssignmentState::Active),
            Some(review_round),
        ));
    }
    if let Some((outbox_id, status, attempts)) = &delivery {
        if safe_to_resume && status == "delivered" && *attempts < MAX_DELIVERY_ATTEMPTS {
            transaction
                .execute(
                    "UPDATE outbox
                     SET status = 'retry', claimed_by = '', claimed_at = NULL,
                         last_error = 'Agent settled before replying; resuming original Assignment',
                         delivered_at = NULL, updated_at = ?1
                     WHERE id = ?2 AND mission_id = ?3",
                    rusqlite::params![observed_at, outbox_id, mission_id],
                )
                .map_err(|error| {
                    sqlite_error("sqlite_handle_failed", "resume_settled_delivery", error)
                })?;
            insert_processed_event(transaction, event_key, mission_id, observed_at)?;
            insert_processed_event(transaction, &sequence_key, mission_id, observed_at)?;
            return Ok(applied_settled_receipt(
                event_id,
                current_revision,
                Some(AssignmentState::Active),
                Some(review_round),
            ));
        }
    }

    let block_reason = match &delivery {
        None => "找不到原始投递记录，无法安全恢复".to_owned(),
        Some((_, _, _)) if !safe_to_resume => {
            "settled 事件缺少可靠 state_change_seq，无法幂等恢复".to_owned()
        }
        Some((_, _, _)) => format!("恢复次数已达到上限 {MAX_DELIVERY_ATTEMPTS}"),
    };
    transaction
        .execute(
            "UPDATE assignments SET state = 'blocked', updated_at = ?1
             WHERE mission_id = ?2 AND id = ?3 AND state = 'active'",
            rusqlite::params![observed_at, mission_id, expected_assignment_id],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "block_settled_assignment", error))?;
    let existing_block = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM context_ledger
                 WHERE mission_id = ?1 AND assignment_id = ?2 AND kind = 'blocked'
             )",
            rusqlite::params![mission_id, expected_assignment_id],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_settled_block", error))?;
    let mut revision = current_revision;
    let mut created_ids = BTreeMap::new();
    if !existing_block {
        revision += 1;
        let revision_sql = i64::try_from(revision)
            .map_err(|_| invalid_receipt("revision exceeds SQLite range"))?;
        let message_id = allocated_id(request, "message")?;
        let outbox_id = allocated_id(request, "outbox")?;
        let summary = format!(
            "{role} 的 Assignment {expected_assignment_id}：{block_reason}；已持久化为 blocked，等待 PM 处理。"
        );
        transaction
            .execute(
                "INSERT INTO context_ledger(
                     mission_id, revision, kind, source_role, summary, refs_json,
                     assignment_id, created_at
                 ) VALUES(?1, ?2, 'blocked', ?3, ?4, '[]', ?5, ?6)",
                rusqlite::params![
                    mission_id,
                    revision_sql,
                    role,
                    summary,
                    expected_assignment_id,
                    observed_at
                ],
            )
            .map_err(|error| {
                sqlite_error("sqlite_handle_failed", "insert_settled_block_ledger", error)
            })?;
        transaction
            .execute(
                "INSERT INTO messages(
                     id, mission_id, assignment_id, source_role, target_role, kind,
                     body, context_rev, in_reply_to, review_id, created_at
                 ) VALUES(?1, ?2, ?3, ?4, 'pm', 'blocked', ?5, ?6, ?3, NULL, ?7)",
                rusqlite::params![
                    message_id,
                    mission_id,
                    expected_assignment_id,
                    role,
                    summary,
                    revision_sql,
                    observed_at
                ],
            )
            .map_err(|error| {
                sqlite_error(
                    "sqlite_handle_failed",
                    "insert_settled_block_message",
                    error,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO outbox(
                     id, message_id, mission_id, target_role, status, attempts,
                     last_error, claimed_by, claimed_at, created_at, updated_at, delivered_at
                 ) VALUES(?1, ?2, ?3, 'pm', 'queued', 0, '', '', NULL, ?4, ?4, NULL)",
                rusqlite::params![outbox_id, message_id, mission_id, observed_at],
            )
            .map_err(|error| {
                sqlite_error("sqlite_handle_failed", "insert_settled_block_outbox", error)
            })?;
        transaction
            .execute(
                "UPDATE team_missions SET context_rev = ?1, updated_at = ?2
                 WHERE mission_id = ?3",
                rusqlite::params![revision_sql, observed_at, mission_id],
            )
            .map_err(|error| sqlite_error("sqlite_handle_failed", "update_mission", error))?;
        created_ids.insert("message".into(), message_id.to_owned());
        created_ids.insert("outbox".into(), outbox_id.to_owned());
    }
    insert_processed_event(transaction, event_key, mission_id, observed_at)?;
    insert_processed_event(transaction, &sequence_key, mission_id, observed_at)?;
    let mut receipt = applied_settled_receipt(
        event_id,
        revision,
        Some(AssignmentState::Blocked),
        Some(review_round),
    );
    receipt.created_ids = created_ids;
    Ok(receipt)
}

fn insert_processed_event(
    transaction: &Transaction<'_>,
    event_key: &str,
    mission_id: &str,
    observed_at: &str,
) -> Result<(), KernelError> {
    transaction
        .execute(
            "INSERT INTO processed_events(event_key, mission_id, created_at)
             VALUES(?1, ?2, ?3)",
            rusqlite::params![event_key, mission_id, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_processed_input", error))?;
    Ok(())
}

fn applied_settled_receipt(
    event_id: &str,
    revision: u64,
    assignment_state: Option<AssignmentState>,
    review_round: Option<u32>,
) -> HandleReceipt {
    HandleReceipt {
        input_id: event_id.to_owned(),
        disposition: HandleDisposition::Applied,
        revision: Some(revision),
        effect_intents: Vec::new(),
        created_ids: BTreeMap::new(),
        relationships: BTreeMap::new(),
        review_round,
        assignment_state,
        review_limit_reached: false,
        error: None,
    }
}

fn allocated_id<'a>(request: &'a KernelInput, key: &str) -> Result<&'a str, KernelError> {
    request
        .decision_context
        .allocated_ids
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| KernelError {
            category: ErrorCategory::Contract,
            code: "missing_decision_value".into(),
            message: format!("decision context is missing allocated ID {key}"),
            retryable: false,
            details: BTreeMap::from([("key".into(), json!(key))]),
        })
}

fn effect_result_authority_rejection(
    transaction: &Transaction<'_>,
    mission_id: &str,
    input: &HandleInput,
    input_id: String,
) -> Result<Option<HandleReceipt>, KernelError> {
    let HandleInput::EffectResult { result } = input else {
        return Ok(None);
    };
    let claim = transaction
        .query_row(
            "SELECT outbox.status, outbox.claimed_by, team_roles.launch_generation
             FROM outbox
             JOIN team_roles
               ON team_roles.mission_id = outbox.mission_id
              AND team_roles.role = outbox.target_role
             WHERE outbox.mission_id = ?1 AND outbox.id = ?2",
            rusqlite::params![mission_id, result.effect_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| {
            sqlite_error(
                "sqlite_handle_failed",
                "load_outbox_result_authority",
                error,
            )
        })?;
    let Some((status, claimed_by, launch_generation)) = claim else {
        return Ok(None);
    };
    if status != "sending" || claimed_by.is_empty() {
        return Ok(Some(rejected_receipt(
            input_id,
            "outbox_not_claimed",
            "effect result requires an active durable Outbox claim",
        )));
    }
    if result.claim_owner.trim().is_empty() || result.claim_owner != claimed_by {
        return Ok(Some(rejected_receipt(
            input_id,
            "outbox_claim_owner_mismatch",
            "effect result claim owner does not own the durable Outbox claim",
        )));
    }
    if result.generation.as_str() != launch_generation {
        return Ok(Some(rejected_receipt(
            input_id,
            "stale_generation",
            "effect result generation no longer owns the target role",
        )));
    }
    Ok(None)
}

fn role_observation_authority_rejection(
    transaction: &Transaction<'_>,
    mission_id: &str,
    input: &HandleInput,
    input_id: String,
    observed_at: &str,
) -> Result<Option<HandleReceipt>, KernelError> {
    let HandleInput::RoleObservation {
        role,
        generation,
        launch_owner,
        ..
    } = input
    else {
        return Ok(None);
    };
    let role_identity = role_storage_identity(role)?;
    let authority = transaction
        .query_row(
            "SELECT team_roles.launch_generation,
                    role_launch_leases.owner,
                    role_launch_leases.generation
             FROM team_roles
             LEFT JOIN role_launch_leases
               ON role_launch_leases.mission_id = team_roles.mission_id
              AND role_launch_leases.role = team_roles.role
              AND role_launch_leases.expires_at
                  > CAST(strftime('%s', ?3) AS INTEGER)
             WHERE team_roles.mission_id = ?1 AND team_roles.role = ?2",
            rusqlite::params![mission_id, role_identity, observed_at,],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| {
            sqlite_error(
                "sqlite_handle_failed",
                "load_role_observation_authority",
                error,
            )
        })?;
    match authority {
        None => Ok(Some(rejected_receipt(
            input_id,
            "role_not_found",
            "role observation requires an existing authoritative role",
        ))),
        Some((launch_generation, _, _)) if launch_generation.is_empty() => {
            Ok(Some(rejected_receipt(
                input_id,
                "missing_authoritative_generation",
                "role observation cannot establish a launch generation",
            )))
        }
        Some((_, None, None)) => Ok(Some(rejected_receipt(
            input_id,
            "role_launch_lease_missing",
            "role observation requires a current unexpired launch lease",
        ))),
        Some((launch_generation, _lease_owner, lease_generation))
            if generation.as_str() != launch_generation
                || lease_generation
                    .as_deref()
                    .is_some_and(|leased| leased != generation.as_str()) =>
        {
            Ok(Some(rejected_receipt(
                input_id,
                "stale_generation",
                "role observation generation no longer owns the role launch",
            )))
        }
        Some((_, Some(lease_owner), _))
            if launch_owner.as_deref().filter(|owner| !owner.is_empty())
                != Some(lease_owner.as_str()) =>
        {
            Ok(Some(rejected_receipt(
                input_id,
                "role_launch_lease_owner_mismatch",
                "role observation launch owner does not own the active lease",
            )))
        }
        Some(_) => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
fn duplicate_role_launch_receipt(
    transaction: &Transaction<'_>,
    mission_id: &str,
    launch_id: String,
    revision: u64,
    role: &RoleRef,
    generation: &crate::Generation,
    launch_owner: &str,
    acquired_at: i64,
    attach_mode: crate::RoleAttachMode,
) -> Result<HandleReceipt, KernelError> {
    let role_identity = role_storage_identity(role)?;
    let lease_matches = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM role_launch_leases
                 WHERE mission_id = ?1 AND role = ?2 AND owner = ?3
                   AND generation = ?4 AND expires_at > ?5
             )",
            rusqlite::params![
                mission_id,
                role_identity,
                launch_owner,
                generation.as_str(),
                acquired_at,
            ],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| {
            sqlite_error(
                "sqlite_handle_failed",
                "load_duplicate_role_launch_lease",
                error,
            )
        })?;
    let mut receipt = duplicate_receipt(launch_id.clone(), revision);
    receipt.relationships.insert("role".into(), role_identity);
    if lease_matches {
        receipt.effect_intents.push(EffectIntent {
            effect_id: launch_id,
            generation: generation.clone(),
            intent: EffectIntentKind::EnsureRoleReady {
                role: role.clone(),
                attach_mode,
            },
        });
    }
    Ok(receipt)
}

#[allow(clippy::too_many_arguments)]
fn persist_role_observation(
    transaction: &Transaction<'_>,
    mission_id: &str,
    event_key: &str,
    request: &KernelInput,
    receipt: &HandleReceipt,
    role: &RoleRef,
    generation: &crate::Generation,
    launch_owner: Option<&str>,
    state: RoleState,
    details: &Value,
) -> Result<(), KernelError> {
    let role_identity = role_storage_identity(role)?;
    let revision = receipt
        .revision
        .ok_or_else(|| invalid_receipt("missing revision"))?;
    let revision =
        i64::try_from(revision).map_err(|_| invalid_receipt("revision exceeds SQLite range"))?;
    let observed_at = &request.decision_context.observed_at;
    let pane_id = optional_detail_string(details, "pane_id")?;
    let terminal_id = optional_detail_string(details, "terminal_id")?;
    let session_json = details
        .get("session")
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| KernelError {
            category: ErrorCategory::Contract,
            code: "invalid_role_observation_details".into(),
            message: "role observation session could not be serialized".into(),
            retryable: false,
            details: BTreeMap::from([("reason".into(), json!(error.to_string()))]),
        })?;
    let updated = transaction
        .execute(
            "UPDATE team_roles
             SET health = ?1,
                 pane_id = COALESCE(?2, pane_id),
                 terminal_id = COALESCE(?3, terminal_id),
                 session_json = COALESCE(?4, session_json),
                 updated_at = ?5
             WHERE mission_id = ?6 AND role = ?7 AND launch_generation = ?8",
            rusqlite::params![
                role_state_name(state),
                pane_id,
                terminal_id,
                session_json,
                observed_at,
                mission_id,
                role_identity,
                generation.as_str(),
            ],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "update_role_state", error))?;
    if updated != 1 {
        return Err(invalid_persisted_state(
            "authoritative role generation",
            generation.as_str(),
        ));
    }
    if role.role == RoleKind::Scout && role.instance.is_some() {
        transaction
            .execute(
                "UPDATE expert_instances
                 SET pane_id = (
                         SELECT pane_id FROM team_roles
                         WHERE mission_id = ?2 AND role = ?3
                     ),
                     terminal_id = (
                         SELECT terminal_id FROM team_roles
                         WHERE mission_id = ?2 AND role = ?3
                     ),
                     session_json = (
                         SELECT session_json FROM team_roles
                         WHERE mission_id = ?2 AND role = ?3
                     ),
                     launch_generation = ?4,
                     state = CASE
                         WHEN EXISTS(
                             SELECT 1 FROM team_roles
                             WHERE mission_id = ?2 AND role = ?3
                               AND (pane_id <> '' OR terminal_id <> ''
                                    OR session_json IS NOT NULL OR launch_generation <> '')
                         ) THEN 'active'
                         ELSE state
                     END,
                     last_active_at = CASE
                         WHEN EXISTS(
                             SELECT 1 FROM team_roles
                             WHERE mission_id = ?2 AND role = ?3
                               AND (pane_id <> '' OR terminal_id <> ''
                                    OR session_json IS NOT NULL OR launch_generation <> '')
                         ) THEN ?1
                         ELSE last_active_at
                     END,
                     updated_at = ?1
                 WHERE mission_id = ?2 AND role = ?3",
                rusqlite::params![observed_at, mission_id, role_identity, generation.as_str(),],
            )
            .map_err(|error| {
                sqlite_error("sqlite_handle_failed", "update_expert_role_state", error)
            })?;
    }
    if matches!(
        state,
        RoleState::Ready | RoleState::Stopped | RoleState::Failed
    ) {
        let launch_owner = launch_owner.ok_or_else(|| invalid_receipt("missing launch owner"))?;
        transaction
            .execute(
                "DELETE FROM role_launch_leases
                 WHERE mission_id = ?1 AND role = ?2 AND owner = ?3 AND generation = ?4",
                rusqlite::params![mission_id, role_identity, launch_owner, generation.as_str(),],
            )
            .map_err(|error| {
                sqlite_error("sqlite_handle_failed", "release_role_launch_lease", error)
            })?;
    }
    transaction
        .execute(
            "INSERT INTO processed_events(event_key, mission_id, created_at)
             VALUES(?1, ?2, ?3)",
            rusqlite::params![event_key, mission_id, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_processed_input", error))?;
    transaction
        .execute(
            "UPDATE team_missions SET context_rev = ?1, updated_at = ?2 WHERE mission_id = ?3",
            rusqlite::params![revision, observed_at, mission_id],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "update_mission", error))?;
    Ok(())
}

fn optional_detail_string<'a>(
    details: &'a Value,
    key: &str,
) -> Result<Option<&'a str>, KernelError> {
    match details.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(value) => Err(KernelError {
            category: ErrorCategory::Contract,
            code: "invalid_role_observation_details".into(),
            message: format!("role observation {key} must be a string"),
            retryable: false,
            details: BTreeMap::from([("value".into(), value.clone())]),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn persist_role_launch_request(
    transaction: &Transaction<'_>,
    mission_id: &str,
    event_key: &str,
    request: &KernelInput,
    receipt: &HandleReceipt,
    launch_id: &str,
    role: &RoleRef,
    generation: &crate::Generation,
    launch_owner: &str,
    acquired_at: i64,
    expires_at: i64,
) -> Result<(), KernelError> {
    let role_identity = role_storage_identity(role)?;
    let revision = receipt
        .revision
        .ok_or_else(|| invalid_receipt("missing revision"))?;
    let revision =
        i64::try_from(revision).map_err(|_| invalid_receipt("revision exceeds SQLite range"))?;
    let observed_at = &request.decision_context.observed_at;
    transaction
        .execute(
            "INSERT INTO team_roles(
                 mission_id, role, provider, launch_generation, health, updated_at
             ) VALUES(?1, ?2, 'codex', ?3, 'starting', ?4)
             ON CONFLICT(mission_id, role) DO UPDATE SET
                 launch_generation = excluded.launch_generation,
                 health = excluded.health,
                 updated_at = excluded.updated_at",
            rusqlite::params![mission_id, role_identity, generation.as_str(), observed_at,],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "upsert_role_launch", error))?;
    let changed = transaction
        .execute(
            "INSERT INTO role_launch_leases(
                 mission_id, role, owner, generation, acquired_at, expires_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(mission_id, role) DO UPDATE SET
                 owner = excluded.owner,
                 generation = excluded.generation,
                 acquired_at = excluded.acquired_at,
                 expires_at = excluded.expires_at
             WHERE role_launch_leases.expires_at <= excluded.acquired_at
                OR (role_launch_leases.owner = excluded.owner
                    AND role_launch_leases.generation = excluded.generation)",
            rusqlite::params![
                mission_id,
                role_identity,
                launch_owner,
                generation.as_str(),
                acquired_at,
                expires_at,
            ],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "acquire_role_launch", error))?;
    if changed != 1 {
        return Err(KernelError {
            category: ErrorCategory::Domain,
            code: "role_launch_lease_conflict".into(),
            message: "another role launch lease already owns this role".into(),
            retryable: true,
            details: BTreeMap::from([
                ("launch_id".into(), json!(launch_id)),
                ("role".into(), json!(role_identity)),
            ]),
        });
    }
    insert_processed_event(transaction, event_key, mission_id, observed_at)?;
    transaction
        .execute(
            "UPDATE team_missions SET context_rev = ?1, updated_at = ?2
             WHERE mission_id = ?3",
            rusqlite::params![revision, observed_at, mission_id],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "advance_revision", error))?;
    Ok(())
}

fn persist_effect_result(
    transaction: &Transaction<'_>,
    mission_id: &str,
    event_key: &str,
    request: &KernelInput,
    receipt: &HandleReceipt,
    result: &crate::EffectResult,
) -> Result<(), KernelError> {
    let revision = receipt
        .revision
        .ok_or_else(|| invalid_receipt("missing revision"))?;
    let revision =
        i64::try_from(revision).map_err(|_| invalid_receipt("revision exceeds SQLite range"))?;
    let observed_at = &request.decision_context.observed_at;
    let (status, last_error, delivered_at, reset_attempts) = match &result.outcome {
        EffectOutcome::Succeeded { .. } => ("delivered", String::new(), Some(observed_at), false),
        // A waiting condition is not a failed delivery: park it back in
        // `queued` with a zeroed retry budget so a late agent start/join can
        // still deliver it, instead of orphaning it at MAX_DELIVERY_ATTEMPTS.
        EffectOutcome::Pending { reason } => ("queued", reason.clone(), None, true),
        EffectOutcome::RetryableFailure { error, .. } => (
            "retry",
            format!("{}: {}", error.code, error.message),
            None,
            false,
        ),
        EffectOutcome::TerminalFailure { error } => (
            "failed",
            format!("{}: {}", error.code, error.message),
            None,
            false,
        ),
    };
    let next_claim_at: Option<i64> = match &result.outcome {
        EffectOutcome::Pending { .. } => Some(now_ms().saturating_add(PENDING_BACKOFF_MS)),
        EffectOutcome::RetryableFailure { .. } => Some(now_ms().saturating_add(RETRY_BACKOFF_MS)),
        EffectOutcome::Succeeded { .. } | EffectOutcome::TerminalFailure { .. } => None,
    };
    let updated = transaction
        .execute(
            "UPDATE outbox
             SET status = ?1, last_error = ?2, claimed_by = '', claimed_at = ?9,
                 updated_at = ?3, delivered_at = ?4,
                 attempts = CASE WHEN ?10 THEN 0 ELSE attempts END
             WHERE mission_id = ?5 AND id = ?6 AND status = 'sending'
               AND claimed_by = ?7
               AND EXISTS(
                   SELECT 1 FROM team_roles
                   WHERE team_roles.mission_id = outbox.mission_id
                     AND team_roles.role = outbox.target_role
                     AND team_roles.launch_generation = ?8
               )",
            rusqlite::params![
                status,
                last_error,
                observed_at,
                delivered_at,
                mission_id,
                result.effect_id,
                result.claim_owner,
                result.generation.as_str(),
                next_claim_at,
                reset_attempts,
            ],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "resolve_outbox", error))?;
    if updated != 1 {
        return Err(invalid_persisted_state(
            "durable Outbox claim authority",
            &result.effect_id,
        ));
    }
    if matches!(result.outcome, EffectOutcome::Succeeded { .. }) {
        if let Some(assignment_id) = receipt.created_ids.get("assignment") {
            let updated = transaction
                .execute(
                    "UPDATE assignments
                 SET state = 'active', updated_at = ?1
                 WHERE mission_id = ?2 AND id = ?3 AND state = 'queued'",
                    rusqlite::params![observed_at, mission_id, assignment_id],
                )
                .map_err(|error| {
                    sqlite_error("sqlite_handle_failed", "activate_assignment", error)
                })?;
            if updated != 1 {
                return Err(invalid_persisted_state(
                    "pending assignment effect",
                    assignment_id,
                ));
            }
        }
    }
    transaction
        .execute(
            "INSERT INTO processed_events(event_key, mission_id, created_at)
             VALUES(?1, ?2, ?3)",
            rusqlite::params![event_key, mission_id, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_processed_input", error))?;
    transaction
        .execute(
            "UPDATE team_missions SET context_rev = ?1, updated_at = ?2 WHERE mission_id = ?3",
            rusqlite::params![revision, observed_at, mission_id],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "update_mission", error))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn persist_reply_command(
    transaction: &Transaction<'_>,
    mission_id: &str,
    event_key: &str,
    request: &KernelInput,
    receipt: &HandleReceipt,
    source: &RoleRef,
    target: &RoleRef,
    kind: &str,
    body: &serde_json::Value,
) -> Result<(), KernelError> {
    let assignment_id = body
        .get("assignment_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_receipt("reply is missing assignment_id"))?;
    let message_id = created_id(receipt, "message")?;
    let outbox_id = created_id(receipt, "outbox")?;
    let source_role = role_storage_identity(source)?;
    let target_role = role_storage_identity(target)?;
    let summary = body
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let revision = receipt
        .revision
        .ok_or_else(|| invalid_receipt("missing revision"))?;
    let revision =
        i64::try_from(revision).map_err(|_| invalid_receipt("revision exceeds SQLite range"))?;
    let assignment_state = assignment_state_name(
        receipt
            .assignment_state
            .ok_or_else(|| invalid_receipt("missing assignment state"))?,
    );
    let observed_at = &request.decision_context.observed_at;

    transaction
        .execute(
            "UPDATE assignments
             SET state = ?1, updated_at = ?2
             WHERE id = ?3 AND mission_id = ?4",
            rusqlite::params![assignment_state, observed_at, assignment_id, mission_id],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "update_assignment", error))?;
    transaction
        .execute(
            "UPDATE outbox
             SET status = 'delivered', claimed_by = '', claimed_at = NULL,
                 updated_at = ?1, delivered_at = COALESCE(delivered_at, ?1)
             WHERE mission_id = ?2
               AND target_role = ?3
               AND message_id IN (
                   SELECT id FROM messages
                   WHERE mission_id = ?2
                     AND assignment_id = ?4
                     AND source_role = 'pm'
               )",
            rusqlite::params![observed_at, mission_id, source_role, assignment_id],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "settle_assignment_outbox", error))?;
    transaction
        .execute(
            "INSERT INTO context_ledger(
                 mission_id, revision, kind, source_role, summary, refs_json,
                 assignment_id, created_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, '[]', ?6, ?7)",
            rusqlite::params![
                mission_id,
                revision,
                kind,
                source_role,
                summary,
                assignment_id,
                observed_at,
            ],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_context_ledger", error))?;
    transaction
        .execute(
            "UPDATE team_missions SET context_rev = ?1, updated_at = ?2 WHERE mission_id = ?3",
            rusqlite::params![revision, observed_at, mission_id],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "update_mission", error))?;
    transaction
        .execute(
            "INSERT INTO messages(
                 id, mission_id, assignment_id, source_role, target_role, kind,
                 body, context_rev, in_reply_to, review_id, created_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?3, NULL, ?9)",
            rusqlite::params![
                message_id,
                mission_id,
                assignment_id,
                source_role,
                target_role,
                kind,
                summary,
                revision,
                observed_at,
            ],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_message", error))?;
    persist_review_revision(
        transaction,
        mission_id,
        request,
        receipt,
        source,
        kind,
        assignment_id,
        summary,
        revision,
        observed_at,
        body,
    )?;
    persist_follow_up_assignment(transaction, mission_id, receipt, revision, observed_at)?;
    persist_review_notices(
        transaction,
        mission_id,
        receipt,
        assignment_id,
        summary,
        revision,
        observed_at,
    )?;
    transaction
        .execute(
            "INSERT INTO processed_events(event_key, mission_id, created_at)
             VALUES(?1, ?2, ?3)",
            rusqlite::params![event_key, mission_id, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_processed_input", error))?;
    transaction
        .execute(
            "INSERT INTO outbox(
                 id, message_id, mission_id, target_role, status, attempts,
                 last_error, claimed_by, claimed_at, created_at, updated_at, delivered_at
             ) VALUES(?1, ?2, ?3, ?4, 'queued', 0, '', '', NULL, ?5, ?5, NULL)",
            rusqlite::params![outbox_id, message_id, mission_id, target_role, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_outbox", error))?;
    persist_effect_generations(transaction, mission_id, receipt, observed_at)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn persist_review_revision(
    transaction: &Transaction<'_>,
    mission_id: &str,
    _request: &KernelInput,
    receipt: &HandleReceipt,
    source: &RoleRef,
    kind: &str,
    reviewer_assignment_id: &str,
    summary: &str,
    context_revision: i64,
    observed_at: &str,
    body: &serde_json::Value,
) -> Result<(), KernelError> {
    let Some(review_revision_id) = receipt.created_ids.get("review_revision") else {
        return Ok(());
    };
    if source.role != RoleKind::Reviewer || !matches!(kind, "approved" | "rejected") {
        return Err(invalid_receipt(
            "review revision is only valid for Reviewer verdict replies",
        ));
    }
    let worker_assignment_id = transaction
        .query_row(
            "SELECT parent_id FROM assignments WHERE id = ?1 AND mission_id = ?2",
            rusqlite::params![reviewer_assignment_id, mission_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|error| {
            sqlite_error(
                "sqlite_handle_failed",
                "load_review_parent_assignment",
                error,
            )
        })?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_receipt("review assignment is missing its Worker parent"))?;
    let references = body
        .get("references")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let refs_json = serde_json::to_string(&references).map_err(|error| KernelError {
        category: ErrorCategory::Internal,
        code: "review_references_serialize_failed".into(),
        message: "review references could not be serialized".into(),
        retryable: false,
        details: BTreeMap::from([("reason".into(), json!(error.to_string()))]),
    })?;
    let acknowledged = kind == "approved"
        && body
            .get("acknowledge_review")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
    let acknowledged_by_pm = i64::from(acknowledged);
    let acknowledged_at = acknowledged.then_some(observed_at);

    transaction
        .execute(
            "INSERT INTO review_revisions(
                 id, mission_id, reviewer_assignment_id, worker_assignment_id,
                 verdict, summary, refs_json, context_rev, acknowledged_by_pm,
                 created_at, acknowledged_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                review_revision_id,
                mission_id,
                reviewer_assignment_id,
                worker_assignment_id,
                kind,
                summary,
                refs_json,
                context_revision,
                acknowledged_by_pm,
                observed_at,
                acknowledged_at,
            ],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_review_revision", error))?;
    Ok(())
}

fn persist_review_notices(
    transaction: &Transaction<'_>,
    mission_id: &str,
    receipt: &HandleReceipt,
    reviewer_assignment_id: &str,
    summary: &str,
    context_revision: i64,
    observed_at: &str,
) -> Result<(), KernelError> {
    let Some(review_revision_id) = receipt.created_ids.get("review_revision") else {
        return Ok(());
    };
    let notice_body = format!(
        "Reviewer {}：{}（review_id={}）",
        assignment_state_name(
            receipt
                .assignment_state
                .ok_or_else(|| invalid_receipt("review notice is missing verdict state"))?,
        ),
        summary,
        review_revision_id,
    );
    let notices = [
        (
            created_id(receipt, "review_pm_notice_message")?,
            created_id(receipt, "review_pm_notice_outbox")?,
            "reviewer",
            "pm",
        ),
        (
            created_id(receipt, "review_worker_notice_message")?,
            created_id(receipt, "review_worker_notice_outbox")?,
            "pm",
            "worker",
        ),
    ];

    for (message_id, outbox_id, source_role, target_role) in notices {
        let effect = receipt
            .effect_intents
            .iter()
            .find(|effect| effect.effect_id == outbox_id)
            .ok_or_else(|| invalid_receipt("review notice is missing delivery effect"))?;
        let EffectIntentKind::DeliverPrompt {
            role,
            assignment_id,
            prompt,
        } = &effect.intent
        else {
            return Err(invalid_receipt(
                "review notice effect is not a delivery prompt",
            ));
        };
        if assignment_id.as_deref() != Some(reviewer_assignment_id)
            || role_storage_identity(role)? != target_role
            || prompt != &notice_body
        {
            return Err(invalid_receipt(
                "review notice effect does not match its durable message",
            ));
        }
        transaction
            .execute(
                "INSERT INTO messages(
                     id, mission_id, assignment_id, source_role, target_role, kind,
                     body, context_rev, in_reply_to, review_id, created_at
                 ) VALUES(?1, ?2, ?3, ?4, ?5, 'context', ?6, ?7, NULL, NULL, ?8)",
                rusqlite::params![
                    message_id,
                    mission_id,
                    reviewer_assignment_id,
                    source_role,
                    target_role,
                    notice_body,
                    context_revision,
                    observed_at,
                ],
            )
            .map_err(|error| {
                sqlite_error(
                    "sqlite_handle_failed",
                    "insert_review_notice_message",
                    error,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO outbox(
                     id, message_id, mission_id, target_role, status, attempts,
                     last_error, claimed_by, claimed_at, created_at, updated_at, delivered_at
                 ) VALUES(?1, ?2, ?3, ?4, 'queued', 0, '', '', NULL, ?5, ?5, NULL)",
                rusqlite::params![outbox_id, message_id, mission_id, target_role, observed_at],
            )
            .map_err(|error| {
                sqlite_error("sqlite_handle_failed", "insert_review_notice_outbox", error)
            })?;
    }
    Ok(())
}

fn persist_follow_up_assignment(
    transaction: &Transaction<'_>,
    mission_id: &str,
    receipt: &HandleReceipt,
    context_revision: i64,
    observed_at: &str,
) -> Result<(), KernelError> {
    let Some(assignment_id) = receipt.created_ids.get("follow_up_assignment") else {
        return Ok(());
    };
    let message_id = created_id(receipt, "follow_up_message")?;
    let outbox_id = created_id(receipt, "follow_up_outbox")?;
    let parent_id = receipt
        .relationships
        .get("parent_assignment")
        .ok_or_else(|| invalid_receipt("follow-up is missing parent assignment"))?;
    let effect = receipt
        .effect_intents
        .iter()
        .find(|effect| effect.effect_id == outbox_id)
        .ok_or_else(|| invalid_receipt("follow-up is missing delivery effect"))?;
    let EffectIntentKind::DeliverPrompt {
        role,
        assignment_id: effect_assignment_id,
        prompt,
    } = &effect.intent
    else {
        return Err(invalid_receipt("follow-up effect is not a delivery prompt"));
    };
    if effect_assignment_id.as_deref() != Some(assignment_id.as_str()) {
        return Err(invalid_receipt(
            "follow-up effect assignment does not match",
        ));
    }
    let target_role = role_storage_identity(role)?;
    let kind = match role.role {
        RoleKind::Reviewer => "review",
        RoleKind::Worker => "fix",
        _ => return Err(invalid_receipt("unsupported follow-up role")),
    };
    let review_round = i64::from(
        receipt
            .review_round
            .ok_or_else(|| invalid_receipt("follow-up is missing review round"))?,
    );

    transaction
        .execute(
            "INSERT INTO assignments(
                 id, mission_id, source_role, target_role, kind, summary, state,
                 parent_id, review_round, skills_json, replace_skills, review_id,
                 created_at, updated_at
             ) VALUES(?1, ?2, 'pm', ?3, ?4, ?5, 'queued', ?6, ?7, '[]', 0, NULL, ?8, ?8)",
            rusqlite::params![
                assignment_id,
                mission_id,
                target_role,
                kind,
                prompt,
                parent_id,
                review_round,
                observed_at,
            ],
        )
        .map_err(|error| {
            sqlite_error("sqlite_handle_failed", "insert_follow_up_assignment", error)
        })?;
    transaction
        .execute(
            "INSERT INTO messages(
                 id, mission_id, assignment_id, source_role, target_role, kind,
                 body, context_rev, in_reply_to, review_id, created_at
             ) VALUES(?1, ?2, ?3, 'pm', ?4, ?5, ?6, ?7, NULL, NULL, ?8)",
            rusqlite::params![
                message_id,
                mission_id,
                assignment_id,
                target_role,
                kind,
                prompt,
                context_revision,
                observed_at,
            ],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_follow_up_message", error))?;
    transaction
        .execute(
            "INSERT INTO outbox(
                 id, message_id, mission_id, target_role, status, attempts,
                 last_error, claimed_by, claimed_at, created_at, updated_at, delivered_at
             ) VALUES(?1, ?2, ?3, ?4, 'queued', 0, '', '', NULL, ?5, ?5, NULL)",
            rusqlite::params![outbox_id, message_id, mission_id, target_role, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_follow_up_outbox", error))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn persist_context_command(
    transaction: &rusqlite::Transaction<'_>,
    mission_id: &str,
    event_key: &str,
    request: &KernelInput,
    receipt: &HandleReceipt,
    source: &RoleRef,
    target: &RoleRef,
    body: &serde_json::Value,
) -> Result<(), KernelError> {
    let message_id = created_id(receipt, "message")?;
    let outbox_id = created_id(receipt, "outbox")?;
    let source_role = role_storage_identity(source)?;
    let target_role = role_storage_identity(target)?;
    let summary = body
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let revision = receipt
        .revision
        .ok_or_else(|| invalid_receipt("missing revision"))?;
    let revision =
        i64::try_from(revision).map_err(|_| invalid_receipt("revision exceeds SQLite range"))?;
    let observed_at = &request.decision_context.observed_at;

    transaction
        .execute(
            "INSERT INTO messages(
                 id, mission_id, assignment_id, source_role, target_role, kind,
                 body, context_rev, in_reply_to, review_id, created_at
             ) VALUES(?1, ?2, NULL, ?3, ?4, 'context', ?5, ?6, NULL, NULL, ?7)",
            rusqlite::params![
                message_id,
                mission_id,
                source_role,
                target_role,
                summary,
                revision,
                observed_at,
            ],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_message", error))?;
    transaction
        .execute(
            "INSERT INTO context_ledger(
                 mission_id, revision, kind, source_role, summary, refs_json,
                 assignment_id, created_at
             ) VALUES(?1, ?2, 'context', ?3, ?4, '[]', NULL, ?5)",
            rusqlite::params![mission_id, revision, source_role, summary, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_context_ledger", error))?;
    transaction
        .execute(
            "INSERT INTO processed_events(event_key, mission_id, created_at)
             VALUES(?1, ?2, ?3)",
            rusqlite::params![event_key, mission_id, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_processed_input", error))?;
    transaction
        .execute(
            "UPDATE team_missions SET context_rev = ?1, updated_at = ?2 WHERE mission_id = ?3",
            rusqlite::params![revision, observed_at, mission_id],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "update_mission", error))?;
    transaction
        .execute(
            "INSERT INTO outbox(
                 id, message_id, mission_id, target_role, status, attempts,
                 last_error, claimed_by, claimed_at, created_at, updated_at, delivered_at
             ) VALUES(?1, ?2, ?3, ?4, 'queued', 0, '', '', NULL, ?5, ?5, NULL)",
            rusqlite::params![outbox_id, message_id, mission_id, target_role, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_outbox", error))?;
    persist_effect_generations(transaction, mission_id, receipt, observed_at)?;
    Ok(())
}

fn persist_tool_job_request(
    transaction: &Transaction<'_>,
    mission_id: &str,
    event_key: &str,
    input: &KernelInput,
    receipt: &HandleReceipt,
    request: &ToolJobRequest,
) -> Result<(), KernelError> {
    let persisted_job_id = created_id(receipt, "tool_job")?;
    if persisted_job_id != request.job_id {
        return Err(invalid_receipt(
            "Tool Job receipt identity does not match the typed request",
        ));
    }
    let source_role = role_storage_identity(&request.source)?;
    let mode = match request.mode {
        ToolJobMode::Bounded => "bounded",
        ToolJobMode::Persistent => "persistent",
    };
    let argv_json = serde_json::to_string(&request.argv)
        .map_err(|error| json_persistence_error("serialize_tool_job_argv", error))?;
    let env_json = serde_json::to_string(&request.env)
        .map_err(|error| json_persistence_error("serialize_tool_job_env", error))?;
    let request_json = tool_job_request_fingerprint(request)?;
    let max_output_bytes = i64::try_from(request.max_output_bytes)
        .map_err(|_| invalid_receipt("Tool Job max output exceeds SQLite range"))?;
    let revision = receipt
        .revision
        .ok_or_else(|| invalid_receipt("missing Tool Job revision"))?;
    let revision =
        i64::try_from(revision).map_err(|_| invalid_receipt("revision exceeds SQLite range"))?;
    let observed_at = &input.decision_context.observed_at;

    transaction
        .execute(
            "INSERT INTO tool_jobs(
                 job_id, mission_id, assignment_id, source_role, mode, label,
                 argv_json, cwd, env_json, timeout_seconds, parallel,
                 max_output_bytes, request_json, state, coordination_dir,
                 created_at, updated_at
             ) VALUES(
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                 ?13, 'queued', '', ?14, ?14
             )",
            rusqlite::params![
                request.job_id,
                mission_id,
                request.assignment_id,
                source_role,
                mode,
                request.label,
                argv_json,
                request.cwd,
                env_json,
                request.timeout_seconds,
                request.parallel,
                max_output_bytes,
                request_json,
                observed_at,
            ],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_tool_job", error))?;
    transaction
        .execute(
            "INSERT INTO processed_events(event_key, mission_id, created_at)
             VALUES(?1, ?2, ?3)",
            rusqlite::params![event_key, mission_id, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_processed_input", error))?;
    transaction
        .execute(
            "UPDATE team_missions SET context_rev = ?1, updated_at = ?2
             WHERE mission_id = ?3",
            rusqlite::params![revision, observed_at, mission_id],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "update_mission", error))?;
    Ok(())
}

fn persist_tool_job_transition(
    transaction: &Transaction<'_>,
    mission_id: &str,
    event_key: &str,
    input: &KernelInput,
    receipt: &HandleReceipt,
    job_id: &str,
    transition: &ToolJobTransition,
) -> Result<(), KernelError> {
    let revision = receipt
        .revision
        .ok_or_else(|| invalid_receipt("missing Tool Job transition revision"))?;
    let revision =
        i64::try_from(revision).map_err(|_| invalid_receipt("revision exceeds SQLite range"))?;
    let observed_at = &input.decision_context.observed_at;
    let updated = match transition {
        ToolJobTransition::Started {
            pane_id,
            coordination_dir,
            request_path,
            stdout_path,
            stderr_path,
            result_path,
        } => transaction
            .execute(
                "UPDATE tool_jobs
                 SET state = 'running', pane_id = ?1, coordination_dir = ?2,
                     request_path = ?3, stdout_path = ?4, stderr_path = ?5,
                     result_path = ?6, started_at = ?7, updated_at = ?7
                 WHERE job_id = ?8 AND mission_id = ?9 AND state = 'queued'",
                rusqlite::params![
                    pane_id,
                    coordination_dir,
                    request_path,
                    stdout_path,
                    stderr_path,
                    result_path,
                    observed_at,
                    job_id,
                    mission_id,
                ],
            )
            .map_err(|error| sqlite_error("sqlite_handle_failed", "start_tool_job", error))?,
        ToolJobTransition::CancelRequested => {
            let state = transaction
                .query_row(
                    "SELECT state FROM tool_jobs WHERE job_id = ?1 AND mission_id = ?2",
                    rusqlite::params![job_id, mission_id],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| {
                    sqlite_error("sqlite_handle_failed", "load_tool_job_state", error)
                })?;
            match state.as_str() {
                "queued" => transaction
                    .execute(
                        "UPDATE tool_jobs
                         SET state = 'cancelled', cancelled_at = ?1,
                             finished_at = ?1, updated_at = ?1
                         WHERE job_id = ?2 AND mission_id = ?3 AND state = 'queued'",
                        rusqlite::params![observed_at, job_id, mission_id],
                    )
                    .map_err(|error| {
                        sqlite_error("sqlite_handle_failed", "cancel_queued_tool_job", error)
                    })?,
                "running" => transaction
                    .execute(
                        "UPDATE tool_jobs
                         SET state = 'cancelling', cancelled_at = ?1, updated_at = ?1
                         WHERE job_id = ?2 AND mission_id = ?3 AND state = 'running'",
                        rusqlite::params![observed_at, job_id, mission_id],
                    )
                    .map_err(|error| {
                        sqlite_error("sqlite_handle_failed", "cancel_running_tool_job", error)
                    })?,
                _ => {
                    return Err(invalid_persisted_state("Tool Job transition state", state));
                }
            }
        }
        ToolJobTransition::Completed { output } => {
            let state = transaction
                .query_row(
                    "SELECT state FROM tool_jobs WHERE job_id = ?1 AND mission_id = ?2",
                    rusqlite::params![job_id, mission_id],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| {
                    sqlite_error("sqlite_handle_failed", "load_tool_job_state", error)
                })?;
            let terminal_state = if state == "cancelling" {
                "cancelled"
            } else {
                tool_job_terminal_state_name(output.state)
            };
            let stdout_bytes = i64::try_from(output.stdout_bytes)
                .map_err(|_| invalid_receipt("Tool Job stdout bytes exceed SQLite range"))?;
            let stderr_bytes = i64::try_from(output.stderr_bytes)
                .map_err(|_| invalid_receipt("Tool Job stderr bytes exceed SQLite range"))?;
            let exit_code = output.exit_code.map(i64::from);
            let error = tail_chars(&output.error, 4_000);
            transaction
                .execute(
                    "UPDATE tool_jobs
                     SET state = ?1, stdout_path = ?2, stderr_path = ?3,
                         result_path = ?4, stdout_bytes = ?5, stderr_bytes = ?6,
                         stdout_truncated = ?7, stderr_truncated = ?8,
                         stdout_checksum = ?9, stderr_checksum = ?10,
                         exit_code = ?11, error = ?12, finished_at = ?13,
                         updated_at = ?13
                     WHERE job_id = ?14 AND mission_id = ?15
                       AND state IN ('running', 'cancelling')",
                    rusqlite::params![
                        terminal_state,
                        output.stdout_path,
                        output.stderr_path,
                        output.result_path,
                        stdout_bytes,
                        stderr_bytes,
                        output.stdout_truncated,
                        output.stderr_truncated,
                        output.stdout_checksum,
                        output.stderr_checksum,
                        exit_code,
                        error,
                        observed_at,
                        job_id,
                        mission_id,
                    ],
                )
                .map_err(|error| sqlite_error("sqlite_handle_failed", "complete_tool_job", error))?
        }
    };
    if updated != 1 {
        return Err(invalid_persisted_state(
            "Tool Job transition",
            format!("updated {updated} rows"),
        ));
    }
    transaction
        .execute(
            "INSERT INTO processed_events(event_key, mission_id, created_at)
             VALUES(?1, ?2, ?3)",
            rusqlite::params![event_key, mission_id, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_processed_input", error))?;
    transaction
        .execute(
            "UPDATE team_missions SET context_rev = ?1, updated_at = ?2
             WHERE mission_id = ?3",
            rusqlite::params![revision, observed_at, mission_id],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "update_mission", error))?;
    Ok(())
}

const fn tool_job_terminal_state_name(state: ToolJobTerminalState) -> &'static str {
    match state {
        ToolJobTerminalState::Succeeded => "succeeded",
        ToolJobTerminalState::Failed => "failed",
        ToolJobTerminalState::TimedOut => "timed_out",
        ToolJobTerminalState::Cancelled => "cancelled",
    }
}

fn tail_chars(value: &str, limit: usize) -> String {
    let mut chars = value.chars().rev().take(limit).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

fn restore_assignments(
    transaction: &rusqlite::Transaction<'_>,
    mission_id: &str,
    reducer: &mut MemoryCoordinationStore,
) -> Result<(), KernelError> {
    let mut statement = transaction
        .prepare(
            "SELECT id, target_role, kind, state, parent_id, review_round, updated_at
             FROM assignments
             WHERE mission_id = ?1
             ORDER BY created_at, id",
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_assignments", error))?;
    let rows = statement
        .query_map([mission_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_assignments", error))?;
    for row in rows {
        let (assignment_id, target_role, kind, state, parent_id, review_round, observed_at) =
            row.map_err(|error| sqlite_error("sqlite_handle_failed", "load_assignments", error))?;
        reducer.restore_assignment(
            assignment_id,
            parse_role_storage_identity(&target_role)?,
            kind,
            parse_assignment_state(&state)?,
            parent_id,
            u32::try_from(review_round)
                .map_err(|_| invalid_persisted_state("assignment review_round", review_round))?,
            observed_at,
        )?;
    }
    Ok(())
}

fn restore_context_ledger(
    transaction: &rusqlite::Transaction<'_>,
    mission_id: &str,
    reducer: &mut MemoryCoordinationStore,
) -> Result<(), KernelError> {
    let mut statement = transaction
        .prepare(
            "SELECT revision, assignment_id, source_role, kind, summary, created_at
             FROM context_ledger
             WHERE mission_id = ?1
             ORDER BY revision",
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_context_ledger", error))?;
    let rows = statement
        .query_map([mission_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_context_ledger", error))?;
    for row in rows {
        let (revision, assignment_id, source_role, kind, body, observed_at) = row
            .map_err(|error| sqlite_error("sqlite_handle_failed", "load_context_ledger", error))?;
        reducer.restore_ledger_entry(
            nonnegative_observation(revision, "context ledger revision")?,
            assignment_id,
            parse_role_storage_identity(&source_role)?,
            kind,
            body,
            observed_at,
        );
    }
    Ok(())
}

fn restore_role_generations(
    transaction: &rusqlite::Transaction<'_>,
    mission_id: &str,
    reducer: &mut MemoryCoordinationStore,
) -> Result<(), KernelError> {
    let mut statement = transaction
        .prepare(
            "SELECT role, launch_generation, health, updated_at
             FROM team_roles
             WHERE mission_id = ?1 AND launch_generation <> ''
             ORDER BY role",
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_role_generations", error))?;
    let rows = statement
        .query_map([mission_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_role_generations", error))?;
    for row in rows {
        let (role_identity, generation, health, updated_at) = row.map_err(|error| {
            sqlite_error("sqlite_handle_failed", "load_role_generations", error)
        })?;
        let canonical_role = parse_role_storage_identity(&role_identity)?;
        let canonical_identity = role_storage_identity(&canonical_role)?;
        let generation = crate::Generation::new(generation.clone())
            .map_err(|_| invalid_persisted_state("launch generation", generation))?;
        reducer.restore_role_generation(canonical_identity.clone(), generation.clone());
        reducer.restore_role_state(
            canonical_identity,
            generation,
            persisted_role_state(&health)?,
            updated_at,
        );
    }
    Ok(())
}

fn persisted_role_state(health: &str) -> Result<RoleState, KernelError> {
    match health {
        "pending" => Ok(RoleState::Pending),
        "starting" | "launching" => Ok(RoleState::Starting),
        "ready" | "idle" | "working" | "running" | "blocked" => Ok(RoleState::Ready),
        "done" | "exited" | "stopped" | "missing" | "unknown" => Ok(RoleState::Stopped),
        "failed" | "error" | "invalid" => Ok(RoleState::Failed),
        value => Err(invalid_persisted_state("role health", value)),
    }
}

fn restore_pending_effects(
    transaction: &rusqlite::Transaction<'_>,
    mission_id: &str,
    reducer: &mut MemoryCoordinationStore,
) -> Result<(), KernelError> {
    let mut statement = transaction
        .prepare(
            "SELECT outbox.id, messages.assignment_id, outbox.target_role,
                    team_roles.launch_generation, messages.body
             FROM outbox
             JOIN messages ON messages.id = outbox.message_id
             JOIN team_roles
               ON team_roles.mission_id = outbox.mission_id
              AND team_roles.role = outbox.target_role
             WHERE outbox.mission_id = ?1
               AND outbox.status IN ('queued', 'retry', 'pending', 'sending')
             ORDER BY outbox.created_at, outbox.id",
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_pending_effects", error))?;
    let rows = statement
        .query_map([mission_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_pending_effects", error))?;
    for row in rows {
        let (effect_id, assignment_id, target_role, generation, prompt) = row
            .map_err(|error| sqlite_error("sqlite_handle_failed", "load_pending_effects", error))?;
        let generation = crate::Generation::new(generation.clone())
            .map_err(|_| invalid_persisted_state("launch generation", generation))?;
        // A PM-bound outbox is a reply notice, not a new assignment dispatch:
        // it must deliver as a plain notice instead of re-activating the
        // original assignment (which is already completed). PM is never a
        // dispatch target, so target_role == "pm" is unambiguous.
        let effect_assignment_id = if target_role == "pm" {
            None
        } else {
            assignment_id
        };
        reducer.restore_effect(
            effect_id,
            parse_role_storage_identity(&target_role)?,
            effect_assignment_id,
            generation,
            prompt,
        )?;
    }
    Ok(())
}

fn restore_tool_jobs(
    transaction: &Transaction<'_>,
    mission_id: &str,
    reducer: &mut MemoryCoordinationStore,
) -> Result<(), KernelError> {
    struct PersistedToolJob {
        job_id: String,
        assignment_id: String,
        source_role: String,
        mode: String,
        label: String,
        argv_json: String,
        cwd: String,
        env_json: String,
        timeout_seconds: f64,
        parallel: bool,
        max_output_bytes: i64,
        request_json: String,
        state: String,
        pane_id: String,
        coordination_dir: String,
        request_path: String,
        stdout_path: String,
        stderr_path: String,
        result_path: String,
        stdout_bytes: i64,
        stderr_bytes: i64,
        stdout_truncated: bool,
        stderr_truncated: bool,
        stdout_checksum: String,
        stderr_checksum: String,
        exit_code: Option<i64>,
        error: String,
        created_at: String,
        started_at: Option<String>,
        finished_at: Option<String>,
        cancelled_at: Option<String>,
        updated_at: String,
    }

    let mut statement = transaction
        .prepare(
            "SELECT job_id, assignment_id, source_role, mode, label, argv_json, cwd,
                    env_json, timeout_seconds, parallel, max_output_bytes, request_json,
                    state, pane_id, coordination_dir, request_path, stdout_path,
                    stderr_path, result_path, stdout_bytes, stderr_bytes,
                    stdout_truncated, stderr_truncated, stdout_checksum,
                    stderr_checksum, exit_code, error, created_at, started_at,
                    finished_at, cancelled_at, updated_at
             FROM tool_jobs WHERE mission_id = ?1 ORDER BY created_at, job_id",
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_tool_jobs", error))?;
    let rows = statement
        .query_map([mission_id], |row| {
            Ok(PersistedToolJob {
                job_id: row.get(0)?,
                assignment_id: row.get(1)?,
                source_role: row.get(2)?,
                mode: row.get(3)?,
                label: row.get(4)?,
                argv_json: row.get(5)?,
                cwd: row.get(6)?,
                env_json: row.get(7)?,
                timeout_seconds: row.get(8)?,
                parallel: row.get(9)?,
                max_output_bytes: row.get(10)?,
                request_json: row.get(11)?,
                state: row.get(12)?,
                pane_id: row.get(13)?,
                coordination_dir: row.get(14)?,
                request_path: row.get(15)?,
                stdout_path: row.get(16)?,
                stderr_path: row.get(17)?,
                result_path: row.get(18)?,
                stdout_bytes: row.get(19)?,
                stderr_bytes: row.get(20)?,
                stdout_truncated: row.get(21)?,
                stderr_truncated: row.get(22)?,
                stdout_checksum: row.get(23)?,
                stderr_checksum: row.get(24)?,
                exit_code: row.get(25)?,
                error: row.get(26)?,
                created_at: row.get(27)?,
                started_at: row.get(28)?,
                finished_at: row.get(29)?,
                cancelled_at: row.get(30)?,
                updated_at: row.get(31)?,
            })
        })
        .map_err(|error| sqlite_error("sqlite_handle_failed", "load_tool_jobs", error))?;
    for row in rows {
        let row =
            row.map_err(|error| sqlite_error("sqlite_handle_failed", "load_tool_jobs", error))?;
        let argv = serde_json::from_str::<Vec<String>>(&row.argv_json)
            .map_err(|error| json_restore_error("tool_jobs.argv_json", error))?;
        let env = serde_json::from_str::<BTreeMap<String, String>>(&row.env_json)
            .map_err(|error| json_restore_error("tool_jobs.env_json", error))?;
        let mode = match row.mode.as_str() {
            "bounded" => ToolJobMode::Bounded,
            "persistent" => ToolJobMode::Persistent,
            _ => return Err(invalid_persisted_state("Tool Job mode", row.mode)),
        };
        let max_output_bytes = nonnegative_observation(row.max_output_bytes, "max_output_bytes")?;
        let stdout_bytes = nonnegative_observation(row.stdout_bytes, "stdout_bytes")?;
        let stderr_bytes = nonnegative_observation(row.stderr_bytes, "stderr_bytes")?;
        let terminal_state = match row.state.as_str() {
            "succeeded" => Some(ToolJobTerminalState::Succeeded),
            "failed" => Some(ToolJobTerminalState::Failed),
            "timed_out" => Some(ToolJobTerminalState::TimedOut),
            "cancelled" => Some(ToolJobTerminalState::Cancelled),
            "queued" | "running" | "cancelling" => None,
            _ => return Err(invalid_persisted_state("Tool Job state", row.state)),
        };
        let exit_code = row
            .exit_code
            .map(i32::try_from)
            .transpose()
            .map_err(|_| invalid_persisted_state("Tool Job exit code", "outside i32"))?;
        let output = terminal_state.map(|state| ToolJobOutputMetadata {
            state,
            exit_code,
            stdout_path: row.stdout_path.clone(),
            stderr_path: row.stderr_path.clone(),
            result_path: row.result_path.clone(),
            stdout_bytes,
            stderr_bytes,
            stdout_truncated: row.stdout_truncated,
            stderr_truncated: row.stderr_truncated,
            stdout_checksum: row.stdout_checksum,
            stderr_checksum: row.stderr_checksum,
            error: row.error,
        });
        reducer.restore_tool_job(
            ToolJobRequest {
                job_id: row.job_id,
                assignment_id: row.assignment_id,
                source: parse_role_storage_identity(&row.source_role)?,
                mode,
                label: row.label,
                argv,
                cwd: row.cwd,
                env,
                timeout_seconds: row.timeout_seconds,
                parallel: row.parallel,
                max_output_bytes,
            },
            row.request_json,
            &row.state,
            row.pane_id,
            row.coordination_dir,
            row.request_path,
            row.stdout_path,
            row.stderr_path,
            row.result_path,
            output,
            row.created_at,
            row.started_at,
            row.finished_at,
            row.cancelled_at,
            row.updated_at,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn persist_assignment_command(
    transaction: &Transaction<'_>,
    mission_id: &str,
    context_revision: u64,
    event_key: &str,
    request: &KernelInput,
    receipt: &HandleReceipt,
    source: &RoleRef,
    target: &RoleRef,
    kind: &str,
    body: &serde_json::Value,
) -> Result<(), KernelError> {
    let assignment_id = created_id(receipt, "assignment")?;
    let message_id = created_id(receipt, "message")?;
    let outbox_id = created_id(receipt, "outbox")?;
    let source_role = role_storage_identity(source)?;
    let target_role = role_storage_identity(target)?;
    let summary = body
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let context_revision = i64::try_from(context_revision)
        .map_err(|_| invalid_receipt("context revision exceeds SQLite range"))?;
    let review_round = i64::from(receipt.review_round.unwrap_or_default());
    let assignment_state = assignment_state_name(
        receipt
            .assignment_state
            .ok_or_else(|| invalid_receipt("missing assignment state"))?,
    );
    let observed_at = &request.decision_context.observed_at;

    transaction
        .execute(
            "INSERT INTO assignments(
                 id, mission_id, source_role, target_role, kind, summary, state,
                 parent_id, review_round, skills_json, replace_skills, review_id,
                 created_at, updated_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, '[]', 0, NULL, ?9, ?9)",
            rusqlite::params![
                assignment_id,
                mission_id,
                source_role,
                target_role,
                kind,
                summary,
                assignment_state,
                review_round,
                observed_at,
            ],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_assignment", error))?;
    transaction
        .execute(
            "INSERT INTO messages(
                 id, mission_id, assignment_id, source_role, target_role, kind,
                 body, context_rev, in_reply_to, review_id, created_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, ?9)",
            rusqlite::params![
                message_id,
                mission_id,
                assignment_id,
                source_role,
                target_role,
                kind,
                summary,
                context_revision,
                observed_at,
            ],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_message", error))?;
    transaction
        .execute(
            "INSERT INTO processed_events(event_key, mission_id, created_at)
             VALUES(?1, ?2, ?3)",
            rusqlite::params![event_key, mission_id, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_processed_input", error))?;
    transaction
        .execute(
            "INSERT INTO outbox(
                 id, message_id, mission_id, target_role, status, attempts,
                 last_error, claimed_by, claimed_at, created_at, updated_at, delivered_at
             ) VALUES(?1, ?2, ?3, ?4, 'queued', 0, '', '', NULL, ?5, ?5, NULL)",
            rusqlite::params![outbox_id, message_id, mission_id, target_role, observed_at],
        )
        .map_err(|error| sqlite_error("sqlite_handle_failed", "insert_outbox", error))?;
    persist_effect_generations(transaction, mission_id, receipt, observed_at)?;
    Ok(())
}

fn persist_effect_generations(
    transaction: &Transaction<'_>,
    mission_id: &str,
    receipt: &HandleReceipt,
    observed_at: &str,
) -> Result<(), KernelError> {
    for effect in &receipt.effect_intents {
        let role = match &effect.intent {
            EffectIntentKind::DeliverPrompt { role, .. }
            | EffectIntentKind::EnsureRoleReady { role, .. }
            | EffectIntentKind::ObserveRole { role } => role,
            EffectIntentKind::RefreshMissionMirror | EffectIntentKind::RecordEvidence { .. } => {
                continue;
            }
        };
        let role_identity = role_storage_identity(role)?;
        transaction
            .execute(
                "INSERT INTO team_roles(
                     mission_id, role, provider, launch_generation, health, updated_at
                 ) VALUES(?1, ?2, 'codex', ?3, 'pending', ?4)
                 ON CONFLICT(mission_id, role) DO UPDATE SET
                     launch_generation = excluded.launch_generation,
                     updated_at = excluded.updated_at",
                rusqlite::params![
                    mission_id,
                    role_identity,
                    effect.generation.as_str(),
                    observed_at,
                ],
            )
            .map_err(|error| {
                sqlite_error("sqlite_handle_failed", "persist_effect_generation", error)
            })?;
    }
    Ok(())
}

fn ensure_schema_v3_effects(
    receipt: &HandleReceipt,
    input: &HandleInput,
) -> Result<(), KernelError> {
    if let HandleInput::RoleLaunchRequest {
        launch_id,
        role,
        generation,
        attach_mode,
        ..
    } = input
    {
        if receipt.effect_intents.len() == 1
            && receipt.effect_intents.iter().all(|effect| {
                effect.effect_id == *launch_id
                    && effect.generation == *generation
                    && matches!(
                        &effect.intent,
                        EffectIntentKind::EnsureRoleReady {
                            role: effect_role,
                            attach_mode: effect_attach_mode,
                        } if effect_role == role && effect_attach_mode == attach_mode
                    )
            })
        {
            return Ok(());
        }
    }
    let durable_outbox_ids = [
        receipt.created_ids.get("outbox"),
        receipt.created_ids.get("follow_up_outbox"),
        receipt.created_ids.get("review_pm_notice_outbox"),
        receipt.created_ids.get("review_worker_notice_outbox"),
    ];
    if receipt.effect_intents.iter().all(|effect| {
        matches!(&effect.intent, EffectIntentKind::DeliverPrompt { .. })
            && durable_outbox_ids
                .iter()
                .flatten()
                .any(|outbox_id| *outbox_id == &effect.effect_id)
    }) {
        return Ok(());
    }
    Err(KernelError {
        category: ErrorCategory::Contract,
        code: "effect_not_representable_in_schema_v3".into(),
        message: "effect intent has no durable representation in SQLite schema v3".into(),
        retryable: false,
        details: BTreeMap::new(),
    })
}

fn handle_input_id(input: &HandleInput) -> &str {
    match input {
        HandleInput::Command { command_id, .. } => command_id,
        HandleInput::TeamEvent { event_id, .. } => event_id,
        HandleInput::RoleLaunchRequest { launch_id, .. } => launch_id,
        HandleInput::RoleObservation { observation_id, .. } => observation_id,
        HandleInput::EffectResult { result } => &result.effect_id,
        HandleInput::ToolJobRequest { request } => &request.job_id,
        HandleInput::ToolJobTransition { transition_id, .. } => transition_id,
    }
}

fn duplicate_receipt(input_id: String, revision: u64) -> HandleReceipt {
    HandleReceipt {
        input_id,
        disposition: HandleDisposition::Duplicate,
        revision: Some(revision),
        effect_intents: Vec::new(),
        created_ids: BTreeMap::new(),
        relationships: BTreeMap::new(),
        review_round: None,
        assignment_state: None,
        review_limit_reached: false,
        error: None,
    }
}

fn created_id<'a>(receipt: &'a HandleReceipt, key: &str) -> Result<&'a str, KernelError> {
    receipt
        .created_ids
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| invalid_receipt(&format!("missing created ID {key}")))
}

fn invalid_receipt(reason: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Internal,
        code: "invalid_handle_receipt".into(),
        message: "domain reducer returned a receipt that cannot be persisted".into(),
        retryable: false,
        details: BTreeMap::from([("reason".into(), json!(reason))]),
    }
}

fn now_ms() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(0)
}

fn role_storage_identity(role: &RoleRef) -> Result<String, KernelError> {
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
            Some(_) => Err(invalid_role_storage_identity(role)),
        },
        _ => Err(invalid_role_storage_identity(role)),
    }
}

fn parse_role_storage_identity(identity: &str) -> Result<RoleRef, KernelError> {
    match identity {
        "pm" => Ok(RoleRef {
            role: RoleKind::Pm,
            instance: None,
        }),
        "worker" => Ok(RoleRef {
            role: RoleKind::Worker,
            instance: None,
        }),
        "reviewer" => Ok(RoleRef {
            role: RoleKind::Reviewer,
            instance: None,
        }),
        "scout" => Ok(RoleRef {
            role: RoleKind::Scout,
            instance: None,
        }),
        scout if scout.starts_with("scout-") && scout.len() > "scout-".len() => Ok(RoleRef {
            role: RoleKind::Scout,
            instance: Some(scout.to_owned()),
        }),
        _ => Err(invalid_persisted_state("role identity", identity)),
    }
}

fn parse_assignment_state(state: &str) -> Result<AssignmentState, KernelError> {
    match state {
        "queued" => Ok(AssignmentState::Queued),
        "active" => Ok(AssignmentState::Active),
        "completed" => Ok(AssignmentState::Completed),
        "approved" => Ok(AssignmentState::Approved),
        "rejected" => Ok(AssignmentState::Rejected),
        "blocked" => Ok(AssignmentState::Blocked),
        _ => Err(invalid_persisted_state("assignment state", state)),
    }
}

fn invalid_persisted_state(field: &str, value: impl serde::Serialize) -> KernelError {
    KernelError {
        category: ErrorCategory::Contract,
        code: "invalid_persisted_state".into(),
        message: format!("schema v3 contains an invalid {field}"),
        retryable: false,
        details: BTreeMap::from([
            ("field".into(), json!(field)),
            ("value".into(), json!(value)),
        ]),
    }
}

fn invalid_role_storage_identity(role: &RoleRef) -> KernelError {
    KernelError {
        category: ErrorCategory::Contract,
        code: "invalid_role_identity".into(),
        message: "role reference cannot be persisted with the Team Mission identity contract"
            .into(),
        retryable: false,
        details: BTreeMap::from([("role".into(), json!(role))]),
    }
}

const fn assignment_state_name(state: AssignmentState) -> &'static str {
    match state {
        AssignmentState::Queued => "queued",
        AssignmentState::Active => "active",
        AssignmentState::Completed => "completed",
        AssignmentState::Approved => "approved",
        AssignmentState::Rejected => "rejected",
        AssignmentState::Blocked => "blocked",
    }
}

const fn role_state_name(state: RoleState) -> &'static str {
    match state {
        RoleState::Pending => "pending",
        RoleState::Starting => "starting",
        RoleState::Ready => "ready",
        RoleState::Stopped => "stopped",
        RoleState::Failed => "failed",
    }
}

fn query_mission_count(
    connection: &Connection,
    table: &str,
    mission_id: &str,
) -> Result<u64, KernelError> {
    let sql = format!("SELECT COUNT(*) FROM \"{table}\" WHERE mission_id = ?1");
    let count = connection
        .query_row(&sql, [mission_id], |row| row.get::<_, i64>(0))
        .map_err(|error| {
            sqlite_error(
                "sqlite_observation_failed",
                &format!("count_{table}"),
                error,
            )
        })?;
    nonnegative_observation(count, &format!("count_{table}"))
}

fn open_read_only_connection(database: &Path) -> Result<Connection, KernelError> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| sqlite_error("sqlite_inspect_failed", "open_read_only", error))?;
    connection
        .execute_batch("PRAGMA query_only = ON;")
        .map_err(|error| sqlite_error("sqlite_inspect_failed", "query_only", error))?;
    let version = connection
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| incompatible_schema_error(None, error.to_string()))?;
    if version != SQLITE_SCHEMA_VERSION {
        return Err(incompatible_schema_error(
            Some(&version),
            "unsupported schema version",
        ));
    }
    verify_schema(&connection)?;
    Ok(connection)
}

fn observe_sqlite_database(
    database: &Path,
    mission_id: &str,
) -> Result<StoreObservation, KernelError> {
    let connection = open_read_only_connection(database)?;
    let revision = connection
        .query_row(
            "SELECT context_rev FROM team_missions WHERE mission_id = ?1",
            [mission_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_observation_failed", "mission", error))?
        .ok_or_else(|| mission_not_found(mission_id))?;
    Ok(StoreObservation {
        schema_version: SQLITE_SCHEMA_VERSION.into(),
        mission_id: mission_id.into(),
        assignment_count: query_mission_count(&connection, "assignments", mission_id)?,
        message_count: query_mission_count(&connection, "messages", mission_id)?,
        outbox_count: query_mission_count(&connection, "outbox", mission_id)?,
        revision: nonnegative_observation(revision, "context_rev")?,
    })
}

fn inspect_sqlite_database(
    database: &Path,
    mission_id: &str,
    query: InspectQuery,
) -> Result<MissionView, KernelError> {
    let connection = open_read_only_connection(database)?;
    sqlite_inspect_projection(&connection, mission_id, query)
}

fn mission_not_found(mission_id: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Domain,
        code: "mission_not_found".into(),
        message: "Mission is not present in the coordination store".into(),
        retryable: false,
        details: BTreeMap::from([("mission_id".into(), json!(mission_id))]),
    }
}

fn nonnegative_observation(value: i64, field: &str) -> Result<u64, KernelError> {
    u64::try_from(value).map_err(|_| KernelError {
        category: ErrorCategory::Contract,
        code: "invalid_coordination_value".into(),
        message: "coordination observation contained a negative value".into(),
        retryable: false,
        details: BTreeMap::from([
            ("field".into(), json!(field)),
            ("value".into(), json!(value)),
        ]),
    })
}

fn sqlite_capability_unavailable(operation: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Operation,
        code: "sqlite_capability_unavailable".into(),
        message: "SQLite coordination operation is not implemented in the current standalone phase"
            .into(),
        retryable: false,
        details: BTreeMap::from([
            ("operation".into(), json!(operation)),
            ("schema_version".into(), json!(SQLITE_SCHEMA_VERSION)),
        ]),
    }
}

fn verify_schema(connection: &Connection) -> Result<(), KernelError> {
    for (table, expected_columns) in REQUIRED_TABLES {
        let exists = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| sqlite_error("sqlite_schema_read_failed", "table_exists", error))?;
        if !exists {
            return Err(schema_object_error("missing_table", table, None));
        }

        let sql = format!("PRAGMA table_info(\"{table}\")");
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| sqlite_error("sqlite_schema_read_failed", "table_info", error))?;
        let actual_columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| sqlite_error("sqlite_schema_read_failed", "table_info", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("sqlite_schema_read_failed", "table_info", error))?;
        if actual_columns != *expected_columns {
            return Err(schema_object_error(
                "column_mismatch",
                table,
                Some((&actual_columns, expected_columns)),
            ));
        }
    }

    for (index, expected_table, expected_columns) in REQUIRED_INDEXES {
        let actual_table = connection
            .query_row(
                "SELECT tbl_name FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [index],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| sqlite_error("sqlite_schema_read_failed", "index_exists", error))?;
        let Some(actual_table) = actual_table else {
            return Err(schema_object_error("missing_index", index, None));
        };

        let sql = format!("PRAGMA index_info(\"{index}\")");
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| sqlite_error("sqlite_schema_read_failed", "index_info", error))?;
        let actual_columns = statement
            .query_map([], |row| row.get::<_, String>(2))
            .map_err(|error| sqlite_error("sqlite_schema_read_failed", "index_info", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("sqlite_schema_read_failed", "index_info", error))?;
        if actual_table != *expected_table || actual_columns != *expected_columns {
            let mut error = schema_object_error(
                "index_mismatch",
                index,
                Some((&actual_columns, expected_columns)),
            );
            error
                .details
                .insert("actual_table".into(), json!(actual_table));
            error
                .details
                .insert("expected_table".into(), json!(expected_table));
            return Err(error);
        }
    }

    Ok(())
}

fn schema_object_error(
    kind: &str,
    object: &str,
    columns: Option<(&[String], &[&str])>,
) -> KernelError {
    let mut error = incompatible_schema_error(
        Some(SQLITE_SCHEMA_VERSION),
        "schema object does not match the frozen v3 contract",
    );
    error.details.insert(kind.into(), json!(object));
    if let Some((actual, expected)) = columns {
        error.details.insert("actual_columns".into(), json!(actual));
        error
            .details
            .insert("expected_columns".into(), json!(expected));
    }
    error
}

fn path_policy_error(code: &str, path: &Path, reason: impl Into<String>) -> KernelError {
    KernelError {
        category: ErrorCategory::Contract,
        code: code.into(),
        message: "SQLite path is outside the permitted temporary fixture boundary".into(),
        retryable: false,
        details: BTreeMap::from([
            ("path".into(), json!(path)),
            ("reason".into(), json!(reason.into())),
        ]),
    }
}

fn incompatible_schema_error(version: Option<&str>, reason: impl Into<String>) -> KernelError {
    KernelError {
        category: ErrorCategory::Contract,
        code: "incompatible_schema".into(),
        message: "SQLite database does not satisfy the Team Mission schema v3 contract".into(),
        retryable: false,
        details: BTreeMap::from([
            ("expected".into(), json!(SQLITE_SCHEMA_VERSION)),
            ("actual".into(), json!(version)),
            ("reason".into(), json!(reason.into())),
        ]),
    }
}

fn sqlite_error(code: &str, operation: &str, error: rusqlite::Error) -> KernelError {
    let busy = matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    );
    KernelError {
        category: ErrorCategory::Infrastructure,
        code: if busy { "sqlite_busy" } else { code }.into(),
        message: "SQLite operation failed".into(),
        retryable: busy,
        details: BTreeMap::from([
            ("operation".into(), json!(operation)),
            ("reason".into(), json!(error.to_string())),
        ]),
    }
}

fn json_persistence_error(operation: &str, error: serde_json::Error) -> KernelError {
    KernelError {
        category: ErrorCategory::Internal,
        code: "json_persistence_failed".into(),
        message: "typed coordination data could not be serialized for SQLite".into(),
        retryable: false,
        details: BTreeMap::from([
            ("operation".into(), json!(operation)),
            ("reason".into(), json!(error.to_string())),
        ]),
    }
}

fn json_restore_error(field: &str, error: serde_json::Error) -> KernelError {
    KernelError {
        category: ErrorCategory::Contract,
        code: "invalid_persisted_json".into(),
        message: "persisted coordination JSON does not match the typed schema".into(),
        retryable: false,
        details: BTreeMap::from([
            ("field".into(), json!(field)),
            ("reason".into(), json!(error.to_string())),
        ]),
    }
}

fn sqlite_inspect_projection(
    connection: &Connection,
    mission_id: &str,
    query: InspectQuery,
) -> Result<MissionView, KernelError> {
    let mission = sqlite_inspect_mission_row(connection, mission_id)?;
    let revision = mission["context_rev"].as_u64();
    let data = match query {
        InspectQuery::Mission => mission,
        InspectQuery::Status => json!({
            "mission": mission,
            "roles": sqlite_inspect_roles(connection, mission_id)?,
            "assignments": sqlite_inspect_assignments(connection, mission_id)?,
            "tool_jobs": sqlite_inspect_tool_jobs(connection, mission_id)?,
            "queued": sqlite_inspect_count(
                connection,
                "SELECT COUNT(*) FROM outbox WHERE mission_id = ?1 AND status IN ('queued', 'retry', 'sending')",
                mission_id,
                "status_queued",
            )?,
        }),
        InspectQuery::Inbox { role } => {
            let target_role = role.as_ref().map(role_storage_identity).transpose()?;
            json!({
                "messages": sqlite_inspect_messages(
                    connection,
                    mission_id,
                    target_role.as_deref(),
                    None,
                )?,
            })
        }
        InspectQuery::AssignmentThread { assignment_id } => json!({
            "assignment": sqlite_inspect_assignment(connection, mission_id, &assignment_id)?,
            "messages": sqlite_inspect_messages(
                connection,
                mission_id,
                None,
                Some(&assignment_id),
            )?,
        }),
        InspectQuery::Diagnostics => json!({
            "counts": {
                "assignments": sqlite_inspect_table_count(connection, "assignments", mission_id)?,
                "messages": sqlite_inspect_table_count(connection, "messages", mission_id)?,
                "outbox": sqlite_inspect_table_count(connection, "outbox", mission_id)?,
                "context_ledger": sqlite_inspect_table_count(connection, "context_ledger", mission_id)?,
                "processed_events": sqlite_inspect_table_count(connection, "processed_events", mission_id)?,
                "tool_jobs": sqlite_inspect_table_count(connection, "tool_jobs", mission_id)?,
            },
        }),
    };
    Ok(MissionView { revision, data })
}

fn sqlite_inspect_mission_row(
    connection: &Connection,
    mission_id: &str,
) -> Result<Value, KernelError> {
    connection
        .query_row(
            "SELECT mission_id, brief, template, agent_profile_id,
                    agent_profile_version, context_rev, created_at, updated_at
             FROM team_missions WHERE mission_id = ?1",
            [mission_id],
            |row| {
                Ok(json!({
                    "mission_id": row.get::<_, String>(0)?,
                    "brief": row.get::<_, String>(1)?,
                    "template": row.get::<_, String>(2)?,
                    "agent_profile_id": row.get::<_, String>(3)?,
                    "agent_profile_version": row.get::<_, i64>(4)?,
                    "context_rev": row.get::<_, i64>(5)?,
                    "created_at": row.get::<_, String>(6)?,
                    "updated_at": row.get::<_, String>(7)?,
                }))
            },
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_inspect_failed", "mission", error))?
        .ok_or_else(|| mission_not_found(mission_id))
}

fn sqlite_inspect_roles(
    connection: &Connection,
    mission_id: &str,
) -> Result<Vec<Value>, KernelError> {
    let mut statement = connection
        .prepare(
            "SELECT role, provider, model, thinking, permission_policy,
                    profile_id, profile_version, config_digest, launch_generation,
                    health, pane_id, terminal_id, last_seen_rev
             FROM team_roles WHERE mission_id = ?1 ORDER BY role",
        )
        .map_err(|error| sqlite_error("sqlite_inspect_failed", "prepare_roles", error))?;
    let rows = statement
        .query_map([mission_id], |row| {
            Ok(json!({
                "role": row.get::<_, String>(0)?,
                "provider": row.get::<_, String>(1)?,
                "model": row.get::<_, String>(2)?,
                "thinking": row.get::<_, String>(3)?,
                "permission_policy": row.get::<_, String>(4)?,
                "profile_id": row.get::<_, String>(5)?,
                "profile_version": row.get::<_, i64>(6)?,
                "config_digest": row.get::<_, String>(7)?,
                "launch_generation": row.get::<_, String>(8)?,
                "health": row.get::<_, String>(9)?,
                "pane_id": row.get::<_, String>(10)?,
                "terminal_id": row.get::<_, String>(11)?,
                "last_seen_rev": row.get::<_, i64>(12)?,
            }))
        })
        .map_err(|error| sqlite_error("sqlite_inspect_failed", "query_roles", error))?;
    let values = collect_inspect_rows(rows, "roles")?;
    Ok(values)
}

fn sqlite_inspect_assignments(
    connection: &Connection,
    mission_id: &str,
) -> Result<Vec<Value>, KernelError> {
    let mut statement = connection
        .prepare(
            "SELECT id, source_role, target_role, kind, state, review_round, summary
             FROM assignments WHERE mission_id = ?1 ORDER BY created_at DESC LIMIT 20",
        )
        .map_err(|error| sqlite_error("sqlite_inspect_failed", "prepare_assignments", error))?;
    let rows = statement
        .query_map([mission_id], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "source_role": row.get::<_, String>(1)?,
                "target_role": row.get::<_, String>(2)?,
                "kind": row.get::<_, String>(3)?,
                "state": row.get::<_, String>(4)?,
                "review_round": row.get::<_, i64>(5)?,
                "summary": row.get::<_, String>(6)?,
            }))
        })
        .map_err(|error| sqlite_error("sqlite_inspect_failed", "query_assignments", error))?;
    let values = collect_inspect_rows(rows, "assignments")?;
    Ok(values)
}

fn sqlite_inspect_assignment(
    connection: &Connection,
    mission_id: &str,
    assignment_id: &str,
) -> Result<Value, KernelError> {
    connection
        .query_row(
            "SELECT id, source_role, target_role, kind, summary, state, parent_id,
                    review_round, skills_json, replace_skills, review_id, created_at, updated_at
             FROM assignments WHERE mission_id = ?1 AND id = ?2",
            [mission_id, assignment_id],
            |row| {
                let skills_json = row.get::<_, String>(8)?;
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "source_role": row.get::<_, String>(1)?,
                    "target_role": row.get::<_, String>(2)?,
                    "kind": row.get::<_, String>(3)?,
                    "summary": row.get::<_, String>(4)?,
                    "state": row.get::<_, String>(5)?,
                    "parent_id": row.get::<_, Option<String>>(6)?,
                    "review_round": row.get::<_, i64>(7)?,
                    "skills": serde_json::from_str::<Value>(&skills_json).unwrap_or_else(|_| json!([])),
                    "replace_skills": row.get::<_, i64>(9)? != 0,
                    "review_id": row.get::<_, Option<String>>(10)?,
                    "created_at": row.get::<_, String>(11)?,
                    "updated_at": row.get::<_, String>(12)?,
                }))
            },
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_inspect_failed", "assignment_thread", error))?
        .ok_or_else(|| KernelError {
            category: ErrorCategory::Domain,
            code: "assignment_not_found".into(),
            message: "Assignment is not present in the Mission".into(),
            retryable: false,
            details: BTreeMap::from([("assignment_id".into(), json!(assignment_id))]),
        })
}

fn sqlite_inspect_messages(
    connection: &Connection,
    mission_id: &str,
    target_role: Option<&str>,
    assignment_id: Option<&str>,
) -> Result<Vec<Value>, KernelError> {
    let (filter, parameter) = match (target_role, assignment_id) {
        (Some(role), None) => ("m.target_role = ?2", role),
        (None, Some(id)) => ("m.assignment_id = ?2", id),
        (None, None) => ("?2 = ?2", ""),
        (Some(_), Some(_)) => unreachable!("inspect has one message filter"),
    };
    let sql = format!(
        "SELECT m.id, m.assignment_id, m.source_role, m.target_role, m.kind,
                m.body, m.context_rev, m.in_reply_to, m.review_id, m.created_at,
                o.id, o.status, o.attempts, o.last_error, o.claimed_by,
                o.claimed_at, o.updated_at, o.delivered_at
         FROM messages m LEFT JOIN outbox o ON o.message_id = m.id
         WHERE m.mission_id = ?1 AND {filter} ORDER BY m.created_at, m.id"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| sqlite_error("sqlite_inspect_failed", "prepare_messages", error))?;
    let rows = statement
        .query_map([mission_id, parameter], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "assignment_id": row.get::<_, Option<String>>(1)?,
                "source_role": row.get::<_, String>(2)?,
                "target_role": row.get::<_, String>(3)?,
                "kind": row.get::<_, String>(4)?,
                "body": row.get::<_, String>(5)?,
                "context_rev": row.get::<_, i64>(6)?,
                "in_reply_to": row.get::<_, Option<String>>(7)?,
                "review_id": row.get::<_, Option<String>>(8)?,
                "created_at": row.get::<_, String>(9)?,
                "outbox_id": row.get::<_, Option<String>>(10)?,
                "status": row.get::<_, Option<String>>(11)?,
                "attempts": row.get::<_, Option<i64>>(12)?.unwrap_or(0),
                "last_error": row.get::<_, Option<String>>(13)?.unwrap_or_default(),
                "claimed_by": row.get::<_, Option<String>>(14)?.unwrap_or_default(),
                "claimed_at": row.get::<_, Option<i64>>(15)?,
                "updated_at": row.get::<_, Option<String>>(16)?,
                "delivered_at": row.get::<_, Option<String>>(17)?,
            }))
        })
        .map_err(|error| sqlite_error("sqlite_inspect_failed", "query_messages", error))?;
    let values = collect_inspect_rows(rows, "messages")?;
    Ok(values)
}

fn sqlite_inspect_tool_jobs(
    connection: &Connection,
    mission_id: &str,
) -> Result<Vec<Value>, KernelError> {
    let mut statement = connection
        .prepare(
            "SELECT job_id, assignment_id, source_role, mode, label, state, pane_id,
                    exit_code, stdout_truncated, stderr_truncated, created_at, started_at, finished_at
             FROM tool_jobs WHERE mission_id = ?1 ORDER BY created_at DESC LIMIT 20",
        )
        .map_err(|error| sqlite_error("sqlite_inspect_failed", "prepare_tool_jobs", error))?;
    let rows = statement
        .query_map([mission_id], |row| {
            Ok(json!({
                "job_id": row.get::<_, String>(0)?,
                "assignment_id": row.get::<_, String>(1)?,
                "source_role": row.get::<_, String>(2)?,
                "mode": row.get::<_, String>(3)?,
                "label": row.get::<_, String>(4)?,
                "state": row.get::<_, String>(5)?,
                "pane_id": row.get::<_, String>(6)?,
                "exit_code": row.get::<_, Option<i64>>(7)?,
                "stdout_truncated": row.get::<_, i64>(8)? != 0,
                "stderr_truncated": row.get::<_, i64>(9)? != 0,
                "created_at": row.get::<_, String>(10)?,
                "started_at": row.get::<_, Option<String>>(11)?,
                "finished_at": row.get::<_, Option<String>>(12)?,
            }))
        })
        .map_err(|error| sqlite_error("sqlite_inspect_failed", "query_tool_jobs", error))?;
    let values = collect_inspect_rows(rows, "tool_jobs")?;
    Ok(values)
}

fn collect_inspect_rows<I>(rows: I, operation: &str) -> Result<Vec<Value>, KernelError>
where
    I: Iterator<Item = rusqlite::Result<Value>>,
{
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| sqlite_error("sqlite_inspect_failed", operation, error))
}

fn sqlite_inspect_table_count(
    connection: &Connection,
    table: &str,
    mission_id: &str,
) -> Result<u64, KernelError> {
    sqlite_inspect_count(
        connection,
        &format!("SELECT COUNT(*) FROM \"{table}\" WHERE mission_id = ?1"),
        mission_id,
        table,
    )
}

fn sqlite_inspect_count(
    connection: &Connection,
    sql: &str,
    mission_id: &str,
    operation: &str,
) -> Result<u64, KernelError> {
    let count = connection
        .query_row(sql, [mission_id], |row| row.get::<_, i64>(0))
        .map_err(|error| sqlite_error("sqlite_inspect_failed", operation, error))?;
    nonnegative_observation(count, operation)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, Barrier,
        },
        thread,
        time::Duration,
    };

    use rusqlite::{Connection, OpenFlags};
    use serde_json::json;

    use super::{SqliteV3CoordinationStore, WritableDatabasePermit};
    use crate::adapters::RecordingAgentProviderAdapter;
    use crate::domain::{DriveContext, EffectExecutor};
    use crate::MissionKernel;
    use crate::{
        DecisionContext, DriveRequest, EffectIntent, EffectIntentKind, EffectOutcome, Generation,
        HandleDisposition, HandleInput, KernelError, KernelInput, RoleKind, RoleRef, RoleState,
        RuntimeOwner, ToolJobMode, ToolJobOutputMetadata, ToolJobRequest, ToolJobTerminalState,
        ToolJobTransition,
    };

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn generation(value: &str) -> Generation {
        Generation::new(value).unwrap()
    }

    struct TempDatabase {
        root: PathBuf,
        path: PathBuf,
    }

    impl TempDatabase {
        fn with_schema_version(version: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-mission-kernel-{}-{}",
                std::process::id(),
                NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            let path = root.join("mission-team.sqlite3");
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO schema_meta(key, value) VALUES('schema_version', ?1)",
                    [version],
                )
                .unwrap();
            drop(connection);
            Self { root, path }
        }

        fn with_schema_v3() -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-mission-kernel-{}-{}",
                std::process::id(),
                NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            let path = root.join("mission-team.sqlite3");
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(include_str!("../tests/fixtures/schema-v3.sql"))
                .unwrap();
            drop(connection);
            Self { root, path }
        }

        fn permit(&self) -> WritableDatabasePermit {
            WritableDatabasePermit::for_test(&self.root, &self.path).unwrap()
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDatabase {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn schema_snapshot(path: &Path) -> Vec<(String, String, String, Option<String>)> {
        let connection =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT type, name, tbl_name, sql
                 FROM sqlite_master
                 WHERE name NOT LIKE 'sqlite_%'
                 ORDER BY type, name",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn insert_mission(database: &TempDatabase, mission_id: &str) {
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "INSERT INTO team_missions(mission_id, created_at, updated_at)
                 VALUES(?1, '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z')",
                [mission_id],
            )
            .unwrap();
    }

    fn insert_worker_role(database: &TempDatabase, mission_id: &str) {
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "INSERT INTO team_roles(
                    mission_id, role, provider, launch_generation, health, updated_at
                 ) VALUES(?1, 'worker', 'recording', 'generation-worker', 'ready',
                          '2026-08-14T06:00:00Z')",
                [mission_id],
            )
            .unwrap();
    }

    fn insert_role_launch_lease(
        database: &TempDatabase,
        mission_id: &str,
        role: &str,
        generation: &str,
    ) {
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "INSERT INTO role_launch_leases(
                     mission_id, role, owner, generation, acquired_at, expires_at
                 ) VALUES(?1, ?2, 'launch-owner-fixture', ?3, 1786660000, 1786700000)
                 ON CONFLICT(mission_id, role) DO UPDATE SET
                     owner = excluded.owner,
                     generation = excluded.generation,
                     acquired_at = excluded.acquired_at,
                     expires_at = excluded.expires_at",
                rusqlite::params![mission_id, role, generation],
            )
            .unwrap();
    }

    #[derive(Default)]
    struct RecordingExecutor {
        intents: Vec<EffectIntent>,
        outcomes: Vec<EffectOutcome>,
    }

    impl EffectExecutor for RecordingExecutor {
        fn execute(&mut self, intent: &EffectIntent) -> EffectOutcome {
            self.intents.push(intent.clone());
            if self.outcomes.is_empty() {
                EffectOutcome::Succeeded {
                    observation: json!({"recorded": true}),
                }
            } else {
                self.outcomes.remove(0)
            }
        }
    }

    struct GenerationSwitchingExecutor {
        database_path: PathBuf,
    }

    impl EffectExecutor for GenerationSwitchingExecutor {
        fn execute(&mut self, _intent: &EffectIntent) -> EffectOutcome {
            let connection = Connection::open(&self.database_path).unwrap();
            connection
                .execute(
                    "UPDATE team_roles SET launch_generation = 'generation-new'
                     WHERE role = 'worker'",
                    [],
                )
                .unwrap();
            EffectOutcome::Succeeded {
                observation: json!({"external_success": true}),
            }
        }
    }

    fn drive_request(effect_budget: u32, time_budget_ms: u64) -> DriveRequest {
        DriveRequest {
            runtime_owner: RuntimeOwner::Rust,
            effect_budget,
            time_budget_ms,
            execution_mode: crate::DriveExecutionMode::Deferred,
            claim_owner: None,
            claimed_at_ms: 0,
        }
    }

    fn drive_context(owner: &str, observed_at: &str) -> DriveContext {
        DriveContext {
            claim_owner: owner.into(),
            observed_at: observed_at.into(),
            claimed_at_ms: 1_786_688_400_000,
        }
    }

    #[test]
    fn sqlite_drive_claims_within_budget_executes_after_commit_and_resolves_idempotently() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-drive-success");
        insert_worker_role(&database, "msn-drive-success");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-drive-success",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        kernel
            .handle(task_command(
                "cmd-drive-success",
                "Deliver once",
                "drive-success",
            ))
            .unwrap();
        let mut executor = RecordingExecutor::default();
        let report = kernel
            .drive_with_executor(
                drive_request(1, 1_000),
                drive_context("driver-success", "2026-08-14T06:20:00Z"),
                &mut executor,
            )
            .unwrap();
        assert_eq!((report.claimed, report.resolved, report.pending), (1, 1, 0));
        assert_eq!(executor.intents.len(), 1);
        assert_eq!(report.effect_results.len(), 1);
        drop(kernel);

        let connection = Connection::open(database.path()).unwrap();
        let state: (String, i64, String, Option<i64>) = connection
            .query_row(
                "SELECT status, attempts, claimed_by, claimed_at FROM outbox
                 WHERE id = 'out-drive-success'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(state, ("delivered".into(), 1, String::new(), None));
    }

    #[test]
    fn sqlite_drive_releases_transaction_before_external_execution_and_respects_zero_budgets() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-drive-budget");
        insert_worker_role(&database, "msn-drive-budget");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-drive-budget",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        kernel
            .handle(task_command(
                "cmd-drive-budget",
                "Stay queued",
                "drive-budget",
            ))
            .unwrap();
        let mut executor = RecordingExecutor::default();
        let report = kernel
            .drive_with_executor(
                drive_request(0, 1_000),
                drive_context("driver-budget", "2026-08-14T06:21:00Z"),
                &mut executor,
            )
            .unwrap();
        assert_eq!(report.claimed, 0);
        assert!(executor.intents.is_empty());
    }

    #[test]
    fn sqlite_drive_reports_pending_retryable_and_terminal_outcomes_without_losing_claims() {
        for (suffix, outcome, expected) in [
            (
                "pending",
                EffectOutcome::Pending {
                    reason: "role is starting".into(),
                },
                (1, 0, 1, 0, 0),
            ),
            (
                "retry",
                EffectOutcome::RetryableFailure {
                    error: KernelError {
                        category: crate::ErrorCategory::Infrastructure,
                        code: "provider_busy".into(),
                        message: "provider busy".into(),
                        retryable: true,
                        details: BTreeMap::new(),
                    },
                    retry_after_ms: 250,
                },
                (1, 0, 0, 1, 0),
            ),
            (
                "terminal",
                EffectOutcome::TerminalFailure {
                    error: KernelError {
                        category: crate::ErrorCategory::Contract,
                        code: "invalid_target".into(),
                        message: "invalid target".into(),
                        retryable: false,
                        details: BTreeMap::new(),
                    },
                },
                (1, 0, 0, 0, 1),
            ),
        ] {
            let database = TempDatabase::with_schema_v3();
            let mission_id = format!("msn-drive-{suffix}");
            insert_mission(&database, &mission_id);
            insert_worker_role(&database, &mission_id);
            let mut kernel = MissionKernel::open_temporary_sqlite_v3(
                &mission_id,
                database.permit(),
                Duration::from_millis(25),
            )
            .unwrap();
            kernel
                .handle(task_command(
                    &format!("cmd-drive-{suffix}"),
                    "Record failure",
                    &format!("drive-{suffix}"),
                ))
                .unwrap();
            let mut executor = RecordingExecutor {
                intents: Vec::new(),
                outcomes: vec![outcome],
            };
            let report = kernel
                .drive_with_executor(
                    drive_request(1, 1_000),
                    drive_context(&format!("driver-{suffix}"), "2026-08-14T06:22:00Z"),
                    &mut executor,
                )
                .unwrap();
            assert_eq!(
                (
                    report.claimed,
                    report.resolved,
                    report.pending,
                    report.retryable_failures,
                    report.terminal_failures,
                ),
                expected
            );
        }
    }

    #[test]
    fn sqlite_drive_reclaims_an_expired_claim_after_external_success_before_resolve() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-drive-restart");
        insert_worker_role(&database, "msn-drive-restart");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-drive-restart",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        kernel
            .handle(task_command(
                "cmd-drive-restart",
                "Observe before retrying the durable effect",
                "drive-restart",
            ))
            .unwrap();
        drop(kernel);

        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "UPDATE outbox
                 SET status = 'sending', attempts = 1,
                     claimed_by = 'crashed-driver:1', claimed_at = 1000
                 WHERE mission_id = 'msn-drive-restart' AND id = 'out-drive-restart'",
                [],
            )
            .unwrap();
        drop(connection);

        let mut restarted = MissionKernel::open_temporary_sqlite_v3(
            "msn-drive-restart",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let mut executor = RecordingExecutor::default();
        let report = restarted
            .drive_with_executor(
                drive_request(1, 1_000),
                DriveContext {
                    claim_owner: "restarted-driver".into(),
                    observed_at: "2026-08-14T06:30:00Z".into(),
                    claimed_at_ms: 61_001,
                },
                &mut executor,
            )
            .unwrap();

        assert_eq!((report.claimed, report.resolved), (1, 1));
        assert_eq!(executor.intents.len(), 1);
        drop(restarted);
        let connection = Connection::open(database.path()).unwrap();
        let state: (String, i64, String, Option<i64>) = connection
            .query_row(
                "SELECT status, attempts, claimed_by, claimed_at FROM outbox
                 WHERE id = 'out-drive-restart'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(state, ("delivered".into(), 2, String::new(), None));
    }

    #[test]
    fn recording_adapter_has_no_coordination_store_mutation_capability() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-adapter-readonly");
        let before = fs::read(database.path()).unwrap();
        let mut adapter = RecordingAgentProviderAdapter::default();
        let intent = EffectIntent {
            effect_id: "out-record-only".into(),
            generation: generation("generation-worker"),
            intent: EffectIntentKind::DeliverPrompt {
                role: RoleRef {
                    role: RoleKind::Worker,
                    instance: None,
                },
                assignment_id: Some("asg-record-only".into()),
                prompt: "do not invoke anything".into(),
            },
        };
        assert!(matches!(
            adapter.execute(&intent),
            EffectOutcome::Succeeded { .. }
        ));
        assert_eq!(adapter.recorded(), &[intent]);
        assert_eq!(fs::read(database.path()).unwrap(), before);
    }

    #[test]
    fn stale_generation_result_cannot_overwrite_the_current_role_or_delivery_state() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-drive-stale-generation");
        insert_worker_role(&database, "msn-drive-stale-generation");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-drive-stale-generation",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        kernel
            .handle(task_command(
                "cmd-drive-stale-generation",
                "Fence stale delivery",
                "drive-stale-generation",
            ))
            .unwrap();
        let mut executor = GenerationSwitchingExecutor {
            database_path: database.path().to_path_buf(),
        };
        let error = kernel
            .drive_with_executor(
                drive_request(1, 1_000),
                drive_context("driver-stale", "2026-08-14T06:23:00Z"),
                &mut executor,
            )
            .unwrap_err();
        assert_eq!(error.code, "stale_generation");
        drop(kernel);

        let connection = Connection::open(database.path()).unwrap();
        let durable: (String, String, String) = connection
            .query_row(
                "SELECT r.launch_generation, o.status, o.claimed_by
                 FROM team_roles r JOIN outbox o USING(mission_id)
                 WHERE r.role = 'worker' AND o.id = 'out-drive-stale-generation'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            durable,
            (
                "generation-new".into(),
                "sending".into(),
                "driver-stale:1".into(),
            )
        );
    }

    fn tool_job_request(job_id: &str, assignment_id: &str) -> KernelInput {
        KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T06:02:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::new(),
            },
            input: HandleInput::ToolJobRequest {
                request: ToolJobRequest {
                    job_id: job_id.into(),
                    assignment_id: assignment_id.into(),
                    source: RoleRef {
                        role: RoleKind::Worker,
                        instance: None,
                    },
                    mode: ToolJobMode::Bounded,
                    label: "Rust tests".into(),
                    argv: vec!["cargo".into(), "test".into()],
                    cwd: "/tmp/mission-worktree".into(),
                    env: BTreeMap::from([
                        ("NO_COLOR".into(), "1".into()),
                        ("CI".into(), "1".into()),
                    ]),
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
        observed_at: &str,
        transition: ToolJobTransition,
    ) -> KernelInput {
        KernelInput {
            decision_context: DecisionContext {
                observed_at: observed_at.into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::new(),
            },
            input: HandleInput::ToolJobTransition {
                transition_id: transition_id.into(),
                job_id: job_id.into(),
                owner: RoleRef {
                    role: RoleKind::Worker,
                    instance: None,
                },
                transition,
            },
        }
    }

    fn started_tool_job_transition() -> ToolJobTransition {
        ToolJobTransition::Started {
            pane_id: "wteam:p3".into(),
            coordination_dir: "/tmp/coordination".into(),
            request_path: "/tmp/coordination/request.json".into(),
            stdout_path: "/tmp/coordination/stdout.log".into(),
            stderr_path: "/tmp/coordination/stderr.log".into(),
            result_path: "/tmp/coordination/result.json".into(),
        }
    }

    fn completed_tool_job_transition() -> ToolJobTransition {
        ToolJobTransition::Completed {
            output: ToolJobOutputMetadata {
                state: ToolJobTerminalState::Succeeded,
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

    fn task_command(command_id: &str, text: &str, suffix: &str) -> KernelInput {
        assignment_command(
            command_id,
            text,
            suffix,
            "task",
            RoleRef {
                role: RoleKind::Worker,
                instance: None,
            },
            "worker",
            "generation-worker",
        )
    }

    fn assignment_command(
        command_id: &str,
        text: &str,
        suffix: &str,
        kind: &str,
        target: RoleRef,
        generation_key: &str,
        generation_token: &str,
    ) -> KernelInput {
        KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T06:00:00Z".into(),
                allocated_ids: BTreeMap::from([
                    ("assignment".into(), format!("asg-{suffix}")),
                    ("message".into(), format!("msg-{suffix}")),
                    ("outbox".into(), format!("out-{suffix}")),
                ]),
                generations: BTreeMap::from([(
                    generation_key.into(),
                    generation(generation_token),
                )]),
            },
            input: HandleInput::Command {
                command_id: command_id.into(),
                kind: kind.into(),
                source: RoleRef {
                    role: RoleKind::Pm,
                    instance: None,
                },
                target: Some(target),
                body: json!({"text": text}),
            },
        }
    }

    fn run_concurrent_handles(
        database: &TempDatabase,
        mission_id: &str,
        inputs: Vec<KernelInput>,
        busy_timeout: Duration,
    ) -> Vec<Result<crate::HandleReceipt, KernelError>> {
        let barrier = Arc::new(Barrier::new(inputs.len()));
        let handles = inputs
            .into_iter()
            .map(|input| {
                let root = database.root.clone();
                let path = database.path.clone();
                let mission_id = mission_id.to_owned();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let permit = WritableDatabasePermit::for_test(&root, &path).unwrap();
                    let mut kernel =
                        MissionKernel::open_temporary_sqlite_v3(mission_id, permit, busy_timeout)
                            .unwrap();
                    barrier.wait();
                    kernel.handle(input)
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("concurrent handle thread panicked"))
            .collect()
    }

    fn context_command(command_id: &str, text: &str, suffix: &str) -> KernelInput {
        KernelInput {
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
                body: json!({"text": text, "context_revision": 3}),
            },
        }
    }

    fn role_observation(
        observation_id: &str,
        role: RoleRef,
        generation_token: &str,
        state: RoleState,
    ) -> KernelInput {
        let role_key = match (&role.role, &role.instance) {
            (RoleKind::Pm, None) => "pm".to_owned(),
            (RoleKind::Worker, None) => "worker".to_owned(),
            (RoleKind::Reviewer, None) => "reviewer".to_owned(),
            (RoleKind::Scout, Some(instance)) => instance.clone(),
            _ => panic!("test helper requires a canonical role identity"),
        };
        let launch_generation = generation(generation_token);
        KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T06:30:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([(role_key, launch_generation.clone())]),
            },
            input: HandleInput::RoleObservation {
                observation_id: observation_id.into(),
                role,
                generation: launch_generation,
                launch_owner: Some("launch-owner-fixture".into()),
                state,
                details: json!({}),
            },
        }
    }

    fn claimed_effect_result(
        effect_id: &str,
        generation_token: &str,
        claim_owner: &str,
    ) -> KernelInput {
        let input = serde_json::from_value::<HandleInput>(json!({
            "type": "effect_result",
            "result": {
                "effect_id": effect_id,
                "generation": generation_token,
                "claim_owner": claim_owner,
                "outcome": {
                    "type": "succeeded",
                    "observation": {"delivered": true}
                }
            }
        }))
        .expect("typed EffectResult must carry the durable Outbox claim owner");
        KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T06:40:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([("worker".into(), generation(generation_token))]),
            },
            input,
        }
    }

    fn leased_role_observation(
        observation_id: &str,
        generation_token: &str,
        launch_owner: &str,
    ) -> KernelInput {
        let input = serde_json::from_value::<HandleInput>(json!({
            "type": "role_observation",
            "observation_id": observation_id,
            "role": {"role": "worker"},
            "generation": generation_token,
            "state": "ready",
            "launch_owner": launch_owner,
            "details": {}
        }))
        .expect("typed RoleObservation must carry the role-launch lease owner");
        KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T06:45:00Z".into(),
                allocated_ids: BTreeMap::new(),
                generations: BTreeMap::from([("worker".into(), generation(generation_token))]),
            },
            input,
        }
    }

    fn worker_reply_command(
        command_id: &str,
        assignment_id: &str,
        kind: &str,
        suffix: &str,
    ) -> KernelInput {
        let mut allocated_ids = BTreeMap::from([
            ("message".into(), format!("msg-{suffix}")),
            ("outbox".into(), format!("out-{suffix}")),
        ]);
        let mut generations = BTreeMap::from([("pm".into(), generation("generation-pm"))]);
        if kind == "completed" {
            allocated_ids.extend([
                (
                    "follow_up_assignment".into(),
                    format!("asg-{suffix}-review"),
                ),
                ("follow_up_message".into(), format!("msg-{suffix}-review")),
                ("follow_up_outbox".into(), format!("out-{suffix}-review")),
            ]);
            generations.insert("reviewer".into(), generation("generation-reviewer"));
        }
        KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T06:10:00Z".into(),
                allocated_ids,
                generations,
            },
            input: HandleInput::Command {
                command_id: command_id.into(),
                kind: kind.into(),
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
                    "reply_id": format!("reply-{suffix}"),
                    "text": "The worker cannot continue safely"
                }),
            },
        }
    }

    fn reviewer_reply_command(
        command_id: &str,
        assignment_id: &str,
        kind: &str,
        suffix: &str,
    ) -> KernelInput {
        let mut allocated_ids = BTreeMap::from([
            ("message".into(), format!("msg-{suffix}")),
            ("outbox".into(), format!("out-{suffix}")),
            ("review_revision".into(), format!("rev-{suffix}")),
            (
                "review_pm_notice_message".into(),
                format!("msg-{suffix}-review-pm-notice"),
            ),
            (
                "review_pm_notice_outbox".into(),
                format!("out-{suffix}-review-pm-notice"),
            ),
            (
                "review_worker_notice_message".into(),
                format!("msg-{suffix}-review-worker-notice"),
            ),
            (
                "review_worker_notice_outbox".into(),
                format!("out-{suffix}-review-worker-notice"),
            ),
        ]);
        let generations = BTreeMap::from([
            ("pm".into(), generation("generation-pm")),
            ("worker".into(), generation("generation-worker")),
        ]);
        if kind == "rejected" {
            allocated_ids.extend([
                ("follow_up_assignment".into(), format!("asg-{suffix}-fix")),
                ("follow_up_message".into(), format!("msg-{suffix}-fix")),
                ("follow_up_outbox".into(), format!("out-{suffix}-fix")),
            ]);
        }
        KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T06:20:00Z".into(),
                allocated_ids,
                generations,
            },
            input: HandleInput::Command {
                command_id: command_id.into(),
                kind: kind.into(),
                source: RoleRef {
                    role: RoleKind::Reviewer,
                    instance: None,
                },
                target: Some(RoleRef {
                    role: RoleKind::Pm,
                    instance: None,
                }),
                body: json!({
                    "assignment_id": assignment_id,
                    "reply_id": format!("reply-{suffix}"),
                    "text": "The implementation needs one focused correction",
                    "references": ["tests/reviewer.rs:42"]
                }),
            },
        }
    }

    fn reviewer_reply_command_with_acknowledgement(
        command_id: &str,
        assignment_id: &str,
        suffix: &str,
    ) -> KernelInput {
        let mut input = reviewer_reply_command(command_id, assignment_id, "approved", suffix);
        let HandleInput::Command { body, .. } = &mut input.input else {
            unreachable!("reviewer reply helper always returns a command")
        };
        body["acknowledge_review"] = json!(true);
        input
    }

    fn assignment_settled_event(
        event_id: &str,
        sequence: u64,
        assignment_id: &str,
        safe_to_resume: bool,
        suffix: &str,
    ) -> KernelInput {
        KernelInput {
            decision_context: DecisionContext {
                observed_at: "2026-08-14T06:30:00Z".into(),
                allocated_ids: BTreeMap::from([
                    ("message".into(), format!("msg-{suffix}")),
                    ("outbox".into(), format!("out-{suffix}")),
                ]),
                generations: BTreeMap::new(),
            },
            input: HandleInput::TeamEvent {
                event_id: event_id.into(),
                sequence,
                name: "assignment_settled".into(),
                body: json!({
                    "role": "worker",
                    "expected_assignment_id": assignment_id,
                    "safe_to_resume": safe_to_resume,
                }),
            },
        }
    }

    fn insert_active_worker_assignment(
        database: &TempDatabase,
        mission_id: &str,
        assignment_id: &str,
    ) {
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(&format!(
                "INSERT INTO assignments(
                     id, mission_id, source_role, target_role, kind, summary, state,
                     created_at, updated_at
                 ) VALUES(
                     '{assignment_id}', '{mission_id}', 'pm', 'worker', 'task',
                     'Run active task', 'active',
                     '2026-08-14T06:00:00Z', '2026-08-14T06:00:00Z'
                 );
                 INSERT INTO messages(
                     id, mission_id, assignment_id, source_role, target_role, kind,
                     body, context_rev, created_at
                 ) VALUES(
                     'msg-active-task', '{mission_id}', '{assignment_id}', 'pm', 'worker',
                     'task', 'Run active task', 0, '2026-08-14T06:00:00Z'
                 );
                 INSERT INTO outbox(
                     id, message_id, mission_id, target_role, status, created_at, updated_at
                 ) VALUES(
                     'out-active-task', 'msg-active-task', '{mission_id}', 'worker',
                     'sending', '2026-08-14T06:00:00Z', '2026-08-14T06:00:00Z'
                 );"
            ))
            .unwrap();
    }

    fn insert_active_reviewer_assignment(
        database: &TempDatabase,
        mission_id: &str,
        worker_assignment_id: &str,
        reviewer_assignment_id: &str,
    ) {
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(&format!(
                "INSERT INTO assignments(
                     id, mission_id, source_role, target_role, kind, summary, state,
                     created_at, updated_at
                 ) VALUES(
                     '{worker_assignment_id}', '{mission_id}', 'pm', 'worker', 'task',
                     'Original Worker task', 'completed',
                     '2026-08-14T06:00:00Z', '2026-08-14T06:10:00Z'
                 );
                 INSERT INTO assignments(
                     id, mission_id, source_role, target_role, kind, summary, state,
                     parent_id, review_round, created_at, updated_at
                 ) VALUES(
                     '{reviewer_assignment_id}', '{mission_id}', 'pm', 'reviewer', 'review',
                     'Review the Worker result', 'active', '{worker_assignment_id}', 0,
                     '2026-08-14T06:10:00Z', '2026-08-14T06:10:00Z'
                 );
                 INSERT INTO messages(
                     id, mission_id, assignment_id, source_role, target_role, kind,
                     body, context_rev, created_at
                 ) VALUES(
                     'msg-active-review', '{mission_id}', '{reviewer_assignment_id}',
                     'pm', 'reviewer', 'review', 'Review the Worker result', 0,
                     '2026-08-14T06:10:00Z'
                 );
                 INSERT INTO outbox(
                     id, message_id, mission_id, target_role, status, created_at, updated_at
                 ) VALUES(
                     'out-active-review', 'msg-active-review', '{mission_id}', 'reviewer',
                     'sending', '2026-08-14T06:10:00Z', '2026-08-14T06:10:00Z'
                 );"
            ))
            .unwrap();
    }

    fn durable_counts(path: &Path, mission_id: &str) -> (i64, i64, i64, i64, i64) {
        let connection =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        connection
            .query_row(
                "SELECT context_rev,
                        (SELECT COUNT(*) FROM assignments WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM messages WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM outbox WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM processed_events WHERE mission_id = ?1)
                 FROM team_missions
                 WHERE mission_id = ?1",
                [mission_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap()
    }

    #[test]
    fn complete_schema_v3_opens_without_rewriting_schema_or_existing_references() {
        let database = TempDatabase::with_schema_v3();
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(
                "INSERT INTO team_missions(mission_id, created_at, updated_at)
                 VALUES('msn-existing', '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z');
                 INSERT INTO assignments(
                     id, mission_id, source_role, target_role, kind, summary, state,
                     created_at, updated_at
                 ) VALUES(
                     'asg-existing', 'msn-existing', 'pm', 'worker', 'task',
                     'Preserve this assignment', 'active',
                     '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z'
                 );
                 INSERT INTO messages(
                     id, mission_id, assignment_id, source_role, target_role, kind,
                     body, context_rev, created_at
                 ) VALUES(
                     'msg-existing', 'msn-existing', 'asg-existing', 'pm', 'worker',
                     'task', 'Preserve this message', 1, '2026-08-14T00:00:00Z'
                 );
                 INSERT INTO outbox(
                     id, message_id, mission_id, target_role, status, created_at, updated_at
                 ) VALUES(
                     'out-existing', 'msg-existing', 'msn-existing', 'worker', 'delivered',
                     '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z'
                 );",
            )
            .unwrap();
        drop(connection);
        let schema_before = schema_snapshot(database.path());

        let store =
            SqliteV3CoordinationStore::open(database.permit(), Duration::from_millis(25)).unwrap();
        drop(store);

        assert_eq!(schema_snapshot(database.path()), schema_before);
        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let references = connection
            .query_row(
                "SELECT assignments.id, messages.id, messages.assignment_id,
                        outbox.id, outbox.message_id
                 FROM assignments
                 JOIN messages ON messages.assignment_id = assignments.id
                 JOIN outbox ON outbox.message_id = messages.id",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            references,
            (
                "asg-existing".into(),
                "msg-existing".into(),
                "asg-existing".into(),
                "out-existing".into(),
                "msg-existing".into(),
            )
        );
    }

    #[test]
    fn sqlite_adapter_observes_only_the_bound_mission_without_mutating_the_fixture() {
        let database = TempDatabase::with_schema_v3();
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(
                "INSERT INTO team_missions(
                     mission_id, context_rev, created_at, updated_at
                 ) VALUES(
                     'msn-observed', 7,
                     '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z'
                 );
                 INSERT INTO assignments(
                     id, mission_id, source_role, target_role, kind, summary, state,
                     created_at, updated_at
                 ) VALUES(
                     'asg-observed', 'msn-observed', 'pm', 'worker', 'task',
                     'Observe this assignment', 'active',
                     '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z'
                 );
                 INSERT INTO messages(
                     id, mission_id, assignment_id, source_role, target_role, kind,
                     body, context_rev, created_at
                 ) VALUES(
                     'msg-observed', 'msn-observed', 'asg-observed', 'pm', 'worker',
                     'task', 'Observe this message', 7, '2026-08-14T00:00:00Z'
                 );
                 INSERT INTO outbox(
                     id, message_id, mission_id, target_role, status, created_at, updated_at
                 ) VALUES(
                     'out-observed', 'msg-observed', 'msn-observed', 'worker', 'pending',
                     '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z'
                 );
                 INSERT INTO team_missions(
                     mission_id, context_rev, created_at, updated_at
                 ) VALUES(
                     'msn-other', 99,
                     '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z'
                 );
                 INSERT INTO assignments(
                     id, mission_id, source_role, target_role, kind, summary, state,
                     created_at, updated_at
                 ) VALUES(
                     'asg-other', 'msn-other', 'pm', 'scout', 'task',
                     'Do not include this assignment', 'active',
                     '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z'
                 );",
            )
            .unwrap();
        drop(connection);
        let before = fs::read(database.path()).unwrap();

        let store =
            SqliteV3CoordinationStore::open(database.permit(), Duration::from_millis(25)).unwrap();
        let observation = store.observe_mission("msn-observed").unwrap();

        assert_eq!(observation.schema_version, "3");
        assert_eq!(observation.mission_id, "msn-observed");
        assert_eq!(observation.assignment_count, 1);
        assert_eq!(observation.message_count, 1);
        assert_eq!(observation.outbox_count, 1);
        assert_eq!(observation.revision, 7);
        drop(store);
        assert_eq!(fs::read(database.path()).unwrap(), before);
    }

    #[test]
    fn sqlite_role_observation_preserves_the_authoritative_opaque_generation_atomically() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-role-observation";
        insert_mission(&database, mission_id);
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "INSERT INTO team_roles(
                     mission_id, role, provider, launch_generation, health, updated_at
                 ) VALUES(?1, 'worker', 'codex', ?2, 'starting', '2026-08-14T06:00:00Z')",
                rusqlite::params![mission_id, "launch.worker/opaque:A"],
            )
            .unwrap();
        drop(connection);
        insert_role_launch_lease(&database, mission_id, "worker", "launch.worker/opaque:A");

        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            mission_id,
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let receipt = kernel
            .handle(role_observation(
                "obs-worker-ready",
                RoleRef {
                    role: RoleKind::Worker,
                    instance: None,
                },
                "launch.worker/opaque:A",
                RoleState::Ready,
            ))
            .unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(receipt.revision, Some(1));
        drop(kernel);

        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let persisted = connection
            .query_row(
                "SELECT launch_generation, health, last_seen_rev, updated_at,
                        (SELECT context_rev FROM team_missions WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM processed_events
                         WHERE mission_id = ?1 AND event_key = 'observation:obs-worker-ready')
                 FROM team_roles
                 WHERE mission_id = ?1 AND role = 'worker'",
                [mission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            persisted,
            (
                "launch.worker/opaque:A".into(),
                "ready".into(),
                0,
                "2026-08-14T06:30:00Z".into(),
                1,
                1,
            )
        );
    }

    #[test]
    fn sqlite_reopen_rejects_a_role_observation_with_a_different_opaque_generation() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-role-generation-reopen";
        insert_mission(&database, mission_id);
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "INSERT INTO team_roles(
                     mission_id, role, provider, launch_generation, health, updated_at
                 ) VALUES(?1, 'worker', 'codex', ?2, 'starting', '2026-08-14T06:00:00Z')",
                rusqlite::params![mission_id, "launch.worker/session:alpha"],
            )
            .unwrap();
        drop(connection);
        insert_role_launch_lease(
            &database,
            mission_id,
            "worker",
            "launch.worker/session:alpha",
        );

        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            mission_id,
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let applied = kernel
            .handle(role_observation(
                "obs-worker-alpha",
                RoleRef {
                    role: RoleKind::Worker,
                    instance: None,
                },
                "launch.worker/session:alpha",
                RoleState::Ready,
            ))
            .unwrap();
        assert_eq!(applied.disposition, HandleDisposition::Applied);
        drop(kernel);
        insert_role_launch_lease(
            &database,
            mission_id,
            "worker",
            "launch.worker/session:alpha",
        );

        let mut reopened = MissionKernel::open_temporary_sqlite_v3(
            mission_id,
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let rejected = reopened
            .handle(role_observation(
                "obs-worker-beta",
                RoleRef {
                    role: RoleKind::Worker,
                    instance: None,
                },
                "launch.worker/session:beta",
                RoleState::Failed,
            ))
            .unwrap();

        assert_eq!(rejected.disposition, HandleDisposition::Rejected);
        assert_eq!(rejected.error.unwrap().code, "stale_generation");
        drop(reopened);

        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let persisted = connection
            .query_row(
                "SELECT launch_generation, health,
                        (SELECT context_rev FROM team_missions WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM processed_events WHERE mission_id = ?1)
                 FROM team_roles
                 WHERE mission_id = ?1 AND role = 'worker'",
                [mission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            persisted,
            ("launch.worker/session:alpha".into(), "ready".into(), 1, 1,)
        );
    }

    #[test]
    fn sqlite_dynamic_scout_observation_updates_the_expert_runtime_mirror_atomically() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-scout-role-observation";
        insert_mission(&database, mission_id);
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(&format!(
                r#"INSERT INTO team_roles(
                     mission_id, role, provider, launch_generation, health, updated_at
                 ) VALUES(
                     '{mission_id}', 'scout-07', 'grok', 'launch.scout/opaque:07',
                     'starting', '2026-08-14T06:00:00Z'
                 );
                 UPDATE team_roles
                 SET pane_id = 'pane-scout-07', terminal_id = 'terminal-scout-07',
                     session_json = '{{"kind":"session","value":"scout-07"}}'
                 WHERE mission_id = '{mission_id}' AND role = 'scout-07';
                 INSERT INTO expert_instances(
                     mission_id, role, provider, pane_id, terminal_id, session_json,
                     launch_generation, state, close_policy, last_active_at, updated_at
                 ) VALUES(
                     '{mission_id}', 'scout-07', 'grok', 'stale-pane', 'stale-terminal',
                     '{{"kind":"stale"}}', 'launch.scout/stale', 'standby', 'mission',
                     '2026-08-14T06:00:00Z',
                     '2026-08-14T06:00:00Z'
                 );"#
            ))
            .unwrap();
        drop(connection);
        insert_role_launch_lease(&database, mission_id, "scout-07", "launch.scout/opaque:07");

        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            mission_id,
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let receipt = kernel
            .handle(role_observation(
                "obs-scout-07-ready",
                RoleRef {
                    role: RoleKind::Scout,
                    instance: Some("scout-07".into()),
                },
                "launch.scout/opaque:07",
                RoleState::Ready,
            ))
            .unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        drop(kernel);

        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mirror = connection
            .query_row(
                "SELECT team_roles.health,
                        team_roles.launch_generation,
                        expert_instances.state,
                        expert_instances.launch_generation,
                        expert_instances.pane_id,
                        expert_instances.terminal_id,
                        expert_instances.session_json,
                        expert_instances.last_active_at,
                        expert_instances.updated_at
                 FROM team_roles
                 JOIN expert_instances
                   ON expert_instances.mission_id = team_roles.mission_id
                  AND expert_instances.role = team_roles.role
                 WHERE team_roles.mission_id = ?1 AND team_roles.role = 'scout-07'",
                [mission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            mirror,
            (
                "ready".into(),
                "launch.scout/opaque:07".into(),
                "active".into(),
                "launch.scout/opaque:07".into(),
                "pane-scout-07".into(),
                "terminal-scout-07".into(),
                "{\"kind\":\"session\",\"value\":\"scout-07\"}".into(),
                "2026-08-14T06:30:00Z".into(),
                "2026-08-14T06:30:00Z".into(),
            )
        );
    }

    #[test]
    fn sqlite_role_observation_cannot_claim_an_empty_authoritative_generation() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-role-without-generation";
        insert_mission(&database, mission_id);
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "INSERT INTO team_roles(
                     mission_id, role, provider, launch_generation, health, updated_at
                 ) VALUES(?1, 'worker', 'codex', '', 'unknown', '2026-08-14T06:00:00Z')",
                [mission_id],
            )
            .unwrap();
        drop(connection);

        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            mission_id,
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let rejected = kernel
            .handle(role_observation(
                "obs-worker-untrusted",
                RoleRef {
                    role: RoleKind::Worker,
                    instance: None,
                },
                "launch.worker/untrusted",
                RoleState::Ready,
            ))
            .unwrap();

        assert_eq!(rejected.disposition, HandleDisposition::Rejected);
        assert_eq!(
            rejected.error.unwrap().code,
            "missing_authoritative_generation"
        );
        drop(kernel);

        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let persisted = connection
            .query_row(
                "SELECT launch_generation, health,
                        (SELECT context_rev FROM team_missions WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM processed_events WHERE mission_id = ?1)
                 FROM team_roles
                 WHERE mission_id = ?1 AND role = 'worker'",
                [mission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(persisted, ("".into(), "unknown".into(), 0, 0));
    }

    #[test]
    fn sqlite_dynamic_scout_mirror_failure_rolls_back_role_state_and_processed_input() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-scout-mirror-rollback";
        insert_mission(&database, mission_id);
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(&format!(
                "INSERT INTO team_roles(
                     mission_id, role, provider, launch_generation, health, updated_at
                 ) VALUES(
                     '{mission_id}', 'scout-09', 'grok', 'launch.scout/opaque:09',
                     'starting', '2026-08-14T06:00:00Z'
                 );
                 INSERT INTO expert_instances(
                     mission_id, role, provider, launch_generation, state,
                     close_policy, last_active_at, updated_at
                 ) VALUES(
                     '{mission_id}', 'scout-09', 'grok', 'launch.scout/opaque:09',
                     'active', 'mission', '2026-08-14T06:00:00Z',
                     '2026-08-14T06:00:00Z'
                 );
                 CREATE TRIGGER fail_scout_mirror_update
                 BEFORE UPDATE ON expert_instances
                 BEGIN
                     SELECT RAISE(ABORT, 'forced expert mirror failure');
                 END;"
            ))
            .unwrap();
        drop(connection);
        insert_role_launch_lease(&database, mission_id, "scout-09", "launch.scout/opaque:09");

        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            mission_id,
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let error = kernel
            .handle(role_observation(
                "obs-scout-09-ready",
                RoleRef {
                    role: RoleKind::Scout,
                    instance: Some("scout-09".into()),
                },
                "launch.scout/opaque:09",
                RoleState::Ready,
            ))
            .unwrap_err();

        assert_eq!(error.code, "sqlite_handle_failed");
        assert_eq!(error.details["operation"], "update_expert_role_state");
        drop(kernel);

        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let persisted = connection
            .query_row(
                "SELECT health,
                        (SELECT context_rev FROM team_missions WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM processed_events WHERE mission_id = ?1)
                 FROM team_roles
                 WHERE mission_id = ?1 AND role = 'scout-09'",
                [mission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(persisted, ("starting".into(), 0, 0));
    }

    #[test]
    fn sqlite_dynamic_scout_observation_does_not_require_an_expert_mirror_row() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-scout-without-mirror";
        insert_mission(&database, mission_id);
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "INSERT INTO team_roles(
                     mission_id, role, provider, launch_generation, health, updated_at
                 ) VALUES(
                     ?1, 'scout-11', 'grok', 'launch.scout/opaque:11',
                     'starting', '2026-08-14T06:00:00Z'
                 )",
                [mission_id],
            )
            .unwrap();
        drop(connection);
        insert_role_launch_lease(&database, mission_id, "scout-11", "launch.scout/opaque:11");

        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            mission_id,
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let receipt = kernel
            .handle(role_observation(
                "obs-scout-11-ready",
                RoleRef {
                    role: RoleKind::Scout,
                    instance: Some("scout-11".into()),
                },
                "launch.scout/opaque:11",
                RoleState::Ready,
            ))
            .unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        drop(kernel);

        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let persisted = connection
            .query_row(
                "SELECT health,
                        (SELECT COUNT(*) FROM expert_instances
                         WHERE mission_id = ?1 AND role = 'scout-11'),
                        (SELECT COUNT(*) FROM processed_events WHERE mission_id = ?1)
                 FROM team_roles
                 WHERE mission_id = ?1 AND role = 'scout-11'",
                [mission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(persisted, ("ready".into(), 0, 1));
    }

    #[test]
    fn memory_and_sqlite_adapters_share_the_internal_observation_seam() {
        let memory = MissionKernel::in_memory("msn-memory");
        let memory_observation = memory.observe_store().unwrap();
        assert_eq!(memory_observation.schema_version, "memory");
        assert_eq!(memory_observation.mission_id, "msn-memory");
        assert_eq!(memory_observation.assignment_count, 0);
        assert_eq!(memory_observation.message_count, 0);
        assert_eq!(memory_observation.outbox_count, 0);
        assert_eq!(memory_observation.revision, 0);

        let database = TempDatabase::with_schema_v3();
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(
                "INSERT INTO team_missions(mission_id, created_at, updated_at)
                 VALUES('msn-sqlite', '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z');",
            )
            .unwrap();
        drop(connection);
        let sqlite = MissionKernel::open_temporary_sqlite_v3(
            "msn-sqlite",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let sqlite_observation = sqlite.observe_store().unwrap();
        assert_eq!(sqlite_observation.schema_version, "3");
        assert_eq!(sqlite_observation.mission_id, "msn-sqlite");
    }

    #[test]
    fn sqlite_handle_rejects_missing_decision_context_without_mutation() {
        let database = TempDatabase::with_schema_v3();
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(
                "INSERT INTO team_missions(mission_id, created_at, updated_at)
                 VALUES('msn-sqlite', '2026-08-14T00:00:00Z', '2026-08-14T00:00:00Z');",
            )
            .unwrap();
        drop(connection);
        let before = fs::read(database.path()).unwrap();
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-sqlite",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let error = kernel
            .handle(KernelInput {
                decision_context: DecisionContext {
                    observed_at: "2026-08-14T06:00:00Z".into(),
                    allocated_ids: BTreeMap::new(),
                    generations: BTreeMap::new(),
                },
                input: HandleInput::Command {
                    command_id: "cmd-store-seam-red".into(),
                    kind: "context".into(),
                    source: RoleRef {
                        role: RoleKind::Pm,
                        instance: None,
                    },
                    target: Some(RoleRef {
                        role: RoleKind::Worker,
                        instance: None,
                    }),
                    body: json!({"text": "Do not persist this yet"}),
                },
            })
            .unwrap_err();

        assert_eq!(error.code, "missing_decision_context");
        assert_eq!(error.details["key"], "message");
        drop(kernel);
        assert_eq!(fs::read(database.path()).unwrap(), before);
    }

    #[test]
    fn sqlite_handle_commits_business_state_processed_input_and_delivery_before_receipt() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-atomic-commit");
        let schema_before = schema_snapshot(database.path());
        let input = task_command(
            "cmd-atomic-commit",
            "Persist the complete transition atomically",
            "atomic-commit",
        );
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-atomic-commit",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let receipt = kernel
            .handle(input)
            .expect("a committed transaction must return its receipt");

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(receipt.revision, Some(1));
        assert_eq!(receipt.created_ids["assignment"], "asg-atomic-commit");
        assert_eq!(receipt.created_ids["message"], "msg-atomic-commit");
        assert_eq!(receipt.created_ids["outbox"], "out-atomic-commit");
        assert_eq!(receipt.effect_intents.len(), 1);
        assert_eq!(receipt.effect_intents[0].effect_id, "out-atomic-commit");
        assert!(matches!(
            &receipt.effect_intents[0].intent,
            EffectIntentKind::DeliverPrompt {
                assignment_id: Some(assignment_id),
                prompt,
                ..
            } if assignment_id == "asg-atomic-commit"
                && prompt == "Persist the complete transition atomically"
        ));

        assert_eq!(
            durable_counts(database.path(), "msn-atomic-commit"),
            (0, 1, 1, 1, 1),
            "all durable state must be visible from another connection before handle returns"
        );
        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let persisted = connection
            .query_row(
                "SELECT assignments.id, messages.id, outbox.id,
                        messages.assignment_id, outbox.message_id, processed_events.event_key
                 FROM assignments
                 JOIN messages ON messages.assignment_id = assignments.id
                 JOIN outbox ON outbox.message_id = messages.id
                 JOIN processed_events ON processed_events.mission_id = assignments.mission_id
                 WHERE assignments.mission_id = ?1",
                ["msn-atomic-commit"],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            persisted,
            (
                "asg-atomic-commit".into(),
                "msg-atomic-commit".into(),
                "out-atomic-commit".into(),
                "asg-atomic-commit".into(),
                "msg-atomic-commit".into(),
                "command:cmd-atomic-commit".into(),
            )
        );
        drop(connection);
        drop(kernel);
        assert_eq!(schema_snapshot(database.path()), schema_before);
    }

    #[test]
    fn sqlite_tool_job_request_persists_fingerprint_and_detects_conflict_after_reopen() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-tool-job-request");
        insert_active_worker_assignment(&database, "msn-tool-job-request", "asg-tool-job-request");
        let request = tool_job_request("job-sqlite-request", "asg-tool-job-request");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-tool-job-request",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let receipt = kernel.handle(request.clone()).unwrap();
        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(receipt.created_ids["tool_job"], "job-sqlite-request");
        drop(kernel);

        let connection = Connection::open(database.path()).unwrap();
        let persisted = connection
            .query_row(
                "SELECT assignment_id, source_role, mode, argv_json, env_json, state,
                        request_json, created_at, updated_at
                 FROM tool_jobs WHERE job_id = 'job-sqlite-request'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(persisted.0, "asg-tool-job-request");
        assert_eq!(persisted.1, "worker");
        assert_eq!(persisted.2, "bounded");
        assert_eq!(persisted.3, r#"["cargo","test"]"#);
        assert_eq!(persisted.4, r#"{"CI":"1","NO_COLOR":"1"}"#);
        assert_eq!(persisted.5, "queued");
        assert!(!persisted.6.is_empty());
        assert_eq!(persisted.7, "2026-08-14T06:02:00Z");
        assert_eq!(persisted.8, "2026-08-14T06:02:00Z");
        let processed: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM processed_events
                 WHERE event_key = 'tool-job:job-sqlite-request'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(processed, 1);
        drop(connection);

        let mut reopened = MissionKernel::open_temporary_sqlite_v3(
            "msn-tool-job-request",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let replay = reopened.handle(request.clone()).unwrap();
        assert_eq!(replay.disposition, HandleDisposition::Duplicate);
        let mut conflicting = request;
        let HandleInput::ToolJobRequest { request } = &mut conflicting.input else {
            unreachable!();
        };
        request.argv.push("--all-targets".into());
        let conflict = reopened.handle(conflicting).unwrap();
        assert_eq!(conflict.disposition, HandleDisposition::Rejected);
        assert_eq!(conflict.error.unwrap().code, "input_id_conflict");
    }

    #[test]
    fn sqlite_duplicate_reply_is_consumed_idempotently_after_reopen() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-reply-reopen");
        insert_active_worker_assignment(&database, "msn-reply-reopen", "asg-reply-reopen");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-reply-reopen",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let first = kernel
            .handle(worker_reply_command(
                "cmd-reply-reopen-first",
                "asg-reply-reopen",
                "completed",
                "reply-reopen-first",
            ))
            .unwrap();
        assert_eq!(first.disposition, HandleDisposition::Applied);
        drop(kernel);

        let mut reopened = MissionKernel::open_temporary_sqlite_v3(
            "msn-reply-reopen",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let duplicate = reopened
            .handle(worker_reply_command(
                "cmd-reply-reopen-second",
                "asg-reply-reopen",
                "completed",
                "reply-reopen-second",
            ))
            .unwrap();
        assert_eq!(duplicate.disposition, HandleDisposition::Duplicate);
        drop(reopened);

        let connection = Connection::open(database.path()).unwrap();
        let counts = connection
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM messages WHERE mission_id = 'msn-reply-reopen'),
                    (SELECT COUNT(*) FROM context_ledger WHERE mission_id = 'msn-reply-reopen'),
                    (SELECT COUNT(*) FROM processed_events WHERE mission_id = 'msn-reply-reopen')",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(counts, (3, 1, 2));
    }

    #[test]
    fn sqlite_settled_recovery_requeues_original_delivery_without_revision_change() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-settled-resume");
        insert_active_worker_assignment(&database, "msn-settled-resume", "asg-settled-resume");
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "UPDATE outbox
                 SET status = 'delivered', attempts = 2, claimed_by = 'old-owner',
                     claimed_at = 1, delivered_at = '2026-08-14T06:20:00Z'
                 WHERE id = 'out-active-task'",
                [],
            )
            .unwrap();
        drop(connection);

        let event = assignment_settled_event(
            "event-settled-resume",
            23,
            "asg-settled-resume",
            true,
            "settled-resume",
        );
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-settled-resume",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let receipt = kernel.handle(event.clone()).unwrap();
        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(receipt.revision, Some(0));
        assert_eq!(
            receipt.assignment_state,
            Some(crate::AssignmentState::Active)
        );
        drop(kernel);

        let connection = Connection::open(database.path()).unwrap();
        let persisted = connection
            .query_row(
                "SELECT assignments.state, outbox.status, outbox.attempts,
                        outbox.claimed_by, outbox.claimed_at, outbox.last_error,
                        outbox.delivered_at, team_missions.context_rev,
                        (SELECT COUNT(*) FROM processed_events
                         WHERE mission_id = 'msn-settled-resume')
                 FROM assignments
                 JOIN messages ON messages.assignment_id = assignments.id
                 JOIN outbox ON outbox.message_id = messages.id
                 JOIN team_missions ON team_missions.mission_id = assignments.mission_id
                 WHERE assignments.id = 'asg-settled-resume'
                   AND messages.source_role = 'pm'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(persisted.0, "active");
        assert_eq!(persisted.1, "retry");
        assert_eq!(persisted.2, 2);
        assert_eq!(persisted.3, "");
        assert_eq!(persisted.4, None);
        assert_eq!(
            persisted.5,
            "Agent settled before replying; resuming original Assignment"
        );
        assert_eq!(persisted.6, None);
        assert_eq!(persisted.7, 0);
        assert_eq!(persisted.8, 2);
        drop(connection);

        let mut reopened = MissionKernel::open_temporary_sqlite_v3(
            "msn-settled-resume",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let duplicate = reopened.handle(event).unwrap();
        assert_eq!(duplicate.disposition, HandleDisposition::Duplicate);
    }

    #[test]
    fn sqlite_settled_sequence_is_idempotent_across_distinct_event_ids() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-settled-sequence");
        insert_active_worker_assignment(&database, "msn-settled-sequence", "asg-settled-sequence");
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "UPDATE outbox SET status = 'delivered' WHERE id = 'out-active-task'",
                [],
            )
            .unwrap();
        drop(connection);

        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-settled-sequence",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let first = kernel
            .handle(assignment_settled_event(
                "event-settled-sequence-a",
                17,
                "asg-settled-sequence",
                true,
                "settled-sequence-a",
            ))
            .unwrap();
        let duplicate = kernel
            .handle(assignment_settled_event(
                "event-settled-sequence-b",
                17,
                "asg-settled-sequence",
                true,
                "settled-sequence-b",
            ))
            .unwrap();
        assert_eq!(first.disposition, HandleDisposition::Applied);
        assert_eq!(duplicate.disposition, HandleDisposition::Duplicate);
        drop(kernel);

        let connection = Connection::open(database.path()).unwrap();
        let persisted = connection
            .query_row(
                "SELECT outbox.status, outbox.attempts,
                        (SELECT COUNT(*) FROM processed_events
                         WHERE mission_id = 'msn-settled-sequence')
                 FROM outbox WHERE id = 'out-active-task'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(persisted, ("retry".into(), 0, 3));
    }

    #[test]
    fn sqlite_unsafe_settled_recovery_blocks_once_and_notifies_pm() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-settled-block");
        insert_active_worker_assignment(&database, "msn-settled-block", "asg-settled-block");
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "UPDATE outbox SET status = 'delivered' WHERE id = 'out-active-task'",
                [],
            )
            .unwrap();
        drop(connection);

        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-settled-block",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let receipt = kernel
            .handle(assignment_settled_event(
                "event-settled-block",
                29,
                "asg-settled-block",
                false,
                "settled-block",
            ))
            .unwrap();
        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(receipt.revision, Some(1));
        assert_eq!(
            receipt.assignment_state,
            Some(crate::AssignmentState::Blocked)
        );
        drop(kernel);

        let connection = Connection::open(database.path()).unwrap();
        let persisted = connection
            .query_row(
                "SELECT assignments.state, team_missions.context_rev,
                        (SELECT COUNT(*) FROM context_ledger
                         WHERE mission_id = 'msn-settled-block' AND kind = 'blocked'),
                        (SELECT COUNT(*) FROM messages
                         WHERE mission_id = 'msn-settled-block' AND kind = 'blocked'),
                        (SELECT COUNT(*) FROM outbox
                         WHERE mission_id = 'msn-settled-block' AND target_role = 'pm'),
                        (SELECT status FROM outbox WHERE id = 'out-active-task')
                 FROM assignments
                 JOIN team_missions ON team_missions.mission_id = assignments.mission_id
                 WHERE assignments.id = 'asg-settled-block'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            persisted,
            ("blocked".into(), 1, 1, 1, 1, "delivered".into())
        );
    }

    #[test]
    fn sqlite_tool_job_lifecycle_survives_reopen_with_terminal_output_metadata() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-tool-job-lifecycle");
        insert_active_worker_assignment(
            &database,
            "msn-tool-job-lifecycle",
            "asg-tool-job-lifecycle",
        );
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-tool-job-lifecycle",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        kernel
            .handle(tool_job_request(
                "job-sqlite-lifecycle",
                "asg-tool-job-lifecycle",
            ))
            .unwrap();

        let started = kernel
            .handle(tool_job_transition(
                "job-transition-sqlite-started",
                "job-sqlite-lifecycle",
                "2026-08-14T06:03:00Z",
                started_tool_job_transition(),
            ))
            .unwrap();
        assert_eq!(started.disposition, HandleDisposition::Applied);
        drop(kernel);

        let connection = Connection::open(database.path()).unwrap();
        let running: (String, String, String) = connection
            .query_row(
                "SELECT state, pane_id, started_at FROM tool_jobs
                 WHERE job_id = 'job-sqlite-lifecycle'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            running,
            (
                "running".into(),
                "wteam:p3".into(),
                "2026-08-14T06:03:00Z".into()
            )
        );
        drop(connection);

        let mut reopened = MissionKernel::open_temporary_sqlite_v3(
            "msn-tool-job-lifecycle",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let completed_input = tool_job_transition(
            "job-transition-sqlite-completed",
            "job-sqlite-lifecycle",
            "2026-08-14T06:04:00Z",
            completed_tool_job_transition(),
        );
        let completed = reopened.handle(completed_input.clone()).unwrap();
        let replay = reopened.handle(completed_input).unwrap();
        assert_eq!(completed.disposition, HandleDisposition::Applied);
        assert_eq!(replay.disposition, HandleDisposition::Duplicate);
        let restart = reopened
            .handle(tool_job_transition(
                "job-transition-sqlite-restart",
                "job-sqlite-lifecycle",
                "2026-08-14T06:05:00Z",
                started_tool_job_transition(),
            ))
            .unwrap();
        assert_eq!(restart.disposition, HandleDisposition::Rejected);
        assert_eq!(restart.error.unwrap().code, "invalid_tool_job_transition");
        drop(reopened);

        let connection = Connection::open(database.path()).unwrap();
        let terminal: (String, Option<i64>, i64, String, String) = connection
            .query_row(
                "SELECT state, exit_code, stdout_bytes, stdout_checksum, finished_at
                 FROM tool_jobs WHERE job_id = 'job-sqlite-lifecycle'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(terminal.0, "succeeded");
        assert_eq!(terminal.1, Some(0));
        assert_eq!(terminal.2, 3);
        assert_eq!(
            terminal.3,
            "dc51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22"
        );
        assert_eq!(terminal.4, "2026-08-14T06:04:00Z");
    }

    #[test]
    fn sqlite_tool_job_cancellation_persists_queued_and_running_paths() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-tool-job-cancel");
        insert_active_worker_assignment(&database, "msn-tool-job-cancel", "asg-tool-job-cancel");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-tool-job-cancel",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        kernel
            .handle(tool_job_request(
                "job-sqlite-cancel-queued",
                "asg-tool-job-cancel",
            ))
            .unwrap();
        kernel
            .handle(tool_job_transition(
                "job-transition-sqlite-cancel-queued",
                "job-sqlite-cancel-queued",
                "2026-08-14T06:03:00Z",
                ToolJobTransition::CancelRequested,
            ))
            .unwrap();
        kernel
            .handle(tool_job_request(
                "job-sqlite-cancel-running",
                "asg-tool-job-cancel",
            ))
            .unwrap();
        kernel
            .handle(tool_job_transition(
                "job-transition-sqlite-start-cancel-running",
                "job-sqlite-cancel-running",
                "2026-08-14T06:04:00Z",
                started_tool_job_transition(),
            ))
            .unwrap();
        kernel
            .handle(tool_job_transition(
                "job-transition-sqlite-cancel-running",
                "job-sqlite-cancel-running",
                "2026-08-14T06:05:00Z",
                ToolJobTransition::CancelRequested,
            ))
            .unwrap();
        drop(kernel);

        let connection = Connection::open(database.path()).unwrap();
        let queued_cancel: (String, Option<String>, Option<String>) = connection
            .query_row(
                "SELECT state, cancelled_at, finished_at FROM tool_jobs
                 WHERE job_id = 'job-sqlite-cancel-queued'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(queued_cancel.0, "cancelled");
        assert_eq!(queued_cancel.1.as_deref(), Some("2026-08-14T06:03:00Z"));
        assert_eq!(queued_cancel.2.as_deref(), Some("2026-08-14T06:03:00Z"));
        let running_cancel: (String, Option<String>, Option<String>) = connection
            .query_row(
                "SELECT state, cancelled_at, finished_at FROM tool_jobs
                 WHERE job_id = 'job-sqlite-cancel-running'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(running_cancel.0, "cancelling");
        assert_eq!(running_cancel.1.as_deref(), Some("2026-08-14T06:05:00Z"));
        assert_eq!(running_cancel.2, None);
        drop(connection);

        let mut reopened = MissionKernel::open_temporary_sqlite_v3(
            "msn-tool-job-cancel",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        reopened
            .handle(tool_job_transition(
                "job-transition-sqlite-finish-cancel-running",
                "job-sqlite-cancel-running",
                "2026-08-14T06:06:00Z",
                completed_tool_job_transition(),
            ))
            .unwrap();
        drop(reopened);
        let connection = Connection::open(database.path()).unwrap();
        let final_state: String = connection
            .query_row(
                "SELECT state FROM tool_jobs
                 WHERE job_id = 'job-sqlite-cancel-running'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(final_state, "cancelled");
    }

    #[test]
    fn sqlite_tool_job_transition_rolls_back_state_processed_identity_and_revision_together() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-tool-job-rollback");
        insert_active_worker_assignment(
            &database,
            "msn-tool-job-rollback",
            "asg-tool-job-rollback",
        );
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-tool-job-rollback",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        kernel
            .handle(tool_job_request(
                "job-sqlite-rollback",
                "asg-tool-job-rollback",
            ))
            .unwrap();
        drop(kernel);

        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_tool_job_revision_update
                 BEFORE UPDATE OF context_rev ON team_missions
                 WHEN NEW.context_rev = 2
                 BEGIN
                     SELECT RAISE(ABORT, 'injected late Tool Job write failure');
                 END;",
            )
            .unwrap();
        drop(connection);

        let mut reopened = MissionKernel::open_temporary_sqlite_v3(
            "msn-tool-job-rollback",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let error = reopened
            .handle(tool_job_transition(
                "job-transition-sqlite-rollback",
                "job-sqlite-rollback",
                "2026-08-14T06:03:00Z",
                started_tool_job_transition(),
            ))
            .unwrap_err();
        assert_eq!(error.code, "sqlite_handle_failed");
        assert_eq!(error.details["operation"], "update_mission");
        drop(reopened);

        let connection = Connection::open(database.path()).unwrap();
        let state: (String, String, Option<String>) = connection
            .query_row(
                "SELECT state, pane_id, started_at FROM tool_jobs
                 WHERE job_id = 'job-sqlite-rollback'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, ("queued".into(), String::new(), None));
        let processed: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM processed_events
                 WHERE event_key = 'tool-job-transition:job-transition-sqlite-rollback'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(processed, 0);
        let revision: i64 = connection
            .query_row(
                "SELECT context_rev FROM team_missions
                 WHERE mission_id = 'msn-tool-job-rollback'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(revision, 1);
    }

    #[test]
    fn sqlite_inspect_projects_mission_status_inbox_thread_and_diagnostics_without_writes() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-inspect");
        insert_active_worker_assignment(&database, "msn-inspect", "asg-inspect");
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "INSERT INTO team_roles(
                    mission_id, role, provider, model, thinking, permission_policy,
                    launch_generation, health, pane_id, terminal_id, last_seen_rev,
                    updated_at
                 ) VALUES(
                    'msn-inspect', 'worker', 'codex', 'gpt-5.5', 'high', 'workspace-write',
                    'generation-worker', 'ready', 'wteam:p2', 'terminal-worker', 0,
                    '2026-08-14T06:00:00Z'
                 )",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO context_ledger(
                    mission_id, revision, kind, source_role, summary, refs_json,
                    assignment_id, created_at
                 ) VALUES(
                    'msn-inspect', 1, 'context', 'pm', 'Inspect without mutation',
                    '[\"docs/runtime.md\"]', 'asg-inspect', '2026-08-14T06:01:00Z'
                 )",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE team_missions
                 SET brief = 'Inspect the Mission runtime', context_rev = 1,
                     updated_at = '2026-08-14T06:01:00Z'
                 WHERE mission_id = 'msn-inspect'",
                [],
            )
            .unwrap();
        drop(connection);

        let kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-inspect",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let before = fs::read(database.path()).unwrap();

        let mission = kernel.inspect(crate::InspectQuery::Mission).unwrap();
        assert_eq!(mission.revision, Some(1));
        assert_eq!(mission.data["mission_id"], "msn-inspect");
        assert_eq!(mission.data["brief"], "Inspect the Mission runtime");

        let status = kernel.inspect(crate::InspectQuery::Status).unwrap();
        assert_eq!(status.revision, Some(1));
        assert_eq!(status.data["mission"]["mission_id"], "msn-inspect");
        assert_eq!(status.data["roles"][0]["role"], "worker");
        assert_eq!(
            status.data["roles"][0]["launch_generation"],
            "generation-worker"
        );
        assert_eq!(status.data["assignments"][0]["id"], "asg-inspect");
        assert_eq!(status.data["queued"], 1);
        assert_eq!(status.data["tool_jobs"], json!([]));

        let inbox = kernel
            .inspect(crate::InspectQuery::Inbox {
                role: Some(RoleRef {
                    role: RoleKind::Worker,
                    instance: None,
                }),
            })
            .unwrap();
        assert_eq!(inbox.revision, Some(1));
        assert_eq!(inbox.data["messages"][0]["id"], "msg-active-task");
        assert_eq!(inbox.data["messages"][0]["status"], "sending");
        assert_eq!(inbox.data["messages"][0]["attempts"], 0);

        let thread = kernel
            .inspect(crate::InspectQuery::AssignmentThread {
                assignment_id: "asg-inspect".into(),
            })
            .unwrap();
        assert_eq!(thread.revision, Some(1));
        assert_eq!(thread.data["assignment"]["id"], "asg-inspect");
        assert_eq!(thread.data["messages"][0]["id"], "msg-active-task");

        let diagnostics = kernel.inspect(crate::InspectQuery::Diagnostics).unwrap();
        assert_eq!(diagnostics.revision, Some(1));
        assert_eq!(diagnostics.data["counts"]["assignments"], 1);
        assert_eq!(diagnostics.data["counts"]["messages"], 1);
        assert_eq!(diagnostics.data["counts"]["outbox"], 1);
        assert_eq!(diagnostics.data["counts"]["context_ledger"], 1);
        assert_eq!(diagnostics.data["counts"]["processed_events"], 0);
        assert_eq!(diagnostics.data["counts"]["tool_jobs"], 0);
        drop(kernel);

        assert_eq!(fs::read(database.path()).unwrap(), before);
        let connection = Connection::open(database.path()).unwrap();
        let durable_state: (i64, String, i64, String) = connection
            .query_row(
                "SELECT team_missions.context_rev, outbox.status, outbox.attempts,
                        outbox.claimed_by
                 FROM team_missions JOIN outbox USING(mission_id)
                 WHERE team_missions.mission_id = 'msn-inspect'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(durable_state, (1, "sending".into(), 0, String::new()));
    }

    #[test]
    fn sqlite_reopen_preserves_worker_singleton_capacity() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-worker-capacity");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-worker-capacity",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        kernel
            .handle(task_command(
                "cmd-worker-capacity-first",
                "Keep the worker assignment active",
                "worker-capacity-first",
            ))
            .unwrap();
        drop(kernel);

        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-worker-capacity",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let receipt = kernel
            .handle(task_command(
                "cmd-worker-capacity-second",
                "Do not create a second worker assignment",
                "worker-capacity-second",
            ))
            .unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Rejected);
        assert_eq!(
            receipt.error.as_ref().map(|error| error.code.as_str()),
            Some("role_capacity_exhausted")
        );
        assert_eq!(
            durable_counts(database.path(), "msn-worker-capacity"),
            (0, 1, 1, 1, 1),
            "a rejected transition must not mutate durable Mission state"
        );
    }

    #[test]
    fn concurrent_worker_dispatch_applies_one_singleton_assignment_without_busy_leakage() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-concurrent-worker-capacity";
        insert_mission(&database, mission_id);

        let results = run_concurrent_handles(
            &database,
            mission_id,
            vec![
                task_command(
                    "cmd-concurrent-worker-a",
                    "Run singleton Worker task A",
                    "concurrent-worker-a",
                ),
                task_command(
                    "cmd-concurrent-worker-b",
                    "Run singleton Worker task B",
                    "concurrent-worker-b",
                ),
            ],
            Duration::from_millis(25),
        );
        let receipts = results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("capacity contention must resolve as typed receipts, not leak SQLite busy");

        assert_eq!(
            receipts
                .iter()
                .filter(|receipt| receipt.disposition == HandleDisposition::Applied)
                .count(),
            1
        );
        assert_eq!(
            receipts
                .iter()
                .filter(|receipt| {
                    receipt.disposition == HandleDisposition::Rejected
                        && receipt.error.as_ref().map(|error| error.code.as_str())
                            == Some("role_capacity_exhausted")
                })
                .count(),
            1
        );
        assert_eq!(
            durable_counts(database.path(), mission_id),
            (0, 1, 1, 1, 1),
            "the losing Worker contender must not create durable state or effects"
        );
    }

    #[test]
    fn concurrent_reviewer_dispatch_applies_one_singleton_assignment_without_busy_leakage() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-concurrent-reviewer-capacity";
        insert_mission(&database, mission_id);
        let reviewer = RoleRef {
            role: RoleKind::Reviewer,
            instance: None,
        };

        let results = run_concurrent_handles(
            &database,
            mission_id,
            vec![
                assignment_command(
                    "cmd-concurrent-reviewer-a",
                    "Review result A",
                    "concurrent-reviewer-a",
                    "review",
                    reviewer.clone(),
                    "reviewer",
                    "generation-reviewer",
                ),
                assignment_command(
                    "cmd-concurrent-reviewer-b",
                    "Review result B",
                    "concurrent-reviewer-b",
                    "review",
                    reviewer,
                    "reviewer",
                    "generation-reviewer",
                ),
            ],
            Duration::from_millis(25),
        );
        let receipts = results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("capacity contention must resolve as typed receipts, not leak SQLite busy");

        assert_eq!(
            receipts
                .iter()
                .filter(|receipt| receipt.disposition == HandleDisposition::Applied)
                .count(),
            1
        );
        assert_eq!(
            receipts
                .iter()
                .filter(|receipt| {
                    receipt.disposition == HandleDisposition::Rejected
                        && receipt.error.as_ref().map(|error| error.code.as_str())
                            == Some("role_capacity_exhausted")
                })
                .count(),
            1
        );
        assert_eq!(durable_counts(database.path(), mission_id), (0, 1, 1, 1, 1));
    }

    #[test]
    fn concurrent_scout_dispatch_keeps_independent_instances_active_in_parallel() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-concurrent-scout-capacity";
        insert_mission(&database, mission_id);

        let results = run_concurrent_handles(
            &database,
            mission_id,
            vec![
                assignment_command(
                    "cmd-concurrent-scout-01",
                    "Investigate lane one",
                    "concurrent-scout-01",
                    "task",
                    RoleRef {
                        role: RoleKind::Scout,
                        instance: Some("scout-01".into()),
                    },
                    "scout-01",
                    "generation-scout-01",
                ),
                assignment_command(
                    "cmd-concurrent-scout-02",
                    "Investigate lane two",
                    "concurrent-scout-02",
                    "task",
                    RoleRef {
                        role: RoleKind::Scout,
                        instance: Some("scout-02".into()),
                    },
                    "scout-02",
                    "generation-scout-02",
                ),
            ],
            Duration::from_millis(25),
        );
        let receipts = results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("parallel Scout dispatch must not leak SQLite busy");

        assert!(receipts
            .iter()
            .all(|receipt| receipt.disposition == HandleDisposition::Applied));
        assert_eq!(durable_counts(database.path(), mission_id), (0, 2, 2, 2, 2));
    }

    #[test]
    fn concurrent_duplicate_command_returns_one_applied_and_one_duplicate_receipt() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-concurrent-duplicate";
        insert_mission(&database, mission_id);
        let input = task_command(
            "cmd-concurrent-duplicate",
            "Apply this logical command once",
            "concurrent-duplicate",
        );

        let results = run_concurrent_handles(
            &database,
            mission_id,
            vec![input.clone(), input],
            Duration::from_millis(25),
        );
        let receipts = results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("duplicate contention must resolve as typed receipts, not leak SQLite busy");

        assert_eq!(
            receipts
                .iter()
                .filter(|receipt| receipt.disposition == HandleDisposition::Applied)
                .count(),
            1
        );
        assert_eq!(
            receipts
                .iter()
                .filter(|receipt| receipt.disposition == HandleDisposition::Duplicate)
                .count(),
            1
        );
        assert_eq!(durable_counts(database.path(), mission_id), (0, 1, 1, 1, 1));
    }

    #[test]
    fn sqlite_busy_is_a_bounded_retryable_error_without_partial_mutation() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-bounded-sqlite-busy";
        insert_mission(&database, mission_id);
        let lock = Connection::open(database.path()).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            mission_id,
            database.permit(),
            Duration::from_millis(1),
        )
        .unwrap();

        let error = kernel
            .handle(task_command(
                "cmd-bounded-sqlite-busy",
                "Retry after the competing writer releases its lock",
                "bounded-sqlite-busy",
            ))
            .unwrap_err();

        assert_eq!(error.code, "sqlite_busy");
        assert!(error.retryable);
        assert_eq!(durable_counts(database.path(), mission_id), (0, 0, 0, 0, 0));
        lock.execute_batch("ROLLBACK;").unwrap();
    }

    #[test]
    fn stale_outbox_claim_owner_cannot_resolve_or_release_the_current_claim() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-outbox-claim-owner";
        insert_mission(&database, mission_id);
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "INSERT INTO team_roles(
                     mission_id, role, provider, launch_generation, health, updated_at
                 ) VALUES(?1, 'worker', 'codex', 'generation-worker', 'idle',
                          '2026-08-14T06:00:00Z')",
                [mission_id],
            )
            .unwrap();
        drop(connection);
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            mission_id,
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let queued = kernel
            .handle(context_command(
                "cmd-outbox-claim-owner",
                "Deliver this notice under one claim owner",
                "outbox-claim-owner",
            ))
            .unwrap();
        let outbox_id = queued.created_ids["outbox"].clone();
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute(
                "UPDATE outbox
                 SET status = 'sending', claimed_by = 'driver-current',
                     claimed_at = 1786689600000
                 WHERE id = ?1",
                [&outbox_id],
            )
            .unwrap();
        drop(connection);

        let stale = kernel
            .handle(claimed_effect_result(
                &outbox_id,
                "generation-worker",
                "driver-stale",
            ))
            .unwrap();

        assert_eq!(stale.disposition, HandleDisposition::Rejected);
        assert_eq!(
            stale.error.as_ref().map(|error| error.code.as_str()),
            Some("outbox_claim_owner_mismatch")
        );
        let connection = Connection::open(database.path()).unwrap();
        let still_claimed = connection
            .query_row(
                "SELECT status, claimed_by FROM outbox WHERE id = ?1",
                [&outbox_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap();
        assert_eq!(still_claimed, ("sending".into(), "driver-current".into()));
        drop(connection);

        let current = kernel
            .handle(claimed_effect_result(
                &outbox_id,
                "generation-worker",
                "driver-current",
            ))
            .unwrap();
        assert_eq!(current.disposition, HandleDisposition::Applied);
        let connection = Connection::open(database.path()).unwrap();
        let resolved = connection
            .query_row(
                "SELECT status, claimed_by, claimed_at FROM outbox WHERE id = ?1",
                [&outbox_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(resolved, ("delivered".into(), String::new(), None));
    }

    #[test]
    fn role_launch_lease_release_requires_the_exact_owner_and_opaque_generation() {
        let database = TempDatabase::with_schema_v3();
        let mission_id = "msn-role-launch-lease-owner";
        insert_mission(&database, mission_id);
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(&format!(
                "INSERT INTO team_roles(
                     mission_id, role, provider, launch_generation, health, updated_at
                 ) VALUES(
                     '{mission_id}', 'worker', 'codex', 'generation-current', 'launching',
                     '2026-08-14T06:00:00Z'
                 );
                 INSERT INTO role_launch_leases(
                     mission_id, role, owner, generation, acquired_at, expires_at
                 ) VALUES(
                     '{mission_id}', 'worker', 'launch-owner-current',
                     'generation-current', 1786660000, 1786700000
                 );"
            ))
            .unwrap();
        drop(connection);
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            mission_id,
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let stale_generation = kernel
            .handle(leased_role_observation(
                "obs-role-launch-stale-generation",
                "generation-stale",
                "launch-owner-current",
            ))
            .unwrap();
        assert_eq!(stale_generation.disposition, HandleDisposition::Rejected);
        assert_eq!(
            stale_generation
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("stale_generation")
        );

        let stale_owner = kernel
            .handle(leased_role_observation(
                "obs-role-launch-stale-owner",
                "generation-current",
                "launch-owner-stale",
            ))
            .unwrap();

        assert_eq!(stale_owner.disposition, HandleDisposition::Rejected);
        assert_eq!(
            stale_owner.error.as_ref().map(|error| error.code.as_str()),
            Some("role_launch_lease_owner_mismatch")
        );
        let connection = Connection::open(database.path()).unwrap();
        let unchanged = connection
            .query_row(
                "SELECT team_roles.health,
                        (SELECT COUNT(*) FROM role_launch_leases
                         WHERE mission_id = ?1 AND role = 'worker'),
                        (SELECT COUNT(*) FROM processed_events WHERE mission_id = ?1),
                        team_missions.context_rev
                 FROM team_roles
                 JOIN team_missions USING(mission_id)
                 WHERE team_roles.mission_id = ?1 AND team_roles.role = 'worker'",
                [mission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(unchanged, ("launching".into(), 1, 0, 0));
        drop(connection);

        let current_owner = kernel
            .handle(leased_role_observation(
                "obs-role-launch-current-owner",
                "generation-current",
                "launch-owner-current",
            ))
            .unwrap();
        assert_eq!(current_owner.disposition, HandleDisposition::Applied);
        let connection = Connection::open(database.path()).unwrap();
        let released = connection
            .query_row(
                "SELECT team_roles.health,
                        (SELECT COUNT(*) FROM role_launch_leases
                         WHERE mission_id = ?1 AND role = 'worker'),
                        (SELECT COUNT(*) FROM processed_events WHERE mission_id = ?1),
                        team_missions.context_rev
                 FROM team_roles
                 JOIN team_missions USING(mission_id)
                 WHERE team_roles.mission_id = ?1 AND team_roles.role = 'worker'",
                [mission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(released, ("ready".into(), 0, 1, 1));
    }

    #[test]
    fn sqlite_context_commits_message_ledger_processed_input_and_delivery_atomically() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-context");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-context",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let receipt = kernel
            .handle(context_command(
                "cmd-context",
                "The worker must see this durable context",
                "context",
            ))
            .unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(receipt.created_ids["message"], "msg-context");
        assert_eq!(receipt.created_ids["outbox"], "out-context");
        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let persisted = connection
            .query_row(
                "SELECT team_missions.context_rev,
                        messages.assignment_id, messages.kind, messages.body,
                        context_ledger.kind, context_ledger.summary,
                        outbox.target_role, outbox.status,
                        processed_events.event_key
                 FROM team_missions
                 JOIN messages ON messages.mission_id = team_missions.mission_id
                 JOIN context_ledger ON context_ledger.mission_id = team_missions.mission_id
                 JOIN outbox ON outbox.message_id = messages.id
                 JOIN processed_events ON processed_events.mission_id = team_missions.mission_id
                 WHERE team_missions.mission_id = ?1",
                ["msn-context"],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            persisted,
            (
                1,
                None,
                "context".into(),
                "The worker must see this durable context".into(),
                "context".into(),
                "The worker must see this durable context".into(),
                "worker".into(),
                "queued".into(),
                "command:cmd-context".into(),
            )
        );
    }

    #[test]
    fn sqlite_worker_blocked_reply_commits_the_complete_schema_v3_transition() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-worker-blocked");
        insert_active_worker_assignment(&database, "msn-worker-blocked", "asg-worker-blocked");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-worker-blocked",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let receipt = kernel
            .handle(worker_reply_command(
                "cmd-worker-blocked",
                "asg-worker-blocked",
                "blocked",
                "worker-blocked",
            ))
            .unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(
            receipt.assignment_state,
            Some(crate::AssignmentState::Blocked)
        );
        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let persisted = connection
            .query_row(
                "SELECT team_missions.context_rev,
                        assignments.state,
                        context_ledger.kind, context_ledger.assignment_id,
                        original_outbox.status,
                        reply_message.kind, reply_message.in_reply_to,
                        reply_outbox.target_role, reply_outbox.status,
                        processed_events.event_key
                 FROM team_missions
                 JOIN assignments ON assignments.mission_id = team_missions.mission_id
                 JOIN context_ledger ON context_ledger.assignment_id = assignments.id
                 JOIN messages AS original_message
                   ON original_message.assignment_id = assignments.id
                  AND original_message.source_role = 'pm'
                 JOIN outbox AS original_outbox
                   ON original_outbox.message_id = original_message.id
                 JOIN messages AS reply_message
                   ON reply_message.in_reply_to = assignments.id
                 JOIN outbox AS reply_outbox
                   ON reply_outbox.message_id = reply_message.id
                 JOIN processed_events
                   ON processed_events.mission_id = team_missions.mission_id
                 WHERE team_missions.mission_id = ?1",
                ["msn-worker-blocked"],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            persisted,
            (
                1,
                "blocked".into(),
                "blocked".into(),
                Some("asg-worker-blocked".into()),
                "delivered".into(),
                "blocked".into(),
                Some("asg-worker-blocked".into()),
                "pm".into(),
                "queued".into(),
                "command:cmd-worker-blocked".into(),
            )
        );
    }

    #[test]
    fn sqlite_worker_completed_creates_reviewer_follow_up_in_the_same_transaction() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-worker-completed");
        insert_active_worker_assignment(&database, "msn-worker-completed", "asg-worker-completed");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-worker-completed",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let receipt = kernel
            .handle(worker_reply_command(
                "cmd-worker-completed",
                "asg-worker-completed",
                "completed",
                "worker-completed",
            ))
            .unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(
            receipt.assignment_state,
            Some(crate::AssignmentState::Completed)
        );
        assert_eq!(
            receipt.created_ids["follow_up_assignment"],
            "asg-worker-completed-review"
        );
        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let assignments = connection
            .prepare(
                "SELECT id, target_role, kind, state, parent_id, review_round
                 FROM assignments
                 WHERE mission_id = ?1
                 ORDER BY id",
            )
            .unwrap()
            .query_map(["msn-worker-completed"], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            assignments,
            vec![
                (
                    "asg-worker-completed".into(),
                    "worker".into(),
                    "task".into(),
                    "completed".into(),
                    None,
                    0,
                ),
                (
                    "asg-worker-completed-review".into(),
                    "reviewer".into(),
                    "review".into(),
                    "queued".into(),
                    Some("asg-worker-completed".into()),
                    0,
                ),
            ]
        );
        let reviewer_delivery: (String, String, String, String) = connection
            .query_row(
                "SELECT messages.id, messages.kind, messages.target_role, outbox.status
                 FROM messages
                 JOIN outbox ON outbox.message_id = messages.id
                 WHERE messages.id = 'msg-worker-completed-review'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            reviewer_delivery,
            (
                "msg-worker-completed-review".into(),
                "review".into(),
                "reviewer".into(),
                "queued".into(),
            )
        );
    }

    #[test]
    fn sqlite_reviewer_approved_persists_dual_notices_and_manual_ack_atomically() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-reviewer-approved-manual");
        insert_active_reviewer_assignment(
            &database,
            "msn-reviewer-approved-manual",
            "asg-reviewer-approved-worker",
            "asg-reviewer-approved-review",
        );
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-reviewer-approved-manual",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let receipt = kernel
            .handle(reviewer_reply_command(
                "cmd-reviewer-approved-manual",
                "asg-reviewer-approved-review",
                "approved",
                "reviewer-approved-manual",
            ))
            .unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(
            receipt.created_ids["review_pm_notice_message"],
            "msg-reviewer-approved-manual-review-pm-notice"
        );
        assert_eq!(
            receipt.created_ids["review_worker_notice_message"],
            "msg-reviewer-approved-manual-review-worker-notice"
        );
        let connection = Connection::open(database.path()).unwrap();
        let notices = connection
            .prepare(
                "SELECT id, source_role, target_role, kind, body, context_rev
                 FROM messages
                 WHERE id IN (
                   'msg-reviewer-approved-manual-review-pm-notice',
                   'msg-reviewer-approved-manual-review-worker-notice'
                 )
                 ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            notices,
            vec![
                (
                    "msg-reviewer-approved-manual-review-pm-notice".into(),
                    "reviewer".into(),
                    "pm".into(),
                    "context".into(),
                    "Reviewer approved：The implementation needs one focused correction（review_id=rev-reviewer-approved-manual）".into(),
                    1,
                ),
                (
                    "msg-reviewer-approved-manual-review-worker-notice".into(),
                    "pm".into(),
                    "worker".into(),
                    "context".into(),
                    "Reviewer approved：The implementation needs one focused correction（review_id=rev-reviewer-approved-manual）".into(),
                    1,
                ),
            ]
        );
        let review_acknowledgement: (i64, Option<String>) = connection
            .query_row(
                "SELECT acknowledged_by_pm, acknowledged_at
                 FROM review_revisions
                 WHERE id = 'rev-reviewer-approved-manual'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(review_acknowledgement, (0, None));
    }

    #[test]
    fn sqlite_reviewer_approved_can_acknowledge_review_in_the_same_transaction() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-reviewer-approved-auto");
        insert_active_reviewer_assignment(
            &database,
            "msn-reviewer-approved-auto",
            "asg-reviewer-approved-auto-worker",
            "asg-reviewer-approved-auto-review",
        );
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-reviewer-approved-auto",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let receipt = kernel
            .handle(reviewer_reply_command_with_acknowledgement(
                "cmd-reviewer-approved-auto",
                "asg-reviewer-approved-auto-review",
                "reviewer-approved-auto",
            ))
            .unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        let connection = Connection::open(database.path()).unwrap();
        let review_acknowledgement: (i64, Option<String>) = connection
            .query_row(
                "SELECT acknowledged_by_pm, acknowledged_at
                 FROM review_revisions
                 WHERE id = 'rev-reviewer-approved-auto'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            review_acknowledgement,
            (1, Some("2026-08-14T06:20:00Z".into()))
        );
    }

    #[test]
    fn sqlite_review_notice_failure_rolls_back_verdict_revision_and_follow_up() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-review-notice-rollback");
        insert_active_reviewer_assignment(
            &database,
            "msn-review-notice-rollback",
            "asg-review-notice-worker",
            "asg-review-notice-reviewer",
        );
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_worker_review_notice
                 BEFORE INSERT ON messages
                 WHEN NEW.id = 'msg-review-notice-rollback-review-worker-notice'
                 BEGIN
                   SELECT RAISE(ABORT, 'injected review notice failure');
                 END;",
            )
            .unwrap();
        drop(connection);
        let input = reviewer_reply_command(
            "cmd-review-notice-rollback",
            "asg-review-notice-reviewer",
            "rejected",
            "review-notice-rollback",
        );
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-review-notice-rollback",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let error = kernel.handle(input.clone()).unwrap_err();

        assert_eq!(error.code, "sqlite_handle_failed");
        let connection = Connection::open(database.path()).unwrap();
        let state: (String, i64, i64, i64, i64, i64, i64) = connection
            .query_row(
                "SELECT state,
                        (SELECT context_rev FROM team_missions WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM assignments WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM messages WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM outbox WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM processed_events WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM review_revisions WHERE mission_id = ?1)
                 FROM assignments WHERE id = 'asg-review-notice-reviewer'",
                ["msn-review-notice-rollback"],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(state, ("active".into(), 0, 2, 1, 1, 0, 0));
        connection
            .execute_batch("DROP TRIGGER fail_worker_review_notice;")
            .unwrap();
        drop(connection);

        let receipt = kernel.handle(input).unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(
            receipt.created_ids["review_revision"],
            "rev-review-notice-rollback"
        );
        assert_eq!(
            receipt.created_ids["follow_up_assignment"],
            "asg-review-notice-rollback-fix"
        );
    }

    #[test]
    fn sqlite_reviewer_rejected_commits_review_revision_and_worker_fix_atomically() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-reviewer-rejected");
        insert_active_reviewer_assignment(
            &database,
            "msn-reviewer-rejected",
            "asg-original-worker",
            "asg-active-review",
        );
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-reviewer-rejected",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let receipt = kernel
            .handle(reviewer_reply_command(
                "cmd-reviewer-rejected",
                "asg-active-review",
                "rejected",
                "reviewer-rejected",
            ))
            .unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(
            receipt.created_ids["review_revision"],
            "rev-reviewer-rejected"
        );
        assert_eq!(
            receipt.created_ids["follow_up_assignment"],
            "asg-reviewer-rejected-fix"
        );
        let connection =
            Connection::open_with_flags(database.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let review_revision: (String, String, String, String, String, i64, i64) = connection
            .query_row(
                "SELECT id, reviewer_assignment_id, worker_assignment_id, verdict,
                        refs_json, context_rev, acknowledged_by_pm
                 FROM review_revisions
                 WHERE mission_id = ?1",
                ["msn-reviewer-rejected"],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            review_revision,
            (
                "rev-reviewer-rejected".into(),
                "asg-active-review".into(),
                "asg-original-worker".into(),
                "rejected".into(),
                "[\"tests/reviewer.rs:42\"]".into(),
                1,
                0,
            )
        );
        let fix: (String, String, String, String, i64) = connection
            .query_row(
                "SELECT id, kind, state, parent_id, review_round
                 FROM assignments
                 WHERE id = 'asg-reviewer-rejected-fix'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            fix,
            (
                "asg-reviewer-rejected-fix".into(),
                "fix".into(),
                "queued".into(),
                "asg-original-worker".into(),
                1,
            )
        );
    }

    #[test]
    fn sqlite_review_revision_failure_rolls_back_verdict_and_allows_retry() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-review-rollback");
        insert_active_reviewer_assignment(
            &database,
            "msn-review-rollback",
            "asg-review-rollback-worker",
            "asg-review-rollback-reviewer",
        );
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_review_revision_insert
                 BEFORE INSERT ON review_revisions
                 BEGIN
                   SELECT RAISE(ABORT, 'injected review revision failure');
                 END;",
            )
            .unwrap();
        drop(connection);
        let input = reviewer_reply_command(
            "cmd-review-rollback",
            "asg-review-rollback-reviewer",
            "rejected",
            "review-rollback",
        );
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-review-rollback",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let error = kernel.handle(input.clone()).unwrap_err();

        assert_eq!(error.code, "sqlite_handle_failed");
        let connection = Connection::open(database.path()).unwrap();
        let state: (String, i64, i64, i64, i64, i64) = connection
            .query_row(
                "SELECT state,
                        (SELECT context_rev FROM team_missions WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM messages WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM outbox WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM processed_events WHERE mission_id = ?1),
                        (SELECT COUNT(*) FROM review_revisions WHERE mission_id = ?1)
                 FROM assignments WHERE id = 'asg-review-rollback-reviewer'",
                ["msn-review-rollback"],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(state, ("active".into(), 0, 1, 1, 0, 0));
        connection
            .execute_batch("DROP TRIGGER fail_review_revision_insert;")
            .unwrap();
        drop(connection);

        let receipt = kernel.handle(input).unwrap();

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(
            receipt.created_ids["review_revision"],
            "rev-review-rollback"
        );
    }

    #[test]
    fn sqlite_handle_rolls_back_late_failure_and_allows_the_same_input_to_retry() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-atomic-rollback");
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_outbox_insert
                 BEFORE INSERT ON outbox
                 BEGIN
                   SELECT RAISE(ABORT, 'injected late outbox failure');
                 END;",
            )
            .unwrap();
        drop(connection);
        let input = task_command(
            "cmd-atomic-rollback",
            "Rollback every durable write",
            "atomic-rollback",
        );
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-atomic-rollback",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        kernel
            .handle(input.clone())
            .expect_err("a late durable write failure must not return a receipt");
        drop(kernel);
        assert_eq!(
            durable_counts(database.path(), "msn-atomic-rollback"),
            (0, 0, 0, 0, 0),
            "business state, processed input, and delivery intent must roll back together"
        );

        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch("DROP TRIGGER fail_outbox_insert;")
            .unwrap();
        drop(connection);
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-atomic-rollback",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();

        let receipt = kernel
            .handle(input)
            .expect("the rolled-back input identity must remain safe to retry");

        assert_eq!(receipt.disposition, HandleDisposition::Applied);
        assert_eq!(
            durable_counts(database.path(), "msn-atomic-rollback"),
            (0, 1, 1, 1, 1)
        );
    }

    #[test]
    fn sqlite_reopen_returns_semantic_duplicate_without_recovering_payload_fingerprint() {
        let database = TempDatabase::with_schema_v3();
        insert_mission(&database, "msn-reopen-duplicate");
        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-reopen-duplicate",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        kernel
            .handle(task_command(
                "cmd-reopen-duplicate",
                "Original payload",
                "reopen-original",
            ))
            .unwrap();
        drop(kernel);
        assert_eq!(
            durable_counts(database.path(), "msn-reopen-duplicate"),
            (0, 1, 1, 1, 1)
        );

        let mut kernel = MissionKernel::open_temporary_sqlite_v3(
            "msn-reopen-duplicate",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap();
        let duplicate = kernel
            .handle(KernelInput {
                decision_context: DecisionContext {
                    observed_at: "2026-08-14T06:05:00Z".into(),
                    allocated_ids: BTreeMap::new(),
                    generations: BTreeMap::new(),
                },
                input: HandleInput::Command {
                    command_id: "cmd-reopen-duplicate".into(),
                    kind: "task".into(),
                    source: RoleRef {
                        role: RoleKind::Pm,
                        instance: None,
                    },
                    target: Some(RoleRef {
                        role: RoleKind::Worker,
                        instance: None,
                    }),
                    body: json!({"text": "Changed payload cannot be compared after reopen"}),
                },
            })
            .expect("schema v3 can only classify the persisted event key as duplicate");

        assert_eq!(duplicate.disposition, HandleDisposition::Duplicate);
        assert!(duplicate.effect_intents.is_empty());
        assert!(duplicate.created_ids.is_empty());
        assert_eq!(
            durable_counts(database.path(), "msn-reopen-duplicate"),
            (0, 1, 1, 1, 1),
            "reopening and replaying the event key must not create a second durable object"
        );
    }

    #[test]
    fn sqlite_kernel_binding_rejects_a_missing_mission_without_mutation() {
        let database = TempDatabase::with_schema_v3();
        let before = fs::read(database.path()).unwrap();

        let error = MissionKernel::open_temporary_sqlite_v3(
            "msn-missing",
            database.permit(),
            Duration::from_millis(25),
        )
        .unwrap_err();

        assert_eq!(error.code, "mission_not_found");
        assert_eq!(error.details["mission_id"], "msn-missing");
        assert_eq!(fs::read(database.path()).unwrap(), before);
    }

    #[test]
    fn future_schema_is_rejected_before_the_adapter_can_mutate_it() {
        let database = TempDatabase::with_schema_version("4");
        let before = fs::read(database.path()).unwrap();

        let error = SqliteV3CoordinationStore::open(database.permit(), Duration::from_millis(25))
            .unwrap_err();

        assert_eq!(error.code, "incompatible_schema");
        assert_eq!(fs::read(database.path()).unwrap(), before);
    }

    #[test]
    fn legacy_and_invalid_schema_versions_are_rejected_without_mutation() {
        for version in ["1", "2", "invalid"] {
            let database = TempDatabase::with_schema_version(version);
            let before = fs::read(database.path()).unwrap();

            let error =
                SqliteV3CoordinationStore::open(database.permit(), Duration::from_millis(25))
                    .unwrap_err();

            assert_eq!(error.code, "incompatible_schema");
            assert_eq!(error.details["actual"], version);
            assert_eq!(fs::read(database.path()).unwrap(), before);
        }
    }

    #[test]
    fn missing_schema_version_is_rejected_without_mutation() {
        let database = TempDatabase::with_schema_version("3");
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute("DELETE FROM schema_meta WHERE key = 'schema_version'", [])
            .unwrap();
        drop(connection);
        let before = fs::read(database.path()).unwrap();

        let error = SqliteV3CoordinationStore::open(database.permit(), Duration::from_millis(25))
            .unwrap_err();

        assert_eq!(error.code, "incompatible_schema");
        assert!(error.details["actual"].is_null());
        assert_eq!(fs::read(database.path()).unwrap(), before);
    }

    #[test]
    fn missing_database_file_is_rejected_without_creating_it() {
        let root = std::env::temp_dir().join(format!(
            "herdr-mission-kernel-missing-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("mission-team.sqlite3");

        let error = WritableDatabasePermit::for_test(&root, &path).unwrap_err();

        assert_eq!(error.code, "temporary_database_unavailable");
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn non_temporary_workspace_file_cannot_receive_database_write_permit() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = root.join("Cargo.toml");
        let before = fs::read(&path).unwrap();

        let error = WritableDatabasePermit::for_test(&root, &path).unwrap_err();

        assert_eq!(error.code, "production_path_forbidden");
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn database_symlink_swap_after_permit_is_rejected_without_touching_the_target() {
        use std::os::unix::fs::symlink;

        let database = TempDatabase::with_schema_v3();
        let outside = TempDatabase::with_schema_v3();
        let permit = database.permit();
        let outside_before = fs::read(outside.path()).unwrap();
        fs::remove_file(database.path()).unwrap();
        symlink(outside.path(), database.path()).unwrap();

        let error = SqliteV3CoordinationStore::open(permit, Duration::from_millis(25)).unwrap_err();

        assert_eq!(error.code, "production_path_forbidden");
        assert_eq!(fs::read(outside.path()).unwrap(), outside_before);
    }

    #[test]
    fn schema_v3_marker_without_the_complete_contract_is_rejected() {
        let database = TempDatabase::with_schema_version("3");

        let error = SqliteV3CoordinationStore::open(database.permit(), Duration::from_millis(25))
            .unwrap_err();

        assert_eq!(error.code, "incompatible_schema");
        assert_eq!(error.details["missing_table"], "team_missions");
    }

    #[test]
    fn schema_v3_index_name_on_the_wrong_columns_is_rejected() {
        let database = TempDatabase::with_schema_v3();
        let connection = Connection::open(database.path()).unwrap();
        connection
            .execute_batch(
                "DROP INDEX outbox_target_status;
                 CREATE INDEX outbox_target_status ON outbox(id);",
            )
            .unwrap();
        drop(connection);

        let error = SqliteV3CoordinationStore::open(database.permit(), Duration::from_millis(25))
            .unwrap_err();

        assert_eq!(error.code, "incompatible_schema");
        assert_eq!(error.details["index_mismatch"], "outbox_target_status");
    }
}
