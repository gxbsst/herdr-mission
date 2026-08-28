use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use herdr_mission::{
    agent_name_token, create_mission, default_codex_team, launch_mission, read_mission_status,
    read_workspace, start_role, upsert_workspace, CreateMissionRequest, LaunchMode, LaunchOptions,
    MissionLayout, MissionWorkspace, ProcessOutput, ProcessRunner, Provider, TabMode,
    WorkspaceSource,
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
    agent_start_error: Option<&'static str>,
    pane_state: Option<FakePaneState>,
    fail_agent_rename: bool,
    fail_pane_run: bool,
    next_tab_number: Cell<u32>,
    tab_labels: RefCell<BTreeMap<String, String>>,
    pane_tabs: RefCell<BTreeMap<String, String>>,
    missing_session_polls: Cell<u32>,
    remaining_agent_start_failures: Cell<Option<u32>>,
    agent_start_failure_pane: Option<&'static str>,
    pane_not_found_pane: Option<&'static str>,
    remaining_pane_not_found: Cell<u32>,
    missing_tab: Option<&'static str>,
    missing_workspace: Option<&'static str>,
}

#[derive(Clone, Copy)]
struct FakePaneState {
    agent: &'static str,
    cwd: &'static str,
    tab_label: &'static str,
    has_session: bool,
}

impl FakeRunner {
    fn new(
        agent_start_error: Option<&'static str>,
        pane_state: Option<FakePaneState>,
        fail_pane_run: bool,
    ) -> Self {
        let work_label = pane_state.map(|state| state.tab_label).unwrap_or("工作区");
        Self {
            calls: RefCell::new(Vec::new()),
            agent_start_error,
            pane_state,
            fail_agent_rename: false,
            fail_pane_run,
            next_tab_number: Cell::new(2),
            tab_labels: RefCell::new(BTreeMap::from([(
                "w6J:t1".to_string(),
                work_label.to_string(),
            )])),
            pane_tabs: RefCell::new(BTreeMap::new()),
            missing_session_polls: Cell::new(0),
            remaining_agent_start_failures: Cell::new(None),
            agent_start_failure_pane: None,
            pane_not_found_pane: None,
            remaining_pane_not_found: Cell::new(0),
            missing_tab: None,
            missing_workspace: None,
        }
    }

    fn success() -> Self {
        Self::new(None, None, false)
    }

    fn failing_agent_start() -> Self {
        Self::new(Some("agent start failed"), None, false)
    }

    fn recoverable_agent_start(error: &'static str, pane_state: FakePaneState) -> Self {
        Self::new(Some(error), Some(pane_state), false)
    }

    fn with_agent_rename_failure(mut self) -> Self {
        self.fail_agent_rename = true;
        self
    }

    fn with_missing_session_polls(self, polls: u32) -> Self {
        self.missing_session_polls.set(polls);
        self
    }

    fn with_agent_start_failures(self, failures: u32) -> Self {
        self.remaining_agent_start_failures.set(Some(failures));
        self
    }

    fn with_agent_start_failure_pane(mut self, pane_id: &'static str) -> Self {
        self.agent_start_failure_pane = Some(pane_id);
        self
    }

    fn with_transient_pane_not_found(mut self, pane_id: &'static str, polls: u32) -> Self {
        self.pane_not_found_pane = Some(pane_id);
        self.remaining_pane_not_found.set(polls);
        self
    }

    fn with_missing_tab(mut self, tab_id: &'static str) -> Self {
        self.missing_tab = Some(tab_id);
        self
    }

    fn with_missing_workspace(mut self, workspace_id: &'static str) -> Self {
        self.missing_workspace = Some(workspace_id);
        self
    }

    fn failing_pane_run() -> Self {
        Self::new(None, None, true)
    }

    fn set_tab_label(&self, tab_id: &str, label: &str) {
        self.tab_labels
            .borrow_mut()
            .insert(tab_id.to_string(), label.to_string());
    }

    fn set_pane_tab(&self, pane_id: &str, tab_id: &str) {
        self.pane_tabs
            .borrow_mut()
            .insert(pane_id.to_string(), tab_id.to_string());
    }

    fn pane_state(&self) -> FakePaneState {
        self.pane_state.unwrap_or(FakePaneState {
            agent: "codex",
            cwd: ".",
            tab_label: "工作区",
            has_session: true,
        })
    }

