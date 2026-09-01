#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

use herdr_mission::sha256_hex;

const TAG: &str = "v9.8.7";
const VERSION: &str = "9.8.7";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const CLI_ASSET: &str = "herdr-mission-aarch64-apple-darwin";
const SKILL_ASSET: &str = "herdr-mission-team.skill.tar.gz";
const REAL_CLI: &str = env!("CARGO_BIN_EXE_herdr-mission");

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct InstallerFixture {
    root: PathBuf,
    home: PathBuf,
    stub_bin: PathBuf,
    release_base: PathBuf,
    herdr_capture: PathBuf,
}

impl InstallerFixture {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "herdr-mission-unified-install-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let home = root.join("home");
        let stub_bin = root.join("stub-bin");
        let release_base = root.join("releases/download");
        let herdr_capture = root.join("herdr-argv");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&stub_bin).unwrap();
        fs::create_dir_all(&release_base).unwrap();

        write_executable(
            &stub_bin.join("uname"),
            "#!/bin/sh\ncase \"${1:-}\" in\n  -s) echo Darwin ;;\n  -m) echo arm64 ;;\n  *) echo Darwin ;;\nesac\n",
        );
        let plugin_json = format!(
            concat!(
                "{{\"id\":\"cli:plugin\",\"result\":{{\"plugins\":[{{",
                "\"plugin_id\":\"weston.herdr-mission\",",
                "\"source\":{{\"installed_unix_ms\":1788234567000,",
                "\"kind\":\"github\",\"owner\":\"gxbsst\",",
                "\"repo\":\"herdr-mission\",\"requested_ref\":\"{tag}\",",
                "\"resolved_commit\":\"{commit}\"}}}}],",
                "\"type\":\"plugin_list\"}}}}"
            ),
            tag = TAG,
            commit = COMMIT,
        );
        write_executable(
            &stub_bin.join("herdr"),
            &format!(
                concat!(
                    "#!/bin/sh\n",
                    "printf '%s\\n' \"$*\" >> \"$HERDR_TEST_CAPTURE\"\n",
                    "if [ \"${{1:-}} ${{2:-}}\" = 'plugin install' ]; then exit 0; fi\n",
                    "if [ \"${{1:-}} ${{2:-}}\" = 'plugin list' ]; then\n",
                    "  if [ -n \"${{HERDR_TEST_REPLACE_TARGET:-}}\" ]; then\n",
                    "    rm -rf \"$HERDR_TEST_REPLACE_TARGET\"\n",
                    "    mkdir -p \"$HERDR_TEST_REPLACE_TARGET\"\n",
                    "    printf '%s\\n' foreign > \"$HERDR_TEST_REPLACE_TARGET/foreign.txt\"\n",
                    "  fi\n",
                    "  printf '%s\\n' '{}'\n",
                    "  exit 0\n",
                    "fi\n",
                    "exit 64\n",
                ),
                plugin_json
            ),
        );

        let fixture = Self {
            root,
            home,
            stub_bin,
            release_base,
            herdr_capture,
        };
        fixture.stage_release();
        fixture
    }

    fn stage_release(&self) {
        let release = self.release_dir();
        let payload = self.root.join("payload/herdr-mission-team");
        fs::create_dir_all(&release).unwrap();
        fs::create_dir_all(&payload).unwrap();

        let cli = release.join(CLI_ASSET);
        write_executable(
            &cli,
            &format!(
                "#!/bin/sh\nif [ \"${{1:-}}\" = '__install-skill-copy' ]; then\n  if [ \"${{HERDR_TEST_FAIL_FRESH_PUBLISH:-}}\" = 1 ]; then exit 73; fi\n  if [ -n \"${{HERDR_TEST_LATE_REPLACE_TARGET:-}}\" ] && [ ! -e \"${{HERDR_TEST_LATE_REPLACE_BACKUP:-}}\" ]; then\n    /bin/mkdir -p \"$HERDR_TEST_LATE_REPLACE_TARGET\"\n    printf '%s\\n' foreign-skill > \"$HERDR_TEST_LATE_REPLACE_TARGET/SKILL.md\"\n    : > \"$HERDR_TEST_LATE_REPLACE_BACKUP\"\n  fi\n  exec \"$HERDR_TEST_REAL_CLI\" \"$@\"\nfi\nprintf '%s\\n' '{{\"binary\":\"herdr-mission\",\"binary_version\":\"{VERSION}\"}}'\n"
            ),
        );

        fs::write(
            payload.join("SKILL.md"),
            "---\nname: herdr-mission-team\ndescription: Test skill.\n---\n\n# Test skill\n",
        )
        .unwrap();
        let archive = release.join(SKILL_ASSET);
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(payload.parent().unwrap())
            .arg("herdr-mission-team")
            .status()
            .unwrap();
        assert!(status.success());

        fs::write(release.join("COMMIT"), COMMIT).unwrap();
        fs::write(
            release.join("SHA256SUMS"),
            format!(
                "{}  {CLI_ASSET}\n{}  {SKILL_ASSET}\n",
                sha256_hex(&fs::read(&cli).unwrap()),
                sha256_hex(&fs::read(&archive).unwrap())
            ),
        )
        .unwrap();
    }

    fn run(&self, agents: &str) -> Output {
        self.run_args(&["--yes", "--agents", agents])
    }

    fn run_rendered(&self, agents: &str) -> Output {
        let rendered = self.root.join("install.sh");
        fs::write(
            &rendered,
            fs::read_to_string(repo_file("install.sh"))
                .unwrap()
                .replace("@HERDR_MISSION_RELEASE_TAG@", TAG),
        )
        .unwrap();
        self.command_for(&rendered, &["--yes", "--agents", agents])
            .output()
            .unwrap()
    }

    fn run_args(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn run_args_with_tty(&self, args: &[&str], tty: &Path) -> Output {
        self.command(args)
            .env("HERDR_MISSION_TTY_PATH", tty)
            .output()
            .unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = self.command_for(&repo_file("install.sh"), args);
        command.env("HERDR_MISSION_RELEASE_TAG", TAG);
        command
    }

    fn command_for(&self, script: &Path, args: &[&str]) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .arg(script)
            .args(args)
            .env("HOME", &self.home)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", self.stub_bin.display()),
            )
            .env("HERDR_TEST_CAPTURE", &self.herdr_capture)
            .env("HERDR_TEST_REAL_CLI", REAL_CLI)
            .env_remove("HERDR_MISSION_RELEASE_TAG")
            .env(
                "HERDR_MISSION_RELEASE_BASE_URL",
                format!("file://{}", self.release_base.display()),
            );
        command
    }

    fn release_dir(&self) -> PathBuf {
        self.release_base.join(TAG)
    }

    fn write_uname(&self, os: &str, arch: &str) {
        write_executable(
            &self.stub_bin.join("uname"),
            &format!(
                "#!/bin/sh\ncase \"${{1:-}}\" in\n  -s) echo {os} ;;\n  -m) echo {arch} ;;\n  *) echo {os} ;;\nesac\n"
            ),
        );
    }

    fn restage_skill(&self, content: &str) {
        let payload = self.root.join("payload/herdr-mission-team");
        fs::write(payload.join("SKILL.md"), content).unwrap();
        let archive = self.release_dir().join(SKILL_ASSET);
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(payload.parent().unwrap())
            .arg("herdr-mission-team")
            .status()
            .unwrap();
        assert!(status.success());
        let cli = self.release_dir().join(CLI_ASSET);
        fs::write(
            self.release_dir().join("SHA256SUMS"),
            format!(
                "{}  {CLI_ASSET}\n{}  {SKILL_ASSET}\n",
                sha256_hex(&fs::read(cli).unwrap()),
                sha256_hex(&fs::read(archive).unwrap())
            ),
        )
        .unwrap();
    }

    fn write_cross_linked_herdr_state(&self) {
        let plugin_json = format!(
            concat!(
                "{{\"id\":\"cli:plugin\",\"result\":{{\"plugins\":[",
                "{{\"plugin_id\":\"weston.herdr-mission\",",
                "\"source\":{{\"kind\":\"github\",\"owner\":\"someone\",",
                "\"repo\":\"elsewhere\",\"requested_ref\":\"{tag}\",",
                "\"resolved_commit\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}}}},",
                "{{\"plugin_id\":\"other.plugin\",",
                "\"source\":{{\"kind\":\"github\",\"owner\":\"gxbsst\",",
                "\"repo\":\"herdr-mission\",\"requested_ref\":\"{tag}\",",
                "\"resolved_commit\":\"{commit}\"}}}}],",
                "\"type\":\"plugin_list\"}}}}"
            ),
            tag = TAG,
            commit = COMMIT,
        );
        write_executable(
            &self.stub_bin.join("herdr"),
            &format!(
                concat!(
                    "#!/bin/sh\n",
                    "printf '%s\\n' \"$*\" >> \"$HERDR_TEST_CAPTURE\"\n",
                    "if [ \"${{1:-}} ${{2:-}}\" = 'plugin install' ]; then exit 0; fi\n",
                    "if [ \"${{1:-}} ${{2:-}}\" = 'plugin list' ]; then\n",
                    "  printf '%s\\n' '{}'\n",
                    "  exit 0\n",
                    "fi\n",
                    "exit 64\n",
                ),
                plugin_json
            ),
        );
    }
}

