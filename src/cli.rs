//! `herdr-mission` command-line interface.
//!
//! Hand-rolled argument parsing (no external CLI dependency) so the runtime
//! never needs to fetch or compile extra crates. Every command writes a single
//! JSON document to stdout and diagnostics to stderr, with stable exit codes
//! for machine consumers.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde_json::json;

use crate::{
    agent_name_token, agent_rename_argv, bootstrap_database, compute_manifest, create_mission,
    delete_mission, herdr_bin, is_valid_role_identity, kernel_deliver, kernel_dispatch_command,
    kernel_read_context, kernel_reply_command, launch_mission, list_missions, make_mission_id,
    manifest_path_for, open_writable, pane_rename_argv, read_generation, read_manifest,
    read_mission_status, read_role_runtime, record_role_runtime, request_stop, resolve_mission_id,
    resolve_roles, run_daemon, run_tui, source_cwd, start_role, utc_timestamp, verify_binary,
    workspace_close_argv, write_manifest, CreateMissionRequest, ErrorCategory, KernelError,
    LaunchConfig, LaunchMode, LaunchOptions, LaunchedRole, MissionLayout, ProcessRunner, Provider,
    RoleOverride, SystemProcessRunner, WorkspaceSource, OWNER_IDENTITY, PROTOCOL_VERSION,
    SCHEMA_VERSION,
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
        Some("new") => run_new(iter),
        Some("status") => run_status(iter),
        Some("list") => run_list(iter),
        Some("init") => run_init(iter),
        Some("send") => run_send(iter),
        Some("reply") => run_reply(iter),
        Some("deliver") => run_deliver(iter),
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
           init        读取角色待办与收件箱\n\
           send        派发 Assignment 给目标角色\n\
           reply       回执 Assignment\n\
           deliver     投递 outbox\n\
           start-role  按需启动一个角色\n\
           join        手动把当前 agent 加入为某角色\n\
           resume      恢复未启动的角色\n\
           delete      删除 Mission\n\
           tui         打开控制台\n\
           doctor      自检\n\
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
    let mut autonomy = "manual".to_string();
    let mut launch_mode = LaunchMode::Manual;
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
            "--autonomy" => autonomy = value.unwrap_or_else(|| "manual".into()),
            "--launch-mode" => match value.as_deref() {
                Some("auto") => launch_mode = LaunchMode::Auto,
                Some("manual") => launch_mode = LaunchMode::Manual,
                Some(other) => {
                    return cli_fail(
                        json_mode,
                        malformed(format!("unknown --launch-mode: {other}")),
                        EXIT_MALFORMED_ARGS,
                    );
                }
                None => launch_mode = LaunchMode::Manual,
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

    let request = CreateMissionRequest {
        mission_id: make_mission_id(&title),
        brief: title.clone(),
        template: "general".into(),
        agent_profile_id: provider.profile_id(),
        agent_profile_version: provider.profile_version(),
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
                    autonomy,
                    prompts_dir,
                    tab_mode: LaunchConfig::load().launch.tab_mode,
                    launch_mode,
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
                for (role, health) in &status.roles {
                    println!("  {role}: {health}");
                }
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
                for assignment in &context.pending_assignments {
                    println!(
                        "  - {} {} ({})",
                        assignment.state, assignment.id, assignment.kind
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
    let mut kind = "task".to_string();
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
            "--target" => target = value,
            "--kind" => kind = value.unwrap_or_else(|| "task".into()),
            "--body" => body = value,
            "--database" => database = value.map(PathBuf::from),
            "--help" | "-h" => {
                return command_help(
                    "send",
                    "--mission-id <id> --role <source> --target <target> --kind <task|review> --body '<text>' [--database <path>] [--json]",
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

    let cwd = source_cwd();
    let options = LaunchOptions {
        direction: "right".into(),
        cwd,
        autonomy: "manual".into(),
        prompts_dir: None,
        tab_mode: LaunchConfig::load().launch.tab_mode,
        launch_mode: LaunchConfig::load().launch.launch_mode,
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
        "manual",
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

    if let Err(error) = record_role_runtime(&database, &mission_id, &role, &pane, &agent_name) {
        return cli_fail(json_mode, error, 1);
    }

    let runner = SystemProcessRunner;
    let _ = runner.run(
        &herdr_bin(),
        &pane_rename_argv(&pane, &format!("⚑ {mission_id} › {role}")),
    );
    // Register the fabricated agent name with Herdr so the delivery wake-up
    // (`herdr agent prompt <name>`) can resolve this pane. Without this,
    // manually joined roles never leave `queued` because the agent target is
    // `agent_not_found`.
    let _ = runner.run(&herdr_bin(), &agent_rename_argv(&pane, &agent_name));
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
    use super::database_path_for;
    use std::{ffi::OsStr, path::PathBuf};

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