    fn count_calls(&self, group: &str, command: &str) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|(_, args)| {
                args.first().map(String::as_str) == Some(group)
                    && args.get(1).map(String::as_str) == Some(command)
            })
            .count()
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
            Some("tab") if args.get(1).map(String::as_str) == Some("list") => {
                let workspace_id = args
                    .windows(2)
                    .find(|pair| pair[0] == "--workspace")
                    .map(|pair| pair[1].as_str())
                    .unwrap_or("w6J:ws1");
                if self.missing_workspace == Some(workspace_id) {
                    return Ok(ProcessOutput {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: format!(
                            r#"{{"error":{{"code":"workspace_not_found","message":"workspace {workspace_id} not found"}},"id":"cli:tab:list"}}"#
                        ),
                    });
                }
                let tabs = self
                    .tab_labels
                    .borrow()
                    .iter()
                    .map(|(tab_id, label)| {
                        serde_json::json!({
                            "workspace_id": workspace_id,
                            "tab_id": tab_id,
                            "label": label,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: serde_json::json!({"result": {"tabs": tabs}}).to_string(),
                    stderr: String::new(),
                })
            }
            Some("tab") if args.get(1).map(String::as_str) == Some("create") => {
                let number = self.next_tab_number.get();
                self.next_tab_number.set(number + 1);
                let tab_id = format!("w6J:t{number}");
                let label = args
                    .windows(2)
                    .find(|pair| pair[0] == "--label")
                    .map(|pair| pair[1].clone())
                    .unwrap_or_default();
                self.set_tab_label(&tab_id, &label);
                Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: format!(
                        r#"{{"result":{{"tab":{{"tab_id":"{tab_id}"}},"root_pane":{{"pane_id":"w6J:pX"}}}}}}"#
                    ),
                    stderr: String::new(),
                })
            }
            Some("tab") if args.get(1).map(String::as_str) == Some("get") => {
                let tab_id = args.get(2).map(String::as_str).unwrap_or("w6J:t1");
                if self.missing_tab == Some(tab_id) {
                    return Ok(ProcessOutput {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: format!(
                            r#"{{"error":{{"code":"tab_not_found","message":"tab {tab_id} not found"}},"id":"cli:tab:get"}}"#
                        ),
                    });
                }
                let label = self
                    .tab_labels
                    .borrow()
                    .get(tab_id)
                    .cloned()
                    .unwrap_or_default();
                Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: format!(
                        r#"{{"result":{{"tab":{{"workspace_id":"w6J:ws1","tab_id":"{tab_id}","label":"{label}"}},"type":"tab_info"}}}}"#
                    ),
                    stderr: String::new(),
                })
            }
            Some("tab") if args.get(1).map(String::as_str) == Some("rename") => {
                if let (Some(tab_id), Some(label)) = (args.get(2), args.get(3)) {
                    self.set_tab_label(tab_id, label);
                }
                Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            Some("pane") if args.get(1).map(String::as_str) == Some("move") => {
                let pane_id = args.get(2).cloned().unwrap_or_default();
                let tab_id = args
                    .windows(2)
                    .find(|pair| pair[0] == "--tab")
                    .map(|pair| pair[1].clone())
                    .unwrap_or_default();
                self.set_pane_tab(&pane_id, &tab_id);
                Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            Some("pane") if args.get(1).map(String::as_str) == Some("split") => {
                Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: r#"{"result":{"pane":{"pane_id":"w6J:pX"}}}"#.into(),
                    stderr: String::new(),
                })
            }
            Some("pane") if args.get(1).map(String::as_str) == Some("list") => {
                let mut panes = vec![serde_json::json!({
                    "workspace_id": "w6J:ws1",
                    "tab_id": "w6J:t1",
                    "pane_id": "w6J:p0",
                })];
                panes.extend(self.pane_tabs.borrow().iter().map(|(pane_id, tab_id)| {
                    serde_json::json!({
                        "workspace_id": "w6J:ws1",
                        "tab_id": tab_id,
                        "pane_id": pane_id,
                    })
                }));
                Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: serde_json::json!({"result": {"panes": panes}}).to_string(),
                    stderr: String::new(),
                })
            }
            Some("pane") if args.get(1).map(String::as_str) == Some("get") => {
                let state = self.pane_state();
                let pane_id = args.get(2).map(String::as_str).unwrap_or("w6J:p0");
                let pane_not_found = self.remaining_pane_not_found.get();
                if self.pane_not_found_pane == Some(pane_id) && pane_not_found > 0 {
                    self.remaining_pane_not_found.set(pane_not_found - 1);
                    return Ok(ProcessOutput {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: format!(
                            r#"{{"error":{{"code":"pane_not_found","message":"pane {pane_id} not found"}}}}"#
                        ),
                    });
                }
                let missing_session_polls = self.missing_session_polls.get();
                let has_session = state.has_session && missing_session_polls == 0;
                self.missing_session_polls
                    .set(missing_session_polls.saturating_sub(1));
                let session = if has_session {
                    r#"{"agent":"codex","kind":"id","source":"herdr:codex","value":"session-1"}"#
                } else {
                    "null"
                };
                let tab_id = self
                    .pane_tabs
                    .borrow()
                    .get(pane_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        if state.tab_label == "工作区" {
                            "w6J:t1".to_string()
                        } else {
                            "w6J:t2".to_string()
                        }
                    });
                Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: format!(
                        r#"{{"result":{{"pane":{{"agent":"{}","agent_session":{},"cwd":"{}","foreground_cwd":"{}","pane_id":"{}","tab_id":"{}","workspace_id":"w6J:ws1"}},"type":"pane_info"}}}}"#,
                        state.agent, session, state.cwd, state.cwd, pane_id, tab_id
                    ),
                    stderr: String::new(),
                })
            }
            Some("pane") if args.get(1).map(String::as_str) == Some("run") && self.fail_pane_run => {
                Err(std::io::Error::other("pane run failed"))
            }
            Some("pane") => Ok(ProcessOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
            Some("agent") if args.get(1).map(String::as_str) == Some("start") => {
                if let Some(error) = self.agent_start_error {
                    let pane_id = args
                        .windows(2)
                        .find(|pair| pair[0] == "--pane")
                        .map(|pair| pair[1].as_str());
                    let remaining = self.remaining_agent_start_failures.get();
                    let pane_matches = self.agent_start_failure_pane.is_none()
                        || self.agent_start_failure_pane == pane_id;
                    if pane_matches && remaining != Some(0) {
                        self.remaining_agent_start_failures
                            .set(remaining.map(|count| count.saturating_sub(1)));
                        return Ok(ProcessOutput {
                            exit_code: 1,
                            stdout: String::new(),
                            stderr: error.into(),
                        });
                    }
                }
                Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            Some("agent") if args.get(1).map(String::as_str) == Some("rename") => {
                Ok(ProcessOutput {
                    exit_code: i32::from(self.fail_agent_rename),
                    stdout: String::new(),
                    stderr: if self.fail_agent_rename {
                        "agent rename failed".into()
                    } else {
                        String::new()
                    },
                })
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
        launch_mode: LaunchMode::Auto,
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
        launch_mode: LaunchMode::Manual,
        roles: Provider::Codex.preset_roles(MissionLayout::Simple),
    }
}

fn manual_mission_request(mission_id: &str) -> CreateMissionRequest {
    let mut request = mission_request(mission_id);
    request.launch_mode = LaunchMode::Manual;
    request
}

#[test]
fn launch_mission_starts_all_roles_and_marks_active() {
    let path = temp_db("active");
    let mission_id = "msn-20260827-150000-auto-prompt-1a2b3c4d";
    create_mission(&path, &mission_request(mission_id)).unwrap();

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
    assert_eq!(outcome.roles.len(), 4);
    assert!(outcome
        .roles
        .iter()
        .all(|role| { role.agent_name.starts_with("mission-") && !role.pane_id.is_empty() }));

    // Lanes 模式下 PM 落在 workspace 的 root pane，而不是 split 出的空 pane。
    let workspace = read_workspace(&path, mission_id).unwrap().unwrap();
    let pm = outcome.roles.iter().find(|role| role.role == "pm").unwrap();
    assert_eq!(pm.pane_id, workspace.root_pane_id);
    assert_eq!(workspace.execution_tab_id, workspace.tab_id);
    assert!(!workspace.review_tab_id.is_empty());
    assert!(!workspace.verification_tab_id.is_empty());
    assert!(runner
        .calls
        .borrow()
        .iter()
        .any(|(_, args)| args == &["tab", "rename", "w6J:t1", "工作区"]));
    assert!(runner.calls.borrow().iter().any(|(_, args)| {
        args.first().map(String::as_str) == Some("tab")
            && args.get(1).map(String::as_str) == Some("create")
            && args.windows(2).any(|pair| pair == ["--label", "审查"])
    }));
    assert!(runner.calls.borrow().iter().any(|(_, args)| {
        args.first().map(String::as_str) == Some("tab")
            && args.get(1).map(String::as_str) == Some("create")
            && args.windows(2).any(|pair| pair == ["--label", "验证"])
    }));

    let status = read_mission_status(&path, mission_id).unwrap();
    assert_eq!(status.stage, "active");
    for role in ["pm", "worker", "scout", "reviewer"] {
        assert_eq!(status.roles.get(role).map(String::as_str), Some("idle"));
    }
    assert_eq!(status.launch_mode, LaunchMode::Auto);

    let pm_prompt = std::fs::read_to_string(
        path.parent()
            .unwrap()
            .join("mission-prompts")
            .join(mission_id)
            .join("pm.md"),
    )
    .unwrap();
    assert!(pm_prompt.contains("自治模式: auto"));

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
    // 工作区 tab 复用 workspace 自带的初始 tab，而不是再新建一个空 tab。
    assert_eq!(workspace.execution_tab_id, workspace.tab_id);
    assert_eq!(workspace.root_pane_id, "w6J:p0");
    assert!(!workspace.execution_tab_id.is_empty());
    assert!(!workspace.review_tab_id.is_empty());
    assert!(!workspace.verification_tab_id.is_empty());

    // 只有审查/验证两个 tab 是通过 `herdr tab create` 新建的。
    let tab_creates = runner.count_calls("tab", "create");
    assert_eq!(tab_creates, 2);
    assert!(runner
        .calls
        .borrow()
        .iter()
        .any(|(_, args)| { args == &["tab", "rename", "w6J:t1", "工作区"] }));

    cleanup(&path);
}

#[test]
fn tabs_mode_is_a_legacy_alias_for_the_three_mission_regions() {
    let path = temp_db("tabs-pm");
    let mission_id = "msn-20260815-120000-demo-1a2b3c4d";
    create_mission(&path, &mission_request(mission_id)).unwrap();

    let options = LaunchOptions {
        tab_mode: TabMode::Tabs,
        ..LaunchOptions::default()
    };
    let runner = FakeRunner::success();
    let outcome =
        launch_mission(&path, mission_id, &options, &runner, "herdr", &mut |_| {}).unwrap();

    assert_eq!(outcome.stage, "active");
    assert_eq!(outcome.roles.len(), 4);

    let workspace = read_workspace(&path, mission_id).unwrap().unwrap();
    let pm = outcome.roles.iter().find(|role| role.role == "pm").unwrap();
    // PM 和其他 Agent 都留在工作区；审查和验证是另外两个固定区域。
    assert_eq!(pm.pane_id, workspace.root_pane_id);
    assert_eq!(workspace.root_pane_id, "w6J:p0");
    assert_eq!(workspace.execution_tab_id, workspace.tab_id);

    assert_eq!(runner.count_calls("tab", "create"), 2);
    assert_eq!(runner.count_calls("pane", "split"), 3);

    cleanup(&path);
}

#[test]
fn manual_mode_launches_only_pm_up_front() {
    let path = temp_db("manual");
    let mission_id = "msn-20260815-120000-demo-1a2b3c4d";
    create_mission(&path, &manual_mission_request(mission_id)).unwrap();

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
    create_mission(&path, &manual_mission_request(mission_id)).unwrap();

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
fn launch_adopts_expected_agent_after_start_timeout_and_is_idempotent() {
    let path = temp_db("timeout-adopt");
    let mission_id = "msn-20260827-082057-rust-version-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    let cwd = "/repo/.worktree/rust-version";
    let runner = FakeRunner::recoverable_agent_start(
        "timed out waiting for agent startup",
        FakePaneState {
            agent: "codex",
            cwd,
            tab_label: "工作区",
            has_session: true,
        },
    );
    let options = LaunchOptions {
        cwd: cwd.into(),
        ..LaunchOptions::default()
    };

    let outcome =
        launch_mission(&path, mission_id, &options, &runner, "herdr", &mut |_| {}).unwrap();
    assert_eq!(outcome.stage, "active");
    assert_eq!(outcome.roles.len(), 1);
    let expected_name = format!("mission-{}-worker", agent_name_token(mission_id));
    assert!(runner
        .calls
        .borrow()
        .iter()
        .any(|(_, args)| { args == &["agent", "rename", "w6J:p0", expected_name.as_str()] }));
    assert_eq!(runner.count_calls("agent", "start"), 1);
    assert_eq!(runner.count_calls("tab", "create"), 2);
    assert_eq!(runner.count_calls("tab", "rename"), 1);

    let resumed =
        launch_mission(&path, mission_id, &options, &runner, "herdr", &mut |_| {}).unwrap();
    assert_eq!(resumed.stage, "active");
    assert!(resumed.roles.is_empty());
    assert_eq!(runner.count_calls("agent", "start"), 1);
    assert_eq!(runner.count_calls("tab", "create"), 2);
    assert_eq!(runner.count_calls("tab", "rename"), 1);

    cleanup(&path);
}

#[test]
fn launch_polls_when_provider_appears_before_the_agent_session() {
    let path = temp_db("session-late");
    let mission_id = "msn-20260827-082057-session-late-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    let runner = FakeRunner::recoverable_agent_start(
        "timed out waiting for agent startup",
        FakePaneState {
            agent: "codex",
            cwd: ".",
            tab_label: "工作区",
            has_session: true,
        },
    )
    .with_missing_session_polls(1);

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
    assert_eq!(runner.count_calls("agent", "start"), 1);
    assert_eq!(runner.count_calls("pane", "get"), 2);
    cleanup(&path);
}

#[test]
fn launch_adopts_expected_agent_when_pane_is_busy() {
    let path = temp_db("busy-adopt");
    let mission_id = "msn-20260827-082057-rust-version-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    let cwd = "/repo/.worktree/rust-version";
    let runner = FakeRunner::recoverable_agent_start(
        "agent_pane_busy",
        FakePaneState {
            agent: "codex",
            cwd,
            tab_label: "工作区",
            has_session: true,
        },
    );
    let outcome = launch_mission(
        &path,
        mission_id,
        &LaunchOptions {
            cwd: cwd.into(),
            ..LaunchOptions::default()
        },
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome.stage, "active");
    assert_eq!(runner.count_calls("agent", "start"), 1);
    assert_eq!(runner.count_calls("pane", "get"), 1);
    cleanup(&path);
}

#[test]
fn launch_polls_busy_agent_until_its_session_appears_without_restarting() {
    let path = temp_db("busy-session-late");
    let mission_id = "msn-20260827-082057-busy-session-late-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    let runner = FakeRunner::recoverable_agent_start(
        "agent_pane_busy",
        FakePaneState {
            agent: "codex",
            cwd: ".",
            tab_label: "工作区",
            has_session: true,
        },
    )
    .with_missing_session_polls(1);

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
    assert_eq!(runner.count_calls("agent", "start"), 1);
    assert_eq!(runner.count_calls("pane", "get"), 2);
    assert_eq!(runner.count_calls("agent", "rename"), 1);
    cleanup(&path);
}