impl Drop for InstallerFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn repo_file(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
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
fn noninteractive_install_copies_plugin_cli_and_both_skills_from_one_release() {
    let fixture = InstallerFixture::new("both");

    let output = fixture.run_rendered("codex,claude");

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains(".local/bin is not in PATH"));
    let herdr_calls = fs::read_to_string(&fixture.herdr_capture).unwrap();
    assert!(herdr_calls.contains("plugin install gxbsst/herdr-mission --ref v9.8.7 --yes"));
    assert!(herdr_calls.contains("plugin list --plugin weston.herdr-mission --json"));

    let cli = fixture.home.join(".local/bin/herdr-mission");
    assert_eq!(
        fs::read(&cli).unwrap(),
        fs::read(fixture.release_base.join(TAG).join(CLI_ASSET)).unwrap()
    );
    assert_ne!(fs::metadata(&cli).unwrap().permissions().mode() & 0o111, 0);

    let canonical = fixture
        .home
        .join(".local/share/herdr-mission/skills/herdr-mission-team");
    assert_eq!(
        fs::read_to_string(canonical.join(".installed-by-herdr-mission")).unwrap(),
        "owner=herdr-mission-unified-installer-v1\ntarget=canonical\n"
    );
    let canonical_skill = fs::read(canonical.join("SKILL.md")).unwrap();
    assert!(String::from_utf8_lossy(&canonical_skill).contains("name: herdr-mission-team"));

    let codex = fixture.home.join(".agents/skills/herdr-mission-team");
    let claude = fixture.home.join(".claude/skills/herdr-mission-team");
    for (agent_copy, target_kind) in [(&codex, "codex"), (&claude, "claude")] {
        assert!(fs::symlink_metadata(agent_copy)
            .unwrap()
            .file_type()
            .is_dir());
        assert_eq!(
            fs::read(agent_copy.join("SKILL.md")).unwrap(),
            canonical_skill
        );
        assert_eq!(
            fs::read_to_string(agent_copy.join(".installed-by-herdr-mission")).unwrap(),
            format!("owner=herdr-mission-unified-installer-v1\ntarget={target_kind}\n")
        );
    }
}

