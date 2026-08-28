use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use herdr_mission::{
    bootstrap_database, create_mission, default_codex_team, delete_mission, make_mission_id,
    parse_role_ref, read_mission_launch_mode, read_mission_overviews, read_mission_status,
    read_workspace, resolve_mission_id, resolve_roles, role_kind, set_mission_launch_mode,
    upsert_workspace, utc_timestamp, CreateMissionRequest, LaunchMode, MissionLayout,
    MissionWorkspace, Provider, RoleKind, RoleOverride, WorkspaceSource, TEAM_ROLES,
};
use rusqlite::Connection;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn temp_db_path(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "herdr-mission-creation-{label}-{}-{id}.sqlite3",
        std::process::id()
    ))
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn request(mission_id: &str) -> CreateMissionRequest {
    CreateMissionRequest {
        mission_id: mission_id.to_string(),
        brief: "Test mission".into(),
        template: "general".into(),
        agent_profile_id: "codex-default-v1".into(),
        agent_profile_version: 1,
        launch_mode: LaunchMode::Manual,
        roles: default_codex_team(),
    }
}

#[test]
fn create_mission_writes_mission_and_all_roles() {
    let path = temp_db_path("create");
    let outcome = create_mission(&path, &request("msn-test-1")).unwrap();

    assert!(outcome.created);
    assert_eq!(outcome.mission_id, "msn-test-1");

    let connection = Connection::open(&path).unwrap();
    let role_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM team_roles WHERE mission_id = 'msn-test-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(role_count, TEAM_ROLES.len() as i64);

    let mission_exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM team_missions WHERE mission_id = 'msn-test-1')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(mission_exists);

    cleanup(&path);
}

#[test]
fn workspace_round_trips_through_persistence() {
    let path = temp_db_path("workspace");
    create_mission(&path, &request("msn-ws-1")).unwrap();

    assert!(read_workspace(&path, "msn-ws-1").unwrap().is_none());

    let workspace = MissionWorkspace {
        source: WorkspaceSource::Current,
        workspace_id: "w6J:ws1".into(),
        tab_id: "w6J:t1".into(),
        root_pane_id: "w6J:p0".into(),
        execution_tab_id: "w6J:te".into(),
        review_tab_id: "w6J:tr".into(),
        verification_tab_id: "w6J:tv".into(),
        worktree_path: "/repo".into(),
        branch: "main".into(),
    };
    upsert_workspace(&path, "msn-ws-1", &workspace).unwrap();

    let read = read_workspace(&path, "msn-ws-1").unwrap().unwrap();
    assert_eq!(read, workspace);

    cleanup(&path);
}

#[test]
fn create_mission_is_idempotent_for_same_id() {
    let path = temp_db_path("idempotent");
    let first = create_mission(&path, &request("msn-test-2")).unwrap();
    let second = create_mission(&path, &request("msn-test-2")).unwrap();

    assert!(first.created);
    assert!(!second.created);

    let connection = Connection::open(&path).unwrap();
    let role_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM team_roles WHERE mission_id = 'msn-test-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(role_count, TEAM_ROLES.len() as i64);

    cleanup(&path);
}

#[test]
fn utc_timestamp_is_iso_8601_utc() {
    let value = utc_timestamp();
    assert_eq!(value.len(), 20);
    assert!(value.ends_with('Z'));
    assert_eq!(&value[4..5], "-");
    assert_eq!(&value[7..8], "-");
    assert_eq!(&value[10..11], "T");
    assert_eq!(&value[13..14], ":");
    assert_eq!(&value[16..17], ":");
}

#[test]
fn make_mission_id_is_prefixed_and_unique() {
    let first = make_mission_id("Fix Team Mission dispatch");
    let second = make_mission_id("Fix Team Mission dispatch");

    assert!(first.starts_with("msn-"));
    assert_ne!(first, second);
}

#[test]
fn read_mission_status_reports_stage_roles_and_pending() {
    let path = temp_db_path("status");
    create_mission(&path, &request("msn-test-3")).unwrap();

    let status = read_mission_status(&path, "msn-test-3").unwrap();
    assert_eq!(status.mission_id, "msn-test-3");
    assert_eq!(status.stage, "preparing");
    assert_eq!(status.launch_mode, LaunchMode::Manual);
    assert_eq!(status.pending_assignments, 0);
    assert_eq!(status.generation, 1);
    assert_eq!(status.roles.len(), TEAM_ROLES.len());
    for role in TEAM_ROLES {
        assert_eq!(status.roles.get(role).map(String::as_str), Some("unknown"));
    }

    cleanup(&path);
}

