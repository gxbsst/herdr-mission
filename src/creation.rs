//! Team Mission creation persistence.
//!
//! Writes a new Mission (and its default team roles) into the Rust-owned
//! database in a single transaction. Reuses the frozen v3 schema shape and the
//! same ISO-8601 UTC timestamp convention as the kernel store so ordering stays
//! consistent across create and later handle operations.

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::json;

use crate::{
    bootstrap_database, open_writable, read_generation, ErrorCategory, KernelError, OWNER_IDENTITY,
};

pub const TEAM_ROLES: [&str; 4] = ["pm", "worker", "scout", "reviewer"];
pub const DEFAULT_AGENT_PROFILE_ID: &str = "codex-default-v1";
pub const DEFAULT_AGENT_PROFILE_VERSION: i64 = 1;
pub const PI_QUALITY_PROFILE_ID: &str = "pi-quality-v1";

/// The two supported Mission layouts.
///
/// `Team` mirrors the Python team layout (PM / Scout / Worker / Reviewer),
/// while `Simple` mirrors the classic single-Worker layout. The layout is an
/// input selection; it is not persisted separately because it is derivable
/// from the persisted role set (one `worker` versus the full team).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionLayout {
    Team,
    Simple,
}

impl MissionLayout {
    /// Parse a user-facing layout token. `classic` and `solo` are accepted as
    /// aliases of `simple` for continuity with the Python vocabulary.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "team" => Some(Self::Team),
            "simple" | "classic" | "solo" => Some(Self::Simple),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Team => "team",
            Self::Simple => "simple",
        }
    }
}

/// The agent providers selectable when composing Mission roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Codex,
    Pi,
    CursorAgent,
    Grok,
    Claude,
    Droid,
}

impl Provider {
    /// All selectable providers in form order.
    pub const ALL: [Provider; 6] = [
        Provider::Codex,
        Provider::Pi,
        Provider::CursorAgent,
        Provider::Grok,
        Provider::Claude,
        Provider::Droid,
    ];

    /// Parse a provider kind or a legacy profile id back into the enum.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" | DEFAULT_AGENT_PROFILE_ID => Some(Self::Codex),
            "pi" | PI_QUALITY_PROFILE_ID => Some(Self::Pi),
            "cursor-agent" | "cursor" => Some(Self::CursorAgent),
            "grok" => Some(Self::Grok),
            "claude" => Some(Self::Claude),
            "droid" => Some(Self::Droid),
            _ => None,
        }
    }

    /// The Herdr agent kind used as `agent start --kind <kind>`.
    pub const fn agent_kind(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::CursorAgent => "cursor-agent",
            Self::Grok => "grok",
            Self::Claude => "claude",
            Self::Droid => "droid",
        }
    }

    /// The stored profile id bundling provider + permissions + model.
    pub fn profile_id(self) -> String {
        match self {
            Self::Codex => DEFAULT_AGENT_PROFILE_ID.to_string(),
            Self::Pi => PI_QUALITY_PROFILE_ID.to_string(),
            other => format!("{}-default-v1", other.agent_kind()),
        }
    }

    pub const fn profile_version(self) -> i64 {
        1
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Pi => "Pi",
            Self::CursorAgent => "Cursor",
            Self::Grok => "Grok",
            Self::Claude => "Claude",
            Self::Droid => "Droid",
        }
    }

    /// The ordered role names for a layout.
    pub const fn role_names(self, layout: MissionLayout) -> &'static [&'static str] {
        match layout {
            MissionLayout::Team => &["pm", "worker", "scout", "reviewer"],
            MissionLayout::Simple => &["worker"],
        }
    }

    /// Build a single role config using this provider's defaults.
    pub fn role_config(self, role: &str) -> RoleConfig {
        RoleConfig {
            role: role.to_string(),
            provider: self.agent_kind().to_string(),
            model: self.default_model().to_string(),
            thinking: self.default_thinking().to_string(),
            permission_policy: permission_policy(self, role),
            profile_id: self.profile_id(),
            profile_version: self.profile_version(),
            config_digest: String::new(),
        }
    }

    /// Build the preset role set for a layout using this provider.
    pub fn preset_roles(self, layout: MissionLayout) -> Vec<RoleConfig> {
        self.role_names(layout)
            .iter()
            .map(|role| self.role_config(role))
            .collect()
    }

    const fn default_model(self) -> &'static str {
        match self {
            Self::Pi => "opencode-deepseek-v4-flash",
            _ => "",
        }
    }

    const fn default_thinking(self) -> &'static str {
        match self {
            Self::Pi => "high",
            _ => "",
        }
    }
}