#[test]
fn agent_selection_supports_codex_claude_and_both() {
    for (selection, has_codex, has_claude) in [
        ("codex", true, false),
        ("claude", false, true),
        ("both", true, true),
    ] {
        let fixture = InstallerFixture::new(selection);

        let output = fixture.run(selection);

        assert_success(&output);
        assert_eq!(
            fixture
                .home
                .join(".agents/skills/herdr-mission-team")
                .exists(),
            has_codex
        );
        assert_eq!(
            fixture
                .home
                .join(".claude/skills/herdr-mission-team")
                .exists(),
            has_claude
        );
    }
}

#[test]
fn interactive_selection_reads_tty_while_script_stdin_is_unavailable() {
    let fixture = InstallerFixture::new("interactive");
    let tty = fixture.root.join("tty-input");
    fs::write(&tty, "1\n").unwrap();

    let output = fixture.run_args_with_tty(&["--yes"], &tty);

    assert_success(&output);
    assert!(fixture
        .home
        .join(".agents/skills/herdr-mission-team")
        .exists());
    assert!(!fixture
        .home
        .join(".claude/skills/herdr-mission-team")
        .exists());
}

#[test]
fn reinstall_is_idempotent_and_keeps_one_copy_per_selected_target() {
    let fixture = InstallerFixture::new("idempotent");

    assert_success(&fixture.run("both"));
    let first_cli = fs::read(fixture.home.join(".local/bin/herdr-mission")).unwrap();
    assert_success(&fixture.run("both"));

    assert_eq!(
        fs::read(fixture.home.join(".local/bin/herdr-mission")).unwrap(),
        first_cli
    );
    let canonical = fixture
        .home
        .join(".local/share/herdr-mission/skills/herdr-mission-team");
    let mut entries = fs::read_dir(&canonical)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries.sort();
    assert_eq!(entries.len(), 2);
    let canonical_skill = fs::read(canonical.join("SKILL.md")).unwrap();
    for agent_copy in [
        fixture.home.join(".agents/skills/herdr-mission-team"),
        fixture.home.join(".claude/skills/herdr-mission-team"),
    ] {
        assert!(!fs::symlink_metadata(&agent_copy)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read(agent_copy.join("SKILL.md")).unwrap(),
            canonical_skill
        );
        assert_eq!(fs::read_dir(agent_copy).unwrap().count(), 2);
    }
}