#[test]
fn auto_launch_retries_agent_start_when_a_fresh_pane_is_transiently_busy() {
    let path = temp_db("fresh-busy-retry");
    let mission_id = "msn-20260827-082057-fresh-busy-retry-0b801f50";
    create_mission(&path, &mission_request(mission_id)).unwrap();
    let runner = FakeRunner::recoverable_agent_start(
        "agent_pane_busy",
        FakePaneState {
            agent: "",
            cwd: ".",
            tab_label: "工作区",
            has_session: false,
        },
    )
    .with_agent_start_failures(1);

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
    assert_eq!(outcome.roles.len(), 4);
    assert_eq!(runner.count_calls("agent", "start"), 5);
    cleanup(&path);
}

#[test]
fn auto_launch_waits_for_a_fresh_split_pane_to_become_visible() {
    let path = temp_db("fresh-pane-not-found");
    let mission_id = "msn-20260827-150000-fresh-pane-not-found-0b801f50";
    create_mission(&path, &mission_request(mission_id)).unwrap();
    let runner = FakeRunner::recoverable_agent_start(
        "agent_pane_busy",
        FakePaneState {
            agent: "",
            cwd: ".",
            tab_label: "工作区",
            has_session: false,
        },
    )
    .with_agent_start_failures(1)
    .with_agent_start_failure_pane("w6J:pX")
    .with_transient_pane_not_found("w6J:pX", 1);

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
    assert_eq!(outcome.roles.len(), 4);
    assert_eq!(runner.count_calls("agent", "start"), 5);
    cleanup(&path);
}

