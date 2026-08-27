//! Rust-owned database bootstrap.
//!
//! Creates the plugin SQLite database from an empty path in a single write
//! transaction, writes the schema/protocol identity, the owner marker, and the
//! initial database generation. The bootstrap is idempotent and safe under
//! concurrent first use: the schema DDL is `IF NOT EXISTS`, and the owner and
//! generation keys are inserted with `ON CONFLICT DO NOTHING` so the first
//! writer wins and later writers observe the same identity.

use std::{collections::BTreeMap, fs, path::Path, time::Duration};

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::json;

use crate::{ErrorCategory, KernelError};

/// Stable owner identity written into every Rust-owned database.
pub const OWNER_IDENTITY: &str = "herdr-mission";

pub const KEY_SCHEMA_VERSION: &str = "schema_version";
pub const KEY_OWNER: &str = "plugin_owner";
pub const KEY_GENERATION: &str = "database_generation";

/// Schema version shared with the frozen v3 store contract.
pub const SCHEMA_VERSION: &str = "3";

/// The frozen v3 DDL reused as the Rust-owned schema shape. The final statement
/// in this fixture pins `schema_version` to `"3"`.
const SCHEMA_DDL: &str = include_str!("../tests/fixtures/schema-v3.sql");

/// Plugin-owned tables layered on top of the frozen v3 coordination schema.
/// The kernel store validates only its required tables/columns, so additive
/// tables are safe. `mission_state` carries the Mission lifecycle stage that the
/// Python system kept in a separate JSON store.
const PLUGIN_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS mission_state (
    mission_id TEXT PRIMARY KEY,
    stage TEXT NOT NULL DEFAULT 'preparing',
    updated_at TEXT NOT NULL,
    FOREIGN KEY (mission_id) REFERENCES team_missions(mission_id)
        ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS mission_workspace (
    mission_id TEXT PRIMARY KEY,
    source TEXT NOT NULL DEFAULT 'current',
    workspace_id TEXT NOT NULL DEFAULT '',
    tab_id TEXT NOT NULL DEFAULT '',
    root_pane_id TEXT NOT NULL DEFAULT '',
    execution_tab_id TEXT NOT NULL DEFAULT '',
    review_tab_id TEXT NOT NULL DEFAULT '',
    verification_tab_id TEXT NOT NULL DEFAULT '',
    worktree_path TEXT NOT NULL DEFAULT '',
    branch TEXT NOT NULL DEFAULT '',
    FOREIGN KEY (mission_id) REFERENCES team_missions(mission_id)
        ON DELETE CASCADE
);
"#;

const BUSY_TIMEOUT: Duration = Duration::from_millis(2_000);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapOutcome {
    pub owner: String,
    pub generation: i64,
    pub created: bool,
}

/// Atomically create and initialize the Rust-owned database.
///
/// Returns the recorded owner identity and generation. `created` is true when
/// the database file did not exist before this call (this process performed the
/// first initialization); it is false when the database already existed.
pub fn bootstrap_database(database: &Path) -> Result<BootstrapOutcome, KernelError> {
    if let Some(parent) = database.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| KernelError {
                category: ErrorCategory::Infrastructure,
                code: "database_parent_create_failed".into(),
                message: "failed to create the database parent directory".into(),
                retryable: false,
                details: BTreeMap::from([
                    ("path".into(), json!(parent)),
                    ("reason".into(), json!(error.to_string())),
                ]),
            })?;
        }
    }

    let created = !database.exists();

    let mut connection = open_create_connection(database)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| sqlite_error("sqlite_begin_failed", "begin_immediate", error))?;

    transaction
        .execute_batch(SCHEMA_DDL)
        .map_err(|error| sqlite_error("sqlite_schema_write_failed", "execute_ddl", error))?;
    transaction
        .execute_batch(PLUGIN_DDL)
        .map_err(|error| sqlite_error("sqlite_schema_write_failed", "execute_plugin_ddl", error))?;
    migrate_mission_workspace(&transaction).map_err(|error| {
        sqlite_error(
            "sqlite_migration_failed",
            "migrate_mission_workspace",
            error,
        )
    })?;
    transaction
        .execute(
            "INSERT INTO schema_meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO NOTHING",
            rusqlite::params![KEY_OWNER, OWNER_IDENTITY],
        )
        .map_err(|error| sqlite_error("sqlite_owner_write_failed", "insert_owner", error))?;
    transaction
        .execute(
            "INSERT INTO schema_meta(key, value) VALUES(?1, '1') ON CONFLICT(key) DO NOTHING",
            rusqlite::params![KEY_GENERATION],
        )
        .map_err(|error| {
            sqlite_error("sqlite_generation_write_failed", "insert_generation", error)
        })?;
    transaction
        .commit()
        .map_err(|error| sqlite_error("sqlite_commit_failed", "commit", error))?;

    let owner = read_meta(&connection, KEY_OWNER)?.unwrap_or_else(|| OWNER_IDENTITY.to_string());
    let generation = read_meta(&connection, KEY_GENERATION)?
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(1);

    Ok(BootstrapOutcome {
        owner,
        generation,
        created,
    })
}