#[test]
fn owned_skill_copies_upgrade_without_exposing_a_partial_skill() {
    let fixture = InstallerFixture::new("owned-copy-upgrade");
    assert_success(&fixture.run("both"));
    let canonical = fixture
        .home
        .join(".local/share/herdr-mission/skills/herdr-mission-team");
    let codex = fixture.home.join(".agents/skills/herdr-mission-team");
    let old_skill = fs::read(canonical.join("SKILL.md")).unwrap();
    fixture.restage_skill(
        "---\nname: herdr-mission-team\ndescription: Updated test skill.\n---\n\n# Updated\n",
    );
    write_executable(
        &fixture.stub_bin.join("mv"),
        "#!/bin/sh\nlast=\nfor value do last=$value; done\nif [ \"${HERDR_TEST_FAIL_MV_DEST:-}\" = \"$last\" ]; then exit 73; fi\nexec /bin/mv \"$@\"\n",
    );

    let output = fixture
        .command(&["--yes", "--agents", "codex"])
        .env("HERDR_TEST_FAIL_MV_DEST", "./SKILL.md")
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(canonical.join("SKILL.md")).unwrap(), old_skill);
    assert_eq!(fs::read(codex.join("SKILL.md")).unwrap(), old_skill);
    assert_eq!(
        fs::read_to_string(canonical.join(".installed-by-herdr-mission")).unwrap(),
        "owner=herdr-mission-unified-installer-v1\ntarget=canonical\n"
    );
}

#[test]
fn preflighted_agent_copy_replacement_is_preserved_and_fails_closed() {
    let fixture = InstallerFixture::new("agent-copy-replacement");
    assert_success(&fixture.run("codex"));
    let codex = fixture.home.join(".agents/skills/herdr-mission-team");

    let output = fixture
        .command(&["--yes", "--agents", "codex"])
        .env("HERDR_TEST_REPLACE_TARGET", &codex)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(codex.join("foreign.txt")).unwrap(),
        "foreign\n"
    );
    assert!(!codex.join("SKILL.md").exists());
}

