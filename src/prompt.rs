//! Per-role initialization prompts, externalized as Markdown templates.
//!
//! Shipped role templates under `prompts/roles/*.md` are embedded at compile
//! time so the binary works standalone, and can be overridden at runtime by
//! pointing `prompts_dir` at a directory containing `{role}.md`. This keeps
//! prompt wording editable without recompiling, matching the legacy kit's
//! external SKILL files but with a simpler `{{placeholder}}` template.

use std::{collections::BTreeMap, fs, path::Path};

use crate::{ErrorCategory, KernelError};

const DEFAULT_PM: &str = include_str!("../prompts/roles/pm.md");
const DEFAULT_WORKER: &str = include_str!("../prompts/roles/worker.md");
const DEFAULT_SCOUT: &str = include_str!("../prompts/roles/scout.md");
const DEFAULT_REVIEWER: &str = include_str!("../prompts/roles/reviewer.md");

fn default_template(role: &str) -> &'static str {
    match crate::role_kind(role) {
        "pm" => DEFAULT_PM,
        "worker" => DEFAULT_WORKER,
        "scout" => DEFAULT_SCOUT,
        "reviewer" => DEFAULT_REVIEWER,
        _ => DEFAULT_WORKER,
    }
}

/// Build the initialization prompt delivered to a freshly started role agent.
///
/// `prompts_dir`, when set and containing `{role}.md`, overrides the embedded
/// default template. A missing override file falls back silently to the
/// embedded default; only an unreadable file that actually exists is an error.
#[allow(clippy::too_many_arguments)]
pub fn role_init_prompt(
    mission_title: &str,
    mission_id: &str,
    role: &str,
    worktree: &str,
    autonomy: &str,
    database: &str,
    bin: &str,
    prompts_dir: Option<&Path>,
) -> Result<String, KernelError> {
    let template = load_template(role, prompts_dir)?;
    Ok(template
        .replace("{{title}}", mission_title)
        .replace("{{mission_id}}", mission_id)
        .replace("{{role}}", role)
        .replace("{{worktree}}", worktree)
        .replace("{{autonomy}}", autonomy)
        .replace("{{database}}", database)
        .replace("{{bin}}", bin))
}

/// Resolve a role template with instance → kind → embedded fallback.
fn load_template(role: &str, prompts_dir: Option<&Path>) -> Result<String, KernelError> {
    let kind = crate::role_kind(role);
    if let Some(dir) = prompts_dir {
        let instance_file = dir.join(format!("{role}.md"));
        if instance_file.exists() {
            return read_template(&instance_file);
        }
        if role != kind {
            let kind_file = dir.join(format!("{kind}.md"));
            if kind_file.exists() {
                return read_template(&kind_file);
            }
        }
    }
    Ok(default_template(role).to_string())
}

fn read_template(path: &Path) -> Result<String, KernelError> {
    fs::read_to_string(path).map_err(|error| KernelError {
        category: ErrorCategory::Infrastructure,
        code: "prompt_template_read_failed".into(),
        message: "failed to read role prompt template".into(),
        retryable: false,
        details: BTreeMap::from([
            ("path".into(), serde_json::json!(path)),
            ("reason".into(), serde_json::json!(error.to_string())),
        ]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(role: &str, prompts_dir: Option<&Path>) -> String {
        role_init_prompt(
            "Fix dispatch",
            "msn-20260815-120000-demo-1a2b3c4d",
            role,
            "/tmp/work",
            "manual",
            "/tmp/herdr-mission.sqlite3",
            "/opt/herdr-mission",
            prompts_dir,
        )
        .unwrap()
    }

    #[test]
    fn embedded_pm_prompt_carries_identity_and_context() {
        let prompt = render("pm", None);
        assert!(prompt.contains("PM"));
        assert!(prompt.contains("Fix dispatch"));
        assert!(prompt.contains("msn-20260815-120000-demo-1a2b3c4d"));
        assert!(prompt.contains("/tmp/work"));
        assert!(prompt.contains("manual"));
        assert!(prompt.contains("/tmp/herdr-mission.sqlite3"));
        assert!(prompt.contains("/opt/herdr-mission"));
        assert!(prompt.contains("--role=pm"));
        assert!(prompt
            .lines()
            .any(|line| { line.contains(" start-role ") && line.contains("--role=reviewer") }));
        assert!(prompt.contains("deliver --json"));
        assert!(!prompt
            .lines()
            .any(|line| line.contains(" init ") && line.contains("--role=reviewer")));
        assert!(!prompt
            .lines()
            .any(|line| line.contains(" send ") && line.contains("--kind=review")));
        assert!(!prompt.contains("{{database}}"));
        assert!(!prompt.contains("{{bin}}"));
        assert!(!prompt.contains("{{role}}"));
    }

    #[test]
    fn worker_prompt_differs_from_scout_prompt() {
        let worker = render("worker", None);
        let scout = render("scout", None);
        assert_ne!(worker, scout);
        assert!(worker.contains("修改"));
        assert!(scout.contains("只读"));
    }

    #[test]
    fn external_template_overrides_embedded_default() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-mission-prompt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pm.md"), "自定义 {{title}} / {{mission_id}}").unwrap();

        let prompt = render("pm", Some(&dir));
        assert_eq!(
            prompt,
            "自定义 Fix dispatch / msn-20260815-120000-demo-1a2b3c4d"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_external_template_falls_back_to_embedded() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-mission-prompt-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let prompt = render("pm", Some(&dir));
        assert!(prompt.contains("PM"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn instance_prompt_falls_back_to_kind_then_embedded() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-mission-prompt-instance-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Provide a kind template but no instance template.
        std::fs::write(dir.join("scout.md"), "kind {{title}}").unwrap();

        let prompt = render("scout-01", Some(&dir));
        assert_eq!(prompt, "kind Fix dispatch");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
