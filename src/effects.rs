//! Real effect execution: Herdr process and agent-provider adapters.
//!
//! These adapters implement the kernel's `EffectExecutor` seam and invoke the
//! real `herdr` CLI (and, through it, the provider CLIs) via an injectable
//! `ProcessRunner`. Tests inject a fake runner, so command construction and
//! outcome parsing are verified without launching any agent or pane.

use std::collections::BTreeMap;

use serde_json::json;

use crate::domain::EffectExecutor;
use crate::{
    EffectIntent, EffectIntentKind, EffectOutcome, ErrorCategory, KernelError, RoleAttachMode,
    RoleKind,
};

/// A seam for running an external process, injected so adapters are testable
/// without side effects.
pub trait ProcessRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<ProcessOutput>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The real process runner backed by `std::process::Command`.
pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<ProcessOutput> {
        let output = std::process::Command::new(program).args(args).output()?;
        Ok(ProcessOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// How to launch and resume a single provider CLI (mirrors Python
/// `AgentAdapter`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAdapter {
    pub command: String,
    pub autonomy_args: Vec<String>,
    pub resume_args: Vec<String>,
}

fn provider_adapter(provider: &str) -> Option<AgentAdapter> {
    let (command, autonomy, resume): (&str, &[&str], &[&str]) = match provider {
        "codex" => (
            "codex",
            &["--dangerously-bypass-approvals-and-sandbox"],
            &["resume", "{value}"],
        ),
        "claude" => (
            "claude",
            &["--dangerously-skip-permissions"],
            &["--resume", "{value}"],
        ),
        "grok" => ("grok", &["--always-approve"], &["--resume", "{value}"]),
        "pi" => ("pi", &["--approve"], &["--session", "{value}"]),
        "cursor-agent" => ("cursor-agent", &["--force"], &["--resume", "{value}"]),
        "droid" => ("droid", &["--auto", "high"], &["--resume", "{value}"]),
        "gemini" => (
            "gemini",
            &["--approval-mode", "yolo"],
            &["--session-file", "{value}"],
        ),
        _ => return None,
    };
    Some(AgentAdapter {
        command: command.to_string(),
        autonomy_args: autonomy.iter().map(|s| s.to_string()).collect(),
        resume_args: resume.iter().map(|s| s.to_string()).collect(),
    })
}

fn role_name(kind: &RoleKind) -> &'static str {
    match kind {
        RoleKind::Pm => "pm",
        RoleKind::Worker => "worker",
        RoleKind::Scout => "scout",
        RoleKind::Reviewer => "reviewer",
    }
}

/// Build the launch argv for a provider (command plus autonomy flags).
pub fn launch_argv(provider: &str) -> Result<Vec<String>, KernelError> {
    let adapter = provider_adapter(provider).ok_or_else(|| unsupported_provider(provider))?;
    let mut argv = vec![adapter.command];
    argv.extend(adapter.autonomy_args);
    Ok(argv)
}

/// Build the resume argv for a provider with an existing session value.
pub fn resume_argv(provider: &str, session: &str) -> Result<Vec<String>, KernelError> {
    let adapter = provider_adapter(provider).ok_or_else(|| unsupported_provider(provider))?;
    let mut argv = vec![adapter.command];
    argv.extend(adapter.autonomy_args);
    argv.extend(
        adapter
            .resume_args
            .iter()
            .map(|arg| arg.replace("{value}", session)),
    );
    Ok(argv)
}

/// Build the agent args passed after `--` in `herdr agent start --kind
/// <provider> ... -- <args>`.
///
/// The agent command itself is selected by `--kind`, so it is deliberately not
/// repeated here; Herdr prepends the canonical executable (confirmed in the
/// `agent start` response `argv` field).
pub fn agent_start_args(provider: &str, session: Option<&str>) -> Result<Vec<String>, KernelError> {
    let adapter = provider_adapter(provider).ok_or_else(|| unsupported_provider(provider))?;
    let mut argv = adapter.autonomy_args.clone();
    if let Some(session) = session {
        argv.extend(
            adapter
                .resume_args
                .iter()
                .map(|arg| arg.replace("{value}", session)),
        );
    }
    Ok(argv)
}