/// Return the structural kind of a role name: `worker` from `worker-frontend`,
/// `scout` from `scout-01`, and `worker` from `worker`.
pub fn role_kind(role: &str) -> &str {
    role.split('-').next().unwrap_or(role)
}

/// Whether a role name is a valid kernel routing identity.
///
/// The kernel's role model keeps `pm` / `worker` / `reviewer` as single slots
/// and makes only `scout` multi-instance (`scout-01`, `scout-02`). Worker
/// instances such as `worker-frontend` are not part of the kernel ACL.
pub fn is_valid_role_identity(role: &str) -> bool {
    matches!(role, "pm" | "worker" | "reviewer" | "scout")
        || (role.starts_with("scout-") && role.len() > "scout-".len())
}

/// Parse a role routing identity into a kernel `RoleRef`.
///
/// `pm` / `worker` / `reviewer` are single slots; `scout-XX` is a scout
/// instance. The bare `scout` template is not a routing identity and is
/// rejected here.
pub fn parse_role_ref(role: &str) -> Result<crate::RoleRef, KernelError> {
    match role {
        "pm" => Ok(crate::RoleRef {
            role: crate::RoleKind::Pm,
            instance: None,
        }),
        "worker" => Ok(crate::RoleRef {
            role: crate::RoleKind::Worker,
            instance: None,
        }),
        "reviewer" => Ok(crate::RoleRef {
            role: crate::RoleKind::Reviewer,
            instance: None,
        }),
        // The default Team layout launches a single unnamed scout as `scout`.
        // Accept it as the canonical single-slot Scout identity; named
        // instances (`scout-01`, ...) remain the multi-scout form below.
        "scout" => Ok(crate::RoleRef {
            role: crate::RoleKind::Scout,
            instance: None,
        }),
        value if value.starts_with("scout-") && value.len() > "scout-".len() => {
            Ok(crate::RoleRef {
                role: crate::RoleKind::Scout,
                instance: Some(value.to_string()),
            })
        }
        _ => Err(KernelError {
            category: ErrorCategory::Contract,
            code: "invalid_role_identity".into(),
            message: "role identity is not part of the Team Mission ACL".into(),
            retryable: false,
            details: BTreeMap::from([("role".into(), json!(role))]),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleConfig {
    pub role: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub permission_policy: String,
    pub profile_id: String,
    pub profile_version: i64,
    pub config_digest: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleOverride {
    pub role: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub permission_policy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateMissionRequest {
    pub mission_id: String,
    pub brief: String,
    pub template: String,
    pub agent_profile_id: String,
    pub agent_profile_version: i64,
    pub roles: Vec<RoleConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateMissionOutcome {
    pub mission_id: String,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissionStatus {
    pub mission_id: String,
    pub stage: String,
    pub roles: BTreeMap<String, String>,
    pub pending_assignments: i64,
    pub generation: i64,
}

/// One mission summary row returned by `list_missions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissionSummary {
    pub mission_id: String,
    pub brief: String,
    pub stage: String,
    pub created_at: String,
}

/// One team role row enriched for the interactive control center.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleOverview {
    pub role: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub health: String,
    pub pane_id: String,
    pub agent_name: String,
}

/// One mission row plus its team roles and coordination counters.
///
/// The control center renders both the list and the per-mission detail view
/// from this single snapshot, so a refresh is one query set rather than one
/// query per selected mission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissionOverview {
    pub mission_id: String,
    pub brief: String,
    pub stage: String,
    pub created_at: String,
    pub agent_profile_id: String,
    pub roles: Vec<RoleOverview>,
    pub pending_assignments: i64,
    pub generation: i64,
}

/// The role fields the runtime needs to launch and address an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRuntimeRow {
    pub role: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub pane_id: String,
}

/// Derive a short, deterministic `[a-z0-9]`-only token from a mission id.
///
/// The mission id is `msn-<stamp>-<slug>-<nanos:08x>`. Keeping the last four
/// seconds digits plus the eight nanosecond hex digits yields a 12-char token
/// that is unique per (second, call) without a hashing dependency, and reads
/// like the 12-hex tokens the existing kit uses for live agent names.
pub fn agent_name_token(mission_id: &str) -> String {
    let nanos = mission_id.rsplit('-').next().unwrap_or("mission");
    let stamp_tail = mission_id
        .strip_prefix("msn-")
        .unwrap_or(mission_id)
        .get(..15)
        .unwrap_or("");
    let tail: String = stamp_tail
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{tail}{nanos}")
}

/// Default Codex team profile roles (mirrors Python `codex-default-v1`).
pub fn default_codex_team() -> Vec<RoleConfig> {
    Provider::Codex.preset_roles(MissionLayout::Team)
}

/// Pi quality-first team profile roles (mirrors Python `pi-quality-v1`).
pub fn pi_quality_team() -> Vec<RoleConfig> {
    Provider::Pi.preset_roles(MissionLayout::Team)
}

fn permission_policy(provider: Provider, role: &str) -> String {
    match provider {
        Provider::Codex => match role {
            "pm" => "codex-coordinate-v1".to_string(),
            "worker" => "codex-workspace-write-v1".to_string(),
            _ => "codex-readonly-v1".to_string(),
        },
        Provider::Pi => match role {
            "pm" | "worker" => "pi-full-v1".to_string(),
            _ => "pi-readonly-v1".to_string(),
        },
        other => {
            let kind = other.agent_kind();
            match role {
                "pm" | "worker" => format!("{kind}-full-v1"),
                _ => format!("{kind}-readonly-v1"),
            }
        }
    }
}

/// Resolve the effective role set from a provider preset plus per-role overrides.
pub fn resolve_roles(
    provider: Provider,
    layout: MissionLayout,
    overrides: &[RoleOverride],
) -> Result<Vec<RoleConfig>, KernelError> {
    let mut roles = provider.preset_roles(layout);

    for item in overrides {
        if let Some(role) = roles.iter_mut().find(|role| role.role == item.role) {
            apply_role_override(role, item);
            continue;
        }
        // New instance: inherit the preset defaults of its structural kind.
        let kind = role_kind(&item.role);
        let Some(mut role) = roles.iter().find(|role| role.role == kind).cloned() else {
            return Err(unknown_role(&item.role));
        };
        role.role = item.role.clone();
        apply_role_override(&mut role, item);
        roles.push(role);
    }
    Ok(roles)
}

fn apply_role_override(role: &mut RoleConfig, item: &RoleOverride) {
    if let Some(value) = &item.provider {
        role.provider = value.clone();
    }
    if let Some(value) = &item.model {
        role.model = value.clone();
    }
    if let Some(value) = &item.thinking {
        role.thinking = value.clone();
    }
    if let Some(value) = &item.permission_policy {
        role.permission_policy = value.clone();
    }
}

fn unknown_role(role: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Operation,
        code: "unknown_role".into(),
        message: "team role kind is not recognized".into(),
        retryable: false,
        details: BTreeMap::from([("role".into(), json!(role))]),
    }
}

/// Generate a stable, unique Mission id from a title.
pub fn make_mission_id(title: &str) -> String {
    let (stamp, nanos) = utc_stamp();
    let slug = slugify(title);
    format!("msn-{stamp}-{}-{nanos:08x}", &slug[..slug.len().min(20)])
}

/// Atomically persist a new Mission and its team roles.
///
/// `created` is false when a mission with the same id already exists; the
/// `ON CONFLICT DO NOTHING` upsert makes the operation idempotent under retry.
pub fn create_mission(
    database: &Path,
    request: &CreateMissionRequest,
) -> Result<CreateMissionOutcome, KernelError> {
    bootstrap_database(database)?;
    let mut connection = open_writable(database, OWNER_IDENTITY)?;
    let now = utc_timestamp();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| sqlite_error("sqlite_begin_failed", "create_mission", error))?;

    let mission_inserted = transaction
        .execute(
            "INSERT INTO team_missions(
                mission_id, brief, template, agent_profile_id, agent_profile_version,
                context_rev, created_at, updated_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, 0, ?6, ?6)
             ON CONFLICT(mission_id) DO NOTHING",
            params![
                request.mission_id,
                request.brief,
                request.template,
                request.agent_profile_id,
                request.agent_profile_version,
                now,
            ],
        )
        .map_err(|error| sqlite_error("sqlite_mission_write_failed", "insert_mission", error))?;

    transaction
        .execute(
            "INSERT INTO mission_state(mission_id, stage, updated_at)
             VALUES(?1, 'preparing', ?2)
             ON CONFLICT(mission_id) DO NOTHING",
            params![request.mission_id, now],
        )
        .map_err(|error| {
            sqlite_error("sqlite_state_write_failed", "insert_mission_state", error)
        })?;

    for role in &request.roles {
        transaction
            .execute(
                "INSERT INTO team_roles(
                    mission_id, role, provider, model, thinking, permission_policy,
                    profile_id, profile_version, config_digest, pane_id, terminal_id,
                    session_json, launch_generation, health, last_seen_rev, updated_at
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, '', '', NULL, '', 'unknown', 0, ?10)
                 ON CONFLICT(mission_id, role) DO NOTHING",
                params![
                    request.mission_id,
                    role.role,
                    role.provider,
                    role.model,
                    role.thinking,
                    role.permission_policy,
                    role.profile_id,
                    role.profile_version,
                    role.config_digest,
                    now,
                ],
            )
            .map_err(|error| sqlite_error("sqlite_role_write_failed", "insert_role", error))?;
    }

    transaction
        .commit()
        .map_err(|error| sqlite_error("sqlite_commit_failed", "create_mission", error))?;

    Ok(CreateMissionOutcome {
        mission_id: request.mission_id.clone(),
        created: mission_inserted > 0,
    })
}

