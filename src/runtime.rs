//! Mission launch orchestration.
//!
//! Connects the persisted Mission/roles (phase 1 of `new`) to the real Herdr
//! CLI (phase 2): for each role, split a sibling pane in the current tab, start
//! its agent with a unique live name, record the pane/agent identity, then
//! advance the Mission stage to `active`. On any failure the stage is parked at
//! `blocked` and the partial effect is reported, matching the two-phase
//! `preparing -> active` model.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::json;

use crate::{
    agent_name_token, agent_rename_argv, agent_start_args, git_head, git_root, pane_get_argv,
    pane_list_argv, pane_move_to_tab_argv, pane_rename_argv, pane_run_argv, pane_split_in_argv,
    parse_pane_get, parse_pane_list, parse_pane_split, parse_tab_create, parse_tab_get,
    parse_tab_list, parse_workspace_create, parse_worktree_create, primary_worktree,
    read_mission_title, read_role_runtime, read_workspace, record_role_pane, record_role_runtime,
    role_init_prompt, set_mission_stage, slugify, tab_create_argv, tab_get_argv, tab_list_argv,
    tab_rename_argv, upsert_workspace, workspace_create_argv, workspace_label,
    worktree_create_argv, worktree_open_argv, ErrorCategory, KernelError, LaunchConfig, LaunchMode,
    MissionWorkspace, PaneInfo, ProcessOutput, ProcessRunner, RoleRuntimeRow, TabCreated, TabMode,
    WorkspaceSource, REVIEW_REGION_NAME, VERIFICATION_REGION_NAME, WORK_REGION_NAME,
};

/// How many times to poll or re-attempt after delayed Agent/shell readiness.
const AGENT_RECOVERY_POLLS: u32 = 20;
const AGENT_START_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Options controlling how role panes are laid out during launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOptions {
    pub direction: String,
    pub cwd: String,
    pub autonomy: String,
    pub prompts_dir: Option<PathBuf>,
    pub tab_mode: TabMode,
    pub launch_mode: LaunchMode,
    pub workspace_source: WorkspaceSource,
    pub worktree_path: Option<String>,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            direction: "right".into(),
            cwd: ".".into(),
            autonomy: "manual".into(),
            prompts_dir: None,
            tab_mode: TabMode::Lanes,
            launch_mode: LaunchMode::Manual,
            workspace_source: WorkspaceSource::Current,
            worktree_path: None,
        }
    }
}

/// One role that was launched into a live agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchedRole {
    pub role: String,
    pub agent_name: String,
    pub pane_id: String,
}

/// Result of a launch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOutcome {
    pub stage: String,
    pub roles: Vec<LaunchedRole>,
}