/// Per-role runtime identity used to construct launch/observe/prompt commands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleRuntimeConfig {
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub pane_id: String,
    pub agent_name: String,
    pub session: Option<String>,
}

/// Resolve the Herdr binary to invoke.
///
/// Prefer `HERDR_BIN_PATH`, which Herdr injects into plugin commands so plugin
/// code talks to the host portably across Unix socket and Windows named-pipe
/// transports. Fall back to a bare `herdr` on `PATH` for standalone CLI use
/// outside a plugin invocation.
pub fn herdr_bin() -> String {
    choose_herdr_bin(std::env::var("HERDR_BIN_PATH").ok().as_deref())
}

fn choose_herdr_bin(resolved: Option<&str>) -> String {
    match resolved {
        Some(path) if !path.is_empty() => path.to_string(),
        _ => "herdr".to_string(),
    }
}

/// Resolve the directory where role agents should start.
///
/// Herdr launches plugin popups (like the Mission dashboard) with the plugin
/// root as the process cwd, so `std::env::current_dir()` would send role
/// agents into the plugin's own source tree instead of the user's current
/// pane. Prefer Herdr's explicit focused-pane cwd, then the plugin context
/// JSON, and only fall back to the process cwd for standalone CLI use.
pub fn source_cwd() -> String {
    if let Some(cwd) = context_json_cwd() {
        return cwd;
    }
    if let Some(cwd) = env_dir("HERDR_ACTIVE_PANE_CWD") {
        return cwd;
    }
    std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
}

fn env_dir(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .filter(|value| std::path::Path::new(value).is_dir())
}

fn context_json_cwd() -> Option<String> {
    let raw = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
    context_json_cwd_from_str(&raw)
}

fn context_json_cwd_from_str(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    for key in ["focused_pane_cwd", "workspace_cwd", "cwd"] {
        if let Some(cwd) = value.get(key).and_then(serde_json::Value::as_str) {
            if !cwd.is_empty() && std::path::Path::new(cwd).is_dir() {
                return Some(cwd.to_string());
            }
        }
    }
    None
}

/// Real Herdr process adapter: launches/observes role panes via `herdr`.
pub struct HerdrProcessAdapter<'a> {
    herdr: String,
    roles: BTreeMap<String, RoleRuntimeConfig>,
    runner: &'a dyn ProcessRunner,
}

impl<'a> HerdrProcessAdapter<'a> {
    pub fn new(
        herdr: impl Into<String>,
        roles: BTreeMap<String, RoleRuntimeConfig>,
        runner: &'a dyn ProcessRunner,
    ) -> Self {
        Self {
            herdr: herdr.into(),
            roles,
            runner,
        }
    }

    fn role(&self, name: &str) -> Option<&RoleRuntimeConfig> {
        self.roles.get(name)
    }

    fn ensure_role_ready(&self, kind: &RoleKind, attach_mode: &RoleAttachMode) -> EffectOutcome {
        let _ = attach_mode;
        let name = role_name(kind);
        let Some(config) = self.role(name) else {
            return EffectOutcome::TerminalFailure {
                error: KernelError {
                    category: ErrorCategory::Domain,
                    code: "role_not_found".into(),
                    message: "role runtime config is missing".into(),
                    retryable: false,
                    details: BTreeMap::from([("role".into(), json!(name))]),
                },
            };
        };
        if config.agent_name.is_empty() || config.pane_id.is_empty() {
            return EffectOutcome::Pending {
                reason: "role agent name and pane are not allocated yet".into(),
            };
        }
        let argv = match agent_start_args(&config.provider, config.session.as_deref()) {
            Ok(argv) => argv,
            Err(error) => return EffectOutcome::TerminalFailure { error },
        };
        let mut command = vec![
            "agent".to_string(),
            "start".to_string(),
            config.agent_name.clone(),
            "--kind".to_string(),
            config.provider.clone(),
            "--pane".to_string(),
            config.pane_id.clone(),
            "--".to_string(),
        ];
        command.extend(argv);
        self.run_herdr(&command, "ensure_role_ready", name, &config.provider)
    }