/// Read the stored Mission lifecycle stage, defaulting to `preparing` when the
/// mission exists but no stage has been recorded yet.
pub fn read_mission_state(
    database: &Path,
    mission_id: &str,
) -> Result<Option<String>, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    connection
        .query_row(
            "SELECT stage FROM mission_state WHERE mission_id = ?1",
            [mission_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_state_read_failed", "read_mission_state", error))
}

/// Read the Mission title (the `brief` column) used for role prompts and
/// human-readable output.
pub fn read_mission_title(database: &Path, mission_id: &str) -> Result<String, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    connection
        .query_row(
            "SELECT brief FROM team_missions WHERE mission_id = ?1",
            [mission_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_mission_read_failed", "read_mission_title", error))?
        .ok_or_else(|| KernelError {
            category: ErrorCategory::Domain,
            code: "mission_not_found".into(),
            message: "Mission is not present in the Rust-owned database".into(),
            retryable: false,
            details: BTreeMap::from([("mission_id".into(), json!(mission_id))]),
        })
}

/// Read the coordination status of a Mission: stage, per-role health, pending
/// assignment count, and database generation.
pub fn read_mission_status(
    database: &Path,
    mission_id: &str,
) -> Result<MissionStatus, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;

    let mission_exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM team_missions WHERE mission_id = ?1)",
            [mission_id],
            |row| row.get(0),
        )
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "mission_exists", error))?;
    if !mission_exists {
        return Err(KernelError {
            category: ErrorCategory::Domain,
            code: "mission_not_found".into(),
            message: "Mission is not present in the Rust-owned database".into(),
            retryable: false,
            details: BTreeMap::from([("mission_id".into(), json!(mission_id))]),
        });
    }

    let stage = connection
        .query_row(
            "SELECT stage FROM mission_state WHERE mission_id = ?1",
            [mission_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "stage", error))?
        .unwrap_or_else(|| "preparing".to_string());

    let mut statement = connection
        .prepare("SELECT role, health FROM team_roles WHERE mission_id = ?1 ORDER BY role")
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "roles", error))?;
    let roles = statement
        .query_map([mission_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "roles", error))?
        .collect::<Result<BTreeMap<_, _>, _>>()
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "roles", error))?;

    let pending_assignments: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM assignments WHERE mission_id = ?1 AND state = 'queued'",
            [mission_id],
            |row| row.get(0),
        )
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "assignments", error))?;

    let generation = read_generation(&connection)?;

    Ok(MissionStatus {
        mission_id: mission_id.to_string(),
        stage,
        roles,
        pending_assignments,
        generation,
    })
}

