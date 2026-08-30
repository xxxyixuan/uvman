//! Self-update installation: checksum parsing/verification and replacement of
//! the running executable.
//!
//! Windows cannot overwrite a running exe but *can* rename it, so the
//! replacement sequence is rename-as-aside → copy-new-in-place → best-effort
//! delete of the aside copy (the "rename-to-delete" pattern used by rustup and
//! the self-replace crate). The aside copy typically stays locked until the
//! process exits, so it is cleaned up on the next launch instead.

use std::fs;
use std::path::{Path, PathBuf};

use crate::core::error::UError;

/// Outcome of a successful executable replacement.
#[derive(Debug)]
pub enum ReplaceOutcome {
    /// New binary in place; no leftover files.
    Replaced,
    /// New binary in place; the old one remains as `<exe>.old` because it is
    /// still locked by the running process (Windows). It will be removed on
    /// the next launch.
    OldPending(PathBuf),
}

/// Extract the hex digest from a `.sha256` sidecar file
/// (`<hash>  <filename>` lines, as produced by sha256sum / Get-FileHash).
pub fn parse_sha256(text: &str) -> Option<String> {
    let token = text.split_whitespace().next()?;
    let is_hex = token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit());
    is_hex.then(|| token.to_lowercase())
}

/// Verify the SHA-256 checksum of a file against the expected hex digest.
pub fn verify_sha256(path: &Path, expected: &str) -> Result<(), UError> {
    let digest = crate::toolset::compute_checksum(path, "sha256")?;
    // HexDigest::parse normalizes case, so an uppercase sidecar digest matches
    match crate::toolset::HexDigest::parse(expected) {
        Some(expected) if expected == digest => Ok(()),
        _ => {
            Err(UError::ChecksumError { message: format!("expected {expected}, got {digest}") })
        },
    }
}

/// The executable self-update writes to: `UVMAN_BIN` when set (lets package
/// managers / tests redirect the target), else the currently running exe.
pub fn install_target() -> Result<PathBuf, UError> {
    if let Some(bin) = std::env::var_os("UVMAN_BIN") {
        let path = PathBuf::from(bin);
        if path.is_file() {
            return Ok(path);
        }
        return Err(UError::PathNotFound { path });
    }
    std::env::current_exe().map_err(|source| UError::IoError { source })
}

/// Replace `exe` with the binary at `new` in a platform-appropriate way.
///
/// On Unix the new file is renamed over the old name (atomic); on Windows the
/// running exe is renamed aside first (see the module docs).
pub fn replace_executable(exe: &Path, new: &Path) -> Result<ReplaceOutcome, UError> {
    if !new.is_file() {
        return Err(UError::PathNotFound { path: new.to_path_buf() });
    }
    replace_impl(exe, new)
}

#[cfg(unix)]
fn replace_impl(exe: &Path, new: &Path) -> Result<ReplaceOutcome, UError> {
    // Archives may not carry the exec bit (zip never does), so set it before
    // the binary lands under its final name.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(new, fs::Permissions::from_mode(0o755))
        .map_err(|source| UError::FileError { path: new.to_path_buf(), source })?;

    // Atomic swap; fall back to copy for cross-device temp dirs
    if fs::rename(new, exe).is_err() {
        fs::copy(new, exe).map_err(|source| UError::FileError { path: exe.to_path_buf(), source })?;
    }
    Ok(ReplaceOutcome::Replaced)
}

#[cfg(windows)]
fn replace_impl(exe: &Path, new: &Path) -> Result<ReplaceOutcome, UError> {
    let old = backup_path(exe);
    // Remove a stale backup from an even earlier update; ignore failure (it
    // just means it is locked, and this update will reuse the name)
    let _ = fs::remove_file(&old);

    // A running exe can be renamed but not deleted/overwritten; renaming it
    // aside frees the original name for the new binary.
    fs::rename(exe, &old).map_err(|source| UError::FileError { path: exe.to_path_buf(), source })?;

    match fs::copy(new, exe) {
        Ok(_) => {
            // The old binary is still mapped by this process, so deletion
            // usually fails; that is expected and non-fatal.
            if fs::remove_file(&old).is_ok() {
                Ok(ReplaceOutcome::Replaced)
            } else {
                Ok(ReplaceOutcome::OldPending(old))
            }
        },
        Err(source) => {
            // Roll back so we never leave the install without an exe
            let _ = fs::rename(&old, exe);
            Err(UError::FileError { path: exe.to_path_buf(), source })
        },
    }
}

