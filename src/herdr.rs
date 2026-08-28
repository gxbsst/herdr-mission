//! Herdr CLI layout response parsing and command construction.
//!
//! Pure helpers that either build `herdr` argv or parse creation responses.
//! They never spawn a process, so they are unit-testable without a live Herdr
//! session. The drive loop will use these to turn opaque JSON responses into
//! typed workspace/tab/pane ids instead of string-mangling them.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::{ErrorCategory, KernelError};

/// Identifiers returned by `herdr workspace create`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCreated {
    pub workspace_id: String,
    pub tab_id: String,
    pub root_pane_id: String,
}

/// Identifiers returned by `herdr tab create`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabCreated {
    pub tab_id: String,
    pub root_pane_id: String,
}

/// Identifier returned by `herdr pane split`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCreated {
    pub pane_id: String,
}

/// Structured identity returned by `herdr pane get` for launch recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub agent: String,
    pub cwd: String,
    pub has_agent_session: bool,
}

/// Stable pane location returned by `herdr pane list` during region recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneLocation {
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
}

/// Structured identity returned by `herdr tab get` for region validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    pub workspace_id: String,
    pub tab_id: String,
    pub label: String,
}

/// Herdr's canonical live Agent states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "idle" => Some(Self::Idle),
            "working" => Some(Self::Working),
            "blocked" => Some(Self::Blocked),
            "done" => Some(Self::Done),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Live identity and status from one `herdr agent list` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub name: Option<String>,
    pub pane_id: String,
    pub status: AgentStatus,
}

/// Identifiers returned by `herdr worktree create` / `worktree open`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCreated {
    pub workspace_id: String,
    pub tab_id: String,
    pub root_pane_id: String,
    pub worktree_path: String,
    pub branch: String,
}

/// Build argv for `herdr workspace create` rooted at a path with a label.
pub fn workspace_create_argv(cwd: &str, label: &str) -> Vec<String> {
    vec![
        "workspace".to_string(),
        "create".to_string(),
        "--cwd".to_string(),
        cwd.to_string(),
        "--label".to_string(),
        label.to_string(),
        "--focus".to_string(),
    ]
}

/// Build argv for `herdr workspace close` so a deleted mission can tear down
/// its leftover workspace instead of leaving an empty tab behind.
pub fn workspace_close_argv(workspace_id: &str) -> Vec<String> {
    vec![
        "workspace".to_string(),
        "close".to_string(),
        workspace_id.to_string(),
    ]
}

/// Build argv for `herdr tab create`, optionally targeting a workspace.
pub fn tab_create_argv(
    workspace: Option<&str>,
    cwd: Option<&str>,
    label: Option<&str>,
) -> Vec<String> {
    let mut argv = vec!["tab".to_string(), "create".to_string()];
    if let Some(id) = workspace {
        argv.extend(["--workspace".to_string(), id.to_string()]);
    }
    if let Some(path) = cwd {
        argv.extend(["--cwd".to_string(), path.to_string()]);
    }
    if let Some(label) = label {
        argv.extend(["--label".to_string(), label.to_string()]);
    }
    argv.push("--no-focus".to_string());
    argv
}

/// Build argv for `herdr tab get` so launch recovery can validate the region.
pub fn tab_get_argv(tab_id: &str) -> Vec<String> {
    vec!["tab".to_string(), "get".to_string(), tab_id.to_string()]
}

/// Build argv for listing tabs in one workspace during crash recovery.
pub fn tab_list_argv(workspace_id: &str) -> Vec<String> {
    vec![
        "tab".to_string(),
        "list".to_string(),
        "--workspace".to_string(),
        workspace_id.to_string(),
    ]
}

/// Build argv for `herdr tab rename` with the Mission region label.
pub fn tab_rename_argv(tab_id: &str, label: &str) -> Vec<String> {
    vec![
        "tab".to_string(),
        "rename".to_string(),
        tab_id.to_string(),
        label.to_string(),
    ]
}

/// Build argv for a sibling `herdr pane split` that preserves the caller's cwd
/// and keeps the user's focus unchanged.
pub fn pane_split_argv(direction: &str, cwd: &str) -> Vec<String> {
    vec![
        "pane".to_string(),
        "split".to_string(),
        "--current".to_string(),
        "--direction".to_string(),
        direction.to_string(),
        "--cwd".to_string(),
        cwd.to_string(),
        "--no-focus".to_string(),
    ]
}