/// Launch every role of a persisted Mission into a live Herdr agent.
///
/// This is the phase-2 half of `new`. Persisted roles are skipped, and a retry
/// safely adopts an Agent that became ready after `agent start` timed out.
pub fn launch_mission(
    database: &Path,
    mission_id: &str,
    options: &LaunchOptions,
    runner: &dyn ProcessRunner,
    herdr: &str,
    progress: &mut dyn FnMut(&str),
) -> Result<LaunchOutcome, KernelError> {
    let roles = read_role_runtime(database, mission_id)?;
    let title = read_mission_title(database, mission_id)?;
    let is_simple = roles.len() == 1 && roles[0].role == "worker";
    let mut workspace = ensure_mission_workspace(
        database, mission_id, &title, options, runner, herdr, progress,
    )?;
    ensure_stage_tabs(
        database,
        mission_id,
        &mut workspace,
        options,
        runner,
        herdr,
        progress,
    )?;
    upsert_workspace(database, mission_id, &workspace)
        .map_err(|error| blocked(database, mission_id, error))?;
    let token = agent_name_token(mission_id);
    let mut launched = Vec::with_capacity(roles.len());
    let database_str = database.to_string_lossy().into_owned();
    let bin = mission_bin();
    let role_cwd = mission_worktree(&workspace, &options.cwd).to_string();

    for row in &roles {
        let agent_name = format!("mission-{token}-{}", row.role);
        if !row.pane_id.is_empty() && !row.agent_name.is_empty() {
            reconcile_completed_role(runner, herdr, row, &agent_name, &workspace, &role_cwd)
                .map_err(|error| blocked(database, mission_id, error))?;
            progress(&format!("跳过已启动的 {role}", role = row.role));
            continue;
        }
        if !is_simple && options.launch_mode == LaunchMode::Manual && row.role != "pm" {
            progress(&format!(
                "manual 模式：跳过 {role}（等待 PM 按需启动）",
                role = row.role
            ));
            continue;
        }
        progress(&format!("启动 {role} ...", role = row.role));

        let pane_id = if !row.pane_id.is_empty() {
            validate_work_region_pane(runner, herdr, &row.pane_id, &workspace, &role_cwd)
                .map_err(|error| blocked(database, mission_id, error))?;
            row.pane_id.clone()
        } else if is_simple || row.role == "pm" {
            workspace.root_pane_id.clone()
        } else {
            let split = match run(
                runner,
                herdr,
                &pane_split_in_argv(&options.direction, &role_cwd, &workspace.root_pane_id),
            ) {
                Ok(output) if output.exit_code == 0 => output,
                Ok(output) => {
                    return Err(blocked(
                        database,
                        mission_id,
                        launch_failed("pane split", &output),
                    ));
                }
                Err(error) => return Err(blocked(database, mission_id, error)),
            };
            match parse_pane_split(&split.stdout) {
                Ok(pane) => pane.pane_id,
                Err(error) => return Err(blocked(database, mission_id, error)),
            }
        };
        if row.pane_id.is_empty() {
            record_role_pane(database, mission_id, &row.role, &pane_id)
                .map_err(|error| blocked(database, mission_id, error))?;
        }

        let mut argv = match agent_start_args(&row.provider, None) {
            Ok(argv) => argv,
            Err(error) => return Err(blocked(database, mission_id, error)),
        };
        let prompt = match role_init_prompt(
            &title,
            mission_id,
            &row.role,
            &role_cwd,
            &options.autonomy,
            &database_str,
            &bin,
            options.prompts_dir.as_deref(),
        ) {
            Ok(prompt) => prompt,
            Err(error) => return Err(blocked(database, mission_id, error)),
        };
        // The full prompt is multi-line Markdown and cannot be encoded as a
        // shell argument; write it beside the database and pass a short
        // single-line reference the freshly started agent will read instead.
        let prompt_path = match write_role_prompt(database, mission_id, &row.role, &prompt) {
            Ok(path) => path,
            Err(error) => return Err(blocked(database, mission_id, error)),
        };
        argv.push(format!(
            "请读取并严格遵循 {prompt_path} 中的 Mission 运行说明。"
        ));
        let mut command = vec![
            "agent".to_string(),
            "start".to_string(),
            agent_name.clone(),
            "--kind".to_string(),
            row.provider.clone(),
            "--pane".to_string(),
            pane_id.clone(),
            "--".to_string(),
        ];
        command.extend(argv);

        let recovery = AgentRecoveryTarget {
            pane_id: &pane_id,
            agent_name: &agent_name,
            provider: &row.provider,
            cwd: &role_cwd,
            workspace_id: &workspace.workspace_id,
            tab_id: non_empty(&workspace.execution_tab_id),
            tab_label: non_empty(&workspace.execution_tab_id).map(|_| WORK_REGION_NAME),
        };
        ensure_agent_running(runner, herdr, !row.pane_id.is_empty(), &command, &recovery)
            .map_err(|error| blocked(database, mission_id, error))?;

        record_role_runtime(database, mission_id, &row.role, &pane_id, &agent_name)
            .map_err(|error| blocked(database, mission_id, error))?;
        // Rename after the agent is started so the descriptive label wins over
        // any agent-detected branding: "⚑ <mission title> › <role label>".
        let pane_label = format!("⚑ {title} › {}", role_label(&row.role));
        if let Err(error) = run(runner, herdr, &pane_rename_argv(&pane_id, &pane_label)) {
            progress(&format!("  警告: 重命名 pane 失败: {}", error.message));
        }
        progress(&format!(
            "✓ {role} → {agent_name} @ {pane_id}",
            role = row.role,
            agent_name = agent_name,
            pane_id = pane_id,
        ));
        launched.push(LaunchedRole {
            role: row.role.clone(),
            agent_name,
            pane_id,
        });
    }

    // herdr-kit's agent-detected branding renames panes asynchronously after
    // each agent settles; re-apply our descriptive labels once more so the
    // sidebar reads like the existing kit instead of the generic agent brand.
    if !launched.is_empty() {
        std::thread::sleep(Duration::from_millis(2_000));
        for role in &launched {
            let label = format!("⚑ {title} › {}", role_label(&role.role));
            let _ = run(runner, herdr, &pane_rename_argv(&role.pane_id, &label));
        }
    }

    set_mission_stage(database, mission_id, "active")?;
    crate::log_event(
        database,
        &format!(
            "mission={mission_id} launch ok stage=active roles={}",
            launched.len()
        ),
    );
    progress("Mission 已进入 active 状态");
    Ok(LaunchOutcome {
        stage: "active".into(),
        roles: launched,
    })
}