#[test]
fn late_agent_directory_replacement_never_overwrites_foreign_skill_content() {
    let fixture = InstallerFixture::new("late-agent-replacement");
    assert_success(&fixture.run("codex"));
    let codex = fixture.home.join(".agents/skills/herdr-mission-team");
    let owned_backup = fixture.root.join("owned-codex-skill");
    write_executable(
        &fixture.stub_bin.join("cp"),
        "#!/bin/sh\nphysical_pwd=$(/bin/pwd -P)\ntarget_pwd=$(cd \"${HERDR_TEST_LATE_REPLACE_TARGET:-/}\" 2>/dev/null && /bin/pwd -P)\nif [ -n \"${HERDR_TEST_LATE_REPLACE_TARGET:-}\" ] && [ \"$physical_pwd\" = \"$target_pwd\" ] && [ ! -e \"$HERDR_TEST_LATE_REPLACE_BACKUP\" ]; then\n  /bin/mv \"$HERDR_TEST_LATE_REPLACE_TARGET\" \"$HERDR_TEST_LATE_REPLACE_BACKUP\"\n  /bin/mkdir -p \"$HERDR_TEST_LATE_REPLACE_TARGET\"\n  printf '%s\\n' foreign-skill > \"$HERDR_TEST_LATE_REPLACE_TARGET/SKILL.md\"\nfi\nexec /bin/cp \"$@\"\n",
    );

    let output = fixture
        .command(&["--yes", "--agents", "codex"])
        .env("HERDR_TEST_LATE_REPLACE_TARGET", &codex)
        .env("HERDR_TEST_LATE_REPLACE_BACKUP", &owned_backup)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(codex.join("SKILL.md")).unwrap(),
        "foreign-skill\n"
    );
    assert!(owned_backup.join(".installed-by-herdr-mission").exists());
}

#[test]
fn fresh_marker_window_replacement_never_claims_or_overwrites_foreign_directory() {
    let fixture = InstallerFixture::new("fresh-marker-replacement");
    let canonical = fixture
        .home
        .join(".local/share/herdr-mission/skills/herdr-mission-team");
    let replacement_trigger = fixture.root.join("fresh-replacement-triggered");

    let output = fixture
        .command(&["--yes", "--agents", "codex"])
        .env("HERDR_TEST_LATE_REPLACE_TARGET", &canonical)
        .env("HERDR_TEST_LATE_REPLACE_BACKUP", &replacement_trigger)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(canonical.join("SKILL.md")).unwrap(),
        "foreign-skill\n"
    );
    assert!(!canonical.join(".installed-by-herdr-mission").exists());
    assert!(replacement_trigger.exists());
    assert_eq!(
        fs::read_dir(canonical.parent().unwrap()).unwrap().count(),
        1
    );
}

#[test]
fn preflighted_cli_replacement_is_preserved_without_nested_installer_files() {
    let fixture = InstallerFixture::new("cli-replacement");
    assert_success(&fixture.run("codex"));
    let cli = fixture.home.join(".local/bin/herdr-mission");

    let output = fixture
        .command(&["--yes", "--agents", "codex"])
        .env("HERDR_TEST_REPLACE_TARGET", &cli)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("CLI target"));
    assert_eq!(
        fs::read_to_string(cli.join("foreign.txt")).unwrap(),
        "foreign\n"
    );
    assert_eq!(fs::read_dir(cli).unwrap().count(), 1);
}

#[test]
fn failed_fresh_helper_invocation_leaves_no_target_and_can_retry() {
    let fixture = InstallerFixture::new("fresh-helper-failure");
    let canonical = fixture
        .home
        .join(".local/share/herdr-mission/skills/herdr-mission-team");
    let failed = fixture
        .command(&["--yes", "--agents", "codex"])
        .env("HERDR_TEST_FAIL_FRESH_PUBLISH", "1")
        .output()
        .unwrap();

    assert!(!failed.status.success());
    assert!(!canonical.exists());
    assert!(!fixture
        .home
        .join(".agents/skills/herdr-mission-team")
        .exists());
    assert_success(&fixture.run("codex"));
}

#[test]
fn stale_pid_sequence_stages_cannot_exhaust_fresh_copy_publication() {
    let fixture = InstallerFixture::new("stale-stage-names");
    let skill_parent = fixture.home.join(".local/share/herdr-mission/skills");
    fs::create_dir_all(&skill_parent).unwrap();
    for sequence in 1..=64 {
        fs::create_dir(skill_parent.join(format!(".herdr-mission-team.install.4242.{sequence}")))
            .unwrap();
    }

    let output = fixture.run("codex");

    assert_success(&output);
    assert!(skill_parent
        .join("herdr-mission-team/.installed-by-herdr-mission")
        .is_file());
}

#[test]
fn managed_marker_without_skill_is_recovered_by_same_release_retry() {
    let fixture = InstallerFixture::new("new-payload-failure");
    let canonical = fixture
        .home
        .join(".local/share/herdr-mission/skills/herdr-mission-team");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(
        canonical.join(".installed-by-herdr-mission"),
        "owner=herdr-mission-unified-installer-v1\ntarget=canonical\n",
    )
    .unwrap();
    assert!(!canonical.join("SKILL.md").exists());
    assert_success(&fixture.run("codex"));
    assert!(canonical.join("SKILL.md").is_file());
}