/// Build argv for `herdr pane split` targeting a specific pane.
pub fn pane_split_in_argv(direction: &str, cwd: &str, pane_id: &str) -> Vec<String> {
    vec![
        "pane".to_string(),
        "split".to_string(),
        pane_id.to_string(),
        "--direction".to_string(),
        direction.to_string(),
        "--cwd".to_string(),
        cwd.to_string(),
        "--no-focus".to_string(),
    ]
}

/// Build argv for `herdr pane get` so launch recovery uses structured state.
pub fn pane_get_argv(pane_id: &str) -> Vec<String> {
    vec!["pane".to_string(), "get".to_string(), pane_id.to_string()]
}

/// Build argv for listing panes in one workspace during region recovery.
pub fn pane_list_argv(workspace_id: &str) -> Vec<String> {
    vec![
        "pane".to_string(),
        "list".to_string(),
        "--workspace".to_string(),
        workspace_id.to_string(),
    ]
}

/// Move an existing pane into the Mission work region without stealing focus.
pub fn pane_move_to_tab_argv(pane_id: &str, tab_id: &str, target_pane_id: &str) -> Vec<String> {
    vec![
        "pane".to_string(),
        "move".to_string(),
        pane_id.to_string(),
        "--tab".to_string(),
        tab_id.to_string(),
        "--split".to_string(),
        "right".to_string(),
        "--target-pane".to_string(),
        target_pane_id.to_string(),
        "--no-focus".to_string(),
    ]
}

/// Build argv for `herdr worktree create`.
pub fn worktree_create_argv(
    cwd: &str,
    branch: &str,
    base: &str,
    path: &str,
    label: &str,
) -> Vec<String> {
    vec![
        "worktree".to_string(),
        "create".to_string(),
        "--cwd".to_string(),
        cwd.to_string(),
        "--branch".to_string(),
        branch.to_string(),
        "--base".to_string(),
        base.to_string(),
        "--path".to_string(),
        path.to_string(),
        "--label".to_string(),
        label.to_string(),
        "--focus".to_string(),
    ]
}

/// Build argv for `herdr worktree open`.
pub fn worktree_open_argv(cwd: &str, path: &str, label: &str) -> Vec<String> {
    vec![
        "worktree".to_string(),
        "open".to_string(),
        "--cwd".to_string(),
        cwd.to_string(),
        "--path".to_string(),
        path.to_string(),
        "--label".to_string(),
        label.to_string(),
        "--focus".to_string(),
    ]
}

/// Build argv for `herdr pane rename` with a descriptive label.
pub fn pane_rename_argv(pane_id: &str, label: &str) -> Vec<String> {
    vec![
        "pane".to_string(),
        "rename".to_string(),
        pane_id.to_string(),
        label.to_string(),
    ]
}

/// Build argv for `herdr agent rename` so a manually joined role registers its
/// agent name with Herdr. Without this, `herdr agent prompt <name>` (the
/// delivery wake-up) resolves to `agent_not_found` and the assignment never
/// leaves `queued`.
pub fn agent_rename_argv(pane_id: &str, name: &str) -> Vec<String> {
    vec![
        "agent".to_string(),
        "rename".to_string(),
        pane_id.to_string(),
        name.to_string(),
    ]
}

/// Build argv for the structured Agent snapshot used by Mission reconciliation.
pub fn agent_list_argv() -> Vec<String> {
    vec!["agent".to_string(), "list".to_string()]
}

/// Build argv for `herdr pane run` to inject a command into a shell pane.
pub fn pane_run_argv(pane_id: &str, command: &str) -> Vec<String> {
    vec![
        "pane".to_string(),
        "run".to_string(),
        pane_id.to_string(),
        command.to_string(),
    ]
}

/// Parse the `herdr workspace create` response into typed identifiers.
pub fn parse_workspace_create(response: &str) -> Result<WorkspaceCreated, KernelError> {
    let value = parse_json(response, "workspace create")?;
    let result = field(&value, "result", "workspace create")?;
    Ok(WorkspaceCreated {
        workspace_id: nested_string(result, "workspace", "workspace_id", "workspace create")?,
        tab_id: nested_string(result, "tab", "tab_id", "workspace create")?,
        root_pane_id: nested_string(result, "root_pane", "pane_id", "workspace create")?,
    })
}

