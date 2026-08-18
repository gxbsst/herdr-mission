use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use herdr_mission::{
    bootstrap_database, bump_generation, open_writable, read_generation, ErrorCategory,
    OWNER_IDENTITY, SCHEMA_VERSION,
};
use rusqlite::Connection;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn temp_db_path(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "herdr-mission-{label}-{}-{id}.sqlite3",
        std::process::id()
    ))
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

fn schema_version(path: &Path) -> String {
    let connection = Connection::open(path).unwrap();
    connection
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
}

#[test]
fn bootstrap_creates_schema_owner_and_generation() {
    let path = temp_db_path("fresh");
    let outcome = bootstrap_database(&path).unwrap();

    assert!(outcome.created);
    assert_eq!(outcome.owner, OWNER_IDENTITY);
    assert_eq!(outcome.generation, 1);
    assert_eq!(schema_version(&path), SCHEMA_VERSION);

    let connection = Connection::open(&path).unwrap();
    let mission_table: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='team_missions')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(mission_table);

    cleanup(&path);
}

#[test]
fn bootstrap_is_idempotent_and_does_not_reset_generation() {
    let path = temp_db_path("repeat");
    let first = bootstrap_database(&path).unwrap();
    let second = bootstrap_database(&path).unwrap();

    assert!(first.created);
    assert!(!second.created);
    assert_eq!(first.owner, second.owner);
    assert_eq!(first.generation, 1);
    assert_eq!(second.generation, 1);

    cleanup(&path);
}

#[test]
fn open_writable_accepts_matching_owner_and_reads_generation() {
    let path = temp_db_path("owner-ok");
    bootstrap_database(&path).unwrap();

    let connection = open_writable(&path, OWNER_IDENTITY).unwrap();
    assert_eq!(read_generation(&connection).unwrap(), 1);

    cleanup(&path);
}

#[test]
fn open_writable_rejects_foreign_owner() {
    let path = temp_db_path("owner-foreign");
    bootstrap_database(&path).unwrap();

    let error = open_writable(&path, "python-runtime").unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(error.code, "database_owner_mismatch");

    cleanup(&path);
}

#[test]
fn open_writable_rejects_database_without_owner_marker() {
    let path = temp_db_path("owner-missing");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE unrelated (id INTEGER PRIMARY KEY);")
        .unwrap();
    drop(connection);

    let error = open_writable(&path, OWNER_IDENTITY).unwrap_err();
    assert_eq!(error.category, ErrorCategory::Contract);
    assert_eq!(error.code, "database_owner_mismatch");

    cleanup(&path);
}

#[test]
fn bootstrap_creates_parent_directories() {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("herdr-mission-nested-{}-{id}", std::process::id()));
    let path = root.join("a").join("b").join("missions.sqlite3");

    let outcome = bootstrap_database(&path).unwrap();
    assert!(outcome.created);
    assert!(path.exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn generation_is_monotonic_across_bumps() {
    let path = temp_db_path("generation");
    bootstrap_database(&path).unwrap();

    let mut connection = open_writable(&path, OWNER_IDENTITY).unwrap();
    assert_eq!(read_generation(&connection).unwrap(), 1);
    assert_eq!(bump_generation(&mut connection).unwrap(), 2);
    assert_eq!(bump_generation(&mut connection).unwrap(), 3);
    assert_eq!(read_generation(&connection).unwrap(), 3);

    cleanup(&path);
}