/// Start a single role on demand, splitting a new pane to the right of the
/// given anchor pane. Idempotent: a role whose pane_id is already recorded is
/// left untouched and `None` is returned.
#[allow(clippy::too_many_arguments)]
pub fn start_role(
    database: &Path,
    mission_id: &str,
    role: &str,
    anchor_pane_id: &str,
    cwd: &str,
    autonomy: &str,
    prompts_dir: Option<&Path>,
    runner: &dyn ProcessRunner,
    herdr: &str,
    progress: &mut dyn FnMut(&str),
) -> Result<Option<LaunchedRole>, KernelError> {
    let roles = read_role_runtime(database, mission_id)?;
    let row = roles
        .iter()
        .find(|row| row.role == role)
        .ok_or_else(|| role_not_found(mission_id, role))?;
    if !row.pane_id.is_empty() && !row.agent_name.is_empty() {
        progress(&format!("{role} 已启动，跳过"));
        return Ok(None);
    }

    let title = read_mission_title(database, mission_id)?;
    let token = agent_name_token(mission_id);
    let agent_name = format!("mission-{token}-{role}");
    let workspace = read_workspace(database, mission_id)?.ok_or_else(|| {
        blocked(
            database,
            mission_id,
            KernelError {
                category: ErrorCategory::Domain,
                code: "workspace_not_found".into(),
                message: "Mission workspace is not recorded".into(),
                retryable: false,
                details: BTreeMap::from([("mission_id".into(), json!(mission_id))]),
            },
        )
    })?;
    let role_cwd = mission_worktree(&workspace, cwd).to_string();
    validate_work_region_pane(runner, herdr, anchor_pane_id, &workspace, &role_cwd)
        .map_err(|error| blocked(database, mission_id, error))?;

    let pane_id = if row.pane_id.is_empty() {
        let split = run(
            runner,
            herdr,
            &pane_split_in_argv("right", &role_cwd, anchor_pane_id),
        )
        .map_err(|error| blocked(database, mission_id, error))?;
        if split.exit_code != 0 {
            return Err(blocked(
                database,
                mission_id,
                launch_failed("pane split", &split),
            ));
        }
        let pane_id = parse_pane_split(&split.stdout)
            .map_err(|error| blocked(database, mission_id, error))?
            .pane_id;
        record_role_pane(database, mission_id, role, &pane_id)
            .map_err(|error| blocked(database, mission_id, error))?;
        pane_id
    } else {
        validate_work_region_pane(runner, herdr, &row.pane_id, &workspace, &role_cwd)
            .map_err(|error| blocked(database, mission_id, error))?;
        row.pane_id.clone()
    };

    let mut argv = agent_start_args(&row.provider, None)
        .map_err(|error| blocked(database, mission_id, error))?;
    let database_str = database.to_string_lossy().into_owned();
    let bin = mission_bin();
    let prompt = role_init_prompt(
        &title,
        mission_id,
        role,
        &role_cwd,
        autonomy,
        &database_str,
        &bin,
        prompts_dir,
    )
    .map_err(|error| blocked(database, mission_id, error))?;
    let prompt_path = write_role_prompt(database, mission_id, role, &prompt)
        .map_err(|error| blocked(database, mission_id, error))?;
    argv.push(format!(
        "请读取并严格遵循 {prompt_path} 中的 Mission 运行说明。"
    ));
    let mut command = vec![
        "agent".to_string(),
        "start".to_string(),
        agent_name.clone(),
        "--kind".to_string(),
        row.provider.clone(),
        "--pane".to_string(),
        pane_id.clone(),
        "--".to_string(),
    ];
    command.extend(argv);

    let recovery = AgentRecoveryTarget {
        pane_id: &pane_id,
        agent_name: &agent_name,
        provider: &row.provider,
        cwd: &role_cwd,
        workspace_id: &workspace.workspace_id,
        tab_id: non_empty(&workspace.execution_tab_id),
        tab_label: non_empty(&workspace.execution_tab_id).map(|_| WORK_REGION_NAME),
    };
    ensure_agent_running(runner, herdr, !row.pane_id.is_empty(), &command, &recovery)
        .map_err(|error| blocked(database, mission_id, error))?;
    record_role_runtime(database, mission_id, role, &pane_id, &agent_name)
        .map_err(|error| blocked(database, mission_id, error))?;

    let pane_label = format!("⚑ {title} › {}", role_label(role));
    let _ = run(runner, herdr, &pane_rename_argv(&pane_id, &pane_label));
    progress(&format!("✓ {role} → {agent_name} @ {pane_id}"));
    Ok(Some(LaunchedRole {
        role: role.to_string(),
        agent_name,
        pane_id,
    }))
}

fn role_not_found(mission_id: &str, role: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Domain,
        code: "role_not_found".into(),
        message: "Mission 没有这个角色".into(),
        retryable: false,
        details: BTreeMap::from([
            ("mission_id".into(), json!(mission_id)),
            ("role".into(), json!(role)),
        ]),
    }
}

fn run(
    runner: &dyn ProcessRunner,
    program: &str,
    args: &[String],
) -> Result<ProcessOutput, KernelError> {
    runner.run(program, args).map_err(|error| KernelError {
        category: ErrorCategory::Infrastructure,
        code: "herdr_spawn_failed".into(),
        message: "failed to spawn herdr".into(),
        retryable: false,
        details: BTreeMap::from([("reason".into(), json!(error.to_string()))]),
    })
}

