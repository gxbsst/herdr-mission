use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use herdr_mission::{
    agent_name_token, create_mission, default_codex_team, launch_mission, read_mission_status,
    read_workspace, start_role, CreateMissionRequest, LaunchMode, LaunchOptions, MissionLayout,
    ProcessOutput, ProcessRunner, Provider, TabMode, WorkspaceSource,
};
use rusqlite::Connection;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn temp_db(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "herdr-mission-runtime-{label}-{}-{id}.sqlite3",
        std::process::id()
    ))
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

struct FakeRunner {
    calls: RefCell<Vec<(String, Vec<String>)>>,
    fail_agent_start: bool,
}

impl FakeRunner {
    fn success() -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            fail_agent_start: false,
        }
    }

    fn failing_agent_start() -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            fail_agent_start: true,
        }
    }
}

impl ProcessRunner for FakeRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<ProcessOutput> {
        self.calls
            .borrow_mut()
            .push((program.to_string(), args.to_vec()));
        if program == "git" {
            let subcommand = args.get(2).map(String::as_str).unwrap_or("");
            let stdout = match subcommand {
                "worktree" => "worktree /repo\nbranch main\n".to_string(),
                "rev-parse" if args.get(3).map(String::as_str) == Some("--show-toplevel") => {
                    "/repo\n".to_string()
                }
                _ => "HEAD\n".to_string(),
            };
            return Ok(ProcessOutput {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            });
        }
        match args.first().map(String::as_str) {
            Some("workspace") => Ok(ProcessOutput {
                exit_code: 0,
                stdout:
                    r#"{"result":{"workspace":{"workspace_id":"w6J:ws1"},"tab":{"tab_id":"w6J:t1"},"root_pane":{"pane_id":"w6J:p0"}}}"#
                        .into(),
                stderr: String::new(),
            }),
            Some("worktree") => Ok(ProcessOutput {
                exit_code: 0,
                stdout: r#"{"result":{"workspace":{"workspace_id":"w6J:ws1"},"worktree":{"branch":"feature/x-abc","path":"/repo/.worktree/x"},"tab":{"tab_id":"w6J:t1"},"root_pane":{"pane_id":"w6J:p0"}}}"#
                    .into(),
                stderr: String::new(),
            }),
            Some("tab") => Ok(ProcessOutput {
                exit_code: 0,
                stdout:
                    r#"{"result":{"tab":{"tab_id":"w6J:tX"},"root_pane":{"pane_id":"w6J:pX"}}}"#
                        .into(),
                stderr: String::new(),
            }),
            Some("pane") => Ok(ProcessOutput {
                exit_code: 0,
                stdout: r#"{"result":{"pane":{"pane_id":"w6J:pX"}}}"#.into(),
                stderr: String::new(),
            }),
            Some("agent") if args.get(1).map(String::as_str) == Some("start") => {
                if self.fail_agent_start {
                    Ok(ProcessOutput {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: "agent start failed".into(),
                    })
                } else {
                    Ok(ProcessOutput {
                        exit_code: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
            }
            _ => Ok(ProcessOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "unexpected command".into(),
            }),
        }
    }
}

fn mission_request(mission_id: &str) -> CreateMissionRequest {
    CreateMissionRequest {
        mission_id: mission_id.to_string(),
        brief: "runtime demo".into(),
        template: "general".into(),
        agent_profile_id: "codex-default-v1".into(),
        agent_profile_version: 1,
        roles: default_codex_team(),
    }
}

fn simple_mission_request(mission_id: &str) -> CreateMissionRequest {
    CreateMissionRequest {
        mission_id: mission_id.to_string(),
        brief: "simple demo".into(),
        template: "general".into(),
        agent_profile_id: "codex-default-v1".into(),
        agent_profile_version: 1,
        roles: Provider::Codex.preset_roles(MissionLayout::Simple),
    }
}

#[test]
fn launch_mission_starts_all_roles_and_marks_active() {
    let path = temp_db("active");
    let mission_id = "msn-20260815-120000-demo-1a2b3c4d";
    create_mission(&path, &mission_request(mission_id)).unwrap();

    let runner = FakeRunner::success();
    let outcome = launch_mission(
        &path,
        mission_id,
        &LaunchOptions {
            launch_mode: LaunchMode::Auto,
            ..LaunchOptions::default()
        },
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome.stage, "active");
    assert_eq!(outcome.roles.len(), 4);
    assert!(outcome
        .roles
        .iter()
        .all(|role| { role.agent_name.starts_with("mission-") && !role.pane_id.is_empty() }));

    // Lanes 模式下 PM 落在 workspace 的 root pane，而不是 split 出的空 pane。
    let workspace = read_workspace(&path, mission_id).unwrap().unwrap();
    let pm = outcome.roles.iter().find(|role| role.role == "pm").unwrap();
    assert_eq!(pm.pane_id, workspace.root_pane_id);

    let status = read_mission_status(&path, mission_id).unwrap();
    assert_eq!(status.stage, "active");
    for role in ["pm", "worker", "scout", "reviewer"] {
        assert_eq!(status.roles.get(role).map(String::as_str), Some("idle"));
    }

    let connection = Connection::open(&path).unwrap();
    let recorded: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM team_roles WHERE mission_id = ?1 AND pane_id != ''",
            [mission_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 4);

    cleanup(&path);
}

#[test]
fn launch_simple_mission_builds_stage_tabs() {
    let path = temp_db("stage-tabs");
    let mission_id = "msn-20260815-120000-demo-1a2b3c4d";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();

    let runner = FakeRunner::success();
    let outcome = launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome.stage, "active");
    assert_eq!(outcome.roles.len(), 1);