    fn observe_role(&self, kind: &RoleKind) -> EffectOutcome {
        let name = role_name(kind);
        let Some(config) = self.role(name) else {
            return EffectOutcome::RetryableFailure {
                error: KernelError {
                    category: ErrorCategory::Domain,
                    code: "role_not_found".into(),
                    message: "role runtime config is missing".into(),
                    retryable: true,
                    details: BTreeMap::from([("role".into(), json!(name))]),
                },
                retry_after_ms: 1_000,
            };
        };
        if config.agent_name.is_empty() {
            return EffectOutcome::Pending {
                reason: "role agent is not assigned yet".into(),
            };
        }
        let command = vec![
            "agent".to_string(),
            "get".to_string(),
            config.agent_name.clone(),
        ];
        self.run_herdr(&command, "observe_role", name, &config.provider)
    }

    fn run_herdr(
        &self,
        command: &[String],
        operation: &str,
        role: &str,
        provider: &str,
    ) -> EffectOutcome {
        match self.runner.run(&self.herdr, command) {
            Ok(output) if output.exit_code == 0 => EffectOutcome::Succeeded {
                observation: json!({
                    "adapter": "herdr",
                    "operation": operation,
                    "role": role,
                    "provider": provider,
                    "stdout": output.stdout,
                }),
            },
            Ok(output) => EffectOutcome::RetryableFailure {
                error: KernelError {
                    category: ErrorCategory::Infrastructure,
                    code: "herdr_command_failed".into(),
                    message: "herdr command exited non-zero".into(),
                    retryable: true,
                    details: BTreeMap::from([
                        ("operation".into(), json!(operation)),
                        ("exit_code".into(), json!(output.exit_code)),
                        ("stderr".into(), json!(output.stderr)),
                    ]),
                },
                retry_after_ms: 1_000,
            },
            Err(error) => EffectOutcome::RetryableFailure {
                error: KernelError {
                    category: ErrorCategory::Infrastructure,
                    code: "herdr_spawn_failed".into(),
                    message: "failed to spawn herdr".into(),
                    retryable: true,
                    details: BTreeMap::from([("reason".into(), json!(error.to_string()))]),
                },
                retry_after_ms: 1_000,
            },
        }
    }
}

impl EffectExecutor for HerdrProcessAdapter<'_> {
    fn execute(&mut self, intent: &EffectIntent) -> EffectOutcome {
        match &intent.intent {
            EffectIntentKind::EnsureRoleReady { role, attach_mode } => {
                self.ensure_role_ready(&role.role, attach_mode)
            }
            EffectIntentKind::ObserveRole { role } => self.observe_role(&role.role),
            EffectIntentKind::RefreshMissionMirror => EffectOutcome::Succeeded {
                observation: json!({"adapter": "herdr", "operation": "refresh_mission_mirror"}),
            },
            _ => EffectOutcome::TerminalFailure {
                error: KernelError {
                    category: ErrorCategory::Operation,
                    code: "effect_not_handled".into(),
                    message: "intent is not a Herdr process effect".into(),
                    retryable: false,
                    details: BTreeMap::new(),
                },
            },
        }
    }
}

/// Real agent-provider adapter: delivers prompts to an agent terminal.
pub struct AgentProviderAdapter<'a> {
    herdr: String,
    roles: BTreeMap<String, RoleRuntimeConfig>,
    runner: &'a dyn ProcessRunner,
}

impl<'a> AgentProviderAdapter<'a> {
    pub fn new(
        herdr: impl Into<String>,
        roles: BTreeMap<String, RoleRuntimeConfig>,
        runner: &'a dyn ProcessRunner,
    ) -> Self {
        Self {
            herdr: herdr.into(),
            roles,
            runner,
        }
    }
}