#[test]
fn auto_launch_stays_blocked_when_a_fresh_pane_never_becomes_visible() {
    let path = temp_db("fresh-pane-missing");
    let mission_id = "msn-20260827-150000-fresh-pane-missing-0b801f50";
    create_mission(&path, &mission_request(mission_id)).unwrap();
    let runner = FakeRunner::recoverable_agent_start(
        "agent_pane_busy",
        FakePaneState {
            agent: "",
            cwd: ".",
            tab_label: "工作区",
            has_session: false,
        },
    )
    .with_agent_start_failures(1)
    .with_agent_start_failure_pane("w6J:pX")
    .with_transient_pane_not_found("w6J:pX", 21);

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
    assert_eq!(
        error.details.get("operation"),
        Some(&serde_json::json!("pane get"))
    );
    assert_eq!(runner.count_calls("pane", "get"), 21);
    assert_eq!(runner.count_calls("pane", "split"), 1);
    assert_eq!(runner.count_calls("agent", "start"), 2);
    assert_eq!(
        read_mission_status(&path, mission_id).unwrap().stage,
        "blocked"
    );
    cleanup(&path);
}

#[test]
fn resume_waits_for_a_persisted_pane_but_starts_it_only_once() {
    let path = temp_db("persisted-pane-not-found");
    let mission_id = "msn-20260827-150000-persisted-pane-not-found-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &FakeRunner::success(),
        "herdr",
        &mut |_| {},
    )
    .unwrap();
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE team_roles SET terminal_id = '' WHERE mission_id = ?1 AND role = 'worker'",
            [mission_id],
        )
        .unwrap();

    let runner = FakeRunner::new(
        None,
        Some(FakePaneState {
            agent: "",
            cwd: ".",
            tab_label: "工作区",
            has_session: false,
        }),
        false,
    )
    .with_transient_pane_not_found("w6J:p0", 1);
    runner.set_tab_label("w6J:t2", "审查");
    runner.set_tab_label("w6J:t3", "验证");
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
    assert_eq!(runner.count_calls("pane", "split"), 0);
    assert_eq!(runner.count_calls("agent", "start"), 1);
    cleanup(&path);
}