    let workspace = read_workspace(&path, mission_id).unwrap().unwrap();
    // 执行 tab 复用 workspace 自带的初始 tab，而不是再新建一个空 tab。
    assert_eq!(workspace.execution_tab_id, workspace.tab_id);
    assert_eq!(workspace.root_pane_id, "w6J:p0");
    assert!(!workspace.execution_tab_id.is_empty());
    assert!(!workspace.review_tab_id.is_empty());
    assert!(!workspace.verification_tab_id.is_empty());

    // 只有审查/验证两个 tab 是通过 `herdr tab create` 新建的。
    let tab_creates = runner
        .calls
        .borrow()
        .iter()
        .filter(|(_, args)| args.first().map(String::as_str) == Some("tab"))
        .count();
    assert_eq!(tab_creates, 2);

    cleanup(&path);
}

#[test]
fn tabs_mode_reuses_initial_tab_for_pm_and_creates_one_tab_per_other_role() {
    let path = temp_db("tabs-pm");
    let mission_id = "msn-20260815-120000-demo-1a2b3c4d";
    create_mission(&path, &mission_request(mission_id)).unwrap();

    let options = LaunchOptions {
        tab_mode: TabMode::Tabs,
        launch_mode: LaunchMode::Auto,
        ..LaunchOptions::default()
    };
    let runner = FakeRunner::success();
    let outcome =
        launch_mission(&path, mission_id, &options, &runner, "herdr", &mut |_| {}).unwrap();

    assert_eq!(outcome.stage, "active");
    assert_eq!(outcome.roles.len(), 4);

    let workspace = read_workspace(&path, mission_id).unwrap().unwrap();
    let pm = outcome.roles.iter().find(|role| role.role == "pm").unwrap();
    // PM 复用 workspace 初始 tab 的 root pane，而不是新建 tab。
    assert_eq!(pm.pane_id, workspace.root_pane_id);
    assert_eq!(workspace.root_pane_id, "w6J:p0");

    // 其余 worker/scout/reviewer 各新建一个 tab。
    let tab_creates = runner
        .calls
        .borrow()
        .iter()
        .filter(|(_, args)| args.first().map(String::as_str) == Some("tab"))
        .count();
    assert_eq!(tab_creates, 3);

    cleanup(&path);
}

#[test]
fn manual_mode_launches_only_pm_up_front() {
    let path = temp_db("manual");
    let mission_id = "msn-20260815-120000-demo-1a2b3c4d";
    create_mission(&path, &mission_request(mission_id)).unwrap();

    let runner = FakeRunner::success();
    let outcome = launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(), // Manual by default
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome.stage, "active");
    assert_eq!(outcome.roles.len(), 1);
    assert_eq!(outcome.roles[0].role, "pm");

    let connection = Connection::open(&path).unwrap();
    let launched: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM team_roles WHERE mission_id = ?1 AND pane_id != ''",
            [mission_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(launched, 1);

    cleanup(&path);
}