/// Ensure the Mission owns a dedicated workspace, creating one if needed.
fn ensure_mission_workspace(
    database: &Path,
    mission_id: &str,
    title: &str,
    options: &LaunchOptions,
    runner: &dyn ProcessRunner,
    herdr: &str,
    progress: &mut dyn FnMut(&str),
) -> Result<MissionWorkspace, KernelError> {
    if let Some(existing) = read_workspace(database, mission_id)? {
        if !existing.workspace_id.is_empty() {
            return Ok(existing);
        }
    }
    let workspace = match options.workspace_source {
        WorkspaceSource::Current => {
            progress("创建独立 Mission workspace…");
            let output = run(
                runner,
                herdr,
                &workspace_create_argv(&options.cwd, &workspace_label(title)),
            )?;
            if output.exit_code != 0 {
                return Err(blocked(
                    database,
                    mission_id,
                    launch_failed("workspace create", &output),
                ));
            }
            let created = parse_workspace_create(&output.stdout)
                .map_err(|error| blocked(database, mission_id, error))?;
            MissionWorkspace {
                source: WorkspaceSource::Current,
                workspace_id: created.workspace_id,
                tab_id: created.tab_id,
                root_pane_id: created.root_pane_id,
                execution_tab_id: String::new(),
                review_tab_id: String::new(),
                verification_tab_id: String::new(),
                worktree_path: options.cwd.clone(),
                branch: String::new(),
            }
        }
        WorkspaceSource::Worktree => {
            let repo_root = git_root(runner, &options.cwd)
                .map_err(|error| blocked(database, mission_id, error))?;
            let primary = primary_worktree(runner, &repo_root)
                .map_err(|error| blocked(database, mission_id, error))?;
            let head = git_head(runner, &repo_root)
                .map_err(|error| blocked(database, mission_id, error))?;
            let slug = slugify(title);
            let branch = format!("feature/{slug}-{}", agent_name_token(mission_id));
            let path = format!("{primary}/.worktree/{slug}");
            progress(&format!("创建 Worktree：{path}"));
            let output = run(
                runner,
                herdr,
                &worktree_create_argv(&primary, &branch, &head, &path, &workspace_label(title)),
            )?;
            if output.exit_code != 0 {
                return Err(blocked(
                    database,
                    mission_id,
                    launch_failed("worktree create", &output),
                ));
            }
            let created = parse_worktree_create(&output.stdout)
                .map_err(|error| blocked(database, mission_id, error))?;
            MissionWorkspace {
                source: WorkspaceSource::Worktree,
                workspace_id: created.workspace_id,
                tab_id: created.tab_id,
                root_pane_id: created.root_pane_id,
                execution_tab_id: String::new(),
                review_tab_id: String::new(),
                verification_tab_id: String::new(),
                worktree_path: if created.worktree_path.is_empty() {
                    path
                } else {
                    created.worktree_path
                },
                branch: if created.branch.is_empty() {
                    branch
                } else {
                    created.branch
                },
            }
        }
        WorkspaceSource::Import => {
            let target = options
                .worktree_path
                .as_deref()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    blocked(
                        database,
                        mission_id,
                        KernelError {
                            category: ErrorCategory::Operation,
                            code: "worktree_path_missing".into(),
                            message: "import worktree requires a target path".into(),
                            retryable: false,
                            details: BTreeMap::new(),
                        },
                    )
                })?;
            let repo_root = git_root(runner, &options.cwd)
                .map_err(|error| blocked(database, mission_id, error))?;
            let primary = primary_worktree(runner, &repo_root)
                .map_err(|error| blocked(database, mission_id, error))?;
            progress(&format!("导入 Worktree：{target}"));
            let output = run(
                runner,
                herdr,
                &worktree_open_argv(&primary, target, &workspace_label(title)),
            )?;
            if output.exit_code != 0 {
                return Err(blocked(
                    database,
                    mission_id,
                    launch_failed("worktree open", &output),
                ));
            }
            let created = parse_worktree_create(&output.stdout)
                .map_err(|error| blocked(database, mission_id, error))?;
            MissionWorkspace {
                source: WorkspaceSource::Import,
                workspace_id: created.workspace_id,
                tab_id: created.tab_id,
                root_pane_id: created.root_pane_id,
                execution_tab_id: String::new(),
                review_tab_id: String::new(),
                verification_tab_id: String::new(),
                worktree_path: if created.worktree_path.is_empty() {
                    target.to_string()
                } else {
                    created.worktree_path
                },
                branch: created.branch,
            }
        }
    };
    upsert_workspace(database, mission_id, &workspace)
        .map_err(|error| blocked(database, mission_id, error))?;
    Ok(workspace)
}