#[test]
fn checksum_mismatch_preserves_existing_cli_and_skips_plugin_install() {
    let fixture = InstallerFixture::new("checksum-mismatch");
    let cli = fixture.home.join(".local/bin/herdr-mission");
    fs::create_dir_all(cli.parent().unwrap()).unwrap();
    fs::write(&cli, "existing-cli\n").unwrap();
    let archive = fixture.release_dir().join(SKILL_ASSET);
    fs::write(
        fixture.release_dir().join("SHA256SUMS"),
        format!(
            "{}  {CLI_ASSET}\n{}  {SKILL_ASSET}\n",
            "0".repeat(64),
            sha256_hex(&fs::read(archive).unwrap())
        ),
    )
    .unwrap();

    let output = fixture.run("codex");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("checksum mismatch"));
    assert_eq!(fs::read_to_string(cli).unwrap(), "existing-cli\n");
    assert!(!fixture
        .home
        .join(".local/share/herdr-mission/skills/herdr-mission-team")
        .exists());
    assert!(!fixture.herdr_capture.exists());
}

#[test]
fn skill_name_must_be_in_yaml_frontmatter_before_plugin_install() {
    let fixture = InstallerFixture::new("frontmatter");
    fixture.restage_skill(
        "---\nname: foreign-skill\ndescription: Wrong skill.\n---\n\nname: herdr-mission-team\n",
    );

    let output = fixture.run("codex");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("frontmatter"));
    assert!(!fixture.home.join(".local").exists());
    assert!(!fixture.herdr_capture.exists());
}

#[test]
fn plugin_revision_must_belong_to_the_mission_plugin_record() {
    let fixture = InstallerFixture::new("plugin-cross-link");
    fixture.write_cross_linked_herdr_state();
    let cli = fixture.home.join(".local/bin/herdr-mission");
    fs::create_dir_all(cli.parent().unwrap()).unwrap();
    fs::write(&cli, "existing-cli\n").unwrap();

    let output = fixture.run("codex");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not resolve"));
    assert_eq!(fs::read_to_string(cli).unwrap(), "existing-cli\n");
    assert!(!fixture
        .home
        .join(".local/share/herdr-mission/skills/herdr-mission-team")
        .exists());
}

#[test]
fn foreign_agent_skill_directory_or_symlink_is_preserved() {
    let directory_fixture = InstallerFixture::new("foreign-directory");
    let codex = directory_fixture
        .home
        .join(".agents/skills/herdr-mission-team");
    fs::create_dir_all(&codex).unwrap();
    fs::write(codex.join("foreign.txt"), "keep\n").unwrap();

    let directory_output = directory_fixture.run("codex");

    assert!(!directory_output.status.success());
    assert_eq!(
        fs::read_to_string(codex.join("foreign.txt")).unwrap(),
        "keep\n"
    );
    assert!(!directory_fixture.herdr_capture.exists());

    let symlink_fixture = InstallerFixture::new("foreign-symlink");
    let foreign = symlink_fixture.root.join("foreign-skill");
    fs::create_dir_all(&foreign).unwrap();
    let claude = symlink_fixture
        .home
        .join(".claude/skills/herdr-mission-team");
    create_foreign_symlink(&foreign, &claude);

    let symlink_output = symlink_fixture.run("claude");

    assert!(!symlink_output.status.success());
    assert_eq!(fs::read_link(&claude).unwrap(), foreign);
    assert!(!symlink_fixture.herdr_capture.exists());
}

#[test]
fn legacy_symlink_to_canonical_is_preserved_and_rejected() {
    let fixture = InstallerFixture::new("legacy-canonical-symlink");
    let canonical = fixture
        .home
        .join(".local/share/herdr-mission/skills/herdr-mission-team");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(
        canonical.join(".installed-by-herdr-mission"),
        "owner=herdr-mission-unified-installer-v1\ntarget=canonical\n",
    )
    .unwrap();
    fs::write(
        canonical.join("SKILL.md"),
        "---\nname: herdr-mission-team\n---\n",
    )
    .unwrap();
    let codex = fixture.home.join(".agents/skills/herdr-mission-team");
    create_foreign_symlink(&canonical, &codex);

    let output = fixture.run("codex");

    assert!(!output.status.success());
    assert_eq!(fs::read_link(&codex).unwrap(), canonical);
    assert!(!fixture.herdr_capture.exists());
}