/// Add `mission_workspace` columns introduced after the first release.
///
/// `CREATE TABLE IF NOT EXISTS` only creates the table once, so databases from
/// earlier builds need the new columns added in-place. `PRAGMA table_info` lets
/// us add each missing column idempotently without relying on a bundled SQLite
/// version that supports `ADD COLUMN IF NOT EXISTS`.
fn migrate_mission_workspace(connection: &Connection) -> rusqlite::Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(mission_workspace)")?;
    let columns: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    for column in ["execution_tab_id", "review_tab_id", "verification_tab_id"] {
        if !columns.iter().any(|name| name == column) {
            connection.execute(
                &format!(
                    "ALTER TABLE mission_workspace ADD COLUMN {column} TEXT NOT NULL DEFAULT ''"
                ),
                [],
            )?;
        }
    }
    Ok(())
}

/// Open an existing Rust-owned database read-write and verify the owner marker.
///
/// Fails with an owner error when the recorded owner is absent or differs from
/// the caller-provided identity, so a foreign database (for example the frozen
/// Python database) is never written by the Rust runtime.
pub fn open_writable(database: &Path, expected_owner: &str) -> Result<Connection, KernelError> {
    let connection = open_create_connection(database)?;
    if !has_schema_meta(&connection)? {
        return Err(owner_error(None, expected_owner));
    }
    let recorded = read_meta(&connection, KEY_OWNER)?;
    match recorded.as_deref() {
        Some(owner) if owner == expected_owner => Ok(connection),
        Some(other) => Err(owner_error(Some(other), expected_owner)),
        None => Err(owner_error(None, expected_owner)),
    }
}

/// Read the current database generation, defaulting to 1 when absent.
pub fn read_generation(connection: &Connection) -> Result<i64, KernelError> {
    Ok(read_meta(connection, KEY_GENERATION)?
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(1))
}

/// Atomically increment the database generation and return the new value.
///
/// The generation is monotonic within a single writer; concurrent callers are
/// serialized by SQLite's write lock so each observes a strictly greater value
/// than the last committed one.
pub fn bump_generation(connection: &mut Connection) -> Result<i64, KernelError> {
    let next = read_generation(connection)?.saturating_add(1);
    connection
        .execute(
            "UPDATE schema_meta SET value = ?1 WHERE key = ?2",
            rusqlite::params![next.to_string(), KEY_GENERATION],
        )
        .map_err(|error| sqlite_error("sqlite_generation_write_failed", "bump", error))?;
    Ok(next)
}

fn open_create_connection(database: &Path) -> Result<Connection, KernelError> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| sqlite_error("sqlite_open_failed", "open", error))?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(|error| sqlite_error("sqlite_config_failed", "busy_timeout", error))?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|error| sqlite_error("sqlite_config_failed", "foreign_keys", error))?;
    Ok(connection)
}

fn read_meta(connection: &Connection, key: &str) -> Result<Option<String>, KernelError> {
    connection
        .query_row(
            "SELECT value FROM schema_meta WHERE key = ?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_meta_read_failed", key, error))
}

fn has_schema_meta(connection: &Connection) -> Result<bool, KernelError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| sqlite_error("sqlite_meta_read_failed", "schema_meta_exists", error))
}

fn owner_error(recorded: Option<&str>, expected: &str) -> KernelError {
    let mut details = BTreeMap::from([
        ("expected_owner".into(), json!(expected)),
        ("actual_owner".into(), json!(recorded.unwrap_or("(absent)"))),
    ]);
    details.insert(
        "reason".into(),
        json!(if recorded.is_none() {
            "database has no owner marker and is not Rust-owned"
        } else {
            "database owner marker does not match the Rust runtime"
        }),
    );
    KernelError {
        category: ErrorCategory::Contract,
        code: "database_owner_mismatch".into(),
        message: "database is not owned by the Rust Mission runtime".into(),
        retryable: false,
        details,
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