/// Build the 工作区/审查/验证 region tabs inside the workspace.
///
/// Mission Agents land in the 工作区 root pane or its splits, while the review and
/// verification tabs best-effort start their configured tool command. Each tab
/// is built at most once (guarded by the persisted tab id) so resume does not
/// open duplicates.
fn ensure_stage_tabs(
    database: &Path,
    mission_id: &str,
    workspace: &mut MissionWorkspace,
    options: &LaunchOptions,
    runner: &dyn ProcessRunner,
    herdr: &str,
    progress: &mut dyn FnMut(&str),
) -> Result<(), KernelError> {
    let config = LaunchConfig::load();
    let worktree = if workspace.worktree_path.is_empty() {
        options.cwd.clone()
    } else {
        workspace.worktree_path.clone()
    };

    if workspace.execution_tab_id.is_empty() {
        if workspace.tab_id.is_empty() {
            let existing =
                find_unique_stage_tab(runner, herdr, &workspace.workspace_id, WORK_REGION_NAME)
                    .map_err(|error| blocked(database, mission_id, error))?;
            if let Some(tab_id) = existing {
                progress("复用已存在的工作区 tab…");
                workspace.root_pane_id =
                    find_unique_stage_pane(runner, herdr, &workspace.workspace_id, &tab_id)
                        .map_err(|error| blocked(database, mission_id, error))?;
                workspace.execution_tab_id = tab_id;
            } else {
                progress("准备工作区 tab…");
                let tab = create_stage_tab(
                    runner,
                    herdr,
                    &workspace.workspace_id,
                    &worktree,
                    WORK_REGION_NAME,
                )
                .map_err(|error| blocked(database, mission_id, error))?;
                workspace.execution_tab_id = tab.tab_id;
                workspace.root_pane_id = tab.root_pane_id;
            }
        } else {
            // 复用 workspace 自带的初始 tab 作为工作区，避免留下一个空 tab。
            // root_pane_id 已经是初始 tab 的 root pane，Agent 会落到这里或其 split。
            progress("复用 workspace 初始 tab 作为工作区…");
            let renamed = run(
                runner,
                herdr,
                &tab_rename_argv(&workspace.tab_id, WORK_REGION_NAME),
            )
            .map_err(|error| blocked(database, mission_id, error))?;
            if renamed.exit_code != 0 {
                return Err(blocked(
                    database,
                    mission_id,
                    launch_failed("tab rename", &renamed),
                ));
            }
            workspace.execution_tab_id = workspace.tab_id.clone();
        }
        upsert_workspace(database, mission_id, workspace)
            .map_err(|error| blocked(database, mission_id, error))?;
    } else {
        ensure_named_tab(
            runner,
            herdr,
            &workspace.workspace_id,
            &workspace.execution_tab_id,
            WORK_REGION_NAME,
        )
        .map_err(|error| blocked(database, mission_id, error))?;
    }
    if workspace.review_tab_id.is_empty() {
        let existing =
            find_unique_stage_tab(runner, herdr, &workspace.workspace_id, REVIEW_REGION_NAME)
                .map_err(|error| blocked(database, mission_id, error))?;
        let created = if let Some(tab_id) = existing {
            progress("复用已存在的审查 tab…");
            workspace.review_tab_id = tab_id;
            None
        } else {
            progress("准备审查 tab…");
            let tab = create_stage_tab(
                runner,
                herdr,
                &workspace.workspace_id,
                &worktree,
                REVIEW_REGION_NAME,
            )
            .map_err(|error| blocked(database, mission_id, error))?;
            workspace.review_tab_id = tab.tab_id.clone();
            Some(tab)
        };
        upsert_workspace(database, mission_id, workspace)
            .map_err(|error| blocked(database, mission_id, error))?;
        if let Some(tab) = created {
            let output = run(
                runner,
                herdr,
                &pane_run_argv(&tab.root_pane_id, &config.tabs.tools.review),
            );
            if !matches!(output, Ok(ref output) if output.exit_code == 0) {
                progress("  警告: 审查工具启动失败，保留审查区域 shell");
            }
        }
    } else {
        ensure_named_tab(
            runner,
            herdr,
            &workspace.workspace_id,
            &workspace.review_tab_id,
            REVIEW_REGION_NAME,
        )
        .map_err(|error| blocked(database, mission_id, error))?;
    }
    if workspace.verification_tab_id.is_empty() {
        let existing = find_unique_stage_tab(
            runner,
            herdr,
            &workspace.workspace_id,
            VERIFICATION_REGION_NAME,
        )
        .map_err(|error| blocked(database, mission_id, error))?;
        let created = if let Some(tab_id) = existing {
            progress("复用已存在的验证 tab…");
            workspace.verification_tab_id = tab_id;
            None
        } else {
            progress("准备验证 tab…");
            let tab = create_stage_tab(
                runner,
                herdr,
                &workspace.workspace_id,
                &worktree,
                VERIFICATION_REGION_NAME,
            )
            .map_err(|error| blocked(database, mission_id, error))?;
            workspace.verification_tab_id = tab.tab_id.clone();
            Some(tab)
        };
        upsert_workspace(database, mission_id, workspace)
            .map_err(|error| blocked(database, mission_id, error))?;
        if let Some(tab) = created {
            let output = run(
                runner,
                herdr,
                &pane_run_argv(&tab.root_pane_id, &config.tabs.tools.processes),
            );
            if !matches!(output, Ok(ref output) if output.exit_code == 0) {
                progress("  警告: 验证工具启动失败，保留验证区域 shell");
            }
        }
    } else {
        ensure_named_tab(
            runner,
            herdr,
            &workspace.workspace_id,
            &workspace.verification_tab_id,
            VERIFICATION_REGION_NAME,
        )
        .map_err(|error| blocked(database, mission_id, error))?;
    }
    Ok(())
}

fn find_unique_stage_tab(
    runner: &dyn ProcessRunner,
    herdr: &str,
    workspace_id: &str,
    label: &str,
) -> Result<Option<String>, KernelError> {
    let output = run(runner, herdr, &tab_list_argv(workspace_id))?;
    if output.exit_code != 0 {
        return Err(launch_failed("tab list", &output));
    }
    let matches = parse_tab_list(&output.stdout)?
        .into_iter()
        .filter(|tab| tab.workspace_id == workspace_id && tab.label == label)
        .map(|tab| tab.tab_id)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [tab_id] => Ok(Some(tab_id.clone())),
        _ => Err(KernelError {
            category: ErrorCategory::Infrastructure,
            code: "launch_effect_failed".into(),
            message: "Mission region discovery found duplicate fixed-name tabs".into(),
            retryable: false,
            details: BTreeMap::from([
                ("workspace_id".into(), json!(workspace_id)),
                ("label".into(), json!(label)),
                ("tab_ids".into(), json!(matches)),
            ]),
        }),
    }
}

fn find_unique_stage_pane(
    runner: &dyn ProcessRunner,
    herdr: &str,
    workspace_id: &str,
    tab_id: &str,
) -> Result<String, KernelError> {
    let output = run(runner, herdr, &pane_list_argv(workspace_id))?;
    if output.exit_code != 0 {
        return Err(launch_failed("pane list", &output));
    }
    let matches = parse_pane_list(&output.stdout)?
        .into_iter()
        .filter(|pane| pane.workspace_id == workspace_id && pane.tab_id == tab_id)
        .map(|pane| pane.pane_id)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [pane_id] => Ok(pane_id.clone()),
        _ => Err(KernelError {
            category: ErrorCategory::Infrastructure,
            code: "launch_effect_failed".into(),
            message: "Recovered Mission work region must contain exactly one pane".into(),
            retryable: false,
            details: BTreeMap::from([
                ("workspace_id".into(), json!(workspace_id)),
                ("tab_id".into(), json!(tab_id)),
                ("pane_ids".into(), json!(matches)),
            ]),
        }),
    }
}