#[test]
fn launch_does_not_repeat_agent_start_when_persisted_busy_pane_has_no_agent_identity() {
    let path = temp_db("busy-unidentified");
    let mission_id = "msn-20260827-082057-busy-unidentified-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &FakeRunner::success(),
        "herdr",
        &mut |_| {},
    )
    .unwrap();
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE team_roles SET terminal_id = '' WHERE mission_id = ?1 AND role = 'worker'",
            [mission_id],
        )
        .unwrap();
    let runner = FakeRunner::recoverable_agent_start(
        "agent_pane_busy",
        FakePaneState {
            agent: "",
            cwd: ".",
            tab_label: "工作区",
            has_session: false,
        },
    );
    runner.set_tab_label("w6J:t2", "审查");
    runner.set_tab_label("w6J:t3", "验证");

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
    assert_eq!(runner.count_calls("agent", "start"), 1);
    cleanup(&path);
}

#[test]
fn fresh_busy_pane_is_not_retried_when_its_cwd_does_not_match() {
    let path = temp_db("fresh-busy-wrong-cwd");
    let mission_id = "msn-20260827-082057-fresh-busy-wrong-cwd-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    let runner = FakeRunner::recoverable_agent_start(
        "agent_pane_busy",
        FakePaneState {
            agent: "",
            cwd: "/repo/other",
            tab_label: "工作区",
            has_session: false,
        },
    )
    .with_agent_start_failures(1);

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
    assert_eq!(error.message, "running Agent could not be safely adopted");
    assert_eq!(runner.count_calls("agent", "start"), 1);
    cleanup(&path);
}

