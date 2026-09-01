//! `herdr-mission` command-line interface.
//!
//! Hand-rolled argument parsing (no external CLI dependency) so the runtime
//! never needs to fetch or compile extra crates. Every command writes a single
//! JSON document to stdout and diagnostics to stderr, with stable exit codes
//! for machine consumers.

use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};

use serde_json::json;

use crate::installer::publish_fresh_skill_copy;
use crate::keybinding::{default_herdr_config_path, install_herdr_keybinding};
use crate::{
    acknowledge_peer_message, agent_list_argv, agent_name_token, agent_rename_argv,
    bootstrap_database, compute_manifest, configure_local_peer, create_mission, delete_mission,
    deliver_peer_messages_with, herdr_bin, is_valid_role_identity, kernel_deliver,
    kernel_dispatch_command, kernel_read_context, kernel_reconcile_with_peer, kernel_reply_command,
    launch_mission, list_missions, make_mission_id, manifest_path_for, new_peer_message_id,
    notify_peer_inboxes, open_writable, pane_rename_argv, parse_agent_list, queue_peer_message,
    read_generation, read_manifest, read_mission_launch_mode, read_mission_status, read_peer_inbox,
    read_role_runtime, receive_peer_envelope, record_role_runtime, request_stop,
    resolve_mission_id, resolve_roles, run_daemon, run_tui, set_mission_launch_mode, source_cwd,
    start_role, upsert_peer, upsert_peer_route, utc_timestamp, verify_binary, workspace_close_argv,
    write_manifest, CreateMissionRequest, ErrorCategory, KernelError, LaunchConfig, LaunchMode,
    LaunchOptions, LaunchedRole, MissionLayout, PeerSendRequest, ProcessRunner, Provider,
    RoleOverride, SystemProcessRunner, SystemSshPeerTransport, WorkspaceSource,
    MAX_PEER_ENVELOPE_BYTES, OWNER_IDENTITY, PROTOCOL_VERSION, SCHEMA_VERSION,
};

/// Exit code for an unknown subcommand (distinct from malformed input).
pub const EXIT_UNKNOWN_COMMAND: i32 = 64;
/// Exit code for malformed arguments.
pub const EXIT_MALFORMED_ARGS: i32 = 65;
/// Exit code for an operation that is not implemented in this phase.
pub const EXIT_UNIMPLEMENTED: i32 = 69;

pub fn run(args: &[String]) -> i32 {
    let mut iter = args.iter();
    let command = iter.next().map(String::as_str);
    match command {
        Some("help") | Some("--help") | Some("-h") => print_usage(),
        Some("doctor") => run_doctor(iter),
        Some("install-keybinding") => run_install_keybinding(iter),
        Some("__install-skill-copy") => run_install_skill_copy(iter),
        Some("new") => run_new(iter),
        Some("status") => run_status(iter),
        Some("set-launch-mode") => run_set_launch_mode(iter),
        Some("list") => run_list(iter),
        Some("init") => run_init(iter),
        Some("send") => run_send(iter),
        Some("peer") => run_peer(iter),
        Some("reply") => run_reply(iter),
        Some("deliver") => run_deliver(iter),
        Some("reconcile") => run_reconcile(iter),
        Some("daemon") => run_daemon_cmd(iter),
        Some("stop") => run_stop(iter),
        Some("resume") => run_resume(iter),
        Some("start-role") => run_start_role(iter),
        Some("join") => run_join(iter),
        Some("delete") => run_delete(iter),
        Some("tui") => run_tui_cmd(iter),
        Some("manifest") => run_manifest(iter),
        Some(other) => {
            eprintln!("unknown command: {other}");
            EXIT_UNKNOWN_COMMAND
        }
        None => print_usage(),
    }
}

fn run_install_skill_copy<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut payload = None;
    let mut target = None;
    let mut target_kind = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let destination = match arg.as_str() {
            "--payload" => &mut payload,
            "--target" => &mut target,
            "--kind" => &mut target_kind,
            other => {
                eprintln!("unexpected installer argument: {other}");
                return EXIT_MALFORMED_ARGS;
            }
        };
        let Some(value) = args.next().filter(|value| !value.is_empty()) else {
            eprintln!("{arg} requires a value");
            return EXIT_MALFORMED_ARGS;
        };
        *destination = Some(value.as_str());
    }

    let (Some(payload), Some(target), Some(target_kind)) = (payload, target, target_kind) else {
        eprintln!("--payload, --target and --kind are required");
        return EXIT_MALFORMED_ARGS;
    };
    if let Err(error) = publish_fresh_skill_copy(Path::new(payload), Path::new(target), target_kind)
    {
        eprintln!("could not publish fresh skill copy: {error}");
        return 1;
    }
    0
}

fn run_install_keybinding<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut config_path = None;
    let mut no_reload = false;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let Some(value) = args.next().filter(|value| !value.starts_with('-')) else {
                    return keybinding_cli_fail(
                        ErrorCategory::Transport,
                        "malformed_args",
                        "--config requires a path",
                        false,
                        EXIT_MALFORMED_ARGS,
                    );
                };
                config_path = Some(PathBuf::from(value));
            }
            "--no-reload" => no_reload = true,
            "--help" | "-h" => {
                return command_help("install-keybinding", "--config <path> [--no-reload]");
            }
            other => {
                return keybinding_cli_fail(
                    ErrorCategory::Transport,
                    "malformed_args",
                    format!("unexpected argument: {other}"),
                    false,
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }
    let Some(config_path) = config_path.or_else(default_herdr_config_path) else {
        return keybinding_cli_fail(
            ErrorCategory::Transport,
            "config_path_unavailable",
            "cannot resolve Herdr config path",
            false,
            EXIT_MALFORMED_ARGS,
        );
    };

    if let Err(error) = install_herdr_keybinding(&config_path) {
        let (category, code, retryable) = match error.kind() {
            std::io::ErrorKind::AlreadyExists => {
                (ErrorCategory::Contract, "keybinding_conflict", false)
            }
            std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput => {
                (ErrorCategory::Contract, "invalid_herdr_config", false)
            }
            std::io::ErrorKind::WouldBlock => {
                (ErrorCategory::Operation, "herdr_config_changed", true)
            }
            _ => (
                ErrorCategory::Infrastructure,
                "keybinding_install_failed",
                false,
            ),
        };
        return keybinding_cli_fail(
            category,
            code,
            format!("failed to install Herdr Mission keybinding: {error}"),
            retryable,
            1,
        );
    }
    if !no_reload {
        reload_herdr_config();
    }
    let prefix = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|content| content.parse::<toml::Value>().ok())
        .and_then(|config| {
            config
                .get("keys")
                .and_then(|keys| keys.get("prefix"))
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "ctrl+b".to_string());
    println!(
        "{}",
        json!({
            "status": "installed",
            "config_path": config_path,
            "key": "prefix+m",
            "prefix": prefix,
        })
    );
    0
}

fn keybinding_cli_fail(
    category: ErrorCategory,
    code: &str,
    message: impl Into<String>,
    retryable: bool,
    exit_code: i32,
) -> i32 {
    let message = message.into();
    eprintln!("failed: {message}");
    println!(
        "{}",
        json!({
            "status": "error",
            "error": KernelError {
                category,
                code: code.to_string(),
                message,
                retryable,
                details: BTreeMap::new(),
            },
        })
    );
    exit_code
}

