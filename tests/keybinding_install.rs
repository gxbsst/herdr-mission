use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct ConfigFixture {
    root: PathBuf,
    config: PathBuf,
}

impl ConfigFixture {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "herdr-mission-keybinding-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let config = root.join("herdr/config.toml");
        fs::create_dir_all(&root).unwrap();
        Self { root, config }
    }

    fn run(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_herdr-mission"))
            .args([
                "install-keybinding",
                "--config",
                self.config.to_str().unwrap(),
                "--no-reload",
            ])
            .output()
            .unwrap()
    }

    fn run_with_default_path(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_herdr-mission"))
            .args(["install-keybinding", "--no-reload"])
            .env("XDG_CONFIG_HOME", &self.root)
            .env_remove("HERDR_CONFIG_PATH")
            .output()
            .unwrap()
    }

    fn run_with_missing_config_value(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_herdr-mission"))
            .args(["install-keybinding", "--config"])
            .env("HERDR_CONFIG_PATH", &self.config)
            .output()
            .unwrap()
    }

    fn run_with_relative_config_path(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_herdr-mission"))
            .args([
                "install-keybinding",
                "--config",
                "config.toml",
                "--no-reload",
            ])
            .current_dir(&self.root)
            .output()
            .unwrap()
    }

    #[cfg(unix)]
    fn run_with_reload_capture(&self, capture: &Path) -> Output {
        let herdr = self.root.join("herdr-stub");
        fs::write(
            &herdr,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HERDR_RELOAD_CAPTURE\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&herdr).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&herdr, permissions).unwrap();
        Command::new(env!("CARGO_BIN_EXE_herdr-mission"))
            .args([
                "install-keybinding",
                "--config",
                self.config.to_str().unwrap(),
            ])
            .env("HERDR_BIN_PATH", herdr)
            .env("HERDR_RELOAD_CAPTURE", capture)
            .output()
            .unwrap()
    }

    #[cfg(unix)]
    fn run_with_failing_reload(&self) -> Output {
        let herdr = self.root.join("herdr-failing-stub");
        fs::write(&herdr, "#!/bin/sh\nexit 9\n").unwrap();
        let mut permissions = fs::metadata(&herdr).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&herdr, permissions).unwrap();
        Command::new(env!("CARGO_BIN_EXE_herdr-mission"))
            .args([
                "install-keybinding",
                "--config",
                self.config.to_str().unwrap(),
            ])
            .env("HERDR_BIN_PATH", herdr)
            .output()
            .unwrap()
    }

    fn read(&self) -> String {
        fs::read_to_string(&self.config).unwrap()
    }

    fn write(&self, content: &str) {
        fs::create_dir_all(self.config.parent().unwrap()).unwrap();
        fs::write(&self.config, content).unwrap();
    }
}

impl Drop for ConfigFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn new_user_install_creates_ctrl_a_mission_shortcut() {
    let fixture = ConfigFixture::new("new-user");

    let output = fixture.run();

    assert_success(&output);
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "installed");
    assert_eq!(result["key"], "prefix+m");
    assert_eq!(result["prefix"], "ctrl+a");
    assert_eq!(
        fixture.read(),
        concat!(
            "[keys]\n",
            "prefix = \"ctrl+a\"\n",
            "\n",
            "[[keys.command]]\n",
            "key = \"prefix+m\"\n",
            "type = \"shell\"\n",
            "command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\"\n",
            "description = \"打开 Mission 看板\"\n",
        )
    );
}

#[test]
fn existing_user_prefix_and_config_are_preserved() {
    let fixture = ConfigFixture::new("existing-prefix");
    fixture.write(concat!(
        "# keep this comment\n",
        "[keys]\n",
        "prefix = \"ctrl+b\"\n",
        "new_tab = \"prefix+t\"\n",
        "\n",
        "[ui]\n",
        "sidebar_width = 40\n",
    ));

    let output = fixture.run();

    assert_success(&output);
    assert_eq!(
        fixture.read(),
        concat!(
            "# keep this comment\n",
            "[keys]\n",
            "prefix = \"ctrl+b\"\n",
            "new_tab = \"prefix+t\"\n",
            "\n",
            "[ui]\n",
            "sidebar_width = 40\n",
            "\n",
            "[[keys.command]]\n",
            "key = \"prefix+m\"\n",
            "type = \"shell\"\n",
            "command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\"\n",
            "description = \"打开 Mission 看板\"\n",
        )
    );
}

#[test]
fn inline_keys_table_preserves_custom_prefix_comment_and_reinstall_is_idempotent() {
    let fixture = ConfigFixture::new("inline-keys");
    fixture.write(concat!(
        "keys = { prefix = \"ctrl+b\" } # keep inline keys\n",
        "[ui]\n",
        "sidebar_width = 40\n",
    ));

    let first = fixture.run();

    assert_success(&first);
    let updated = fixture.read();
    assert_eq!(
        updated,
        concat!(
            "keys = { prefix = \"ctrl+b\", command = [{ key = \"prefix+m\", type = \"shell\", command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\", description = \"打开 Mission 看板\" }] } # keep inline keys\n",
            "[ui]\n",
            "sidebar_width = 40\n",
        )
    );

    let second = fixture.run();

    assert_success(&second);
    assert_eq!(fixture.read(), updated);
}