#[test]
fn missing_herdr_and_unsupported_platform_fail_before_writes() {
    let missing_fixture = InstallerFixture::new("missing-herdr");
    fs::remove_file(missing_fixture.stub_bin.join("herdr")).unwrap();

    let missing_output = missing_fixture.run("codex");

    assert!(!missing_output.status.success());
    assert!(String::from_utf8_lossy(&missing_output.stderr).contains("herdr is required"));
    assert!(!missing_fixture.home.join(".local").exists());

    let platform_fixture = InstallerFixture::new("unsupported-platform");
    platform_fixture.write_uname("Linux", "riscv64");

    let platform_output = platform_fixture.run("codex");

    assert!(!platform_output.status.success());
    assert!(String::from_utf8_lossy(&platform_output.stderr).contains("no prebuilt"));
    assert!(!platform_fixture.home.join(".local").exists());
    assert!(!platform_fixture.herdr_capture.exists());
}

#[test]
fn invalid_canonical_ownership_marker_fails_without_writes() {
    let fixture = InstallerFixture::new("invalid-owned-marker");
    let cli = fixture.home.join(".local/bin/herdr-mission");
    fs::create_dir_all(cli.parent().unwrap()).unwrap();
    fs::write(&cli, "existing-cli\n").unwrap();
    let canonical = fixture
        .home
        .join(".local/share/herdr-mission/skills/herdr-mission-team");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(canonical.join("SKILL.md"), "existing-skill\n").unwrap();
    fs::write(
        canonical.join(".installed-by-herdr-mission"),
        "owner=herdr-mission-unified-installer-v1\nrelease=not-a-tag\n",
    )
    .unwrap();

    let output = fixture.run("codex");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not owned"));
    assert_eq!(fs::read_to_string(&cli).unwrap(), "existing-cli\n");
    assert_eq!(
        fs::read_to_string(canonical.join("SKILL.md")).unwrap(),
        "existing-skill\n"
    );
    assert_eq!(
        fs::read_to_string(canonical.join(".installed-by-herdr-mission")).unwrap(),
        "owner=herdr-mission-unified-installer-v1\nrelease=not-a-tag\n"
    );
    assert!(!fixture.herdr_capture.exists());
}

#[test]
fn release_workflow_publishes_stamped_installer_and_checksum_covered_skill() {
    let workflow = fs::read_to_string(repo_file(".github/workflows/release.yml")).unwrap();

    let stamp = workflow
        .find("@HERDR_MISSION_RELEASE_TAG@")
        .expect("release workflow stamps install.sh with its tag");
    let archive = workflow
        .find("herdr-mission-team.skill.tar.gz")
        .expect("release workflow builds the skill archive");
    let checksums = workflow
        .find("sha256sum herdr-mission-*")
        .expect("release workflow builds checksums after staging payloads");
    assert!(stamp < checksums);
    assert!(archive < checksums);
    assert!(workflow.contains("dist/install.sh"));
    assert!(workflow.contains("dist/herdr-mission-team.skill.tar.gz"));
}

#[test]
fn readme_documents_latest_pinned_and_noninteractive_installation() {
    let readme = fs::read_to_string(repo_file("README.md")).unwrap();

    assert!(readme.contains("releases/latest/download/install.sh"));
    assert!(readme.contains("releases/download/vX.Y.Z/install.sh"));
    assert!(readme.contains("sh -s -- --yes --agents codex,claude"));
    assert!(readme.contains("~/.local/bin/herdr-mission"));
    assert!(readme.contains("~/.agents/skills/herdr-mission-team"));
    assert!(readme.contains("~/.claude/skills/herdr-mission-team"));
}

#[allow(dead_code)]
fn create_foreign_symlink(target: &Path, link: &Path) {
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(target, link).unwrap();
}