fn reload_herdr_config() {
    let herdr = std::env::var_os("HERDR_BIN_PATH")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "herdr".into());
    let result = std::process::Command::new(herdr)
        .args(["server", "reload-config"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match result {
        Ok(status) if status.success() => {}
        Ok(_) => eprintln!("Herdr config updated; it will apply when the server next starts"),
        Err(error) => eprintln!(
            "Herdr config updated; reload was unavailable ({error}); it will apply when the server next starts"
        ),
    }
}

fn print_usage() -> i32 {
    println!(
        "usage: herdr-mission <command> [options]\n\
         \n\
         协调 Herdr Team Mission（Rust 版）。持久化事实来源是 SQLite，不是终端文本。\n\
         \n\
         commands:\n\
           new         创建 Mission\n\
           list        列出所有 Mission\n\
           status      查看单个 Mission 状态\n\
           set-launch-mode  切换 Mission 的 Auto/Manual 模式\n\
           init        读取角色待办与收件箱\n\
           send        派发 Assignment 给目标角色\n\
           peer        跨 Mission / 跨设备 PM relay\n\
           reply       回执 Assignment\n\
           deliver     投递 outbox\n\
           reconcile   协调 Agent 实时状态并投递 outbox\n\
           start-role  按需启动一个角色\n\
           join        手动把当前 agent 加入为某角色\n\
           resume      恢复未启动的角色\n\
           delete      删除 Mission\n\
           tui         打开控制台\n\
           doctor      自检\n\
           install-keybinding  安装 Mission 看板快捷键\n\
           daemon      常驻投递 daemon\n\
           stop        停止 daemon\n\
           manifest    校验二进制\n\
         \n\
         用 `herdr-mission <command> --help` 查看单个命令的用法。"
    );
    0
}

fn command_help(name: &str, usage: &str) -> i32 {
    println!("usage: herdr-mission {name} {usage}");
    0
}

fn run_new<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut title: Option<String> = None;
    let mut database: Option<PathBuf> = None;
    let mut layout = MissionLayout::Team;
    let mut provider = Provider::Codex;
    let mut workspace_source = WorkspaceSource::Current;
    let mut no_start = false;
    let mut json_mode = false;
    let mut request_id: Option<String> = None;
    let mut prompts_dir: Option<PathBuf> = None;
    let mut legacy_autonomy: Option<LaunchMode> = None;
    let mut launch_mode: Option<LaunchMode> = None;
    let mut role_overrides: Vec<RoleOverride> = Vec::new();

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        match key {
            "--json" => {
                json_mode = true;
                continue;
            }
            "--no-start" => {
                no_start = true;
                continue;
            }
            _ => {}
        }
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--title" => title = value,
            "--database" => database = value.map(PathBuf::from),
            "--layout" => match value {
                Some(spec) => match MissionLayout::parse(&spec) {
                    Some(parsed) => layout = parsed,
                    None => return cli_fail(json_mode, unknown_layout(&spec), EXIT_UNIMPLEMENTED),
                },
                None => layout = MissionLayout::Team,
            },
            "--profile" => match value {
                Some(spec) => match Provider::parse(&spec) {
                    Some(parsed) => provider = parsed,
                    None => {
                        return cli_fail(json_mode, unknown_provider(&spec), EXIT_MALFORMED_ARGS);
                    }
                },
                None => provider = Provider::Codex,
            },
            "--workspace-source" => match value {
                Some(spec) => match WorkspaceSource::parse(&spec) {
                    Some(parsed) => workspace_source = parsed,
                    None => {
                        return cli_fail(
                            json_mode,
                            unknown_workspace_source(&spec),
                            EXIT_MALFORMED_ARGS,
                        );
                    }
                },
                None => workspace_source = WorkspaceSource::Current,
            },
            "--prompts-dir" => prompts_dir = value.map(PathBuf::from),
            "--autonomy" => match value.as_deref().and_then(LaunchMode::parse) {
                Some(mode) => legacy_autonomy = Some(mode),
                None => {
                    return cli_fail(
                        json_mode,
                        malformed("--autonomy only accepts auto or manual"),
                        EXIT_MALFORMED_ARGS,
                    );
                }
            },
            "--launch-mode" => match value.as_deref().and_then(LaunchMode::parse) {
                Some(mode) => launch_mode = Some(mode),
                None => {
                    return cli_fail(
                        json_mode,
                        malformed("--launch-mode only accepts auto or manual"),
                        EXIT_MALFORMED_ARGS,
                    );
                }
            },
            "--role" => match value {
                Some(spec) => match parse_role_spec(&spec) {
                    Ok(role) => role_overrides.push(role),
                    Err(reason) => {
                        return cli_fail(
                            json_mode,
                            malformed(format!("invalid --role: {reason}")),
                            EXIT_MALFORMED_ARGS,
                        );
                    }
                },
                None => {
                    return cli_fail(
                        json_mode,
                        malformed("--role requires a value"),
                        EXIT_MALFORMED_ARGS,
                    );
                }
            },
            "--request-id" => request_id = value,
            "--repo" | "--mode" | "--branch" => {}
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let title = match title {
        Some(title) if !title.trim().is_empty() => title.trim().to_string(),
        _ => {
            return cli_fail(
                json_mode,
                malformed("--title is required"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };
    let roles = match resolve_roles(provider, layout, &role_overrides) {
        Ok(roles) => roles,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };

    let database = match database {
        Some(path) => path,
        None => match default_database() {
            Some(path) => path,
            None => {
                return cli_fail(
                    json_mode,
                    malformed("cannot resolve a state directory or HOME for default database path"),
                    EXIT_MALFORMED_ARGS,
                );
            }
        },
    };

    let config = LaunchConfig::load();
    let launch_mode = resolve_launch_mode(launch_mode.or(legacy_autonomy), &config);
    let request = CreateMissionRequest {
        mission_id: make_mission_id(&title),
        brief: title.clone(),
        template: "general".into(),
        agent_profile_id: provider.profile_id(),
        agent_profile_version: provider.profile_version(),
        launch_mode,
        roles,
    };
    match create_mission(&database, &request) {
        Ok(outcome) => {
            let mission_id = outcome.mission_id;
            let created = outcome.created;
            let mut state = "preparing".to_string();
            let mut launched: Vec<LaunchedRole> = Vec::new();

            if !no_start {
                let cwd = source_cwd();
                let options = LaunchOptions {
                    direction: "right".into(),
                    cwd,
                    prompts_dir,
                    tab_mode: config.launch.tab_mode,
                    workspace_source,
                    worktree_path: None,
                };
                let runner = SystemProcessRunner;
                let mut progress = |message: &str| eprintln!("  {message}");
                match launch_mission(
                    &database,
                    &mission_id,
                    &options,
                    &runner,
                    &herdr_bin(),
                    &mut progress,
                ) {
                    Ok(launch) => {
                        state = launch.stage;
                        launched = launch.roles;
                    }
                    Err(error) => {
                        if json_mode {
                            println!(
                                "{}",
                                serde_json::to_string(&json!({
                                    "status": "error",
                                    "mission_id": mission_id,
                                    "state": "blocked",
                                    "error": error,
                                }))
                                .expect("new outcome must serialize")
                            );
                        } else {
                            eprintln!(
                                "mission {mission_id} launch failed: {} ({})",
                                error.message, error.code
                            );
                        }
                        return 1;
                    }
                }
            }

            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "mission_id": mission_id,
                        "state": state,
                        "no_start": no_start,
                        "created": created,
                        "database": database,
                        "request_id": request_id,
                        "provider": provider.agent_kind(),
                        "launch_mode": launch_mode.as_str(),
                        "roles": launched.iter().map(|role| json!({
                            "role": role.role.clone(),
                            "agent_name": role.agent_name.clone(),
                            "pane_id": role.pane_id.clone(),
                        })).collect::<Vec<_>>(),
                    }))
                    .expect("new outcome must serialize")
                );
            } else {
                println!("Mission《{title}》 (id={mission_id})");
                println!("  provider: {}", provider.agent_kind());
                println!("  launch mode: {}", launch_mode.as_str());
                println!("  database: {}", database.display());
                for role in &launched {
                    println!(
                        "  ✓ {:<9} → {:<28} {}",
                        role.role, role.agent_name, role.pane_id
                    );
                }
                println!("状态: {state}");
            }
            0
        }
        Err(error) => {
            crate::log_error(
                &database,
                &format!("mission={} create failed", request.mission_id),
                &error,
            );
            cli_fail(json_mode, error, 1)
        }
    }
}

fn run_list<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if key == "--json" {
            json_mode = true;
            continue;
        }
        if key == "--help" || key == "-h" {
            return command_help("list", "[--database <path>] [--json]");
        }
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--database" => database = value.map(PathBuf::from),
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    if let Err(error) = bootstrap_database(&database) {
        return cli_fail(json_mode, error, 1);
    }

    match list_missions(&database) {
        Ok(missions) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "missions": missions.iter().map(|m| json!({
                            "mission_id": m.mission_id,
                            "brief": m.brief,
                            "stage": m.stage,
                            "created_at": m.created_at,
                        })).collect::<Vec<_>>(),
                    }))
                    .expect("list outcome must serialize")
                );
            } else {
                if missions.is_empty() {
                    println!("(no missions)");
                }
                for mission in &missions {
                    println!(
                        "{}  [{}]  {}",
                        mission.stage, mission.mission_id, mission.brief
                    );
                }
            }
            0
        }
        Err(error) => cli_fail(json_mode, error, 1),
    }
}

fn run_tui_cmd<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut database: Option<PathBuf> = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--database" => database = value.map(PathBuf::from),
            other => {
                eprintln!("failed: unexpected argument: {other}");
                return EXIT_MALFORMED_ARGS;
            }
        }
    }

    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            eprintln!("failed: cannot resolve a state directory or HOME for default database path");
            return EXIT_MALFORMED_ARGS;
        }
    };

    match run_tui(&database) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("failed: {error}");
            1
        }
    }
}