#[test]
fn mission_launch_mode_round_trips_and_switches_both_ways() {
    let path = temp_db_path("launch-mode");
    let mut create = request("msn-launch-mode");
    create.launch_mode = LaunchMode::Auto;
    create_mission(&path, &create).unwrap();

    assert_eq!(
        read_mission_launch_mode(&path, "msn-launch-mode").unwrap(),
        LaunchMode::Auto
    );
    set_mission_launch_mode(&path, "msn-launch-mode", LaunchMode::Manual).unwrap();
    assert_eq!(
        read_mission_status(&path, "msn-launch-mode")
            .unwrap()
            .launch_mode,
        LaunchMode::Manual
    );
    set_mission_launch_mode(&path, "msn-launch-mode", LaunchMode::Auto).unwrap();
    assert_eq!(
        read_mission_launch_mode(&path, "msn-launch-mode").unwrap(),
        LaunchMode::Auto
    );

    cleanup(&path);
}

#[test]
fn read_mission_status_rejects_missing_mission() {
    let path = temp_db_path("status-missing");
    create_mission(&path, &request("msn-test-4")).unwrap();

    let error = read_mission_status(&path, "msn-does-not-exist").unwrap_err();
    assert_eq!(error.code, "mission_not_found");

    cleanup(&path);
}

#[test]
fn mission_status_and_overview_count_queued_and_active_assignments_as_pending() {
    let path = temp_db_path("status-pending-scope");
    create_mission(&path, &request("msn-test-pending-scope")).unwrap();

    let insert = |id: &str, state: &str| {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO assignments(
                    id, mission_id, source_role, target_role, kind, summary, state,
                    parent_id, review_round, skills_json, replace_skills, review_id,
                    created_at, updated_at
                 ) VALUES(?1, 'msn-test-pending-scope', 'pm', 'worker', 'task', 'x', ?2,
                    NULL, 0, '[]', 0, NULL, '2026-08-16T00:00:00Z', '2026-08-16T00:00:00Z')",
                rusqlite::params![id, state],
            )
            .unwrap();
    };

    insert("asg-active", "active");
    insert("asg-queued", "queued");

    let status = read_mission_status(&path, "msn-test-pending-scope").unwrap();
    assert_eq!(status.pending_assignments, 2);

    let overview = read_mission_overviews(&path)
        .unwrap()
        .into_iter()
        .find(|mission| mission.mission_id == "msn-test-pending-scope")
        .unwrap();
    assert_eq!(overview.pending_assignments, 2);

    cleanup(&path);
}

#[test]
fn resolve_roles_returns_preset_for_known_profile() {
    let roles = resolve_roles(Provider::Pi, MissionLayout::Team, &[]).unwrap();
    assert_eq!(roles.len(), TEAM_ROLES.len());
    for role in &roles {
        assert_eq!(role.provider, "pi");
        assert_eq!(role.thinking, "high");
    }
}

#[test]
fn mission_layout_parses_team_and_simple_aliases() {
    assert_eq!(MissionLayout::parse("team"), Some(MissionLayout::Team));
    assert_eq!(MissionLayout::parse("simple"), Some(MissionLayout::Simple));
    assert_eq!(MissionLayout::parse("classic"), Some(MissionLayout::Simple));
    assert_eq!(MissionLayout::parse("solo"), Some(MissionLayout::Simple));
    assert_eq!(MissionLayout::parse("nope"), None);
    assert_eq!(MissionLayout::Simple.as_str(), "simple");
    assert_eq!(MissionLayout::Team.as_str(), "team");
}

