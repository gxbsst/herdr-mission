//! Launch configuration loaded from an external, user-editable TOML file.
//!
//! The config controls how role/tool panes are laid out when a Mission starts.
//! It lives outside the plugin so users can tune it without recompiling, while
//! defaults keep the file optional: a missing or malformed config silently
//! falls back to the built-in behavior.

use std::path::PathBuf;

use serde::Deserialize;

pub const WORK_REGION_NAME: &str = "工作区";
pub const REVIEW_REGION_NAME: &str = "审查";
pub const VERIFICATION_REGION_NAME: &str = "验证";

/// How role/tool panes are laid out when a Mission starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabMode {
    /// All role panes are split into the current tab (the original behavior).
    #[default]
    Lanes,
    /// Legacy alias retained for config compatibility; Agents still share 工作区.
    Tabs,
}

/// How many team roles are launched up front when a Mission starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    /// Launch every role (PM/Worker/Scout/Reviewer) immediately.
    Auto,
    /// Launch only PM up front; PM pulls up other roles on demand.
    #[default]
    Manual,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LaunchSection {
    pub tab_mode: TabMode,
    pub launch_mode: LaunchMode,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TabsSection {
    pub tools: ToolsSection,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ToolsSection {
    pub editor: String,
    pub review: String,
    pub processes: String,
}

impl Default for ToolsSection {
    fn default() -> Self {
        Self {
            editor: "nvim".to_string(),
            review: "lazygit".to_string(),
            processes: "mprocs".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LaunchConfig {
    pub launch: LaunchSection,
    pub tabs: TabsSection,
}

impl LaunchConfig {
    /// Load from the configured path, falling back to defaults on any error.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => return Self::default(),
        };
        Self::parse(&raw).unwrap_or_default()
    }

    /// Parse a TOML document, rejecting malformed input.
    pub fn parse(raw: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(raw)
    }
}

fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HERDR_MISSION_CONFIG") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("herdr-mission")
            .join("config.toml"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_lanes() {
        assert_eq!(LaunchConfig::default().launch.tab_mode, TabMode::Lanes);
        assert_eq!(
            LaunchConfig::default().launch.launch_mode,
            LaunchMode::Manual
        );
    }

    #[test]
    fn parses_tabs_mode() {
        let config = LaunchConfig::parse("[launch]\ntab_mode = \"tabs\"\n").unwrap();
        assert_eq!(config.launch.tab_mode, TabMode::Tabs);
    }

    #[test]
    fn parses_launch_mode() {
        let config = LaunchConfig::parse("[launch]\nlaunch_mode = \"auto\"\n").unwrap();
        assert_eq!(config.launch.launch_mode, LaunchMode::Auto);
    }

    #[test]
    fn unknown_launch_mode_is_rejected() {
        assert!(LaunchConfig::parse("[launch]\nlaunch_mode = \"lazy\"\n").is_err());
    }

    #[test]
    fn missing_section_falls_back_to_defaults() {
        let config = LaunchConfig::parse("").unwrap();
        assert_eq!(config.launch.tab_mode, TabMode::Lanes);
        assert_eq!(config.launch.launch_mode, LaunchMode::Manual);
    }

    #[test]
    fn unknown_tab_mode_is_rejected() {
        assert!(LaunchConfig::parse("[launch]\ntab_mode = \"carousel\"\n").is_err());
    }

    #[test]
    fn default_regions_use_builtin_tools() {
        let config = LaunchConfig::default();
        assert_eq!(config.tabs.tools.editor, "nvim");
        assert_eq!(config.tabs.tools.review, "lazygit");
        assert_eq!(config.tabs.tools.processes, "mprocs");
    }

    #[test]
    fn legacy_region_toggles_cannot_disable_fixed_regions() {
        let config =
            LaunchConfig::parse("[tabs]\nreview = false\n[tabs.tools]\neditor = \"vim\"\n")
                .unwrap();
        assert_eq!(config.tabs.tools.editor, "vim");
        assert_eq!(config.tabs.tools.review, "lazygit");
        assert_eq!(WORK_REGION_NAME, "工作区");
        assert_eq!(REVIEW_REGION_NAME, "审查");
        assert_eq!(VERIFICATION_REGION_NAME, "验证");
    }

    #[test]
    fn legacy_region_name_overrides_cannot_change_fixed_names() {
        LaunchConfig::parse(
            "[tabs.names]\nexecution = \"工作区\"\nreview = \"审\"\nverification = \"验\"\n",
        )
        .unwrap();
        assert_eq!(WORK_REGION_NAME, "工作区");
        assert_eq!(REVIEW_REGION_NAME, "审查");
        assert_eq!(VERIFICATION_REGION_NAME, "验证");
    }
}