fn run_status<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut mission_id: Option<String> = None;
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if key == "--json" {
            json_mode = true;
            continue;
        }
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--mission-id" | "--mission" => mission_id = value,
            "--database" => database = value.map(PathBuf::from),
            "--help" | "-h" => {
                return command_help("status", "[--mission-id <id>] [--database <path>] [--json]");
            }
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let mission_id = match mission_id {
        Some(id) if !id.trim().is_empty() => id,
        _ => {
            return cli_fail(
                json_mode,
                malformed("--mission-id is required"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };
    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    match read_mission_status(&database, &mission_id) {
        Ok(status) => {
            let payload = json!({
                "status": "ok",
                "mission_id": status.mission_id,
                "stage": status.stage,
                "launch_mode": status.launch_mode.as_str(),
                "roles": status.roles,
                "pending_assignments": status.pending_assignments,
                "generation": status.generation,
            });
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&payload).expect("status outcome must serialize")
                );
            } else {
                println!(
                    "mission {} stage={} pending={} generation={}",
                    status.mission_id, status.stage, status.pending_assignments, status.generation
                );
                println!("  launch mode: {}", status.launch_mode.as_str());
                for (role, health) in &status.roles {
                    println!("  {role}: {health}");
                }
            }
            0
        }
        Err(error) => cli_fail(json_mode, error, 1),
    }
}

fn run_set_launch_mode<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut mission_id: Option<String> = None;
    let mut database: Option<PathBuf> = None;
    let mut launch_mode: Option<LaunchMode> = None;
    let mut json_mode = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if key == "--json" {
            json_mode = true;
            continue;
        }
        let value = inline.or_else(|| args.next().cloned());
        match key {
            "--mission-id" | "--mission" => mission_id = value,
            "--database" => database = value.map(PathBuf::from),
            "--launch-mode" => match value.as_deref().and_then(LaunchMode::parse) {
                Some(mode) => launch_mode = Some(mode),
                None => {
                    return cli_fail(
                        json_mode,
                        malformed("--launch-mode only accepts auto or manual"),
                        EXIT_MALFORMED_ARGS,
                    );
                }
            },
            "--help" | "-h" => {
                return command_help(
                    "set-launch-mode",
                    "--mission-id <id> --launch-mode <auto|manual> [--database <path>] [--json]",
                );
            }
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let mission_id = match required_string(mission_id, "--mission-id") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let launch_mode = match launch_mode {
        Some(value) => value,
        None => {
            return cli_fail(
                json_mode,
                malformed("--launch-mode is required"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };
    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    match set_mission_launch_mode(&database, &mission_id, launch_mode) {
        Ok(()) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "mission_id": mission_id,
                        "launch_mode": launch_mode.as_str(),
                    }))
                    .expect("set launch mode outcome must serialize")
                );
            } else {
                println!("{mission_id} launch mode -> {}", launch_mode.as_str());
            }
            0
        }
        Err(error) => cli_fail(json_mode, error, 1),
    }
}

fn run_init<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut mission_id: Option<String> = None;
    let mut role: Option<String> = None;
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if key == "--json" {
            json_mode = true;
            continue;
        }
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--mission-id" | "--mission" => mission_id = value,
            "--role" => role = value,
            "--database" => database = value.map(PathBuf::from),
            "--help" | "-h" => {
                return command_help(
                    "init",
                    "--mission-id <id> --role <role> [--database <path>] [--json]",
                );
            }
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let mission_id = match mission_id {
        Some(id) if !id.trim().is_empty() => id,
        _ => {
            return cli_fail(
                json_mode,
                malformed("--mission-id is required"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };
    let role = match role {
        Some(role) if !role.trim().is_empty() => role,
        _ => {
            return cli_fail(
                json_mode,
                malformed("--role is required"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };
    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    match kernel_read_context(&database, &mission_id, &role) {
        Ok(context) => {
            let launch_mode = match read_mission_launch_mode(&database, &mission_id) {
                Ok(mode) => mode,
                Err(error) => return cli_fail(json_mode, error, 1),
            };
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "mission_id": context.mission_id,
                        "title": context.title,
                        "role": context.role,
                        "health": context.health,
                        "generation": context.generation,
                        "launch_mode": launch_mode.as_str(),
                        "pending_assignments": context.pending_assignments.iter().map(|a| json!({
                            "id": a.id.clone(),
                            "source": a.source.clone(),
                            "kind": a.kind.clone(),
                            "summary": a.summary.clone(),
                            "state": a.state.clone(),
                        })).collect::<Vec<_>>(),
                        "inbox": context.inbox.iter().map(|m| json!({
                            "id": m.id.clone(),
                            "assignment_id": m.assignment_id.clone(),
                            "source": m.source.clone(),
                            "kind": m.kind.clone(),
                            "body": m.body.clone(),
                        })).collect::<Vec<_>>(),
                        "peer_inbox": context.peer_inbox.iter().map(|m| json!({
                            "message_id": m.message_id.clone(),
                            "source_peer_id": m.source_peer_id.clone(),
                            "target_peer_id": m.target_peer_id.clone(),
                            "source_mission_id": m.source_mission_id.clone(),
                            "target_mission_id": m.target_mission_id.clone(),
                            "source_pm_generation": m.source_pm_generation.clone(),
                            "kind": m.kind.clone(),
                            "body": m.body.clone(),
                            "in_reply_to": m.in_reply_to.clone(),
                            "payload_sha256": m.payload_sha256.clone(),
                            "received_at": m.received_at.clone(),
                        })).collect::<Vec<_>>(),
                    }))
                    .expect("init outcome must serialize")
                );
            } else {
                println!(
                    "Mission《{}》 role={} health={} pending={}",
                    context.title,
                    context.role,
                    context.health,
                    context.pending_assignments.len()
                );
                println!("launch mode: {}", launch_mode.as_str());
                for assignment in &context.pending_assignments {
                    println!(
                        "  - {} {} ({})",
                        assignment.state, assignment.id, assignment.kind
                    );
                }
                for message in &context.peer_inbox {
                    println!(
                        "  - peer {} from {}:{} to {}:{} ({})",
                        message.message_id,
                        message.source_peer_id,
                        message.source_mission_id,
                        message.target_peer_id,
                        message.target_mission_id,
                        message.kind
                    );
                }
            }
            0
        }
        Err(error) => cli_fail(json_mode, error, 1),
    }
}