impl EffectExecutor for AgentProviderAdapter<'_> {
    fn execute(&mut self, intent: &EffectIntent) -> EffectOutcome {
        match &intent.intent {
            EffectIntentKind::DeliverPrompt { role, prompt, .. } => {
                let name = role_name(&role.role);
                let Some(config) = self.roles.get(name) else {
                    return EffectOutcome::RetryableFailure {
                        error: KernelError {
                            category: ErrorCategory::Domain,
                            code: "role_not_found".into(),
                            message: "role runtime config is missing".into(),
                            retryable: true,
                            details: BTreeMap::from([("role".into(), json!(name))]),
                        },
                        retry_after_ms: 1_000,
                    };
                };
                if config.agent_name.is_empty() {
                    return EffectOutcome::Pending {
                        reason: "role agent is not assigned yet".into(),
                    };
                }
                let command = vec![
                    "agent".to_string(),
                    "prompt".to_string(),
                    config.agent_name.clone(),
                    prompt.clone(),
                ];
                match self.runner.run(&self.herdr, &command) {
                    Ok(output) if output.exit_code == 0 => EffectOutcome::Succeeded {
                        observation: json!({
                            "adapter": "herdr",
                            "operation": "deliver_prompt",
                            "role": name,
                        }),
                    },
                    Ok(output) if is_agent_not_found(&output) => EffectOutcome::Pending {
                        reason: "target agent has not started yet".into(),
                    },
                    Ok(output) => EffectOutcome::RetryableFailure {
                        error: KernelError {
                            category: ErrorCategory::Infrastructure,
                            code: "herdr_command_failed".into(),
                            message: "herdr command exited non-zero".into(),
                            retryable: true,
                            details: BTreeMap::from([
                                ("operation".into(), json!("deliver_prompt")),
                                ("exit_code".into(), json!(output.exit_code)),
                            ]),
                        },
                        retry_after_ms: 1_000,
                    },
                    Err(error) => EffectOutcome::RetryableFailure {
                        error: KernelError {
                            category: ErrorCategory::Infrastructure,
                            code: "herdr_spawn_failed".into(),
                            message: "failed to spawn herdr".into(),
                            retryable: true,
                            details: BTreeMap::from([("reason".into(), json!(error.to_string()))]),
                        },
                        retry_after_ms: 1_000,
                    },
                }
            }
            EffectIntentKind::RecordEvidence { .. } => EffectOutcome::Succeeded {
                observation: json!({"adapter": "herdr", "operation": "record_evidence"}),
            },
            _ => EffectOutcome::TerminalFailure {
                error: KernelError {
                    category: ErrorCategory::Operation,
                    code: "effect_not_handled".into(),
                    message: "intent is not an agent-provider effect".into(),
                    retryable: false,
                    details: BTreeMap::new(),
                },
            },
        }
    }
}

fn unsupported_provider(provider: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Operation,
        code: "unsupported_provider".into(),
        message: "agent provider is not supported".into(),
        retryable: false,
        details: BTreeMap::from([("provider".into(), json!(provider))]),
    }
}