/// Result of deleting a Mission and its coordination rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteMissionOutcome {
    pub mission_id: String,
    pub deleted: bool,
    /// The workspace id bound to this mission (if any), so the caller can close
    /// the leftover workspace after the rows are gone.
    pub workspace_id: Option<String>,
    pub prompt_dir_removed: bool,
}

/// Delete a Mission and every row that references it.
///
/// All coordination tables declare `ON DELETE CASCADE` against
/// `team_missions`, and `open_writable` enables foreign keys, so a single
/// `DELETE FROM team_missions` also clears roles, assignments, messages,
/// outbox, state, and the workspace identity. The per-mission prompt directory
/// is removed separately since it lives beside the database, not inside it.
pub fn delete_mission(
    database: &Path,
    mission_id: &str,
) -> Result<DeleteMissionOutcome, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    let workspace_id = connection
        .query_row(
            "SELECT workspace_id FROM mission_workspace WHERE mission_id = ?1",
            [mission_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_workspace_read_failed", "delete_mission", error))?
        .filter(|value| !value.is_empty());
    let deleted = connection
        .execute(
            "DELETE FROM team_missions WHERE mission_id = ?1",
            [mission_id],
        )
        .map_err(|error| sqlite_error("sqlite_mission_delete_failed", "delete_mission", error))?;
    drop(connection);

    let prompt_dir_removed = remove_mission_prompts(database, mission_id);
    Ok(DeleteMissionOutcome {
        mission_id: mission_id.to_string(),
        deleted: deleted > 0,
        workspace_id,
        prompt_dir_removed,
    })
}

fn remove_mission_prompts(database: &Path, mission_id: &str) -> bool {
    let Some(parent) = database.parent() else {
        return false;
    };
    let dir = parent.join("mission-prompts").join(mission_id);
    fs::remove_dir_all(&dir).is_ok()
}

/// Resolve a user-supplied mission spec (id or title) to a canonical id.
///
/// An exact id match wins. Otherwise the spec is treated as a brief/title and
/// must match exactly one mission; multiple matches fail closed so the caller
/// can ask for an explicit id.
pub fn resolve_mission_id(database: &Path, spec: &str) -> Result<String, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    let by_id = connection
        .query_row(
            "SELECT mission_id FROM team_missions WHERE mission_id = ?1",
            [spec],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_mission_read_failed", "resolve_mission_id", error))?;
    if let Some(id) = by_id {
        return Ok(id);
    }

    let mut statement = connection
        .prepare("SELECT mission_id FROM team_missions WHERE brief = ?1 ORDER BY created_at DESC")
        .map_err(|error| sqlite_error("sqlite_mission_read_failed", "resolve_mission_id", error))?;
    let matches = statement
        .query_map([spec], |row| row.get::<_, String>(0))
        .map_err(|error| sqlite_error("sqlite_mission_read_failed", "resolve_mission_id", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sqlite_error("sqlite_mission_read_failed", "resolve_mission_id", error))?;

    match matches.as_slice() {
        [id] => Ok(id.clone()),
        [] => Err(KernelError {
            category: ErrorCategory::Domain,
            code: "mission_not_found".into(),
            message: "Mission 不存在".into(),
            retryable: false,
            details: BTreeMap::from([("mission".into(), json!(spec))]),
        }),
        _ => Err(KernelError {
            category: ErrorCategory::Domain,
            code: "mission_ambiguous".into(),
            message: "Mission 名称匹配到多个，请用 Mission ID".into(),
            retryable: false,
            details: BTreeMap::from([
                ("mission".into(), json!(spec)),
                ("count".into(), json!(matches.len())),
            ]),
        }),
    }
}

/// List every persisted mission with its stage, newest first.
pub fn list_missions(database: &Path) -> Result<Vec<MissionSummary>, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    let mut statement = connection
        .prepare(
            "SELECT m.mission_id, m.brief, COALESCE(s.stage, 'preparing'), m.created_at
             FROM team_missions m
             LEFT JOIN mission_state s ON s.mission_id = m.mission_id
             ORDER BY m.created_at DESC, m.mission_id",
        )
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "list_missions", error))?;
    let rows = statement
        .query_map([], |row| {
            Ok(MissionSummary {
                mission_id: row.get(0)?,
                brief: row.get(1)?,
                stage: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "list_missions", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "list_missions", error))?;
    Ok(rows)
}

/// Read every mission together with its team roles and coordination counters.
///
/// The control center uses this single snapshot for both the queue table and
/// the selected-mission detail box, avoiding one query per mission on every
/// refresh. Missions are returned newest first.
pub fn read_mission_overviews(database: &Path) -> Result<Vec<MissionOverview>, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    let generation = read_generation(&connection)?;

    let mut mission_statement = connection
        .prepare(
            "SELECT m.mission_id, m.brief, COALESCE(s.stage, 'preparing'),
                    m.created_at, m.agent_profile_id
             FROM team_missions m
             LEFT JOIN mission_state s ON s.mission_id = m.mission_id
             ORDER BY m.created_at DESC, m.mission_id",
        )
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "overviews", error))?;
    let mission_rows = mission_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "overviews", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "overviews", error))?;

    let mut role_statement = connection
        .prepare(
            "SELECT mission_id, role, provider, model, thinking, health, pane_id, terminal_id
             FROM team_roles ORDER BY mission_id, role",
        )
        .map_err(|error| sqlite_error("sqlite_roles_read_failed", "overviews", error))?;
    let role_rows = role_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(|error| sqlite_error("sqlite_roles_read_failed", "overviews", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sqlite_error("sqlite_roles_read_failed", "overviews", error))?;

    let mut pending_statement = connection
        .prepare(
            "SELECT mission_id, COUNT(*) FROM assignments
             WHERE state = 'queued' GROUP BY mission_id",
        )
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "overviews", error))?;
    let pending_rows = pending_statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "overviews", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sqlite_error("sqlite_status_read_failed", "overviews", error))?;
    let pending_by_mission: BTreeMap<String, i64> = pending_rows.into_iter().collect();

    let mut roles_by_mission: BTreeMap<String, Vec<RoleOverview>> = BTreeMap::new();
    for (mission_id, role, provider, model, thinking, health, pane_id, agent_name) in role_rows {
        roles_by_mission
            .entry(mission_id)
            .or_default()
            .push(RoleOverview {
                role,
                provider,
                model,
                thinking,
                health,
                pane_id,
                agent_name,
            });
    }

    let mut overviews = Vec::with_capacity(mission_rows.len());
    for (mission_id, brief, stage, created_at, agent_profile_id) in mission_rows {
        let mut roles = roles_by_mission.remove(&mission_id).unwrap_or_default();
        roles.sort_by_key(|role| team_role_order(&role.role));
        let pending_assignments = pending_by_mission.get(&mission_id).copied().unwrap_or(0);
        overviews.push(MissionOverview {
            mission_id,
            brief,
            stage,
            created_at,
            agent_profile_id,
            roles,
            pending_assignments,
            generation,
        });
    }

    Ok(overviews)
}