fn run_send<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut mission_id: Option<String> = None;
    let mut role: Option<String> = None;
    let mut target: Option<String> = None;
    let mut target_mission: Option<String> = None;
    let mut peer_id: Option<String> = None;
    let mut kind = "task".to_string();
    let mut kind_explicit = false;
    let mut body: Option<String> = None;
    let mut message_id: Option<String> = None;
    let mut in_reply_to: Option<String> = None;
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if key == "--json" {
            json_mode = true;
            continue;
        }
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--mission-id" | "--mission" => mission_id = value,
            "--role" => role = value,
            "--target" => target = value,
            "--target-mission" => target_mission = value,
            "--peer" => peer_id = value,
            "--kind" => {
                kind_explicit = true;
                kind = value.unwrap_or_else(|| "task".into());
            }
            "--body" => body = value,
            "--message-id" => message_id = value,
            "--in-reply-to" => in_reply_to = value,
            "--database" => database = value.map(PathBuf::from),
            "--help" | "-h" => {
                return command_help(
                    "send",
                    "--mission-id <id> --role <source> --target <target> --kind <task|fix|context> --body '<text>' [--target-mission <id> [--peer <id>] --kind <delegate|context|result|blocked>] [--database <path>] [--json]",
                );
            }
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let mission_id = match mission_id {
        Some(id) if !id.trim().is_empty() => id,
        _ => {
            return cli_fail(
                json_mode,
                malformed("--mission-id is required"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };
    let role = match role {
        Some(role) if !role.trim().is_empty() => role,
        _ => {
            return cli_fail(
                json_mode,
                malformed("--role is required"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };
    let target = match target {
        Some(target) if !target.trim().is_empty() => target,
        _ => {
            return cli_fail(
                json_mode,
                malformed("--target is required"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };
    let body = match body {
        Some(body) if !body.trim().is_empty() => body,
        _ => {
            return cli_fail(
                json_mode,
                malformed("--body is required"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };
    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    if target == "pm" {
        if let Some(target_mission) = target_mission {
            let mut values = BTreeMap::from([
                ("mission-id".into(), mission_id),
                ("role".into(), role),
                ("target-mission".into(), target_mission),
                (
                    "kind".into(),
                    if kind_explicit {
                        kind
                    } else {
                        "context".into()
                    },
                ),
                ("body".into(), body),
                ("database".into(), database.to_string_lossy().into_owned()),
            ]);
            if let Some(peer_id) = peer_id {
                values.insert("peer".into(), peer_id);
            }
            if let Some(message_id) = message_id {
                values.insert("message-id".into(), message_id);
            }
            if let Some(in_reply_to) = in_reply_to {
                values.insert("in-reply-to".into(), in_reply_to);
            }
            return run_peer_send(
                &database,
                &PeerCliArgs {
                    values,
                    json: json_mode,
                },
            );
        }
    }
    if target_mission.is_some()
        || peer_id.is_some()
        || message_id.is_some()
        || in_reply_to.is_some()
    {
        return cli_fail(
            json_mode,
            malformed("peer routing options require --target=pm and --target-mission"),
            EXIT_MALFORMED_ARGS,
        );
    }

    match kernel_dispatch_command(&database, &mission_id, &role, &target, &kind, &body) {
        Ok(outcome) => {
            deliver_now(&database);
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "mission_id": mission_id,
                        "target": target,
                        "kind": kind,
                        "assignment_id": outcome.assignment_id,
                        "message_id": outcome.message_id,
                    }))
                    .expect("send outcome must serialize")
                );
            } else {
                match &outcome.assignment_id {
                    Some(id) => println!("sent task to {target}: assignment {id}"),
                    None => println!("sent {kind} notice to {target}"),
                }
            }
            0
        }
        Err(error) => cli_fail(json_mode, error, 1),
    }
}

fn run_peer<'a>(mut args: impl Iterator<Item = &'a String>) -> i32 {
    let Some(command) = args.next().map(String::as_str) else {
        return command_help(
            "peer",
            "<identity|add|link|send|receive|inbox|ack|deliver> [options]",
        );
    };
    let parsed = match parse_peer_cli_args(args) {
        Ok(parsed) => parsed,
        Err(error) => return cli_fail(false, error, EXIT_MALFORMED_ARGS),
    };
    let database = match parsed
        .values
        .get("database")
        .map(PathBuf::from)
        .or_else(default_database)
    {
        Some(path) => path,
        None => {
            return cli_fail(
                parsed.json,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };
    match command {
        "identity" => run_peer_identity(&database, &parsed),
        "add" => run_peer_add(&database, &parsed),
        "link" => run_peer_link(&database, &parsed),
        "send" => run_peer_send(&database, &parsed),
        "receive" => run_peer_receive(&database, &parsed),
        "inbox" => run_peer_inbox(&database, &parsed),
        "ack" => run_peer_ack(&database, &parsed),
        "deliver" => run_peer_deliver(&database, &parsed),
        "--help" | "-h" | "help" => command_help(
            "peer",
            "<identity|add|link|send|receive|inbox|ack|deliver> [options]",
        ),
        other => cli_fail(
            parsed.json,
            malformed(format!("unknown peer command: {other}")),
            EXIT_MALFORMED_ARGS,
        ),
    }
}

#[derive(Debug, Default)]
struct PeerCliArgs {
    values: BTreeMap<String, String>,
    json: bool,
}

fn parse_peer_cli_args<'a>(
    args: impl Iterator<Item = &'a String>,
) -> Result<PeerCliArgs, KernelError> {
    let mut parsed = PeerCliArgs::default();
    let mut args = args.peekable();
    while let Some(argument) = args.next() {
        if argument == "--json" {
            parsed.json = true;
            continue;
        }
        let (raw_key, inline) = match argument.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (argument.as_str(), None),
        };
        let Some(key) = raw_key.strip_prefix("--") else {
            return Err(malformed(format!("unexpected argument: {argument}")));
        };
        if key.is_empty() || key == "json" {
            return Err(malformed(format!("unexpected argument: {argument}")));
        }
        let value = match inline {
            Some(value) => value,
            None => args
                .next()
                .filter(|value| !value.starts_with("--"))
                .cloned()
                .ok_or_else(|| malformed(format!("--{key} requires a value")))?,
        };
        if parsed.values.insert(key.to_string(), value).is_some() {
            return Err(malformed(format!("--{key} was provided more than once")));
        }
    }
    Ok(parsed)
}

fn peer_required<'a>(parsed: &'a PeerCliArgs, key: &str) -> Result<&'a str, KernelError> {
    parsed
        .values
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| malformed(format!("--{key} is required")))
}

fn reject_unknown_peer_options(parsed: &PeerCliArgs, allowed: &[&str]) -> Result<(), KernelError> {
    if let Some(key) = parsed
        .values
        .keys()
        .find(|key| key.as_str() != "database" && !allowed.contains(&key.as_str()))
    {
        return Err(malformed(format!("unexpected argument: --{key}")));
    }
    Ok(())
}

fn run_peer_identity(database: &Path, parsed: &PeerCliArgs) -> i32 {
    if let Err(error) = reject_unknown_peer_options(parsed, &["local-peer"]) {
        return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS);
    }
    let local_peer = match peer_required(parsed, "local-peer") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    match configure_local_peer(database, local_peer) {
        Ok(()) => peer_cli_ok(parsed.json, json!({"local_peer_id": local_peer})),
        Err(error) => cli_fail(parsed.json, error, 1),
    }
}

fn run_peer_add(database: &Path, parsed: &PeerCliArgs) -> i32 {
    if let Err(error) = reject_unknown_peer_options(parsed, &["peer", "ssh"]) {
        return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS);
    }
    let peer_id = match peer_required(parsed, "peer") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    let ssh = match peer_required(parsed, "ssh") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    match upsert_peer(database, peer_id, ssh) {
        Ok(()) => peer_cli_ok(
            parsed.json,
            json!({"peer_id": peer_id, "ssh_destination": ssh}),
        ),
        Err(error) => cli_fail(parsed.json, error, 1),
    }
}

fn run_peer_link(database: &Path, parsed: &PeerCliArgs) -> i32 {
    if let Err(error) = reject_unknown_peer_options(
        parsed,
        &["peer", "local-mission", "remote-mission", "direction"],
    ) {
        return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS);
    }
    let peer_id = match peer_required(parsed, "peer") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    let local_mission = match peer_required(parsed, "local-mission") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    let remote_mission = match peer_required(parsed, "remote-mission") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    let direction = parsed
        .values
        .get("direction")
        .map(String::as_str)
        .unwrap_or("bidirectional");
    match upsert_peer_route(database, peer_id, local_mission, remote_mission, direction) {
        Ok(()) => peer_cli_ok(
            parsed.json,
            json!({
                "peer_id": peer_id,
                "local_mission_id": local_mission,
                "remote_mission_id": remote_mission,
                "direction": direction,
            }),
        ),
        Err(error) => cli_fail(parsed.json, error, 1),
    }
}

fn run_peer_send(database: &Path, parsed: &PeerCliArgs) -> i32 {
    if let Err(error) = reject_unknown_peer_options(
        parsed,
        &[
            "mission-id",
            "role",
            "target-mission",
            "peer",
            "kind",
            "body",
            "message-id",
            "in-reply-to",
        ],
    ) {
        return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS);
    }
    let source_mission = match peer_required(parsed, "mission-id") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    let role = parsed
        .values
        .get("role")
        .map(String::as_str)
        .unwrap_or("pm");
    let target_mission = match peer_required(parsed, "target-mission") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    let kind = parsed
        .values
        .get("kind")
        .map(String::as_str)
        .unwrap_or("context");
    let body = match peer_required(parsed, "body") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    let message_id = parsed
        .values
        .get("message-id")
        .cloned()
        .unwrap_or_else(new_peer_message_id);
    let request = PeerSendRequest {
        message_id,
        source_mission_id: source_mission.into(),
        target_mission_id: target_mission.into(),
        source_role: role.into(),
        peer_id: parsed.values.get("peer").cloned(),
        kind: kind.into(),
        body: body.into(),
        in_reply_to: parsed.values.get("in-reply-to").cloned(),
    };
    match queue_peer_message(database, &request) {
        Ok(outcome) => {
            let runner = SystemProcessRunner;
            let delivery = if request.peer_id.is_some() {
                deliver_peer_messages_with(database, &SystemSshPeerTransport)
                    .map(|report| json!(report))
            } else {
                notify_peer_inboxes(database, &runner, &herdr_bin()).map(|report| json!(report))
            };
            let delivery = match delivery {
                Ok(report) => report,
                Err(error) => json!({"status": "queued", "error": error}),
            };
            peer_cli_ok(
                parsed.json,
                json!({
                    "message_id": outcome.message_id,
                    "payload_sha256": outcome.payload_sha256,
                    "state": outcome.state,
                    "duplicate": outcome.duplicate,
                    "delivery": delivery,
                }),
            )
        }
        Err(error) => cli_fail(parsed.json, error, 1),
    }
}