/// Parse the `herdr tab create` response into typed identifiers.
pub fn parse_tab_create(response: &str) -> Result<TabCreated, KernelError> {
    let value = parse_json(response, "tab create")?;
    let result = field(&value, "result", "tab create")?;
    Ok(TabCreated {
        tab_id: nested_string(result, "tab", "tab_id", "tab create")?,
        root_pane_id: nested_string(result, "root_pane", "pane_id", "tab create")?,
    })
}

/// Parse the `herdr pane split` response into the new pane id.
pub fn parse_pane_split(response: &str) -> Result<PaneCreated, KernelError> {
    let value = parse_json(response, "pane split")?;
    let result = field(&value, "result", "pane split")?;
    let pane = field(result, "pane", "pane split")?;
    Ok(PaneCreated {
        pane_id: string_field(pane, "pane_id", "pane split")?,
    })
}

/// Parse the structured `herdr pane get` response used for safe Agent adoption.
pub fn parse_pane_get(response: &str) -> Result<PaneInfo, KernelError> {
    let value = parse_json(response, "pane get")?;
    let result = field(&value, "result", "pane get")?;
    let pane = field(result, "pane", "pane get")?;
    let cwd = optional_string_field(pane, "foreground_cwd")
        .or_else(|| optional_string_field(pane, "cwd"))
        .ok_or_else(|| missing_field("pane get", "foreground_cwd|cwd"))?;
    let has_agent_session = pane
        .get("agent_session")
        .and_then(|session| session.get("value"))
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());

    Ok(PaneInfo {
        workspace_id: string_field(pane, "workspace_id", "pane get")?,
        tab_id: string_field(pane, "tab_id", "pane get")?,
        pane_id: string_field(pane, "pane_id", "pane get")?,
        agent: optional_string_field(pane, "agent").unwrap_or_default(),
        cwd,
        has_agent_session,
    })
}

/// Parse `herdr pane list --workspace` for a recovered region's root pane.
pub fn parse_pane_list(response: &str) -> Result<Vec<PaneLocation>, KernelError> {
    let value = parse_json(response, "pane list")?;
    let result = field(&value, "result", "pane list")?;
    let panes = result
        .get("panes")
        .and_then(Value::as_array)
        .ok_or_else(|| missing_field("pane list", "panes"))?;
    panes
        .iter()
        .map(|pane| {
            Ok(PaneLocation {
                workspace_id: string_field(pane, "workspace_id", "pane list")?,
                tab_id: string_field(pane, "tab_id", "pane list")?,
                pane_id: string_field(pane, "pane_id", "pane list")?,
            })
        })
        .collect()
}

/// Parse the structured `herdr tab get` response used for region validation.
pub fn parse_tab_get(response: &str) -> Result<TabInfo, KernelError> {
    let value = parse_json(response, "tab get")?;
    let result = field(&value, "result", "tab get")?;
    let tab = field(result, "tab", "tab get")?;
    Ok(TabInfo {
        workspace_id: string_field(tab, "workspace_id", "tab get")?,
        tab_id: string_field(tab, "tab_id", "tab get")?,
        label: string_field(tab, "label", "tab get")?,
    })
}

/// Parse `herdr tab list --workspace` for unique fixed-region discovery.
pub fn parse_tab_list(response: &str) -> Result<Vec<TabInfo>, KernelError> {
    let value = parse_json(response, "tab list")?;
    let result = field(&value, "result", "tab list")?;
    let tabs = result
        .get("tabs")
        .and_then(Value::as_array)
        .ok_or_else(|| missing_field("tab list", "tabs"))?;
    tabs.iter()
        .map(|tab| {
            Ok(TabInfo {
                workspace_id: string_field(tab, "workspace_id", "tab list")?,
                tab_id: string_field(tab, "tab_id", "tab list")?,
                label: string_field(tab, "label", "tab list")?,
            })
        })
        .collect()
}