fn reconcile_completed_role(
    runner: &dyn ProcessRunner,
    herdr: &str,
    row: &RoleRuntimeRow,
    agent_name: &str,
    workspace: &MissionWorkspace,
    cwd: &str,
) -> Result<(), KernelError> {
    let output = run(runner, herdr, &pane_get_argv(&row.pane_id))?;
    if output.exit_code != 0 {
        return Err(launch_failed("pane get", &output));
    }
    let actual = parse_pane_get(&output.stdout)?;
    let expected = AgentRecoveryTarget {
        pane_id: &row.pane_id,
        agent_name,
        provider: &row.provider,
        cwd,
        workspace_id: &workspace.workspace_id,
        tab_id: non_empty(&workspace.execution_tab_id),
        tab_label: non_empty(&workspace.execution_tab_id).map(|_| WORK_REGION_NAME),
    };
    if actual.pane_id != row.pane_id
        || actual.workspace_id != workspace.workspace_id
        || actual.agent != row.provider
        || !actual.has_agent_session
        || !same_path(&actual.cwd, cwd)
    {
        return Err(recovery_rejected(
            &expected,
            Some(&actual),
            "completed role pane identity does not match Mission",
        ));
    }
    if actual.tab_id != workspace.execution_tab_id {
        let moved = run(
            runner,
            herdr,
            &pane_move_to_tab_argv(
                &row.pane_id,
                &workspace.execution_tab_id,
                &workspace.root_pane_id,
            ),
        )?;
        if moved.exit_code != 0 {
            return Err(launch_failed("pane move", &moved));
        }
    }
    if adopt_running_agent(runner, herdr, &expected)? != AgentAdoption::Adopted {
        return Err(recovery_rejected(
            &expected,
            None,
            "completed role Agent identity is not available",
        ));
    }
    Ok(())
}

fn ensure_named_tab(
    runner: &dyn ProcessRunner,
    herdr: &str,
    workspace_id: &str,
    tab_id: &str,
    label: &str,
) -> Result<(), KernelError> {
    let output = run(runner, herdr, &tab_get_argv(tab_id))?;
    if output.exit_code != 0 {
        return Err(launch_failed("tab get", &output));
    }
    let current = parse_tab_get(&output.stdout)?;
    if current.tab_id != tab_id || current.workspace_id != workspace_id {
        return Err(KernelError {
            category: ErrorCategory::Infrastructure,
            code: "launch_effect_failed".into(),
            message: "Mission region tab identity does not match persisted workspace".into(),
            retryable: false,
            details: BTreeMap::from([
                ("expected_workspace_id".into(), json!(workspace_id)),
                ("expected_tab_id".into(), json!(tab_id)),
                ("actual_workspace_id".into(), json!(current.workspace_id)),
                ("actual_tab_id".into(), json!(current.tab_id)),
            ]),
        });
    }
    if current.label == label {
        return Ok(());
    }
    let renamed = run(runner, herdr, &tab_rename_argv(tab_id, label))?;
    if renamed.exit_code != 0 {
        return Err(launch_failed("tab rename", &renamed));
    }
    Ok(())
}

fn create_stage_tab(
    runner: &dyn ProcessRunner,
    herdr: &str,
    workspace_id: &str,
    cwd: &str,
    label: &str,
) -> Result<TabCreated, KernelError> {
    let output = run(
        runner,
        herdr,
        &tab_create_argv(Some(workspace_id), Some(cwd), Some(label)),
    )?;
    if output.exit_code != 0 {
        return Err(launch_failed("tab create", &output));
    }
    parse_tab_create(&output.stdout)
}

fn validate_work_region_pane(
    runner: &dyn ProcessRunner,
    herdr: &str,
    pane_id: &str,
    workspace: &MissionWorkspace,
    cwd: &str,
) -> Result<(), KernelError> {
    let expected_tab_id = non_empty(&workspace.execution_tab_id).ok_or_else(|| KernelError {
        category: ErrorCategory::Infrastructure,
        code: "launch_effect_failed".into(),
        message: "Mission work region is not recorded".into(),
        retryable: false,
        details: BTreeMap::from([("operation".into(), json!("work region validation"))]),
    })?;
    let output = run(runner, herdr, &pane_get_argv(pane_id))?;
    if output.exit_code != 0 {
        return Err(launch_failed("pane get", &output));
    }
    let pane = parse_pane_get(&output.stdout)?;
    if pane.pane_id != pane_id
        || pane.workspace_id != workspace.workspace_id
        || pane.tab_id != expected_tab_id
        || !same_path(&pane.cwd, cwd)
    {
        return Err(work_region_rejected(
            pane_id,
            &workspace.workspace_id,
            expected_tab_id,
            cwd,
            Some(&pane),
            "anchor pane does not belong to the Mission work region",
        ));
    }

    let output = run(runner, herdr, &tab_get_argv(expected_tab_id))?;
    if output.exit_code != 0 {
        return Err(launch_failed("tab get", &output));
    }
    let tab = parse_tab_get(&output.stdout)?;
    if tab.workspace_id != workspace.workspace_id
        || tab.tab_id != expected_tab_id
        || tab.label != WORK_REGION_NAME
    {
        return Err(work_region_rejected(
            pane_id,
            &workspace.workspace_id,
            expected_tab_id,
            cwd,
            Some(&pane),
            "anchor pane tab is not named 工作区",
        ));
    }
    Ok(())
}

fn work_region_rejected(
    pane_id: &str,
    workspace_id: &str,
    tab_id: &str,
    cwd: &str,
    actual: Option<&PaneInfo>,
    reason: &str,
) -> KernelError {
    let mut details = BTreeMap::from([
        ("operation".into(), json!("work region validation")),
        ("reason".into(), json!(reason)),
        ("expected_pane_id".into(), json!(pane_id)),
        ("expected_workspace_id".into(), json!(workspace_id)),
        ("expected_tab_id".into(), json!(tab_id)),
        ("expected_cwd".into(), json!(cwd)),
    ]);
    if let Some(actual) = actual {
        details.extend([
            ("actual_pane_id".into(), json!(actual.pane_id)),
            ("actual_workspace_id".into(), json!(actual.workspace_id)),
            ("actual_tab_id".into(), json!(actual.tab_id)),
            ("actual_cwd".into(), json!(actual.cwd)),
        ]);
    }
    KernelError {
        category: ErrorCategory::Infrastructure,
        code: "launch_effect_failed".into(),
        message: "pane is outside the Mission work region".into(),
        retryable: false,
        details,
    }
}