fn run_peer_receive(database: &Path, parsed: &PeerCliArgs) -> i32 {
    if let Err(error) = reject_unknown_peer_options(parsed, &["peer"]) {
        return cli_fail(true, error, EXIT_MALFORMED_ARGS);
    }
    let peer_id = match peer_required(parsed, "peer") {
        Ok(value) => value,
        Err(error) => return cli_fail(true, error, EXIT_MALFORMED_ARGS),
    };
    let mut envelope = Vec::new();
    let limit = u64::try_from(MAX_PEER_ENVELOPE_BYTES + 1).unwrap_or(u64::MAX);
    if let Err(error) = std::io::stdin().take(limit).read_to_end(&mut envelope) {
        return cli_fail(
            true,
            KernelError {
                category: ErrorCategory::Transport,
                code: "peer_stdin_read_failed".into(),
                message: "failed to read peer envelope from stdin".into(),
                retryable: false,
                details: BTreeMap::from([("reason".into(), json!(error.to_string()))]),
            },
            1,
        );
    }
    match receive_peer_envelope(database, peer_id, &envelope) {
        Ok(receipt) => {
            println!(
                "{}",
                serde_json::to_string(&receipt).expect("peer receipt must serialize")
            );
            let runner = SystemProcessRunner;
            if let Err(error) = notify_peer_inboxes(database, &runner, &herdr_bin()) {
                crate::log_error(database, "peer_receive_notify", &error);
            }
            0
        }
        Err(error) => cli_fail(true, error, 1),
    }
}

fn run_peer_inbox(database: &Path, parsed: &PeerCliArgs) -> i32 {
    if let Err(error) = reject_unknown_peer_options(parsed, &["mission-id", "role"]) {
        return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS);
    }
    let mission_id = match peer_required(parsed, "mission-id") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    let role = parsed
        .values
        .get("role")
        .map(String::as_str)
        .unwrap_or("pm");
    match read_peer_inbox(database, mission_id, role) {
        Ok(messages) => peer_cli_ok(parsed.json, json!({"peer_inbox": messages})),
        Err(error) => cli_fail(parsed.json, error, 1),
    }
}

fn run_peer_ack(database: &Path, parsed: &PeerCliArgs) -> i32 {
    if let Err(error) = reject_unknown_peer_options(parsed, &["mission-id", "role", "message-id"]) {
        return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS);
    }
    let mission_id = match peer_required(parsed, "mission-id") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    let role = parsed
        .values
        .get("role")
        .map(String::as_str)
        .unwrap_or("pm");
    let message_id = match peer_required(parsed, "message-id") {
        Ok(value) => value,
        Err(error) => return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS),
    };
    match acknowledge_peer_message(database, mission_id, role, message_id) {
        Ok(changed) => peer_cli_ok(
            parsed.json,
            json!({"message_id": message_id, "acknowledged": true, "changed": changed}),
        ),
        Err(error) => cli_fail(parsed.json, error, 1),
    }
}

fn run_peer_deliver(database: &Path, parsed: &PeerCliArgs) -> i32 {
    if let Err(error) = reject_unknown_peer_options(parsed, &[]) {
        return cli_fail(parsed.json, error, EXIT_MALFORMED_ARGS);
    }
    let runner = SystemProcessRunner;
    let delivery = deliver_peer_messages_with(database, &SystemSshPeerTransport);
    let notification = notify_peer_inboxes(database, &runner, &herdr_bin());
    match (delivery, notification) {
        (Ok(delivery), Ok(notification)) => peer_cli_ok(
            parsed.json,
            json!({
                "sent": delivery.sent,
                "retried": delivery.retried,
                "notified": notification.notified,
                "notify_failed": notification.notify_failed,
            }),
        ),
        (Err(error), _) | (_, Err(error)) => cli_fail(parsed.json, error, 1),
    }
}

fn peer_cli_ok(json_mode: bool, fields: serde_json::Value) -> i32 {
    if json_mode {
        let mut value = fields.as_object().cloned().unwrap_or_default();
        value.insert("status".into(), json!("ok"));
        println!(
            "{}",
            serde_json::to_string(&value).expect("peer CLI outcome must serialize")
        );
    } else {
        println!("{fields}");
    }
    0
}

fn run_reply<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut mission_id: Option<String> = None;
    let mut role: Option<String> = None;
    let mut assignment: Option<String> = None;
    let mut kind: Option<String> = None;
    let mut body: Option<String> = None;
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if key == "--json" {
            json_mode = true;
            continue;
        }
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--mission-id" | "--mission" => mission_id = value,
            "--role" => role = value,
            "--assignment" => assignment = value,
            "--kind" => kind = value,
            "--body" => body = value,
            "--database" => database = value.map(PathBuf::from),
            "--help" | "-h" => {
                return command_help(
                    "reply",
                    "--mission-id <id> --role <role> --assignment <id> --kind <kind> --body '<text>' [--database <path>] [--json]",
                );
            }
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let mission_id = match required_string(mission_id, "--mission-id") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let role = match required_string(role, "--role") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let assignment = match required_string(assignment, "--assignment") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let kind = match required_string(kind, "--kind") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let body = match required_string(body, "--body") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    match kernel_reply_command(&database, &mission_id, &role, &assignment, &kind, &body) {
        Ok(outcome) => {
            deliver_now(&database);
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "assignment_id": outcome.assignment_id,
                        "assignment_state": outcome.assignment_state,
                        "message_id": outcome.message_id,
                    }))
                    .expect("reply outcome must serialize")
                );
            } else {
                match &outcome.assignment_state {
                    Some(state) => println!("reply {assignment} recorded (state={state})"),
                    None => println!("reply {assignment} recorded"),
                }
            }
            0
        }
        Err(error) => cli_fail(json_mode, error, 1),
    }
}

/// Deliver the outbox once immediately after a queue write, so a message is
/// pushed right away instead of waiting for a lifecycle event. Best-effort: a
/// delivery failure never masks the send/reply outcome, and the event-driven
/// reconciler still acts as a fallback.
fn deliver_now(database: &Path) {
    let runner = SystemProcessRunner;
    if let Err(error) = kernel_deliver(database, &runner, &herdr_bin()) {
        crate::log_error(database, "deliver_now", &error);
    }
}

fn run_deliver<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json_mode = true,
            "--database" => {
                return cli_fail(
                    json_mode,
                    malformed("--database requires a value"),
                    EXIT_MALFORMED_ARGS,
                );
            }
            value if value.starts_with("--database=") => {
                database = Some(PathBuf::from(&value["--database=".len()..]));
            }
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    let runner = SystemProcessRunner;
    match kernel_deliver(&database, &runner, &herdr_bin()) {
        Ok(report) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "delivered": report.delivered,
                        "failed": report.failed,
                    }))
                    .expect("deliver outcome must serialize")
                );
            } else {
                println!("delivered={} failed={}", report.delivered, report.failed);
            }
            0
        }
        Err(error) => cli_fail(json_mode, error, 1),
    }
}

fn run_reconcile<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json_mode = true,
            "--help" | "-h" => {
                return command_help("reconcile", "[--database <path>] [--json]");
            }
            "--database" => {
                return cli_fail(
                    json_mode,
                    malformed("--database requires a value"),
                    EXIT_MALFORMED_ARGS,
                );
            }
            value if value.starts_with("--database=") => {
                database = Some(PathBuf::from(&value["--database=".len()..]));
            }
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    let runner = SystemProcessRunner;
    let report = kernel_reconcile_with_peer(&database, &runner, &herdr_bin());
    let health_ok = report.health.is_ok();
    let delivery_ok = report.delivery.is_ok();
    let peer_ok = report.peer.is_ok();
    let status = if health_ok && delivery_ok && peer_ok {
        "ok"
    } else if !health_ok && !delivery_ok && !peer_ok {
        "error"
    } else {
        "partial"
    };
    let health = match report.health {
        Ok(health) => json!({
            "status": "ok",
            "matched": health.matched,
            "missing": health.missing,
            "updated": health.updated,
        }),
        Err(error) => json!({"status": "error", "error": error}),
    };
    let delivery = match report.delivery {
        Ok(delivery) => json!({
            "status": "ok",
            "delivered": delivery.delivered,
            "failed": delivery.failed,
        }),
        Err(error) => json!({"status": "error", "error": error}),
    };
    let peer = match report.peer {
        Ok(peer) => json!({
            "status": "ok",
            "sent": peer.sent,
            "retried": peer.retried,
            "notified": peer.notified,
            "notify_failed": peer.notify_failed,
        }),
        Err(error) => json!({"status": "error", "error": error}),
    };

    if json_mode {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "status": status,
                "health": health,
                "delivery": delivery,
                "peer": peer,
            }))
            .expect("reconcile outcome must serialize")
        );
    } else {
        println!("status={status} health={health} delivery={delivery} peer={peer}");
    }

    if health_ok && delivery_ok && peer_ok {
        0
    } else {
        1
    }
}

