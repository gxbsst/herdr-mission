use std::{
    ffi::CString,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        raw::{c_int, c_uint},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawFd},
    },
    path::{Path, PathBuf},
};

const SKILL_OWNER: &str = "owner=herdr-mission-unified-installer-v1";

pub(crate) fn publish_fresh_skill_copy(
    payload: &Path,
    target: &Path,
    target_kind: &str,
) -> io::Result<()> {
    publish_fresh_skill_copy_with_hook(payload, target, target_kind, |_, _| Ok(()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublishStep {
    StageCreated,
    MarkerWritten,
}

fn publish_fresh_skill_copy_with_hook(
    payload: &Path,
    target: &Path,
    target_kind: &str,
    mut hook: impl FnMut(PublishStep, &Path) -> io::Result<()>,
) -> io::Result<()> {
    if !matches!(target_kind, "canonical" | "codex" | "claude") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported skill target kind",
        ));
    }

    let payload_metadata = fs::symlink_metadata(payload)?;
    if !payload_metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill payload must be a regular file",
        ));
    }

    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "skill target has no parent"))?;
    let target_name = target.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "skill target has no file name")
    })?;
    fs::create_dir_all(parent)?;
    let parent_directory = File::open(parent)?;
    if !parent_directory.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill target parent is not a directory",
        ));
    }

    let (stage_name, stage) = create_stage_directory(parent, target_name)?;
    let mut cleanup = StageCleanup(Some(stage.clone()));
    hook(PublishStep::StageCreated, &stage)?;

    write_new_file(
        &stage.join(".installed-by-herdr-mission"),
        format!("{SKILL_OWNER}\ntarget={target_kind}\n").as_bytes(),
    )?;
    hook(PublishStep::MarkerWritten, &stage)?;

    let mut source = File::open(payload)?;
    if !source.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill payload must remain a regular file",
        ));
    }
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(stage.join("SKILL.md"))?;
    io::copy(&mut source, &mut destination)?;
    destination.sync_all()?;
    File::open(&stage)?.sync_all()?;

    rename_noreplace(
        parent_directory.as_raw_fd(),
        stage_name.as_ref(),
        target_name,
    )?;
    cleanup.0 = None;
    parent_directory.sync_all()?;
    Ok(())
}

fn create_stage_directory(
    parent: &Path,
    target_name: &std::ffi::OsStr,
) -> io::Result<(String, PathBuf)> {
    loop {
        let nonce = random_stage_nonce()?;
        let stage_name = format!(".{}.install.{}", target_name.to_string_lossy(), nonce);
        let stage = parent.join(&stage_name);
        match fs::create_dir(&stage) {
            Ok(()) => return Ok((stage_name, stage)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn random_stage_nonce() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut nonce = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut nonce, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(nonce)
}

fn write_new_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

struct StageCleanup(Option<PathBuf>);

impl Drop for StageCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn name_to_c_string(name: &std::ffi::OsStr) -> io::Result<CString> {
    if name.as_bytes().contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill install name contains a slash",
        ));
    }
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill install name contains a NUL byte",
        )
    })
}

#[cfg(target_os = "macos")]
fn rename_noreplace(
    parent_fd: c_int,
    source: &std::ffi::OsStr,
    target: &std::ffi::OsStr,
) -> io::Result<()> {
    const RENAME_EXCL: c_uint = 0x0000_0004;

    unsafe extern "C" {
        fn renameatx_np(
            from_fd: c_int,
            from: *const std::os::raw::c_char,
            to_fd: c_int,
            to: *const std::os::raw::c_char,
            flags: c_uint,
        ) -> c_int;
    }

    let source = name_to_c_string(source)?;
    let target = name_to_c_string(target)?;
    // SAFETY: the parent FD remains open, both names are NUL-terminated C
    // strings, and RENAME_EXCL rejects an existing target atomically.
    let result = unsafe {
        renameatx_np(
            parent_fd,
            source.as_ptr(),
            parent_fd,
            target.as_ptr(),
            RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn rename_noreplace(
    parent_fd: c_int,
    source: &std::ffi::OsStr,
    target: &std::ffi::OsStr,
) -> io::Result<()> {
    const SYS_RENAMEAT2: std::os::raw::c_long = 316;
    const RENAME_NOREPLACE: c_uint = 1;

    unsafe extern "C" {
        fn syscall(number: std::os::raw::c_long, ...) -> std::os::raw::c_long;
    }

    let source = name_to_c_string(source)?;
    let target = name_to_c_string(target)?;
    // SAFETY: Linux x86_64 assigns syscall 316 to renameat2. The parent FD
    // remains open, both names are NUL-terminated C strings, and the argument
    // types match renameat2(2). RENAME_NOREPLACE rejects an existing target
    // atomically. Calling syscall avoids relying on a libc renameat2 symbol,
    // which musl intentionally does not export.
    let result = unsafe {
        syscall(
            SYS_RENAMEAT2,
            parent_fd,
            source.as_ptr(),
            parent_fd,
            target.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
fn rename_noreplace(
    _parent_fd: c_int,
    _source: &std::ffi::OsStr,
    _target: &std::ffi::OsStr,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic skill publication is only supported on Linux x86_64",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_noreplace(
    _parent_fd: c_int,
    _source: &std::ffi::OsStr,
    _target: &std::ffi::OsStr,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic skill publication is only supported on macOS and Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::{publish_fresh_skill_copy, publish_fresh_skill_copy_with_hook, PublishStep};
    use std::{fs, io, path::PathBuf, time::SystemTime};

    fn fixture_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-mission-installer-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn assert_step_failure_cleans_and_retries(step: PublishStep) {
        let root = fixture_root(match step {
            PublishStep::StageCreated => "marker-failure",
            PublishStep::MarkerWritten => "payload-failure",
        });
        let payload = root.join("payload/SKILL.md");
        let parent = root.join("skills");
        let target = parent.join("herdr-mission-team");
        fs::create_dir_all(payload.parent().unwrap()).unwrap();
        fs::create_dir_all(&parent).unwrap();
        fs::write(&payload, "---\nname: herdr-mission-team\n---\n").unwrap();

        let error =
            publish_fresh_skill_copy_with_hook(&payload, &target, "canonical", |current, _| {
                if current == step {
                    Err(io::Error::other("injected failure"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(!target.exists());
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        publish_fresh_skill_copy(&payload, &target, "canonical").unwrap();
        assert!(target.join(".installed-by-herdr-mission").is_file());
        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            fs::read(payload).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn marker_write_failure_cleans_random_stage_and_allows_retry() {
        assert_step_failure_cleans_and_retries(PublishStep::StageCreated);
    }

    #[test]
    fn payload_write_failure_cleans_random_stage_and_allows_retry() {
        assert_step_failure_cleans_and_retries(PublishStep::MarkerWritten);
    }
}