struct AgentRecoveryTarget<'a> {
    pane_id: &'a str,
    agent_name: &'a str,
    provider: &'a str,
    cwd: &'a str,
    workspace_id: &'a str,
    tab_id: Option<&'a str>,
    tab_label: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentAdoption {
    Empty,
    PendingSession,
    Adopted,
}

fn ensure_agent_running(
    runner: &dyn ProcessRunner,
    herdr: &str,
    staged: bool,
    command: &[String],
    recovery: &AgentRecoveryTarget<'_>,
) -> Result<(), KernelError> {
    if staged {
        for poll in 0..=AGENT_RECOVERY_POLLS {
            match adopt_running_agent(runner, herdr, recovery)? {
                AgentAdoption::Adopted => return Ok(()),
                AgentAdoption::Empty => break,
                AgentAdoption::PendingSession if poll < AGENT_RECOVERY_POLLS => {
                    std::thread::sleep(AGENT_START_RETRY_DELAY);
                }
                AgentAdoption::PendingSession => {
                    return Err(recovery_rejected(
                        recovery,
                        None,
                        "Agent session did not become available before recovery timeout",
                    ));
                }
            }
        }
    }
    start_agent(runner, herdr, command, recovery, !staged)
}

fn start_agent(
    runner: &dyn ProcessRunner,
    herdr: &str,
    command: &[String],
    recovery: &AgentRecoveryTarget<'_>,
    retry_fresh_busy: bool,
) -> Result<(), KernelError> {
    let mut busy_retries = 0;
    loop {
        let output = run(runner, herdr, command)?;
        if output.exit_code == 0 {
            return Ok(());
        }

        if is_agent_start_timeout(&output) {
            let original = launch_failed("agent start", &output);
            for poll in 0..=AGENT_RECOVERY_POLLS {
                match adopt_running_agent(runner, herdr, recovery)? {
                    AgentAdoption::Adopted => return Ok(()),
                    AgentAdoption::Empty | AgentAdoption::PendingSession
                        if poll < AGENT_RECOVERY_POLLS =>
                    {
                        std::thread::sleep(AGENT_START_RETRY_DELAY);
                    }
                    AgentAdoption::Empty | AgentAdoption::PendingSession => return Err(original),
                }
            }
        }

        if !is_agent_pane_busy(&output) {
            return Err(launch_failed("agent start", &output));
        }

        let original = launch_failed("agent start", &output);
        match adopt_running_agent(runner, herdr, recovery)? {
            AgentAdoption::Adopted => return Ok(()),
            AgentAdoption::PendingSession => {
                for poll in 0..AGENT_RECOVERY_POLLS {
                    std::thread::sleep(AGENT_START_RETRY_DELAY);
                    match adopt_running_agent(runner, herdr, recovery)? {
                        AgentAdoption::Adopted => return Ok(()),
                        AgentAdoption::PendingSession if poll + 1 < AGENT_RECOVERY_POLLS => {}
                        AgentAdoption::Empty | AgentAdoption::PendingSession => {
                            return Err(original);
                        }
                    }
                }
                return Err(original);
            }
            AgentAdoption::Empty if retry_fresh_busy && busy_retries < AGENT_RECOVERY_POLLS => {
                busy_retries += 1;
                std::thread::sleep(AGENT_START_RETRY_DELAY);
            }
            AgentAdoption::Empty => return Err(original),
        }
    }
}

fn adopt_running_agent(
    runner: &dyn ProcessRunner,
    herdr: &str,
    expected: &AgentRecoveryTarget<'_>,
) -> Result<AgentAdoption, KernelError> {
    let output = run(runner, herdr, &pane_get_argv(expected.pane_id))?;
    if output.exit_code != 0 {
        return Err(launch_failed("pane get", &output));
    }
    let actual = parse_pane_get(&output.stdout)
        .map_err(|error| recovery_rejected(expected, None, error.message))?;

    if actual.pane_id != expected.pane_id {
        return Err(recovery_rejected(
            expected,
            Some(&actual),
            "pane id does not match launch target",
        ));
    }
    if actual.workspace_id != expected.workspace_id {
        return Err(recovery_rejected(
            expected,
            Some(&actual),
            "workspace does not match Mission",
        ));
    }
    if !same_path(&actual.cwd, expected.cwd) {
        return Err(recovery_rejected(
            expected,
            Some(&actual),
            "Agent cwd does not match Mission worktree",
        ));
    }

    if let Some(expected_tab_id) = expected.tab_id {
        if actual.tab_id != expected_tab_id {
            return Err(recovery_rejected(
                expected,
                Some(&actual),
                "Agent is outside the Mission work region",
            ));
        }
        let output = run(runner, herdr, &tab_get_argv(expected_tab_id))?;
        if output.exit_code != 0 {
            return Err(launch_failed("tab get", &output));
        }
        let tab = parse_tab_get(&output.stdout)
            .map_err(|error| recovery_rejected(expected, Some(&actual), error.message))?;
        if tab.workspace_id != expected.workspace_id
            || tab.tab_id != expected_tab_id
            || expected.tab_label != Some(tab.label.as_str())
        {
            return Err(recovery_rejected(
                expected,
                Some(&actual),
                "Agent tab label does not match the Mission work region",
            ));
        }
    }

    if actual.agent.is_empty() && !actual.has_agent_session {
        return Ok(AgentAdoption::Empty);
    }
    if actual.agent != expected.provider {
        return Err(recovery_rejected(
            expected,
            Some(&actual),
            "Agent provider does not match role",
        ));
    }
    if !actual.has_agent_session {
        return Ok(AgentAdoption::PendingSession);
    }

    let output = run(
        runner,
        herdr,
        &agent_rename_argv(expected.pane_id, expected.agent_name),
    )?;
    if output.exit_code != 0 {
        return Err(launch_failed("agent rename", &output));
    }
    Ok(AgentAdoption::Adopted)
}