fn run_daemon_cmd<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut database: Option<PathBuf> = None;
    let mut interval_ms: u64 = 2_000;
    for arg in args {
        match arg.as_str() {
            "--database" => {
                return cli_fail(
                    false,
                    malformed("--database requires a value"),
                    EXIT_MALFORMED_ARGS,
                );
            }
            value if value.starts_with("--database=") => {
                database = Some(PathBuf::from(&value["--database=".len()..]));
            }
            value if value.starts_with("--interval-ms=") => {
                interval_ms = match value["--interval-ms=".len()..].parse::<u64>() {
                    Ok(ms) if ms > 0 => ms,
                    _ => {
                        return cli_fail(
                            false,
                            malformed("--interval-ms must be a positive integer"),
                            EXIT_MALFORMED_ARGS,
                        );
                    }
                };
            }
            other => {
                return cli_fail(
                    false,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                false,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    let runner = SystemProcessRunner;
    match run_daemon(
        &database,
        std::time::Duration::from_millis(interval_ms),
        &runner,
        &herdr_bin(),
    ) {
        Ok(()) => 0,
        Err(error) => cli_fail(false, error, 1),
    }
}

fn run_stop<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json_mode = true,
            "--database" => {
                return cli_fail(
                    json_mode,
                    malformed("--database requires a value"),
                    EXIT_MALFORMED_ARGS,
                );
            }
            value if value.starts_with("--database=") => {
                database = Some(PathBuf::from(&value["--database=".len()..]));
            }
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    match request_stop(&database) {
        Ok(()) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({"status": "ok", "stopped": true}))
                        .expect("stop outcome must serialize")
                );
            } else {
                println!("stop requested");
            }
            0
        }
        Err(error) => cli_fail(json_mode, error, 1),
    }
}

fn run_resume<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut mission_id: Option<String> = None;
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if key == "--json" {
            json_mode = true;
            continue;
        }
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--mission-id" | "--mission" => mission_id = value,
            "--database" => database = value.map(PathBuf::from),
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let mission_id = match required_string(mission_id, "--mission-id") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    if let Err(error) = bootstrap_database(&database) {
        return cli_fail(json_mode, error, 1);
    }

    let cwd = source_cwd();
    let options = LaunchOptions {
        direction: "right".into(),
        cwd,
        prompts_dir: None,
        tab_mode: LaunchConfig::load().launch.tab_mode,
        workspace_source: WorkspaceSource::Current,
        worktree_path: None,
    };
    let runner = SystemProcessRunner;
    let mut progress = |message: &str| eprintln!("  {message}");
    match launch_mission(
        &database,
        &mission_id,
        &options,
        &runner,
        &herdr_bin(),
        &mut progress,
    ) {
        Ok(launch) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "mission_id": mission_id,
                        "stage": launch.stage,
                        "roles": launch.roles.iter().map(|role| json!({
                            "role": role.role,
                            "agent_name": role.agent_name,
                            "pane_id": role.pane_id,
                        })).collect::<Vec<_>>(),
                    }))
                    .expect("resume outcome must serialize")
                );
            } else {
                println!("resumed {mission_id} -> {}", launch.stage);
            }
            0
        }
        Err(error) => cli_fail(json_mode, error, 1),
    }
}

fn run_start_role<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut mission_id: Option<String> = None;
    let mut role: Option<String> = None;
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if key == "--json" {
            json_mode = true;
            continue;
        }
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--mission-id" | "--mission" => mission_id = value,
            "--role" => role = value,
            "--database" => database = value.map(PathBuf::from),
            "--help" | "-h" => {
                return command_help(
                    "start-role",
                    "--mission-id <id> --role <worker|scout|reviewer> [--database <path>] [--json]",
                );
            }
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let mission_id = match required_string(mission_id, "--mission-id") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let role = match required_string(role, "--role") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    if let Err(error) = bootstrap_database(&database) {
        return cli_fail(json_mode, error, 1);
    }

    let pm_pane_id = match read_role_runtime(&database, &mission_id)
        .ok()
        .and_then(|roles| roles.into_iter().find(|row| row.role == "pm"))
        .and_then(|row| (!row.pane_id.is_empty()).then_some(row.pane_id))
    {
        Some(pane_id) => pane_id,
        None => {
            return cli_fail(
                json_mode,
                KernelError {
                    category: ErrorCategory::Domain,
                    code: "pm_not_started".into(),
                    message: "PM 尚未启动，请先启动 PM 再按需拉起角色".into(),
                    retryable: false,
                    details: BTreeMap::new(),
                },
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    let runner = SystemProcessRunner;
    let cwd = source_cwd();
    let mut progress = |message: &str| eprintln!("  {message}");
    match start_role(
        &database,
        &mission_id,
        &role,
        &pm_pane_id,
        &cwd,
        None,
        &runner,
        &herdr_bin(),
        &mut progress,
    ) {
        Ok(Some(launched)) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "mission_id": mission_id,
                        "role": launched.role,
                        "agent_name": launched.agent_name,
                        "pane_id": launched.pane_id,
                    }))
                    .expect("start-role outcome must serialize")
                );
            } else {
                println!(
                    "started {role} → {} @ {}",
                    launched.agent_name, launched.pane_id
                );
            }
            0
        }
        Ok(None) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "mission_id": mission_id,
                        "role": role,
                        "already_started": true,
                    }))
                    .expect("start-role outcome must serialize")
                );
            } else {
                println!("{role} 已启动，跳过");
            }
            0
        }
        Err(error) => cli_fail(json_mode, error, 1),
    }
}

fn run_join<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut mission: Option<String> = None;
    let mut role: Option<String> = None;
    let mut pane: Option<String> = None;
    let mut agent_name: Option<String> = None;
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if key == "--json" {
            json_mode = true;
            continue;
        }
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--mission" | "--mission-id" => mission = value,
            "--role" => role = value,
            "--pane" => pane = value,
            "--agent-name" => agent_name = value,
            "--database" => database = value.map(PathBuf::from),
            "--help" | "-h" => {
                return command_help(
                    "join",
                    "--mission <id|标题> --role <worker|scout|reviewer> [--pane <id>] [--agent-name <name>] [--database <path>] [--json]",
                );
            }
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let mission_spec = match required_string(mission, "--mission") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let role = match required_string(role, "--role") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    let mission_id = match resolve_mission_id(&database, &mission_spec) {
        Ok(id) => id,
        Err(error) => return cli_fail(json_mode, error, 1),
    };
    let pane = match pane.or_else(|| {
        std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|value| !value.is_empty())
    }) {
        Some(pane) => pane,
        None => {
            return cli_fail(
                json_mode,
                malformed("--pane 未提供且 HERDR_PANE_ID 不可用"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };
    let agent_name =
        agent_name.unwrap_or_else(|| format!("mission-{}-{role}", agent_name_token(&mission_id)));

    let runner = SystemProcessRunner;
    let _ = runner.run(
        &herdr_bin(),
        &pane_rename_argv(&pane, &format!("⚑ {mission_id} › {role}")),
    );
    if let Err(error) = verify_joined_agent_identity(&runner, &herdr_bin(), &pane, &agent_name) {
        return cli_fail(json_mode, error, 1);
    }
    if let Err(error) = record_role_runtime(&database, &mission_id, &role, &pane, &agent_name) {
        return cli_fail(json_mode, error, 1);
    }
    notify_pm_of_join(&database, &mission_id, &role);

    if json_mode {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "status": "ok",
                "mission_id": mission_id,
                "role": role,
                "pane_id": pane,
                "agent_name": agent_name,
            }))
            .expect("join outcome must serialize")
        );
    } else {
        println!("{role} 已加入 {mission_id} @ {pane}");
        println!(
            "继续：herdr-mission init --mission-id={mission_id} --role={role} --database={}",
            database.display()
        );
    }
    0
}

