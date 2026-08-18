//! Release manifest and binary integrity gate.
//!
//! A release binds the built binary to a manifest that records its SHA-256,
//! the crate version, and the schema/protocol versions the binary speaks. This
//! lets the plugin fail closed when a running binary does not match the
//! manifest it claims, and lets an install step swap binaries atomically while
//! keeping the previous version for rollback.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// SHA-256 digest rendered as 64 lowercase hex characters.
pub fn sha256_hex(bytes: &[u8]) -> String {
    sha256_hex_of(bytes)
}

/// A release manifest binding a binary to its identity and content digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub binary: String,
    pub version: String,
    pub schema_version: String,
    pub protocol_version: String,
    pub sha256: String,
}

/// Compute a manifest for the binary at `binary`.
pub fn compute_manifest(
    binary: &Path,
    version: &str,
    schema_version: &str,
    protocol_version: &str,
) -> Result<ReleaseManifest, String> {
    let bytes = fs::read(binary).map_err(|error| {
        format!(
            "cannot read binary {} for manifest: {error}",
            binary.display()
        )
    })?;
    Ok(ReleaseManifest {
        binary: binary.to_string_lossy().into_owned(),
        version: version.to_string(),
        schema_version: schema_version.to_string(),
        protocol_version: protocol_version.to_string(),
        sha256: sha256_hex(&bytes),
    })
}

/// Read a manifest from disk.
pub fn read_manifest(path: &Path) -> Result<ReleaseManifest, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("cannot read manifest {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("cannot parse manifest {}: {error}", path.display()))
}

/// Verify that the binary at `binary` matches `manifest`.
///
/// The declared path inside the manifest is informative only; verification is
/// against the binary actually present at `binary`.
pub fn verify_binary(binary: &Path, manifest: &ReleaseManifest) -> Result<(), String> {
    let bytes = fs::read(binary)
        .map_err(|error| format!("cannot read binary {}: {error}", binary.display()))?;
    let actual = sha256_hex(&bytes);
    if actual != manifest.sha256 {
        return Err(format!(
            "binary hash mismatch for {}: manifest={} actual={}",
            binary.display(),
            manifest.sha256,
            actual
        ));
    }
    Ok(())
}

/// Atomically install `source` binary over `target`, keeping the previous
/// binary as `target.prev` and writing a fresh manifest next to it.
///
/// The new bytes are written to a temporary sibling, flushed, then renamed
/// over the target so a crash never leaves a truncated binary at the target
/// path. On failure before the rename the temporary file is removed.
pub fn install_atomic(
    source: &Path,
    target: &Path,
    version: &str,
    schema_version: &str,
    protocol_version: &str,
) -> Result<ReleaseManifest, String> {
    let bytes = fs::read(source)
        .map_err(|error| format!("cannot read source binary {}: {error}", source.display()))?;
    let digest = sha256_hex(&bytes);

    let parent = target
        .parent()
        .ok_or_else(|| format!("target binary {} has no parent directory", target.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;

    let temp = temp_sibling(target)?;
    let write_result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temp)
            .map_err(|error| format!("cannot create {}: {error}", temp.display()))?;
        file.write_all(&bytes)
            .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
        file.sync_all()
            .map_err(|error| format!("cannot sync {}: {error}", temp.display()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }

    // Preserve the current binary (if any) as a one-deep rollback point.
    if target.exists() {
        let backup = backup_path(target);
        let _ = fs::remove_file(&backup);
        fs::rename(target, &backup).map_err(|error| {
            format!(
                "cannot back up {} to {}: {error}",
                target.display(),
                backup.display()
            )
        })?;
    }

    if let Err(error) = fs::rename(&temp, target) {
        // Best-effort restore of the backup if the install rename failed.
        let backup = backup_path(target);
        if backup.exists() {
            let _ = fs::rename(&backup, target);
        }
        let _ = fs::remove_file(&temp);
        return Err(format!(
            "cannot install {} over {}: {error}",
            temp.display(),
            target.display()
        ));
    }

    let manifest = ReleaseManifest {
        binary: target.to_string_lossy().into_owned(),
        version: version.to_string(),
        schema_version: schema_version.to_string(),
        protocol_version: protocol_version.to_string(),
        sha256: digest,
    };
    let manifest_path = manifest_path_for(target);
    write_manifest(&manifest_path, &manifest)?;
    Ok(manifest)
}

/// Path of the manifest that accompanies a binary.
pub fn manifest_path_for(binary: &Path) -> PathBuf {
    let mut name = binary
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "herdr-mission".to_string());
    name.push_str(".manifest.json");
    binary.with_file_name(name)
}

pub fn write_manifest(path: &Path, manifest: &ReleaseManifest) -> Result<(), String> {
    let text = serde_json::to_string_pretty(manifest)
        .map_err(|error| format!("cannot serialize manifest: {error}"))?;
    let mut file = fs::File::create(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync {}: {error}", path.display()))?;
    Ok(())
}

fn temp_sibling(target: &Path) -> Result<PathBuf, String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("target binary {} has no parent directory", target.display()))?;
    let base = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "binary".to_string());
    let mut path = parent.join(format!(".{base}.tmp-{}", std::process::id()));
    let mut counter = 0u32;
    while path.exists() {
        counter += 1;
        path = parent.join(format!(".{base}.tmp-{}-{}", std::process::id(), counter));
    }
    Ok(path)
}