#[test]
fn launch_adopts_a_staged_root_pane_before_retrying_agent_start() {
    let path = temp_db("staged-root-adopt");
    let mission_id = "msn-20260827-082057-staged-root-adopt-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &FakeRunner::failing_agent_start(),
        "herdr",
        &mut |_| {},
    )
    .unwrap_err();
    let recovery = FakeRunner::recoverable_agent_start(
        "agent_name_taken",
        FakePaneState {
            agent: "codex",
            cwd: ".",
            tab_label: "工作区",
            has_session: true,
        },
    )
    .with_missing_session_polls(2);
    let workspace = read_workspace(&path, mission_id).unwrap().unwrap();
    recovery.set_tab_label(&workspace.review_tab_id, "审查");
    recovery.set_tab_label(&workspace.verification_tab_id, "验证");

    let outcome = launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &recovery,
        "herdr",
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome.stage, "active");
    assert_eq!(outcome.roles[0].pane_id, "w6J:p0");
    assert_eq!(recovery.count_calls("agent", "start"), 0);
    assert_eq!(recovery.count_calls("pane", "get"), 3);
    assert_eq!(recovery.count_calls("agent", "rename"), 1);
    cleanup(&path);
}

#[test]
fn launch_rejects_unrelated_or_misplaced_agent_after_timeout() {
    let cases = [
        (
            "wrong-provider",
            "claude",
            "/repo/.worktree/rust-version",
            "工作区",
            true,
        ),
        (
            "wrong-cwd",
            "codex",
            "/repo/.worktree/other",
            "工作区",
            true,
        ),
        (
            "wrong-tab",
            "codex",
            "/repo/.worktree/rust-version",
            "审查",
            true,
        ),
        (
            "missing-session",
            "codex",
            "/repo/.worktree/rust-version",
            "工作区",
            false,
        ),
    ];

    for (label, agent, pane_cwd, tab_label, has_session) in cases {
        let path = temp_db(label);
        let mission_id = format!("msn-20260827-082057-{label}-0b801f50");
        create_mission(&path, &simple_mission_request(&mission_id)).unwrap();
        let runner = FakeRunner::recoverable_agent_start(
            "timed out waiting for agent startup",
            FakePaneState {
                agent,
                cwd: pane_cwd,
                tab_label,
                has_session,
            },
        );
        let error = launch_mission(
            &path,
            &mission_id,
            &LaunchOptions {
                cwd: "/repo/.worktree/rust-version".into(),
                ..LaunchOptions::default()
            },
            &runner,
            "herdr",
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error.code, "launch_effect_failed", "case {label}");
        assert_eq!(
            read_mission_status(&path, &mission_id).unwrap().stage,
            "blocked"
        );
        assert_eq!(runner.count_calls("agent", "rename"), 0);
        cleanup(&path);
    }
}

#[test]
fn launch_stays_blocked_when_adopted_agent_cannot_be_renamed() {
    let path = temp_db("rename-failed");
    let mission_id = "msn-20260827-082057-rust-version-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    let cwd = "/repo/.worktree/rust-version";
    let runner = FakeRunner::recoverable_agent_start(
        "timed out waiting for agent startup",
        FakePaneState {
            agent: "codex",
            cwd,
            tab_label: "工作区",
            has_session: true,
        },
    )
    .with_agent_rename_failure();

    let error = launch_mission(
        &path,
        mission_id,
        &LaunchOptions {
            cwd: cwd.into(),
            ..LaunchOptions::default()
        },
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap_err();

    assert_eq!(error.code, "launch_effect_failed");
    assert_eq!(
        read_mission_status(&path, mission_id).unwrap().stage,
        "blocked"
    );
    let (pane_id, agent_name): (String, String) = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT pane_id, terminal_id FROM team_roles WHERE mission_id = ?1 AND role = 'worker'",
            [mission_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(pane_id, "w6J:p0");
    assert!(agent_name.is_empty());
    cleanup(&path);
}

#[test]
fn region_tool_failure_keeps_all_three_fixed_regions_available() {
    let path = temp_db("region-partial");
    let mission_id = "msn-20260827-082057-region-partial-0b801f50";
    create_mission(&path, &mission_request(mission_id)).unwrap();

    let failing = FakeRunner::failing_pane_run();
    let mut progress = Vec::new();
    let outcome = launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &failing,
        "herdr",
        &mut |message| progress.push(message.to_string()),
    )
    .unwrap();
    assert_eq!(outcome.stage, "active");
    assert_eq!(failing.count_calls("tab", "create"), 2);
    let workspace = read_workspace(&path, mission_id).unwrap().unwrap();
    assert_eq!(workspace.execution_tab_id, workspace.tab_id);
    assert!(!workspace.review_tab_id.is_empty());
    assert!(!workspace.verification_tab_id.is_empty());
    assert!(progress
        .iter()
        .any(|message| message.contains("审查工具启动失败")));
    assert!(progress
        .iter()
        .any(|message| message.contains("验证工具启动失败")));
    cleanup(&path);
}

#[test]
fn persisted_region_tabs_converge_back_to_the_three_fixed_names() {
    let path = temp_db("region-rename");
    let mission_id = "msn-20260827-082057-region-rename-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    let runner = FakeRunner::success();
    launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();
    let workspace = read_workspace(&path, mission_id).unwrap().unwrap();
    runner.set_tab_label(&workspace.execution_tab_id, "Mission 工作区");
    runner.set_tab_label(&workspace.review_tab_id, "代码检查");
    runner.set_tab_label(&workspace.verification_tab_id, "测试区");

    launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();

    let labels = runner.tab_labels.borrow();
    assert_eq!(
        labels.get(&workspace.execution_tab_id).map(String::as_str),
        Some("工作区")
    );
    assert_eq!(
        labels.get(&workspace.review_tab_id).map(String::as_str),
        Some("审查")
    );
    assert_eq!(
        labels
            .get(&workspace.verification_tab_id)
            .map(String::as_str),
        Some("验证")
    );
    cleanup(&path);
}

#[test]
fn persisted_workspace_missing_from_current_session_fails_before_launch_effects() {
    let path = temp_db("workspace-current-session-missing");
    let mission_id = "msn-20260828-093000-session-missing-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    upsert_workspace(
        &path,
        mission_id,
        &MissionWorkspace {
            source: WorkspaceSource::Worktree,
            workspace_id: "w78".into(),
            tab_id: "w78:t1".into(),
            root_pane_id: "w78:p1".into(),
            execution_tab_id: "w78:t1".into(),
            review_tab_id: "w78:t2".into(),
            verification_tab_id: "w78:t3".into(),
            worktree_path: "/repo/.worktree/x".into(),
            branch: "feature/x".into(),
        },
    )
    .unwrap();
    let runner = FakeRunner::success().with_missing_workspace("w78");

    let error = launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap_err();

    assert_eq!(error.code, "mission_workspace_unavailable");
    assert!(!error.retryable);
    assert_eq!(
        error.details.get("operation"),
        Some(&serde_json::json!("tab list"))
    );
    assert_eq!(
        error.details.get("workspace_id"),
        Some(&serde_json::json!("w78"))
    );
    assert_eq!(runner.count_calls("tab", "get"), 0);
    assert_eq!(runner.count_calls("tab", "create"), 0);
    assert_eq!(runner.count_calls("pane", "split"), 0);
    assert_eq!(runner.count_calls("agent", "start"), 0);
    assert_eq!(
        read_mission_status(&path, mission_id).unwrap().stage,
        "blocked"
    );
    cleanup(&path);
}

