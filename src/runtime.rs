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
    agent_name_token, agent_start_args, git_head, git_root, pane_rename_argv, pane_run_argv,
    pane_split_in_argv, parse_pane_split, parse_tab_create, parse_workspace_create,
    parse_worktree_create, primary_worktree, read_mission_title, read_role_runtime, read_workspace,
    record_role_runtime, role_init_prompt, set_mission_stage, slugify, tab_create_argv,
    upsert_workspace, workspace_create_argv, workspace_label, worktree_create_argv,
    worktree_open_argv, ErrorCategory, KernelError, LaunchConfig, LaunchMode, MissionWorkspace,
    ProcessOutput, ProcessRunner, TabCreated, TabMode, WorkspaceSource,
};

/// How many times to re-attempt `agent start` after a transient
/// `agent_pane_busy` (the freshly split shell pane has not reached its prompt).
const AGENT_START_RETRIES: u32 = 20;
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
/// This is the phase-2 half of `new`. It is idempotent in the DB sense (each
/// role records its pane/agent identity), but deliberately does not guard
/// against double `agent start`; retry after a partial failure re-launches
/// remaining roles and skips none automatically in this first version.
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
    if is_simple {
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
    }
    let token = agent_name_token(mission_id);
    let mut launched = Vec::with_capacity(roles.len());
    let database_str = database.to_string_lossy().into_owned();
    let bin = mission_bin();

    for row in &roles {
        if !row.pane_id.is_empty() {
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
        let agent_name = format!("mission-{token}-{}", row.role);
        progress(&format!("启动 {role} ...", role = row.role));

        let pane_id = if is_simple {
            workspace.root_pane_id.clone()
        } else {
            match options.tab_mode {
                TabMode::Lanes => {
                    if row.role == "pm" {
                        // PM 直接落在 workspace 的 root pane，不额外 split，
                        // 避免 root pane 空置；其余角色在 PM 右边 split。
                        workspace.root_pane_id.clone()
                    } else {
                        let split = match run(
                            runner,
                            herdr,
                            &pane_split_in_argv(
                                &options.direction,
                                &options.cwd,
                                &workspace.root_pane_id,
                            ),
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
                    }
                }
                TabMode::Tabs => {
                    if row.role == "pm" && !workspace.tab_id.is_empty() {
                        // PM 复用 workspace 自带的初始 tab，避免留下一个空 tab；
                        // 其余角色各自新建独立 tab。
                        workspace.root_pane_id.clone()
                    } else {
                        let created = match run(
                            runner,
                            herdr,
                            &tab_create_argv(
                                Some(&workspace.workspace_id),
                                Some(&options.cwd),
                                None,
                            ),
                        ) {
                            Ok(output) if output.exit_code == 0 => output,
                            Ok(output) => {
                                return Err(blocked(
                                    database,
                                    mission_id,
                                    launch_failed("tab create", &output),
                                ));
                            }
                            Err(error) => return Err(blocked(database, mission_id, error)),
                        };
                        match parse_tab_create(&created.stdout) {
                            Ok(tab) => tab.root_pane_id,
                            Err(error) => return Err(blocked(database, mission_id, error)),
                        }
                    }
                }
            }
        };

        let mut argv = match agent_start_args(&row.provider, None) {
            Ok(argv) => argv,
            Err(error) => return Err(blocked(database, mission_id, error)),
        };
        let prompt = match role_init_prompt(
            &title,
            mission_id,
            &row.role,
            &options.cwd,
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

        if let Err(error) = start_agent(runner, herdr, &command) {
            return Err(blocked(database, mission_id, error));
        }

        record_role_runtime(database, mission_id, &row.role, &pane_id, &agent_name)?;
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
    std::thread::sleep(Duration::from_millis(2_000));
    for role in &launched {
        let label = format!("⚑ {title} › {}", role_label(&role.role));
        let _ = run(runner, herdr, &pane_rename_argv(&role.pane_id, &label));
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
    if !row.pane_id.is_empty() {
        progress(&format!("{role} 已启动，跳过"));
        return Ok(None);
    }

    let title = read_mission_title(database, mission_id)?;
    let token = agent_name_token(mission_id);
    let agent_name = format!("mission-{token}-{role}");

    let split = run(
        runner,
        herdr,
        &pane_split_in_argv("right", cwd, anchor_pane_id),
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

    let mut argv = agent_start_args(&row.provider, None)
        .map_err(|error| blocked(database, mission_id, error))?;
    let database_str = database.to_string_lossy().into_owned();
    let bin = mission_bin();
    let prompt = role_init_prompt(
        &title,
        mission_id,
        role,
        cwd,
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

    start_agent(runner, herdr, &command).map_err(|error| blocked(database, mission_id, error))?;
    record_role_runtime(database, mission_id, role, &pane_id, &agent_name)?;

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

/// Build the simple layout's 执行/审查/验证 stage tabs inside the workspace.
///
/// The Worker later lands in the execution tab's root pane, while the review and
/// verification tabs immediately host their configured tool command. Each tab is
/// built at most once (guarded by the persisted tab id) so resume does not open
/// duplicates.
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

    if config.tabs.execution && workspace.execution_tab_id.is_empty() {
        if workspace.tab_id.is_empty() {
            // 防御分支：正常情况下 workspace/worktree create 都会带回一个初始
            // tab，只有缺少初始 tab 时才真正新建执行 tab。
            progress("准备执行 tab…");
            let tab = create_stage_tab(
                runner,
                herdr,
                &workspace.workspace_id,
                &worktree,
                &config.tabs.names.execution,
            )?;
            workspace.execution_tab_id = tab.tab_id;
            workspace.root_pane_id = tab.root_pane_id;
        } else {
            // 复用 workspace 自带的初始 tab 作为执行 tab，避免留下一个空 tab。
            // root_pane_id 已经是初始 tab 的 root pane，Worker 会直接落到这里。
            progress("复用 workspace 初始 tab 作为执行 tab…");
            workspace.execution_tab_id = workspace.tab_id.clone();
        }
    }
    if config.tabs.review && workspace.review_tab_id.is_empty() {
        progress("准备审查 tab…");
        let tab = create_stage_tab(
            runner,
            herdr,
            &workspace.workspace_id,
            &worktree,
            &config.tabs.names.review,
        )?;
        workspace.review_tab_id = tab.tab_id.clone();
        run(
            runner,
            herdr,
            &pane_run_argv(&tab.root_pane_id, &config.tabs.tools.review),
        )
        .map_err(|error| blocked(database, mission_id, error))?;
    }
    if config.tabs.verification && workspace.verification_tab_id.is_empty() {
        progress("准备验证 tab…");
        let tab = create_stage_tab(
            runner,
            herdr,
            &workspace.workspace_id,
            &worktree,
            &config.tabs.names.verification,
        )?;
        workspace.verification_tab_id = tab.tab_id.clone();
        run(
            runner,
            herdr,
            &pane_run_argv(&tab.root_pane_id, &config.tabs.tools.processes),
        )
        .map_err(|error| blocked(database, mission_id, error))?;
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

fn start_agent(
    runner: &dyn ProcessRunner,
    herdr: &str,
    command: &[String],
) -> Result<(), KernelError> {
    for _ in 0..=AGENT_START_RETRIES {
        let output = run(runner, herdr, command)?;
        if output.exit_code == 0 {
            return Ok(());
        }
        if !output.stderr.contains("agent_pane_busy") && !output.stdout.contains("agent_pane_busy")
        {
            return Err(launch_failed("agent start", &output));
        }
        std::thread::sleep(AGENT_START_RETRY_DELAY);
    }
    Err(KernelError {
        category: ErrorCategory::Infrastructure,
        code: "launch_effect_failed".into(),
        message: "agent pane never became an available shell".into(),
        retryable: true,
        details: BTreeMap::from([("operation".into(), json!("agent start"))]),
    })
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