fn is_agent_pane_busy(output: &ProcessOutput) -> bool {
    output.stderr.contains("agent_pane_busy") || output.stdout.contains("agent_pane_busy")
}

fn is_agent_start_timeout(output: &ProcessOutput) -> bool {
    let text = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    text.contains("agent")
        && (text.contains("timed out")
            || text.contains("startup_timeout")
            || text.contains("startup timeout"))
}

fn same_path(actual: &str, expected: &str) -> bool {
    match (fs::canonicalize(actual), fs::canonicalize(expected)) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => Path::new(actual) == Path::new(expected),
    }
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn mission_worktree<'a>(workspace: &'a MissionWorkspace, fallback: &'a str) -> &'a str {
    non_empty(&workspace.worktree_path).unwrap_or(fallback)
}

fn recovery_rejected(
    expected: &AgentRecoveryTarget<'_>,
    actual: Option<&PaneInfo>,
    reason: impl Into<String>,
) -> KernelError {
    let mut details = BTreeMap::from([
        ("operation".into(), json!("agent recovery")),
        ("reason".into(), json!(reason.into())),
        ("expected_pane_id".into(), json!(expected.pane_id)),
        ("expected_provider".into(), json!(expected.provider)),
        ("expected_cwd".into(), json!(expected.cwd)),
        ("expected_workspace_id".into(), json!(expected.workspace_id)),
        ("expected_tab_id".into(), json!(expected.tab_id)),
    ]);
    if let Some(actual) = actual {
        details.extend([
            ("actual_pane_id".into(), json!(actual.pane_id)),
            ("actual_provider".into(), json!(actual.agent)),
            ("actual_cwd".into(), json!(actual.cwd)),
            ("actual_workspace_id".into(), json!(actual.workspace_id)),
            ("actual_tab_id".into(), json!(actual.tab_id)),
            (
                "actual_has_agent_session".into(),
                json!(actual.has_agent_session),
            ),
        ]);
    }
    KernelError {
        category: ErrorCategory::Infrastructure,
        code: "launch_effect_failed".into(),
        message: "running Agent could not be safely adopted".into(),
        retryable: false,
        details,
    }
}

fn launch_failed(operation: &str, output: &ProcessOutput) -> KernelError {
    KernelError {
        category: ErrorCategory::Infrastructure,
        code: "launch_effect_failed".into(),
        message: "mission launch effect failed".into(),
        retryable: true,
        details: BTreeMap::from([
            ("operation".into(), json!(operation)),
            ("exit_code".into(), json!(output.exit_code)),
            ("stderr".into(), json!(output.stderr)),
        ]),
    }
}

fn blocked(database: &Path, mission_id: &str, error: KernelError) -> KernelError {
    let _ = set_mission_stage(database, mission_id, "blocked");
    crate::log_mission_error(database, mission_id, &error);
    error
}

/// Short display label for a role, matching the existing kit's sidebar labels.
fn role_label(role: &str) -> &str {
    match role {
        "pm" => "PM",
        "worker" => "Worker",
        "scout" => "Scout",
        "reviewer" => "Reviewer",
        _ => role,
    }
}

/// Persist a role's full init prompt beside the database so the freshly
/// started agent can read it from a stable file instead of a shell argument.
fn write_role_prompt(
    database: &Path,
    mission_id: &str,
    role: &str,
    prompt: &str,
) -> Result<String, KernelError> {
    let parent = database.parent().unwrap_or_else(|| Path::new("."));
    let dir = parent.join("mission-prompts").join(mission_id);
    fs::create_dir_all(&dir).map_err(|error| KernelError {
        category: ErrorCategory::Infrastructure,
        code: "prompt_dir_create_failed".into(),
        message: "failed to create the mission prompt directory".into(),
        retryable: false,
        details: BTreeMap::from([
            ("path".into(), json!(dir)),
            ("reason".into(), json!(error.to_string())),
        ]),
    })?;
    let path = dir.join(format!("{role}.md"));
    fs::write(&path, prompt).map_err(|error| KernelError {
        category: ErrorCategory::Infrastructure,
        code: "prompt_write_failed".into(),
        message: "failed to write the role prompt file".into(),
        retryable: false,
        details: BTreeMap::from([
            ("path".into(), json!(path)),
            ("reason".into(), json!(error.to_string())),
        ]),
    })?;
    Ok(path.to_string_lossy().into_owned())
}

/// Resolve the absolute path to the `herdr-mission` binary so role prompts can
/// invoke the coordination CLI (`init` / `send` / `reply` / `deliver`) without
/// depending on `PATH`. When the runtime is running as the compiled binary this
/// is `std::env::current_exe()`; standalone fallback keeps the prompt usable.
fn mission_bin() -> String {
    match std::env::current_exe() {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(_) => "herdr-mission".to_string(),
    }
}