#[test]
fn inline_command_array_keeps_existing_binding_and_appends_mission_binding() {
    let fixture = ConfigFixture::new("inline-command-array");
    fixture.write(concat!(
        "keys = { prefix = \"ctrl+b\", command = [{ key = \"prefix+f\", command = \"open-files\" }] } # keep inline commands\n",
        "[ui]\n",
        "sidebar_width = 40\n",
    ));

    let output = fixture.run();

    assert_success(&output);
    let updated = fixture.read();
    assert_eq!(
        updated,
        concat!(
            "keys = { prefix = \"ctrl+b\", command = [{ key = \"prefix+f\", command = \"open-files\" }, { key = \"prefix+m\", type = \"shell\", command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\", description = \"打开 Mission 看板\" }] } # keep inline commands\n",
            "[ui]\n",
            "sidebar_width = 40\n",
        )
    );
}

#[test]
fn quoted_inline_keys_table_gets_mission_binding() {
    let fixture = ConfigFixture::new("quoted-inline-keys");
    fixture.write("\"keys\" = { prefix = \"ctrl+a\" } # keep quoted key\n");

    let output = fixture.run();

    assert_success(&output);
    assert_eq!(
        fixture.read(),
        concat!(
            "\"keys\" = { prefix = \"ctrl+a\", command = [{ key = \"prefix+m\", type = \"shell\", command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\", description = \"打开 Mission 看板\" }] } # keep quoted key\n",
        )
    );
}

#[test]
fn quoted_inline_command_field_keeps_existing_binding() {
    let fixture = ConfigFixture::new("quoted-inline-command");
    fixture.write(concat!(
        "keys = { prefix = \"ctrl+b\", \"command\" = [{ key = \"prefix+f\", command = \"open-files\" }] }\n",
    ));

    let output = fixture.run();

    assert_success(&output);
    assert_eq!(
        fixture.read(),
        concat!(
            "keys = { prefix = \"ctrl+b\", \"command\" = [{ key = \"prefix+f\", command = \"open-files\" }, { key = \"prefix+m\", type = \"shell\", command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\", description = \"打开 Mission 看板\" }] }\n",
        )
    );
}

#[test]
fn reinstall_keeps_a_single_mission_shortcut() {
    let fixture = ConfigFixture::new("reinstall");
    assert_success(&fixture.run());
    let first_install = fixture.read();

    let output = fixture.run();

    assert_success(&output);
    assert_eq!(fixture.read(), first_install);
    assert_eq!(first_install.matches("key = \"prefix+m\"").count(), 1);
}

#[test]
fn occupied_prefix_m_fails_without_changing_user_config() {
    let fixture = ConfigFixture::new("occupied-prefix-m");
    let original = concat!(
        "[keys]\n",
        "prefix = \"ctrl+a\"\n",
        "\n",
        "[[keys.command]]\n",
        "key = \"prefix+m\"\n",
        "type = \"shell\"\n",
        "command = \"open-my-menu\"\n",
    );
    fixture.write(original);

    let output = fixture.run();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("prefix+m is already assigned"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "error");
    assert_eq!(result["error"]["code"], "keybinding_conflict");
    assert_eq!(fixture.read(), original);
}

#[test]
fn invalid_prefix_type_fails_without_changing_user_config() {
    let fixture = ConfigFixture::new("invalid-prefix-type");
    let original = "[keys]\nprefix = 1\n";
    fixture.write(original);

    let output = fixture.run();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("keys.prefix must be a string"));
    assert_eq!(fixture.read(), original);
}

#[test]
fn existing_config_without_prefix_preserves_herdr_default_prefix() {
    let fixture = ConfigFixture::new("implicit-keys-table");
    fixture.write(concat!(
        "[theme]\n",
        "name = \"nord\"\n",
        "\n",
        "[[keys.command]]\n",
        "key = \"prefix+f\"\n",
        "command = \"open-files\"\n",
    ));

    let output = fixture.run();

    assert_success(&output);
    assert_eq!(
        fixture.read(),
        concat!(
            "[theme]\n",
            "name = \"nord\"\n",
            "\n",
            "[[keys.command]]\n",
            "key = \"prefix+f\"\n",
            "command = \"open-files\"\n",
            "\n",
            "[[keys.command]]\n",
            "key = \"prefix+m\"\n",
            "type = \"shell\"\n",
            "command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\"\n",
            "description = \"打开 Mission 看板\"\n",
        )
    );
}

#[test]
fn built_in_action_using_prefix_m_fails_without_changing_config() {
    let fixture = ConfigFixture::new("built-in-conflict");
    let original = concat!("[keys]\n", "prefix = \"ctrl+a\"\n", "help = \"prefix+m\"\n",);
    fixture.write(original);

    let output = fixture.run();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("prefix+m is already assigned"));
    assert_eq!(fixture.read(), original);
}