/// Parse the `herdr worktree create` / `worktree open` response.
pub fn parse_worktree_create(response: &str) -> Result<WorktreeCreated, KernelError> {
    let value = parse_json(response, "worktree create")?;
    let result = field(&value, "result", "worktree create")?;
    let workspace_id = result
        .get("workspace")
        .and_then(|workspace| workspace.get("workspace_id"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .or_else(|| {
            result
                .get("worktree")
                .and_then(|worktree| worktree.get("open_workspace_id"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
        .ok_or_else(|| missing_field("worktree create", "workspace_id"))?;
    let nested_string = |object: &str, key: &str| {
        result
            .get(object)
            .and_then(|item| item.get(key))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .unwrap_or_default()
    };
    Ok(WorktreeCreated {
        workspace_id,
        tab_id: nested_string("tab", "tab_id"),
        root_pane_id: nested_string("root_pane", "pane_id"),
        worktree_path: nested_string("worktree", "path"),
        branch: nested_string("worktree", "branch"),
    })
}

/// Parse the complete `herdr agent list` response.
pub fn parse_agent_list(response: &str) -> Result<Vec<AgentSnapshot>, KernelError> {
    let value = parse_json(response, "agent list")?;
    let result = field(&value, "result", "agent list")?;
    let agents = result
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| missing_field("agent list", "agents"))?;

    agents
        .iter()
        .map(|agent| {
            let status_value = string_field(agent, "agent_status", "agent list")?;
            let status = AgentStatus::parse(&status_value).ok_or_else(|| KernelError {
                category: ErrorCategory::Contract,
                code: "herdr_agent_status_unrecognized".into(),
                message: "herdr returned an unrecognized Agent status".into(),
                retryable: false,
                details: BTreeMap::from([("agent_status".into(), json!(status_value))]),
            })?;
            Ok(AgentSnapshot {
                name: optional_nullable_string_field(agent, "name", "agent list")?,
                pane_id: string_field(agent, "pane_id", "agent list")?,
                status,
            })
        })
        .collect()
}

fn parse_json(response: &str, operation: &str) -> Result<Value, KernelError> {
    serde_json::from_str::<Value>(response).map_err(|error| KernelError {
        category: ErrorCategory::Transport,
        code: "herdr_response_malformed".into(),
        message: "herdr response is not valid JSON".into(),
        retryable: false,
        details: BTreeMap::from([
            ("operation".into(), json!(operation)),
            ("reason".into(), json!(error.to_string())),
        ]),
    })
}

fn field<'a>(value: &'a Value, key: &str, operation: &str) -> Result<&'a Value, KernelError> {
    value.get(key).ok_or_else(|| missing_field(operation, key))
}

fn string_field(value: &Value, key: &str, operation: &str) -> Result<String, KernelError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| missing_field(operation, key))
}

fn optional_string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn optional_nullable_string_field(
    value: &Value,
    key: &str,
    operation: &str,
) -> Result<Option<String>, KernelError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(KernelError {
            category: ErrorCategory::Transport,
            code: "herdr_response_invalid_field".into(),
            message: "herdr response field has an invalid type".into(),
            retryable: false,
            details: BTreeMap::from([
                ("operation".into(), json!(operation)),
                ("field".into(), json!(key)),
            ]),
        }),
    }
}

/// Read `value[object][key]` as a non-empty string. Herdr's `WorkspaceCreated`
/// and `TabCreated` responses nest their identifiers inside typed objects
/// (`workspace.workspace_id`, `tab.tab_id`, `root_pane.pane_id`), so callers
/// must drill one level deeper than a flat string field.
fn nested_string(
    value: &Value,
    object: &str,
    key: &str,
    operation: &str,
) -> Result<String, KernelError> {
    value
        .get(object)
        .and_then(|item| item.get(key))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| missing_field(operation, key))
}