#[test]
fn start_role_launches_a_single_role_and_is_idempotent() {
    let path = temp_db("start-role");
    let mission_id = "msn-20260815-120000-demo-1a2b3c4d";
    create_mission(&path, &mission_request(mission_id)).unwrap();

    let runner = FakeRunner::success();
    let launch = launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(launch.roles.len(), 1);
    let pm_pane = launch.roles[0].pane_id.clone();

    let started = start_role(
        &path,
        mission_id,
        "scout",
        &pm_pane,
        "/repo",
        "manual",
        None,
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap()
    .unwrap();
    assert_eq!(started.role, "scout");
    assert_eq!(started.pane_id, "w6J:pX");

    let again = start_role(
        &path,
        mission_id,
        "scout",
        &pm_pane,
        "/repo",
        "manual",
        None,
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(again, None);

    cleanup(&path);
}

#[test]
fn launch_mission_parks_at_blocked_on_failed_agent_start() {
    let path = temp_db("blocked");
    let mission_id = "msn-20260815-120000-demo-1a2b3c4d";
    create_mission(&path, &mission_request(mission_id)).unwrap();

    let runner = FakeRunner::failing_agent_start();
    let error = launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap_err();

    assert_eq!(error.code, "launch_effect_failed");
    let status = read_mission_status(&path, mission_id).unwrap();
    assert_eq!(status.stage, "blocked");

    cleanup(&path);
}

#[test]
fn launch_mission_creates_worktree_workspace() {
    let path = temp_db("worktree");
    let mission_id = "msn-20260815-120000-demo-1a2b3c4d";
    create_mission(&path, &mission_request(mission_id)).unwrap();

    let runner = FakeRunner::success();
    let options = LaunchOptions {
        workspace_source: WorkspaceSource::Worktree,
        launch_mode: LaunchMode::Auto,
        ..LaunchOptions::default()
    };
    let outcome =
        launch_mission(&path, mission_id, &options, &runner, "herdr", &mut |_| {}).unwrap();

    assert_eq!(outcome.stage, "active");
    assert_eq!(outcome.roles.len(), 4);

    let workspace = read_workspace(&path, mission_id).unwrap().unwrap();
    assert_eq!(workspace.source, WorkspaceSource::Worktree);
    assert_eq!(workspace.worktree_path, "/repo/.worktree/x");
    assert_eq!(workspace.branch, "feature/x-abc");

    cleanup(&path);
}

#[test]
fn launch_mission_imports_worktree_workspace() {
    let path = temp_db("import");
    let mission_id = "msn-20260815-120000-demo-1a2b3c4d";
    create_mission(&path, &mission_request(mission_id)).unwrap();

    let runner = FakeRunner::success();
    let options = LaunchOptions {
        workspace_source: WorkspaceSource::Import,
        worktree_path: Some("/repo/.worktree/existing".into()),
        ..LaunchOptions::default()
    };
    let outcome =
        launch_mission(&path, mission_id, &options, &runner, "herdr", &mut |_| {}).unwrap();

    assert_eq!(outcome.stage, "active");
    let workspace = read_workspace(&path, mission_id).unwrap().unwrap();
    assert_eq!(workspace.source, WorkspaceSource::Import);
    assert_eq!(workspace.worktree_path, "/repo/.worktree/x");

    cleanup(&path);
}

#[test]
fn launch_mission_import_requires_a_target_path() {
    let path = temp_db("import-missing");
    let mission_id = "msn-20260815-120000-demo-1a2b3c4d";
    create_mission(&path, &mission_request(mission_id)).unwrap();

    let runner = FakeRunner::success();
    let options = LaunchOptions {
        workspace_source: WorkspaceSource::Import,
        worktree_path: None,
        ..LaunchOptions::default()
    };
    let error =
        launch_mission(&path, mission_id, &options, &runner, "herdr", &mut |_| {}).unwrap_err();

    assert_eq!(error.code, "worktree_path_missing");

    cleanup(&path);
}

#[test]
fn agent_name_token_is_compact_and_unique_per_mission() {
    let token = agent_name_token("msn-20260815-120000-demo-1a2b3c4d");
    assert_eq!(token.len(), 12);
    assert!(token.chars().all(|c| c.is_ascii_alphanumeric()));

    let other = agent_name_token("msn-20260815-120001-demo-1a2b3c4d");
    assert_ne!(token, other);
}