#[test]
fn persisted_region_missing_from_existing_workspace_has_a_specific_error() {
    let path = temp_db("region-current-session-missing");
    let mission_id = "msn-20260828-093100-region-missing-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    upsert_workspace(
        &path,
        mission_id,
        &MissionWorkspace {
            source: WorkspaceSource::Worktree,
            workspace_id: "w78".into(),
            tab_id: "w78:t1".into(),
            root_pane_id: "w78:p1".into(),
            execution_tab_id: "w78:t1".into(),
            review_tab_id: "w78:t2".into(),
            verification_tab_id: "w78:t3".into(),
            worktree_path: "/repo/.worktree/x".into(),
            branch: "feature/x".into(),
        },
    )
    .unwrap();
    let runner = FakeRunner::success().with_missing_tab("w78:t1");

    let error = launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap_err();

    assert_eq!(error.code, "mission_region_unavailable");
    assert!(!error.retryable);
    assert_eq!(
        error.details.get("operation"),
        Some(&serde_json::json!("tab get"))
    );
    assert_eq!(
        error.details.get("workspace_id"),
        Some(&serde_json::json!("w78"))
    );
    assert_eq!(
        error.details.get("tab_id"),
        Some(&serde_json::json!("w78:t1"))
    );
    assert_eq!(
        error.details.get("region"),
        Some(&serde_json::json!("工作区"))
    );
    cleanup(&path);
}

#[test]
fn missing_region_ids_reuse_unique_existing_fixed_tabs_before_creating() {
    let path = temp_db("region-discovery");
    let mission_id = "msn-20260827-082057-region-discovery-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    upsert_workspace(
        &path,
        mission_id,
        &MissionWorkspace {
            source: WorkspaceSource::Current,
            workspace_id: "w6J:ws1".into(),
            tab_id: String::new(),
            root_pane_id: String::new(),
            execution_tab_id: String::new(),
            review_tab_id: String::new(),
            verification_tab_id: String::new(),
            worktree_path: ".".into(),
            branch: "main".into(),
        },
    )
    .unwrap();
    let runner = FakeRunner::success();
    runner.set_tab_label("w6J:t1", "1");
    runner.set_tab_label("w6J:t7", "工作区");
    runner.set_tab_label("w6J:t8", "审查");
    runner.set_tab_label("w6J:t9", "验证");
    runner.set_pane_tab("w6J:p7", "w6J:t7");

    launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();

    let workspace = read_workspace(&path, mission_id).unwrap().unwrap();
    assert_eq!(workspace.execution_tab_id, "w6J:t7");
    assert_eq!(workspace.root_pane_id, "w6J:p7");
    assert_eq!(workspace.review_tab_id, "w6J:t8");
    assert_eq!(workspace.verification_tab_id, "w6J:t9");
    assert_eq!(runner.count_calls("tab", "create"), 0);
    cleanup(&path);
}