fn verify_joined_agent_identity(
    runner: &dyn ProcessRunner,
    herdr: &str,
    pane_id: &str,
    agent_name: &str,
) -> Result<(), KernelError> {
    let renamed = runner
        .run(herdr, &agent_rename_argv(pane_id, agent_name))
        .map_err(|error| {
            role_runtime_process_failed("role_agent_rename_failed", "agent rename", error)
        })?;
    if renamed.exit_code != 0 {
        return Err(role_runtime_command_failed(
            "role_agent_rename_failed",
            "agent rename",
            renamed.exit_code,
        ));
    }

    let listed = runner.run(herdr, &agent_list_argv()).map_err(|error| {
        role_runtime_process_failed("role_agent_list_failed", "agent list", error)
    })?;
    if listed.exit_code != 0 {
        return Err(role_runtime_command_failed(
            "role_agent_list_failed",
            "agent list",
            listed.exit_code,
        ));
    }
    let agents = parse_agent_list(&listed.stdout)?;
    let exact = agents
        .iter()
        .filter(|agent| agent.pane_id == pane_id && agent.name.as_deref() == Some(agent_name))
        .count();
    let pane_matches = agents
        .iter()
        .filter(|agent| agent.pane_id == pane_id)
        .count();
    let name_matches = agents
        .iter()
        .filter(|agent| agent.name.as_deref() == Some(agent_name))
        .count();
    if exact != 1 || pane_matches != 1 || name_matches != 1 {
        return Err(KernelError {
            category: ErrorCategory::Contract,
            code: "role_runtime_identity_unverified".into(),
            message: "joined role did not resolve to one exact live Agent binding".into(),
            retryable: false,
            details: BTreeMap::from([
                ("agent_name".into(), json!(agent_name)),
                ("exact_matches".into(), json!(exact)),
                ("name_matches".into(), json!(name_matches)),
                ("pane_id".into(), json!(pane_id)),
                ("pane_matches".into(), json!(pane_matches)),
            ]),
        });
    }
    Ok(())
}

fn role_runtime_process_failed(code: &str, operation: &str, error: std::io::Error) -> KernelError {
    KernelError {
        category: ErrorCategory::Infrastructure,
        code: code.into(),
        message: format!("{operation} could not be executed"),
        retryable: true,
        details: BTreeMap::from([
            ("io_kind".into(), json!(format!("{:?}", error.kind()))),
            ("operation".into(), json!(operation)),
        ]),
    }
}

fn role_runtime_command_failed(code: &str, operation: &str, exit_code: i32) -> KernelError {
    KernelError {
        category: ErrorCategory::Infrastructure,
        code: code.into(),
        message: format!("{operation} failed"),
        retryable: true,
        details: BTreeMap::from([
            ("exit_code".into(), json!(exit_code)),
            ("operation".into(), json!(operation)),
        ]),
    }
}

/// Write a one-line context notice into PM's inbox so PM knows a role joined.
///
/// This is a best-effort direct insert into the frozen `messages` table; it
/// does not drive the kernel state machine, only makes the join visible to PM's
/// next `init`.
fn notify_pm_of_join(database: &Path, mission_id: &str, role: &str) {
    let connection = match open_writable(database, OWNER_IDENTITY) {
        Ok(connection) => connection,
        Err(_) => return,
    };
    let message_id = format!("msg-join-{mission_id}-{role}");
    let body = format!("{role} 已手动加入 Mission");
    let _ = connection.execute(
        "INSERT OR REPLACE INTO messages(
            id, mission_id, assignment_id, source_role, target_role, kind, body,
            context_rev, in_reply_to, review_id, created_at
         ) VALUES(?1, ?2, NULL, ?3, 'pm', 'context', ?4, 0, NULL, NULL, ?5)",
        rusqlite::params![message_id, mission_id, role, body, utc_timestamp()],
    );
}

fn run_delete<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut mission_id: Option<String> = None;
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if key == "--json" {
            json_mode = true;
            continue;
        }
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--mission-id" | "--mission" => mission_id = value,
            "--database" => database = value.map(PathBuf::from),
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let mission_id = match required_string(mission_id, "--mission-id") {
        Ok(value) => value,
        Err(error) => return cli_fail(json_mode, error, EXIT_MALFORMED_ARGS),
    };
    let database = match database.or_else(default_database) {
        Some(path) => path,
        None => {
            return cli_fail(
                json_mode,
                malformed("cannot resolve a state directory or HOME for default database path"),
                EXIT_MALFORMED_ARGS,
            );
        }
    };

    match delete_mission(&database, &mission_id) {
        Ok(outcome) => {
            let workspace_closed = match &outcome.workspace_id {
                Some(workspace_id) if !workspace_id.is_empty() => {
                    let runner = SystemProcessRunner;
                    matches!(
                        runner.run(&herdr_bin(), &workspace_close_argv(workspace_id)),
                        Ok(output) if output.exit_code == 0
                    )
                }
                _ => false,
            };
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "mission_id": mission_id,
                        "deleted": outcome.deleted,
                        "workspace_id": outcome.workspace_id,
                        "workspace_closed": workspace_closed,
                        "prompt_dir_removed": outcome.prompt_dir_removed,
                    }))
                    .expect("delete outcome must serialize")
                );
            } else if outcome.deleted {
                println!("deleted {mission_id}");
                if let Some(workspace_id) = &outcome.workspace_id {
                    if workspace_closed {
                        println!("  closed workspace {workspace_id}");
                    } else {
                        eprintln!("  warning: failed to close workspace {workspace_id}");
                    }
                }
            } else {
                println!("(no mission {mission_id})");
            }
            0
        }
        Err(error) => cli_fail(json_mode, error, 1),
    }
}

fn run_manifest<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut binary: Option<PathBuf> = None;
    let mut json_mode = false;
    let mut verify = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if key == "--json" {
            json_mode = true;
            continue;
        }
        if key == "--verify" {
            verify = true;
            continue;
        }
        let value = match inline {
            Some(value) => Some(value),
            None => args.next().cloned(),
        };
        match key {
            "--binary" => binary = value.map(PathBuf::from),
            other => {
                return cli_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        }
    }

    let binary = match binary {
        Some(path) => path,
        None => match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                return cli_fail(
                    json_mode,
                    malformed(format!("cannot resolve current executable: {error}")),
                    EXIT_MALFORMED_ARGS,
                );
            }
        },
    };

    if verify {
        let manifest_path = manifest_path_for(&binary);
        let manifest = match read_manifest(&manifest_path) {
            Ok(manifest) => manifest,
            Err(error) => {
                return cli_fail(
                    json_mode,
                    KernelError {
                        category: ErrorCategory::Infrastructure,
                        code: "manifest_unavailable".into(),
                        message: "cannot read release manifest".into(),
                        retryable: false,
                        details: BTreeMap::from([
                            ("path".into(), json!(manifest_path)),
                            ("reason".into(), json!(error)),
                        ]),
                    },
                    EXIT_MALFORMED_ARGS,
                );
            }
        };
        match verify_binary(&binary, &manifest) {
            Ok(()) => {
                if json_mode {
                    println!(
                        "{}",
                        serde_json::to_string(&json!({
                            "status": "ok",
                            "binary": binary,
                            "sha256": manifest.sha256,
                        }))
                        .expect("manifest verify outcome must serialize")
                    );
                } else {
                    println!("ok: {} matches {}", binary.display(), manifest.sha256);
                }
                0
            }
            Err(error) => cli_fail(
                json_mode,
                KernelError {
                    category: ErrorCategory::Infrastructure,
                    code: "binary_hash_mismatch".into(),
                    message: "binary does not match its release manifest".into(),
                    retryable: false,
                    details: BTreeMap::from([("reason".into(), json!(error))]),
                },
                1,
            ),
        }
    } else {
        match compute_manifest(
            &binary,
            env!("CARGO_PKG_VERSION"),
            SCHEMA_VERSION,
            PROTOCOL_VERSION,
        ) {
            Ok(manifest) => {
                if let Err(error) = write_manifest(&manifest_path_for(&binary), &manifest) {
                    return cli_fail(
                        json_mode,
                        KernelError {
                            category: ErrorCategory::Infrastructure,
                            code: "manifest_write_failed".into(),
                            message: "cannot persist release manifest".into(),
                            retryable: false,
                            details: BTreeMap::from([("reason".into(), json!(error))]),
                        },
                        1,
                    );
                }
                if json_mode {
                    println!(
                        "{}",
                        serde_json::to_string(&json!({
                            "status": "ok",
                            "manifest": manifest,
                        }))
                        .expect("manifest outcome must serialize")
                    );
                } else {
                    println!(
                        "binary={} version={} sha256={}",
                        manifest.binary, manifest.version, manifest.sha256
                    );
                }
                0
            }
            Err(error) => cli_fail(
                json_mode,
                KernelError {
                    category: ErrorCategory::Infrastructure,
                    code: "manifest_compute_failed".into(),
                    message: "cannot compute release manifest".into(),
                    retryable: false,
                    details: BTreeMap::from([("reason".into(), json!(error))]),
                },
                1,
            ),
        }
    }
}