/// True when a `herdr agent` command failed only because the target agent has
/// not been started/registered yet. This is a *waiting* condition, not a
/// transient failure, so callers should treat it as `Pending` rather than burn
/// a delivery retry.
fn is_agent_not_found(output: &ProcessOutput) -> bool {
    output.stdout.contains("agent_not_found") || output.stderr.contains("agent_not_found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Generation, RoleRef};
    use std::cell::RefCell;

    struct FakeRunner {
        calls: RefCell<Vec<(String, Vec<String>)>>,
        output: std::io::Result<ProcessOutput>,
    }

    impl FakeRunner {
        fn success() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                output: Ok(ProcessOutput {
                    exit_code: 0,
                    stdout: "ok".into(),
                    stderr: String::new(),
                }),
            }
        }

        fn output(exit_code: i32, stdout: &str, stderr: &str) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                output: Ok(ProcessOutput {
                    exit_code,
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                }),
            }
        }
    }

    impl ProcessRunner for FakeRunner {
        fn run(&self, program: &str, args: &[String]) -> std::io::Result<ProcessOutput> {
            self.calls
                .borrow_mut()
                .push((program.to_string(), args.to_vec()));
            match &self.output {
                Ok(output) => Ok(output.clone()),
                Err(error) => Err(std::io::Error::new(error.kind(), error.to_string())),
            }
        }
    }

    fn intent(kind: EffectIntentKind) -> EffectIntent {
        EffectIntent {
            effect_id: "effect-real".into(),
            generation: Generation::new("generation-real").unwrap(),
            intent: kind,
        }
    }

    fn role_ref(kind: RoleKind) -> RoleRef {
        RoleRef {
            role: kind,
            instance: None,
        }
    }

    #[test]
    fn herdr_bin_prefers_plugin_injected_path() {
        assert_eq!(
            choose_herdr_bin(Some("/opt/herdr/bin/herdr")),
            "/opt/herdr/bin/herdr"
        );
    }

    #[test]
    fn herdr_bin_falls_back_to_bare_binary() {
        assert_eq!(choose_herdr_bin(None), "herdr");
        assert_eq!(choose_herdr_bin(Some("")), "herdr");
    }

    #[test]
    fn context_json_prefers_focused_pane_cwd() {
        let dir = std::env::temp_dir().to_string_lossy().into_owned();
        let raw = format!(
            r#"{{"focused_pane_cwd": "{dir}", "workspace_cwd": "/nowhere", "cwd": "/also-nowhere"}}"#
        );
        assert_eq!(
            context_json_cwd_from_str(&raw).as_deref(),
            Some(dir.as_str())
        );
    }

    #[test]
    fn context_json_falls_back_to_workspace_then_cwd() {
        let dir = std::env::temp_dir().to_string_lossy().into_owned();
        let raw = format!(r#"{{"workspace_cwd": "{dir}"}}"#);
        assert_eq!(
            context_json_cwd_from_str(&raw).as_deref(),
            Some(dir.as_str())
        );

        let raw = format!(r#"{{"cwd": "{dir}"}}"#);
        assert_eq!(
            context_json_cwd_from_str(&raw).as_deref(),
            Some(dir.as_str())
        );
    }

    #[test]
    fn context_json_rejects_empty_missing_and_nonexistent() {
        assert_eq!(
            context_json_cwd_from_str(r#"{"focused_pane_cwd": ""}"#),
            None
        );
        assert_eq!(context_json_cwd_from_str(r#"{"other": "x"}"#), None);
        assert_eq!(
            context_json_cwd_from_str(r#"{"cwd": "/definitely/not/a/dir"}"#),
            None
        );
        assert_eq!(context_json_cwd_from_str("not json"), None);
    }

    #[test]
    fn launch_argv_maps_provider_to_command_and_flags() {
        assert_eq!(
            launch_argv("codex").unwrap(),
            vec!["codex", "--dangerously-bypass-approvals-and-sandbox"]
        );
        assert_eq!(
            launch_argv("claude").unwrap(),
            vec!["claude", "--dangerously-skip-permissions"]
        );
        assert_eq!(
            launch_argv("grok").unwrap(),
            vec!["grok", "--always-approve"]
        );
        assert_eq!(launch_argv("pi").unwrap(), vec!["pi", "--approve"]);
    }

    #[test]
    fn launch_argv_rejects_unsupported_provider() {
        let error = launch_argv("unknown").unwrap_err();
        assert_eq!(error.code, "unsupported_provider");
    }

    #[test]
    fn resume_argv_includes_session_value() {
        assert_eq!(
            resume_argv("claude", "sess-123").unwrap(),
            vec![
                "claude",
                "--dangerously-skip-permissions",
                "--resume",
                "sess-123"
            ]
        );
    }

    #[test]
    fn agent_start_args_omit_command_and_include_resume() {
        assert_eq!(
            agent_start_args("codex", None).unwrap(),
            vec!["--dangerously-bypass-approvals-and-sandbox"]
        );
        assert_eq!(
            agent_start_args("claude", Some("sess-123")).unwrap(),
            vec!["--dangerously-skip-permissions", "--resume", "sess-123"]
        );
    }

    #[test]
    fn herdr_adapter_runs_agent_start_for_ensure_role_ready() {
        let runner = FakeRunner::success();
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleRuntimeConfig {
                provider: "claude".into(),
                pane_id: "w1:p2".into(),
                agent_name: "worker".into(),
                ..Default::default()
            },
        );
        let mut adapter = HerdrProcessAdapter::new("herdr", roles, &runner);

        let effect = intent(EffectIntentKind::EnsureRoleReady {
            role: role_ref(RoleKind::Worker),
            attach_mode: RoleAttachMode::Managed,
        });
        let outcome = adapter.execute(&effect);
        assert!(matches!(outcome, EffectOutcome::Succeeded { .. }));

        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "herdr");
        assert_eq!(
            calls[0].1,
            vec![
                "agent".to_string(),
                "start".to_string(),
                "worker".to_string(),
                "--kind".to_string(),
                "claude".to_string(),
                "--pane".to_string(),
                "w1:p2".to_string(),
                "--".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ]
        );
    }

    #[test]
    fn herdr_adapter_returns_pending_without_pane() {
        let runner = FakeRunner::success();
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleRuntimeConfig {
                provider: "codex".into(),
                ..Default::default()
            },
        );
        let mut adapter = HerdrProcessAdapter::new("herdr", roles, &runner);

        let effect = intent(EffectIntentKind::EnsureRoleReady {
            role: role_ref(RoleKind::Worker),
            attach_mode: RoleAttachMode::Managed,
        });
        let outcome = adapter.execute(&effect);
        assert!(matches!(outcome, EffectOutcome::Pending { .. }));
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn herdr_adapter_fails_closed_on_unknown_provider() {
        let runner = FakeRunner::success();
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleRuntimeConfig {
                provider: "unknown".into(),
                pane_id: "w1:p2".into(),
                agent_name: "worker".into(),
                ..Default::default()
            },
        );
        let mut adapter = HerdrProcessAdapter::new("herdr", roles, &runner);

        let effect = intent(EffectIntentKind::EnsureRoleReady {
            role: role_ref(RoleKind::Worker),
            attach_mode: RoleAttachMode::Managed,
        });
        let outcome = adapter.execute(&effect);
        match outcome {
            EffectOutcome::TerminalFailure { error } => {
                assert_eq!(error.code, "unsupported_provider")
            }
            other => panic!("expected terminal failure, got {other:?}"),
        }
    }

    #[test]
    fn agent_adapter_delivers_prompt_via_agent_prompt() {
        let runner = FakeRunner::success();
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleRuntimeConfig {
                provider: "codex".into(),
                agent_name: "worker".into(),
                ..Default::default()
            },
        );
        let mut adapter = AgentProviderAdapter::new("herdr", roles, &runner);

        let effect = intent(EffectIntentKind::DeliverPrompt {
            role: role_ref(RoleKind::Worker),
            assignment_id: Some("asg-1".into()),
            prompt: "do the thing".into(),
        });
        let outcome = adapter.execute(&effect);
        assert!(matches!(outcome, EffectOutcome::Succeeded { .. }));

        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].1,
            vec![
                "agent".to_string(),
                "prompt".to_string(),
                "worker".to_string(),
                "do the thing".to_string(),
            ]
        );
    }

    #[test]
    fn agent_adapter_returns_pending_when_agent_not_found() {
        let runner = FakeRunner::output(
            1,
            r#"{"error":{"code":"agent_not_found","message":"agent target worker not found"}}"#,
            "",
        );
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleRuntimeConfig {
                provider: "codex".into(),
                agent_name: "worker".into(),
                ..Default::default()
            },
        );
        let mut adapter = AgentProviderAdapter::new("herdr", roles, &runner);

        let effect = intent(EffectIntentKind::DeliverPrompt {
            role: role_ref(RoleKind::Worker),
            assignment_id: Some("asg-1".into()),
            prompt: "do the thing".into(),
        });
        let outcome = adapter.execute(&effect);
        assert!(matches!(outcome, EffectOutcome::Pending { .. }));
    }

    #[test]
    fn agent_adapter_still_retries_on_other_herdr_failures() {
        let runner = FakeRunner::output(1, "", "some other herdr error");
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleRuntimeConfig {
                provider: "codex".into(),
                agent_name: "worker".into(),
                ..Default::default()
            },
        );
        let mut adapter = AgentProviderAdapter::new("herdr", roles, &runner);

        let effect = intent(EffectIntentKind::DeliverPrompt {
            role: role_ref(RoleKind::Worker),
            assignment_id: Some("asg-1".into()),
            prompt: "do the thing".into(),
        });
        let outcome = adapter.execute(&effect);
        assert!(matches!(outcome, EffectOutcome::RetryableFailure { .. }));
    }
}