/// Locate the uvman binary inside an extracted release archive. Release
/// layouts are inconsistent between platforms: tar.gz assets wrap the binary
/// in a top-level directory while the Windows zip stores it at the root, so
/// search instead of assuming a layout.
pub fn find_binary(root: &Path, bin_name: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, bin_name: &str) -> Option<PathBuf> {
        for entry in fs::read_dir(dir).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                if let Some(found) = walk(&path, bin_name) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|n| n == bin_name) {
                return Some(path);
            }
        }
        None
    }
    walk(root, bin_name)
}

/// Best-effort removal of a leftover `<exe>.old` from a previous self-update.
/// Called on every launch: on Windows the old binary is locked by the dying
/// process, so deletion only succeeds from a fresh process.
pub fn cleanup_stale_backup(exe: &Path) {
    let _ = fs::remove_file(backup_path(exe));
}

fn backup_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn write_file(path: &Path, content: &[u8]) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(content).unwrap();
    }

    #[test]
    fn parse_sha256_extracts_digest() {
        let text = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2  uvman-v0.2.0.zip\n";
        assert_eq!(
            parse_sha256(text).as_deref(),
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2")
        );
        // uppercase digests are normalized
        assert_eq!(
            parse_sha256("A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5F6A1B2  x").as_deref(),
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2")
        );
        assert!(parse_sha256("too short").is_none());
        assert!(parse_sha256("").is_none());
    }

    #[test]
    fn verify_sha256_matches_and_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bin");
        write_file(&file, b"hello");

        // sha256("hello")
        let ok = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(&file, ok).is_ok());
        assert!(verify_sha256(&file, &ok.to_uppercase()).is_ok());

        let bad = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(verify_sha256(&file, bad).is_err());
    }

    #[test]
    fn replace_executable_missing_source_fails() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("uvman");
        write_file(&exe, b"old");
        let err = replace_executable(&exe, &dir.path().join("missing")).unwrap_err();
        assert!(matches!(err, UError::PathNotFound { .. }));
        // the target must be untouched
        assert_eq!(fs::read(&exe).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn replace_executable_unix_swaps_content() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("uvman");
        let new = dir.path().join("uvman.new");
        write_file(&exe, b"old");
        write_file(&new, b"new");

        let outcome = replace_executable(&exe, &new).unwrap();
        assert!(matches!(outcome, ReplaceOutcome::Replaced));
        assert_eq!(fs::read(&exe).unwrap(), b"new");
    }

    #[cfg(windows)]
    #[test]
    fn replace_executable_windows_swaps_content() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("uvman.exe");
        let new = dir.path().join("uvman.new");
        write_file(&exe, b"old");
        write_file(&new, b"new");

        // not the running exe, so the .old delete succeeds in tests
        let outcome = replace_executable(&exe, &new).unwrap();
        assert!(matches!(outcome, ReplaceOutcome::Replaced));
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert!(!exe.with_file_name("uvman.exe.old").exists());
    }

    #[test]
    fn find_binary_locates_flat_and_wrapped_layouts() {
        // flat: zip-style, binary at the archive root
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("uvman.exe"), b"bin");
        assert_eq!(find_binary(dir.path(), "uvman.exe"), Some(dir.path().join("uvman.exe")));

        // wrapped: tar.gz-style, binary inside a top-level dir
        let dir2 = tempfile::tempdir().unwrap();
        let nested = dir2.path().join("uvman-v0.2.0-x86_64-pc-windows-msvc");
        fs::create_dir_all(&nested).unwrap();
        write_file(&nested.join("uvman.exe"), b"bin");
        assert_eq!(find_binary(dir2.path(), "uvman.exe"), Some(nested.join("uvman.exe")));

        // absent
        let dir3 = tempfile::tempdir().unwrap();
        assert_eq!(find_binary(dir3.path(), "uvman.exe"), None);
    }

    #[test]
    fn cleanup_stale_backup_removes_old() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("uvman.exe");
        write_file(&exe, b"new");
        let old = exe.with_file_name("uvman.exe.old");
        write_file(&old, b"old");

        cleanup_stale_backup(&exe);
        assert!(!old.exists());
        assert!(exe.exists());
    }
}