fn run_doctor<'a>(args: impl Iterator<Item = &'a String>) -> i32 {
    let mut database: Option<PathBuf> = None;
    let mut json_mode = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json_mode = true,
            "--database" => {
                return doctor_fail(json_mode, malformed("--database requires a value"));
            }
            value if value.starts_with("--database=") => {
                database = Some(PathBuf::from(&value["--database=".len()..]));
            }
            other => {
                return doctor_fail(
                    json_mode,
                    malformed(format!("unexpected argument: {other}")),
                );
            }
        }
    }

    let database = match database {
        Some(path) => path,
        None => match default_database() {
            Some(path) => path,
            None => {
                return doctor_fail(
                    json_mode,
                    malformed("cannot resolve a state directory or HOME for default database path"),
                )
            }
        },
    };

    match doctor(database, json_mode) {
        Ok(()) => 0,
        Err(error) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "status": "error",
                        "error": error,
                    }))
                    .expect("error outcome must serialize")
                );
            } else {
                eprintln!("doctor failed: {} ({})", error.message, error.code);
            }
            1
        }
    }
}

fn doctor(database: PathBuf, json_mode: bool) -> Result<(), KernelError> {
    let outcome = bootstrap_database(&database)?;
    let connection = open_writable(&database, OWNER_IDENTITY)?;
    let generation = read_generation(&connection)?;

    let binary_hash = match std::env::current_exe() {
        Ok(exe) => {
            let manifest_path = manifest_path_for(&exe);
            match read_manifest(&manifest_path) {
                Ok(manifest) => match verify_binary(&exe, &manifest) {
                    Ok(()) => Some(manifest.sha256),
                    Err(error) => {
                        return Err(KernelError {
                            category: ErrorCategory::Infrastructure,
                            code: "binary_hash_mismatch".into(),
                            message: "running binary does not match its release manifest".into(),
                            retryable: false,
                            details: BTreeMap::from([("reason".into(), json!(error))]),
                        });
                    }
                },
                Err(_) => None,
            }
        }
        Err(_) => None,
    };

    if json_mode {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "status": "ok",
                "database": database,
                "schema_version": SCHEMA_VERSION,
                "owner": outcome.owner,
                "generation": generation,
                "created": outcome.created,
                "binary_sha256": binary_hash,
            }))
            .expect("doctor outcome must serialize")
        );
    } else {
        match binary_hash {
            Some(hash) => println!(
                "ok: schema={SCHEMA_VERSION} owner={} generation={generation} created={} sha256={hash}",
                outcome.owner, outcome.created
            ),
            None => println!(
                "ok: schema={SCHEMA_VERSION} owner={} generation={generation} created={}",
                outcome.owner, outcome.created
            ),
        }
    }
    Ok(())
}

fn resolve_launch_mode(explicit: Option<LaunchMode>, config: &LaunchConfig) -> LaunchMode {
    explicit.unwrap_or(config.launch.launch_mode)
}

pub fn default_database() -> Option<PathBuf> {
    database_path_for(
        std::env::var_os("HERDR_PLUGIN_STATE_DIR").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Choose the mission database path.
///
/// Herdr plugins must keep runtime state under `HERDR_PLUGIN_STATE_DIR`: the
/// plugin root is a managed source checkout and must not hold durable state.
/// Outside a plugin invocation, fall back to the same state directory herdr
/// derives from the plugin id, so standalone and plugin invocations share one
/// database instead of splitting state across two files.
fn database_path_for(
    state_dir: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(dir) = state_dir.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir).join("missions.sqlite3"));
    }
    let home = home?;
    let state_root = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".local/state"));
    Some(
        state_root
            .join("herdr")
            .join("plugins")
            .join("weston.herdr-mission")
            .join("missions.sqlite3"),
    )
}

fn malformed(reason: impl Into<String>) -> KernelError {
    KernelError {
        category: ErrorCategory::Transport,
        code: "malformed_args".into(),
        message: "invalid command-line arguments".into(),
        retryable: false,
        details: BTreeMap::from([("reason".into(), json!(reason.into()))]),
    }
}

fn required_string(value: Option<String>, flag: &str) -> Result<String, KernelError> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(malformed(format!("{flag} is required"))),
    }
}

fn doctor_fail(json_mode: bool, error: KernelError) -> i32 {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "status": "error",
                "error": error,
            }))
            .expect("error outcome must serialize")
        );
    } else {
        eprintln!("doctor failed: {} ({})", error.message, error.code);
    }
    EXIT_MALFORMED_ARGS
}

fn cli_fail(json_mode: bool, error: KernelError, exit_code: i32) -> i32 {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "status": "error",
                "error": error,
            }))
            .expect("error outcome must serialize")
        );
    } else {
        eprintln!("failed: {} ({})", error.message, error.code);
    }
    exit_code
}

fn unknown_layout(layout: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Operation,
        code: "unknown_layout".into(),
        message: "mission layout is not recognized".into(),
        retryable: false,
        details: BTreeMap::from([("layout".into(), json!(layout))]),
    }
}

fn unknown_provider(provider: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Operation,
        code: "unknown_provider".into(),
        message: "agent provider is not recognized".into(),
        retryable: false,
        details: BTreeMap::from([("provider".into(), json!(provider))]),
    }
}

fn unknown_workspace_source(source: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Operation,
        code: "unknown_workspace_source".into(),
        message: "workspace source is not recognized".into(),
        retryable: false,
        details: BTreeMap::from([("source".into(), json!(source))]),
    }
}

fn parse_role_spec(spec: &str) -> Result<RoleOverride, String> {
    let (role, rest) = spec
        .split_once(':')
        .ok_or_else(|| "expected <role>:<key=value,...>".to_string())?;
    let role = role.trim();
    if !is_valid_role_identity(role) {
        return Err(format!("unknown role '{role}'"));
    }
    let mut item = RoleOverride {
        role: role.to_string(),
        ..Default::default()
    };
    for pair in rest.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("expected key=value in '{pair}'"))?;
        let key = key.trim();
        let value = value.trim().to_string();
        match key {
            "provider" => item.provider = Some(value),
            "model" => item.model = Some(value),
            "thinking" => item.thinking = Some(value),
            "permission_policy" => item.permission_policy = Some(value),
            other => return Err(format!("unknown role key '{other}'")),
        }
    }
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::{database_path_for, resolve_launch_mode};
    use crate::{LaunchConfig, LaunchMode};
    use std::{ffi::OsStr, path::PathBuf};

    #[test]
    fn new_launch_mode_prefers_explicit_override_then_config_then_manual() {
        let auto = LaunchConfig::parse("[launch]\nlaunch_mode = \"auto\"\n").unwrap();
        let manual = LaunchConfig::default();

        assert_eq!(resolve_launch_mode(None, &auto), LaunchMode::Auto);
        assert_eq!(
            resolve_launch_mode(Some(LaunchMode::Manual), &auto),
            LaunchMode::Manual
        );
        assert_eq!(
            resolve_launch_mode(Some(LaunchMode::Auto), &manual),
            LaunchMode::Auto
        );
        assert_eq!(resolve_launch_mode(None, &manual), LaunchMode::Manual);
    }

    #[test]
    fn database_path_prefers_plugin_state_dir() {
        let state = OsStr::new("/var/lib/herdr/plugins/mission/state");
        let home = OsStr::new("/home/user");
        assert_eq!(
            database_path_for(Some(state), Some(home)),
            Some(PathBuf::from(
                "/var/lib/herdr/plugins/mission/state/missions.sqlite3"
            ))
        );
    }

    #[test]
    fn database_path_ignores_empty_state_dir() {
        let home = OsStr::new("/home/user");
        assert_eq!(
            database_path_for(Some(OsStr::new("")), Some(home)),
            Some(PathBuf::from(
                "/home/user/.local/state/herdr/plugins/weston.herdr-mission/missions.sqlite3"
            ))
        );
    }

    #[test]
    fn database_path_falls_back_to_home() {
        let home = OsStr::new("/home/user");
        assert_eq!(
            database_path_for(None, Some(home)),
            Some(PathBuf::from(
                "/home/user/.local/state/herdr/plugins/weston.herdr-mission/missions.sqlite3"
            ))
        );
    }

    #[test]
    fn database_path_is_none_without_state_or_home() {
        assert_eq!(database_path_for(None, None), None);
    }
}