fn team_role_order(role: &str) -> usize {
    TEAM_ROLES
        .iter()
        .position(|candidate| role_kind(role) == *candidate)
        .unwrap_or(TEAM_ROLES.len())
}

/// Read the role/provider/model/thinking rows needed to launch a Mission's
/// team roles, ordered by role name.
pub fn read_role_runtime(
    database: &Path,
    mission_id: &str,
) -> Result<Vec<RoleRuntimeRow>, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    let mut statement = connection
        .prepare(
            "SELECT role, provider, model, thinking, pane_id
             FROM team_roles WHERE mission_id = ?1 ORDER BY role",
        )
        .map_err(|error| sqlite_error("sqlite_roles_read_failed", "read_role_runtime", error))?;
    let rows = statement
        .query_map([mission_id], |row| {
            Ok(RoleRuntimeRow {
                role: row.get(0)?,
                provider: row.get(1)?,
                model: row.get(2)?,
                thinking: row.get(3)?,
                pane_id: row.get(4)?,
            })
        })
        .map_err(|error| sqlite_error("sqlite_roles_read_failed", "read_role_runtime", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sqlite_error("sqlite_roles_read_failed", "read_role_runtime", error))?;
    Ok(rows)
}

/// Record a role's live runtime identity after its agent is started.
///
/// `pane_id` is stored in the `pane_id` column; the live agent name is stored
/// in the legacy `terminal_id` column because the frozen v3 schema has no
/// dedicated `agent_name` column and the kernel validates its table shape.
pub fn record_role_runtime(
    database: &Path,
    mission_id: &str,
    role: &str,
    pane_id: &str,
    agent_name: &str,
) -> Result<(), KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    let now = utc_timestamp();
    connection
        .execute(
            "UPDATE team_roles
             SET pane_id = ?1, terminal_id = ?2, health = 'idle', updated_at = ?3
             WHERE mission_id = ?4 AND role = ?5",
            rusqlite::params![pane_id, agent_name, now, mission_id, role],
        )
        .map_err(|error| sqlite_error("sqlite_role_update_failed", "record_role_runtime", error))?;
    Ok(())
}

/// Set the Mission lifecycle stage.
pub fn set_mission_stage(
    database: &Path,
    mission_id: &str,
    stage: &str,
) -> Result<(), KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    let now = utc_timestamp();
    connection
        .execute(
            "INSERT INTO mission_state(mission_id, stage, updated_at)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(mission_id) DO UPDATE SET stage = excluded.stage, updated_at = excluded.updated_at",
            rusqlite::params![mission_id, stage, now],
        )
        .map_err(|error| sqlite_error("sqlite_stage_update_failed", "set_mission_stage", error))?;
    Ok(())
}

/// Current time as an ISO-8601 UTC string (`YYYY-MM-DDTHH:MM:SSZ`), matching the
/// kernel store's `observed_at` convention.
pub fn utc_timestamp() -> String {
    let secs = unix_seconds();
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn utc_stamp() -> (String, u32) {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs() as i64;
    let nanos = duration.subsec_nanos();
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    (
        format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}"),
        nanos,
    )
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn slugify(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            separator = false;
        } else if !output.is_empty() && !separator {
            output.push('-');
            separator = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "task".to_string()
    } else {
        output
    }
}

/// Howard Hinnant's civil-from-days algorithm (public domain).
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
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
