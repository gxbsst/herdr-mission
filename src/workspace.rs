//! Mission workspace lifecycle state.
//!
//! Every Mission owns a dedicated Herdr workspace (mirroring the Python v2
//! flow). The workspace source is chosen at creation; the resulting workspace
//! identifiers are persisted in a plugin-owned table so resume can re-enter
//! the same workspace instead of creating a new one.

use std::{collections::BTreeMap, path::Path};

use rusqlite::{params, OptionalExtension};
use serde_json::json;

use crate::{open_writable, ErrorCategory, KernelError, ProcessRunner, OWNER_IDENTITY};

/// How the Mission's workspace is provisioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceSource {
    #[default]
    /// A separate workspace rooted at the current project directory.
    Current,
    /// A new Git worktree plus a new workspace.
    Worktree,
    /// An existing linked Git worktree reused as the workspace.
    Import,
}

impl WorkspaceSource {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "current" | "register" => Some(Self::Current),
            "worktree" | "new" => Some(Self::Worktree),
            "import" => Some(Self::Import),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Worktree => "worktree",
            Self::Import => "import",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Current => "当前项目",
            Self::Worktree => "新建 Worktree",
            Self::Import => "导入 Worktree",
        }
    }
}

/// Persisted workspace identity for a Mission.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MissionWorkspace {
    pub source: WorkspaceSource,
    pub workspace_id: String,
    pub tab_id: String,
    pub root_pane_id: String,
    pub execution_tab_id: String,
    pub review_tab_id: String,
    pub verification_tab_id: String,
    pub worktree_path: String,
    pub branch: String,
}

/// The Herdr workspace label for a Mission, matching the Python v2 convention.
pub fn workspace_label(title: &str) -> String {
    format!("⚑ {title}")
}

/// Read the persisted workspace identity for a Mission, if any.
pub fn read_workspace(
    database: &Path,
    mission_id: &str,
) -> Result<Option<MissionWorkspace>, KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    let row = connection
        .query_row(
            "SELECT source, workspace_id, tab_id, root_pane_id, execution_tab_id,
                    review_tab_id, verification_tab_id, worktree_path, branch
             FROM mission_workspace WHERE mission_id = ?1",
            [mission_id],
            |row| {
                let source: String = row.get(0)?;
                Ok(MissionWorkspace {
                    source: WorkspaceSource::parse(&source).unwrap_or(WorkspaceSource::Current),
                    workspace_id: row.get(1)?,
                    tab_id: row.get(2)?,
                    root_pane_id: row.get(3)?,
                    execution_tab_id: row.get(4)?,
                    review_tab_id: row.get(5)?,
                    verification_tab_id: row.get(6)?,
                    worktree_path: row.get(7)?,
                    branch: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(|error| sqlite_error("sqlite_workspace_read_failed", "read_workspace", error))?;
    Ok(row)
}

/// Persist (or replace) a Mission's workspace identity.
pub fn upsert_workspace(
    database: &Path,
    mission_id: &str,
    workspace: &MissionWorkspace,
) -> Result<(), KernelError> {
    let connection = open_writable(database, OWNER_IDENTITY)?;
    connection
        .execute(
            "INSERT INTO mission_workspace(
                mission_id, source, workspace_id, tab_id, root_pane_id, execution_tab_id,
                review_tab_id, verification_tab_id, worktree_path, branch
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(mission_id) DO UPDATE SET
                source = excluded.source,
                workspace_id = excluded.workspace_id,
                tab_id = excluded.tab_id,
                root_pane_id = excluded.root_pane_id,
                execution_tab_id = excluded.execution_tab_id,
                review_tab_id = excluded.review_tab_id,
                verification_tab_id = excluded.verification_tab_id,
                worktree_path = excluded.worktree_path,
                branch = excluded.branch",
            params![
                mission_id,
                workspace.source.as_str(),
                workspace.workspace_id,
                workspace.tab_id,
                workspace.root_pane_id,
                workspace.execution_tab_id,
                workspace.review_tab_id,
                workspace.verification_tab_id,
                workspace.worktree_path,
                workspace.branch,
            ],
        )
        .map_err(|error| {
            sqlite_error("sqlite_workspace_write_failed", "upsert_workspace", error)
        })?;
    Ok(())
}

fn sqlite_error(code: &str, operation: &str, error: rusqlite::Error) -> KernelError {
    KernelError {
        category: ErrorCategory::Infrastructure,
        code: code.into(),
        message: "SQLite operation failed".into(),
        retryable: false,
        details: BTreeMap::from([
            ("operation".into(), json!(operation)),
            ("reason".into(), json!(error.to_string())),
        ]),
    }
}

/// Run `git` rooted at `cwd` and return trimmed stdout, failing on any error.
pub fn run_git(
    runner: &dyn ProcessRunner,
    cwd: &str,
    args: &[&str],
) -> Result<String, KernelError> {
    let argv: Vec<String> = std::iter::once("-C".to_string())
        .chain(std::iter::once(cwd.to_string()))
        .chain(args.iter().map(|arg| arg.to_string()))
        .collect();
    let output = runner.run("git", &argv).map_err(|error| KernelError {
        category: ErrorCategory::Infrastructure,
        code: "git_spawn_failed".into(),
        message: "failed to run git".into(),
        retryable: false,
        details: BTreeMap::from([("reason".into(), json!(error.to_string()))]),
    })?;
    if output.exit_code != 0 {
        return Err(KernelError {
            category: ErrorCategory::Operation,
            code: "git_command_failed".into(),
            message: "git command failed".into(),
            retryable: false,
            details: BTreeMap::from([("stderr".into(), json!(output.stderr))]),
        });
    }
    Ok(output.stdout.trim().to_string())
}

/// Resolve the repository root for a path.
pub fn git_root(runner: &dyn ProcessRunner, cwd: &str) -> Result<String, KernelError> {
    run_git(runner, cwd, &["rev-parse", "--show-toplevel"])
}

/// Resolve the primary (first) worktree for a repository.
pub fn primary_worktree(runner: &dyn ProcessRunner, cwd: &str) -> Result<String, KernelError> {
    let listing = run_git(runner, cwd, &["worktree", "list", "--porcelain"])?;
    listing
        .lines()
        .find_map(|line| line.strip_prefix("worktree ").map(str::to_string))
        .ok_or_else(|| KernelError {
            category: ErrorCategory::Operation,
            code: "primary_worktree_not_found".into(),
            message: "no primary git worktree found".into(),
            retryable: false,
            details: BTreeMap::new(),
        })
}

/// Resolve the repository HEAD commit.
pub fn git_head(runner: &dyn ProcessRunner, cwd: &str) -> Result<String, KernelError> {
    run_git(runner, cwd, &["rev-parse", "HEAD"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_source_parses_tokens() {
        assert_eq!(
            WorkspaceSource::parse("current"),
            Some(WorkspaceSource::Current)
        );
        assert_eq!(
            WorkspaceSource::parse("worktree"),
            Some(WorkspaceSource::Worktree)
        );
        assert_eq!(
            WorkspaceSource::parse("import"),
            Some(WorkspaceSource::Import)
        );
        assert_eq!(WorkspaceSource::parse("nope"), None);
        assert_eq!(WorkspaceSource::Current.as_str(), "current");
    }

    #[test]
    fn workspace_label_uses_mission_flag() {
        assert_eq!(workspace_label("集成验证"), "⚑ 集成验证");
    }
}