#[test]
fn completed_role_in_a_legacy_tab_moves_into_work_region() {
    let path = temp_db("legacy-role-tab");
    let mission_id = "msn-20260827-082057-legacy-role-tab-0b801f50";
    create_mission(&path, &mission_request(mission_id)).unwrap();
    let runner = FakeRunner::success();
    launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE team_roles SET pane_id = 'w6J:p-worker', terminal_id = 'mission-legacy-worker', health = 'idle' WHERE mission_id = ?1 AND role = 'worker'",
            [mission_id],
        )
        .unwrap();
    runner.set_tab_label("w6J:t7", "worker");
    runner.set_pane_tab("w6J:p-worker", "w6J:t7");

    launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(
        runner
            .pane_tabs
            .borrow()
            .get("w6J:p-worker")
            .map(String::as_str),
        Some("w6J:t1")
    );
    assert_eq!(runner.count_calls("pane", "move"), 1);
    cleanup(&path);
}

#[test]
fn start_role_adopts_a_late_agent_in_the_work_region() {
    let path = temp_db("start-role-adopt");
    let mission_id = "msn-20260827-082057-rust-version-0b801f50";
    create_mission(&path, &manual_mission_request(mission_id)).unwrap();

    let initial = FakeRunner::success();
    let launch = launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &initial,
        "herdr",
        &mut |_| {},
    )
    .unwrap();
    let pm_pane = launch.roles[0].pane_id.clone();
    let recovery = FakeRunner::recoverable_agent_start(
        "timed out waiting for agent startup",
        FakePaneState {
            agent: "codex",
            cwd: ".",
            tab_label: "工作区",
            has_session: true,
        },
    );

    let started = start_role(
        &path,
        mission_id,
        "scout",
        &pm_pane,
        ".",
        None,
        &recovery,
        "herdr",
        &mut |_| {},
    )
    .unwrap()
    .unwrap();

    assert_eq!(started.pane_id, "w6J:pX");
    assert_eq!(recovery.count_calls("agent", "start"), 1);
    assert_eq!(recovery.count_calls("agent", "rename"), 1);
    cleanup(&path);
}

#[test]
fn start_role_recovers_a_persisted_unfinished_pane_without_splitting_again() {
    let path = temp_db("start-role-staged");
    let mission_id = "msn-20260827-082057-start-role-staged-0b801f50";
    create_mission(&path, &manual_mission_request(mission_id)).unwrap();
    let initial = FakeRunner::success();
    let launch = launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &initial,
        "herdr",
        &mut |_| {},
    )
    .unwrap();
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE team_roles SET pane_id = 'w6J:p-staged', terminal_id = '' WHERE mission_id = ?1 AND role = 'scout'",
            [mission_id],
        )
        .unwrap();
    let recovery = FakeRunner::recoverable_agent_start(
        "agent_pane_busy",
        FakePaneState {
            agent: "codex",
            cwd: ".",
            tab_label: "工作区",
            has_session: true,
        },
    )
    .with_missing_session_polls(3);

    let started = start_role(
        &path,
        mission_id,
        "scout",
        &launch.roles[0].pane_id,
        ".",
        None,
        &recovery,
        "herdr",
        &mut |_| {},
    )
    .unwrap()
    .unwrap();

    assert_eq!(started.pane_id, "w6J:p-staged");
    assert_eq!(recovery.count_calls("pane", "split"), 0);
    assert_eq!(recovery.count_calls("agent", "start"), 0);
    assert_eq!(recovery.count_calls("pane", "get"), 4);
    assert_eq!(recovery.count_calls("agent", "rename"), 1);
    cleanup(&path);
}

#[test]
fn start_role_rejects_an_anchor_outside_the_work_region_before_split() {
    let path = temp_db("start-role-wrong-region");
    let mission_id = "msn-20260827-082057-wrong-region-0b801f50";
    create_mission(&path, &manual_mission_request(mission_id)).unwrap();
    let initial = FakeRunner::success();
    let launch = launch_mission(
        &path,
        mission_id,
        &LaunchOptions::default(),
        &initial,
        "herdr",
        &mut |_| {},
    )
    .unwrap();
    let runner = FakeRunner::recoverable_agent_start(
        "agent_pane_busy",
        FakePaneState {
            agent: "codex",
            cwd: ".",
            tab_label: "审查",
            has_session: true,
        },
    );

    let error = start_role(
        &path,
        mission_id,
        "scout",
        &launch.roles[0].pane_id,
        ".",
        None,
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap_err();

    assert_eq!(error.code, "launch_effect_failed");
    assert_eq!(runner.count_calls("pane", "split"), 0);
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
fn launch_adopts_a_timed_out_agent_using_the_persisted_worktree_cwd() {
    let path = temp_db("worktree-timeout");
    let mission_id = "msn-20260827-082057-worktree-timeout-0b801f50";
    create_mission(&path, &simple_mission_request(mission_id)).unwrap();
    let runner = FakeRunner::recoverable_agent_start(
        "timed out waiting for agent startup",
        FakePaneState {
            agent: "codex",
            cwd: "/repo/.worktree/x",
            tab_label: "工作区",
            has_session: true,
        },
    );

    let outcome = launch_mission(
        &path,
        mission_id,
        &LaunchOptions {
            cwd: "/repo".into(),
            workspace_source: WorkspaceSource::Worktree,
            ..LaunchOptions::default()
        },
        &runner,
        "herdr",
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome.stage, "active");
    assert_eq!(runner.count_calls("agent", "start"), 1);
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