#[test]
fn install_uses_herdr_xdg_config_path_by_default() {
    let fixture = ConfigFixture::new("xdg-config");

    let output = fixture.run_with_default_path();

    assert_success(&output);
    assert!(fixture.read().contains("prefix = \"ctrl+a\""));
    assert!(fixture.read().contains("key = \"prefix+m\""));
}

#[test]
fn missing_config_option_value_fails_without_writing_the_default_path() {
    let fixture = ConfigFixture::new("missing-config-value");

    let output = fixture.run_with_missing_config_value();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--config requires a path"));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "error");
    assert_eq!(result["error"]["code"], "malformed_args");
    assert!(!fixture.config.exists());
}

#[test]
fn relative_config_path_is_resolved_from_the_current_directory() {
    let fixture = ConfigFixture::new("relative-config");

    let output = fixture.run_with_relative_config_path();

    assert_success(&output);
    let config = fs::read_to_string(fixture.root.join("config.toml")).unwrap();
    assert!(config.contains("prefix = \"ctrl+a\""));
    assert!(config.contains("key = \"prefix+m\""));
}

#[cfg(unix)]
#[test]
fn install_preserves_a_symlinked_config_path() {
    let fixture = ConfigFixture::new("symlink-config");
    let target = fixture.root.join("dotfiles/herdr-config.toml");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "[ui]\nsidebar_width = 40\n").unwrap();
    fs::create_dir_all(fixture.config.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &fixture.config).unwrap();

    let output = fixture.run();

    assert_success(&output);
    assert!(fs::symlink_metadata(&fixture.config)
        .unwrap()
        .file_type()
        .is_symlink());
    let updated = fs::read_to_string(target).unwrap();
    assert!(!updated.contains("prefix ="));
    assert!(updated.contains("key = \"prefix+m\""));
}

#[cfg(unix)]
#[test]
fn dangling_symlink_config_fails_without_replacing_the_link() {
    let fixture = ConfigFixture::new("dangling-symlink-config");
    let target = fixture.root.join("missing/herdr-config.toml");
    fs::create_dir_all(fixture.config.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &fixture.config).unwrap();

    let output = fixture.run();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("dangling symlink"));
    assert!(fs::symlink_metadata(&fixture.config)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_link(&fixture.config).unwrap(), target);
}

#[test]
fn plugin_install_manifest_runs_keybinding_setup_after_binary_build() {
    let manifest =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin.toml"))
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();
    let builds = manifest["build"].as_array().unwrap();

    assert_eq!(builds.len(), 2);
    assert_eq!(
        builds[1]["command"].as_array().unwrap(),
        &[
            toml::Value::String("./target/release/herdr-mission".into()),
            toml::Value::String("install-keybinding".into()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn install_reloads_running_herdr_config_after_write() {
    let fixture = ConfigFixture::new("reload");
    let capture = fixture.root.join("reload-argv");

    let output = fixture.run_with_reload_capture(&capture);

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(capture).unwrap(),
        "server\nreload-config\n"
    );
}

#[cfg(unix)]
#[test]
fn unavailable_config_reload_does_not_undo_the_installed_shortcut() {
    let fixture = ConfigFixture::new("reload-unavailable");

    let output = fixture.run_with_failing_reload();

    assert_success(&output);
    assert!(fixture.read().contains("prefix = \"ctrl+a\""));
    assert!(fixture.read().contains("key = \"prefix+m\""));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Herdr config updated; it will apply when the server next starts"));
}

#[test]
fn keys_header_without_trailing_newline_remains_valid() {
    let fixture = ConfigFixture::new("header-without-newline");
    fixture.write("[keys]");

    let output = fixture.run();

    assert_success(&output);
    assert_eq!(
        fixture.read(),
        concat!(
            "[keys]\n",
            "\n",
            "[[keys.command]]\n",
            "key = \"prefix+m\"\n",
            "type = \"shell\"\n",
            "command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\"\n",
            "description = \"打开 Mission 看板\"\n",
        )
    );
}

#[test]
fn keys_header_with_inline_comment_gets_binding_without_duplicate_table() {
    let fixture = ConfigFixture::new("header-inline-comment");
    fixture.write("[keys] # keep this comment\nnew_tab = \"prefix+t\"\n");

    let output = fixture.run();

    assert_success(&output);
    assert_eq!(
        fixture.read(),
        concat!(
            "[keys] # keep this comment\n",
            "new_tab = \"prefix+t\"\n",
            "\n",
            "[[keys.command]]\n",
            "key = \"prefix+m\"\n",
            "type = \"shell\"\n",
            "command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\"\n",
            "description = \"打开 Mission 看板\"\n",
        )
    );
}

#[test]
fn existing_mission_binding_without_prefix_is_left_unchanged() {
    let fixture = ConfigFixture::new("binding-without-prefix");
    fixture.write(concat!(
        "[[keys.command]]\n",
        "key = \"prefix+m\"\n",
        "type = \"shell\"\n",
        "command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\"\n",
        "description = \"打开 Mission 看板\"\n",
    ));

    let output = fixture.run();

    assert_success(&output);
    let updated = fixture.read();
    assert!(!updated.contains("prefix ="));
    assert_eq!(updated.matches("key = \"prefix+m\"").count(), 1);
}