#[test]
fn provider_parses_kinds_and_legacy_profile_ids() {
    assert_eq!(Provider::parse("codex-default-v1"), Some(Provider::Codex));
    assert_eq!(Provider::parse("pi-quality-v1"), Some(Provider::Pi));
    assert_eq!(Provider::parse("cursor-agent"), Some(Provider::CursorAgent));
    assert_eq!(Provider::parse("cursor"), Some(Provider::CursorAgent));
    assert_eq!(Provider::parse("grok"), Some(Provider::Grok));
    assert_eq!(Provider::parse("claude"), Some(Provider::Claude));
    assert_eq!(Provider::parse("droid"), Some(Provider::Droid));
    assert_eq!(Provider::parse("nope"), None);
    assert_eq!(Provider::Codex.agent_kind(), "codex");
    assert_eq!(Provider::CursorAgent.agent_kind(), "cursor-agent");
    assert_eq!(Provider::Droid.agent_kind(), "droid");
}

#[test]
fn simple_layouts_produce_a_single_worker() {
    let codex = Provider::Codex.preset_roles(MissionLayout::Simple);
    assert_eq!(codex.len(), 1);
    assert_eq!(codex[0].role, "worker");
    assert_eq!(codex[0].provider, "codex");

    let pi = Provider::Pi.preset_roles(MissionLayout::Simple);
    assert_eq!(pi.len(), 1);
    assert_eq!(pi[0].role, "worker");
    assert_eq!(pi[0].provider, "pi");
}

#[test]
fn resolve_roles_simple_returns_a_single_worker() {
    let roles = resolve_roles(Provider::Codex, MissionLayout::Simple, &[]).unwrap();
    assert_eq!(roles.len(), 1);
    assert_eq!(roles[0].role, "worker");
    assert_eq!(roles[0].permission_policy, "codex-workspace-write-v1");
}

#[test]
fn resolve_roles_simple_applies_worker_override() {
    let overrides = vec![RoleOverride {
        role: "worker".into(),
        model: Some("claude-sonnet-5".into()),
        ..Default::default()
    }];
    let roles = resolve_roles(Provider::Codex, MissionLayout::Simple, &overrides).unwrap();
    assert_eq!(roles.len(), 1);
    assert_eq!(roles[0].model, "claude-sonnet-5");
}

#[test]
fn resolve_roles_applies_per_role_override() {
    let overrides = vec![RoleOverride {
        role: "worker".into(),
        provider: Some("claude".into()),
        model: Some("claude-sonnet-5".into()),
        thinking: Some("high".into()),
        permission_policy: None,
    }];
    let roles = resolve_roles(Provider::Codex, MissionLayout::Team, &overrides).unwrap();

    let worker = roles.iter().find(|role| role.role == "worker").unwrap();
    assert_eq!(worker.provider, "claude");
    assert_eq!(worker.model, "claude-sonnet-5");
    assert_eq!(worker.thinking, "high");
    // Preset permission is preserved when not overridden.
    assert_eq!(worker.permission_policy, "codex-workspace-write-v1");

    let pm = roles.iter().find(|role| role.role == "pm").unwrap();
    assert_eq!(pm.provider, "codex");
}

#[test]
fn resolve_roles_rejects_unknown_role() {
    let overrides = vec![RoleOverride {
        role: "architect".into(),
        ..Default::default()
    }];
    let error = resolve_roles(Provider::Codex, MissionLayout::Team, &overrides).unwrap_err();
    assert_eq!(error.code, "unknown_role");
}

#[test]
fn role_kind_parses_kind_and_instance() {
    assert_eq!(role_kind("worker"), "worker");
    assert_eq!(role_kind("scout-01"), "scout");
    assert_eq!(role_kind("pm"), "pm");
}

#[test]
fn parse_role_ref_maps_kernel_identity() {
    assert_eq!(parse_role_ref("worker").unwrap().role, RoleKind::Worker);
    assert_eq!(parse_role_ref("pm").unwrap().role, RoleKind::Pm);
    assert_eq!(parse_role_ref("reviewer").unwrap().role, RoleKind::Reviewer);
    let scout = parse_role_ref("scout-01").unwrap();
    assert_eq!(scout.role, RoleKind::Scout);
    assert_eq!(scout.instance.as_deref(), Some("scout-01"));
    let bare_scout = parse_role_ref("scout").unwrap();
    assert_eq!(bare_scout.role, RoleKind::Scout);
    assert_eq!(bare_scout.instance, None);
}

#[test]
fn parse_role_ref_rejects_worker_instance() {
    assert!(parse_role_ref("worker-frontend").is_err());
}

