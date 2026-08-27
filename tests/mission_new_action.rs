use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct ActionFixture {
    root: PathBuf,
    capture: PathBuf,
    state: PathBuf,
}

impl ActionFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "herdr-mission-action-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let binary = root.join("target/release/herdr-mission");
        let capture = root.join("captured-argv");
        let state = root.join("state");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(
            &binary,
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$HERDR_ACTION_CAPTURE\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions).unwrap();
        Self {
            root,
            capture,
            state,
        }
    }

    fn run(&self, input: &str) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("actions/mission-new.sh");
        let mut child = Command::new("bash")
            .arg(script)
            .env("HERDR_PLUGIN_ROOT", &self.root)
            .env("HERDR_PLUGIN_STATE_DIR", &self.state)
            .env("HERDR_ACTION_CAPTURE", &self.capture)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn captured_args(&self) -> Vec<String> {
        fs::read_to_string(&self.capture)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for ActionFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn expected_base(fixture: &ActionFixture) -> Vec<String> {
    vec![
        "new".into(),
        "--title=Auto Mission".into(),
        format!(
            "--database={}",
            fixture.state.join("missions.sqlite3").display()
        ),
    ]
}

#[test]
fn action_passes_explicit_auto_launch_mode() {
    let fixture = ActionFixture::new();
    let output = fixture.run("Auto Mission\nauto\n");

    assert!(output.status.success());
    let mut expected = expected_base(&fixture);
    expected.push("--launch-mode=auto".into());
    assert_eq!(fixture.captured_args(), expected);
}

#[test]
fn action_passes_explicit_manual_launch_mode() {
    let fixture = ActionFixture::new();
    let output = fixture.run("Auto Mission\nmanual\n");

    assert!(output.status.success());
    let mut expected = expected_base(&fixture);
    expected.push("--launch-mode=manual".into());
    assert_eq!(fixture.captured_args(), expected);
}

#[test]
fn action_omits_launch_mode_when_using_config_default() {
    let fixture = ActionFixture::new();
    let output = fixture.run("Auto Mission\n\n");

    assert!(output.status.success());
    assert_eq!(fixture.captured_args(), expected_base(&fixture));
}

#[test]
fn action_rejects_unknown_launch_mode_before_calling_binary() {
    let fixture = ActionFixture::new();
    let output = fixture.run("Auto Mission\nlazy\n");

    assert_eq!(output.status.code(), Some(65));
    assert!(!fixture.capture.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("auto、manual"));
}