fn missing_field(operation: &str, key: &str) -> KernelError {
    KernelError {
        category: ErrorCategory::Transport,
        code: "herdr_response_missing_field".into(),
        message: "herdr response is missing a required field".into(),
        retryable: false,
        details: BTreeMap::from([
            ("operation".into(), json!(operation)),
            ("field".into(), json!(key)),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_workspace_create_response() {
        let parsed = parse_workspace_create(
            r#"{"result":{"workspace":{"workspace_id":"w1"},"tab":{"tab_id":"w1:t1"},"root_pane":{"pane_id":"w1:p1"}}}"#,
        )
        .unwrap();
        assert_eq!(
            parsed,
            WorkspaceCreated {
                workspace_id: "w1".into(),
                tab_id: "w1:t1".into(),
                root_pane_id: "w1:p1".into(),
            }
        );
    }

    #[test]
    fn parses_tab_create_response() {
        let parsed = parse_tab_create(
            r#"{"result":{"tab":{"tab_id":"w1:t2"},"root_pane":{"pane_id":"w1:p2"}}}"#,
        )
        .unwrap();
        assert_eq!(
            parsed,
            TabCreated {
                tab_id: "w1:t2".into(),
                root_pane_id: "w1:p2".into(),
            }
        );
    }

    #[test]
    fn parses_pane_split_response() {
        let parsed = parse_pane_split(r#"{"result":{"pane":{"pane_id":"w1:p3"}}}"#).unwrap();
        assert_eq!(
            parsed,
            PaneCreated {
                pane_id: "w1:p3".into()
            }
        );
    }

    #[test]
    fn parses_pane_get_response_for_agent_recovery() {
        let parsed = parse_pane_get(
            r#"{"result":{"pane":{"agent":"codex","agent_session":{"agent":"codex","kind":"id","source":"herdr:codex","value":"session-1"},"cwd":"/repo","foreground_cwd":"/repo/.worktree/rust-version","pane_id":"w16:p1","tab_id":"w16:t1","workspace_id":"w16"},"type":"pane_info"}}"#,
        )
        .unwrap();
        assert_eq!(
            parsed,
            PaneInfo {
                workspace_id: "w16".into(),
                tab_id: "w16:t1".into(),
                pane_id: "w16:p1".into(),
                agent: "codex".into(),
                cwd: "/repo/.worktree/rust-version".into(),
                has_agent_session: true,
            }
        );
    }

    #[test]
    fn parses_tab_get_response_for_region_recovery() {
        let parsed = parse_tab_get(
            r#"{"result":{"tab":{"label":"工作区","tab_id":"w16:t1","workspace_id":"w16"},"type":"tab_info"}}"#,
        )
        .unwrap();
        assert_eq!(
            parsed,
            TabInfo {
                workspace_id: "w16".into(),
                tab_id: "w16:t1".into(),
                label: "工作区".into(),
            }
        );
    }

    #[test]
    fn builds_pane_get_and_tab_region_commands() {
        assert_eq!(pane_get_argv("w16:p1"), vec!["pane", "get", "w16:p1"]);
        assert_eq!(
            pane_list_argv("w16"),
            vec!["pane", "list", "--workspace", "w16"]
        );
        assert_eq!(tab_get_argv("w16:t1"), vec!["tab", "get", "w16:t1"]);
        assert_eq!(
            tab_list_argv("w16"),
            vec!["tab", "list", "--workspace", "w16"]
        );
        assert_eq!(
            tab_rename_argv("w16:t1", "工作区"),
            vec!["tab", "rename", "w16:t1", "工作区"]
        );
        assert_eq!(
            pane_move_to_tab_argv("w16:p2", "w16:t1", "w16:p1"),
            vec![
                "pane",
                "move",
                "w16:p2",
                "--tab",
                "w16:t1",
                "--split",
                "right",
                "--target-pane",
                "w16:p1",
                "--no-focus",
            ]
        );
    }

    #[test]
    fn parses_tab_list_for_region_discovery() {
        let tabs = parse_tab_list(
            r#"{"result":{"tabs":[{"workspace_id":"w16","tab_id":"w16:t1","label":"工作区"},{"workspace_id":"w16","tab_id":"w16:t2","label":"审查"}]}}"#,
        )
        .unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[1].label, "审查");
    }

    #[test]
    fn parses_pane_list_for_region_recovery() {
        let panes = parse_pane_list(
            r#"{"result":{"panes":[{"workspace_id":"w16","tab_id":"w16:t1","pane_id":"w16:p1"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            panes,
            vec![PaneLocation {
                workspace_id: "w16".into(),
                tab_id: "w16:t1".into(),
                pane_id: "w16:p1".into(),
            }]
        );
    }

    #[test]
    fn parses_worktree_create_response() {
        let parsed = parse_worktree_create(
            r#"{"result":{"workspace":{"workspace_id":"w1"},"worktree":{"branch":"feature/x-abc","path":"/repo/.worktree/x"},"tab":{"tab_id":"w1:t1"},"root_pane":{"pane_id":"w1:p1"}}}"#,
        )
        .unwrap();
        assert_eq!(
            parsed,
            WorktreeCreated {
                workspace_id: "w1".into(),
                tab_id: "w1:t1".into(),
                root_pane_id: "w1:p1".into(),
                worktree_path: "/repo/.worktree/x".into(),
                branch: "feature/x-abc".into(),
            }
        );
    }

    #[test]
    fn parses_agent_list_with_optional_names_and_all_supported_statuses() {
        let agents = parse_agent_list(
            r#"{"result":{"agents":[
                {"name":"mission-pm","pane_id":"w16:p1","agent_status":"idle"},
                {"name":"mission-worker","pane_id":"w16:p2","agent_status":"working"},
                {"name":"mission-scout","pane_id":"w16:p3","agent_status":"blocked"},
                {"name":"mission-reviewer","pane_id":"w16:p4","agent_status":"done"},
                {"pane_id":"w16:p5","agent_status":"unknown"}
            ]}}"#,
        )
        .unwrap();

        assert_eq!(agents.len(), 5);
        assert_eq!(agents[0].name.as_deref(), Some("mission-pm"));
        assert_eq!(agents[0].status, AgentStatus::Idle);
        assert_eq!(agents[1].status, AgentStatus::Working);
        assert_eq!(agents[2].status, AgentStatus::Blocked);
        assert_eq!(agents[3].status, AgentStatus::Done);
        assert_eq!(agents[4].name, None);
        assert_eq!(agents[4].status, AgentStatus::Unknown);
    }

    #[test]
    fn rejects_unrecognized_agent_status_and_malformed_agent_list_json() {
        let status_error = parse_agent_list(
            r#"{"result":{"agents":[{"name":"mission-pm","pane_id":"w16:p1","agent_status":"paused"}]}}"#,
        )
        .unwrap_err();
        assert_eq!(status_error.code, "herdr_agent_status_unrecognized");

        let json_error = parse_agent_list("not json").unwrap_err();
        assert_eq!(json_error.code, "herdr_response_malformed");

        let name_error = parse_agent_list(
            r#"{"result":{"agents":[{"name":123,"pane_id":"w16:p1","agent_status":"working"}]}}"#,
        )
        .unwrap_err();
        assert_eq!(name_error.code, "herdr_response_invalid_field");
    }

    #[test]
    fn builds_agent_list_argv() {
        assert_eq!(agent_list_argv(), vec!["agent", "list"]);
    }

    #[test]
    fn worktree_create_argv_builds_expected_command() {
        assert_eq!(
            worktree_create_argv("/repo", "feature/x-abc", "HEAD", "/repo/.worktree/x", "⚑ x"),
            vec![
                "worktree",
                "create",
                "--cwd",
                "/repo",
                "--branch",
                "feature/x-abc",
                "--base",
                "HEAD",
                "--path",
                "/repo/.worktree/x",
                "--label",
                "⚑ x",
                "--focus",
            ]
        );
    }

    #[test]
    fn workspace_close_argv_builds_expected_command() {
        assert_eq!(
            workspace_close_argv("w71"),
            vec!["workspace", "close", "w71"]
        );
    }

    #[test]
    fn rejects_missing_field() {
        let error = parse_pane_split(r#"{"result":{"pane":{}}}"#).unwrap_err();
        assert_eq!(error.code, "herdr_response_missing_field");
    }

    #[test]
    fn rejects_non_json() {
        let error = parse_workspace_create("not json").unwrap_err();
        assert_eq!(error.code, "herdr_response_malformed");
    }

    #[test]
    fn builds_pane_split_argv() {
        assert_eq!(
            pane_split_argv("right", "/tmp/mission"),
            vec![
                "pane",
                "split",
                "--current",
                "--direction",
                "right",
                "--cwd",
                "/tmp/mission",
                "--no-focus",
            ]
        );
    }
}