fn backup_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "binary".to_string());
    name.push_str(".prev");
    target.with_file_name(name)
}

// ---------------------------------------------------------------------------
// SHA-256 (FIPS 180-4), dependency-free so the release gate stays offline.
// ---------------------------------------------------------------------------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256_hex_of(bytes: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let j = i * 4;
            *word = u32::from_be_bytes([chunk[j], chunk[j + 1], chunk[j + 2], chunk[j + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_fips_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let long = "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            sha256_hex(long.as_bytes()),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let manifest = ReleaseManifest {
            binary: "/tmp/herdr-mission".into(),
            version: "0.1.0".into(),
            schema_version: "3".into(),
            protocol_version: "1".into(),
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        };
        let text = serde_json::to_string(&manifest).unwrap();
        let parsed: ReleaseManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn install_atomic_writes_binary_manifest_and_rollback_point() {
        let root = std::env::temp_dir().join(format!(
            "herdr-mission-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();

        let source = root.join("source-bin");
        let target = root.join("target-bin");
        std::fs::write(&source, b"version-one").unwrap();

        let manifest = install_atomic(&source, &target, "0.1.0", "3", "kernel.v1").unwrap();
        assert_eq!(manifest.sha256, sha256_hex(b"version-one"));
        assert_eq!(std::fs::read(&target).unwrap(), b"version-one");
        assert!(manifest_path_for(&target).exists());
        verify_binary(&target, &manifest).unwrap();

        // Install a second version: the previous binary is kept as a rollback.
        std::fs::write(&source, b"version-two").unwrap();
        let manifest2 = install_atomic(&source, &target, "0.2.0", "3", "kernel.v1").unwrap();
        assert_eq!(manifest2.sha256, sha256_hex(b"version-two"));
        assert_eq!(std::fs::read(&target).unwrap(), b"version-two");

        let backup = target.with_file_name("target-bin.prev");
        assert!(backup.exists());
        assert_eq!(std::fs::read(&backup).unwrap(), b"version-one");
        verify_binary(&target, &manifest2).unwrap();

        // A mismatched manifest fails closed.
        let tampered = ReleaseManifest {
            binary: target.to_string_lossy().into_owned(),
            version: "0.2.0".into(),
            schema_version: "3".into(),
            protocol_version: "kernel.v1".into(),
            sha256: sha256_hex(b"not-the-binary"),
        };
        assert!(verify_binary(&target, &tampered).is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
