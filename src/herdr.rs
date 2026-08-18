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