#[test]
fn resolve_roles_adds_new_instance_inheriting_kind_defaults() {
    let overrides = vec![RoleOverride {
        role: "scout-01".into(),
        model: Some("claude-sonnet-5".into()),
        ..Default::default()
    }];
    let roles = resolve_roles(Provider::Codex, MissionLayout::Team, &overrides).unwrap();

    assert_eq!(roles.len(), TEAM_ROLES.len() + 1);
    let instance = roles.iter().find(|role| role.role == "scout-01").unwrap();
    assert_eq!(instance.provider, "codex");
    assert_eq!(instance.model, "claude-sonnet-5");
    assert_eq!(instance.permission_policy, "codex-readonly-v1");

    // The original scout template is unchanged.
    let scout = roles.iter().find(|role| role.role == "scout").unwrap();
    assert_eq!(scout.provider, "codex");
}

#[test]
fn resolve_roles_rejects_instance_of_unknown_kind() {
    let overrides = vec![RoleOverride {
        role: "qa-lead".into(),
        ..Default::default()
    }];
    let error = resolve_roles(Provider::Codex, MissionLayout::Team, &overrides).unwrap_err();
    assert_eq!(error.code, "unknown_role");
}

#[test]
fn delete_mission_cascades_rows_and_reports_workspace() {
    let path = temp_db_path("delete");
    let mission_id = "msn-20260816-120000-delete-1a2b3c4d";
    create_mission(&path, &request(mission_id)).unwrap();

    let workspace = MissionWorkspace {
        source: WorkspaceSource::Current,
        workspace_id: "w71".into(),
        tab_id: "w71:t1".into(),
        root_pane_id: "w71:p1".into(),
        ..Default::default()
    };
    upsert_workspace(&path, mission_id, &workspace).unwrap();

    let outcome = delete_mission(&path, mission_id).unwrap();
    assert!(outcome.deleted);
    assert_eq!(outcome.workspace_id.as_deref(), Some("w71"));

    let connection = Connection::open(&path).unwrap();
    for table in [
        "team_missions",
        "team_roles",
        "mission_state",
        "mission_workspace",
    ] {
        let count: i64 = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE mission_id = ?1"),
                [mission_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "{table} should have no rows after delete");
    }

    cleanup(&path);
}

#[test]
fn delete_mission_removes_prompt_directory() {
    let path = temp_db_path("delete-prompts");
    let mission_id = "msn-20260816-120000-delete-prompts-1a2b3c4d";
    create_mission(&path, &request(mission_id)).unwrap();

    let dir = path
        .parent()
        .unwrap()
        .join("mission-prompts")
        .join(mission_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("scout.md"), "prompt").unwrap();

    let outcome = delete_mission(&path, mission_id).unwrap();
    assert!(outcome.prompt_dir_removed);
    assert!(!dir.exists());

    cleanup(&path);
}

#[test]
fn delete_mission_is_idempotent_for_missing_mission() {
    let path = temp_db_path("delete-missing");
    bootstrap_database(&path).unwrap();
    let outcome = delete_mission(&path, "msn-20260816-120000-missing-1a2b3c4d").unwrap();
    assert!(!outcome.deleted);
    assert_eq!(outcome.workspace_id, None);

    cleanup(&path);
}

#[test]
fn resolve_mission_id_accepts_id_then_unique_title() {
    let path = temp_db_path("resolve");
    let id = "msn-20260816-120000-resolve-1a2b3c4d";
    let mut request = request(id);
    request.brief = "唯一标题".into();
    create_mission(&path, &request).unwrap();

    assert_eq!(resolve_mission_id(&path, id).unwrap(), id);
    assert_eq!(resolve_mission_id(&path, "唯一标题").unwrap(), id);
    assert!(resolve_mission_id(&path, "不存在的标题").is_err());

    cleanup(&path);
}

#[test]
fn resolve_mission_id_rejects_ambiguous_title() {
    let path = temp_db_path("resolve-ambiguous");
    for (id, brief) in [
        ("msn-20260816-120000-amb-1a2b3c4d", "重复标题"),
        ("msn-20260816-120000-amb-2a2b3c4d", "重复标题"),
    ] {
        let mut request = request(id);
        request.brief = brief.into();
        create_mission(&path, &request).unwrap();
    }

    let error = resolve_mission_id(&path, "重复标题").unwrap_err();
    assert_eq!(error.code, "mission_ambiguous");

    cleanup(&path);
}
