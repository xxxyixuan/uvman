//! Executable lookup shared by `uvman which` and the `uvman-shim` forwarder.
//!
//! Given a tool and its active version, locate a command-named executable in
//! the version dir (root first, then `bin/`) using the platform extension
//! rules. The shim forwarder reuses the same rules so a shim resolves exactly
//! what `which` would print — one resolution source, zero drift (plan 0.3.0
//! task 1: shim and main command share `core`).

use std::path::{Path, PathBuf};

use crate::core::error::UError;
use crate::core::paths::absolute;

/// Executable file names to probe for, in priority order: the deploy
/// extensions on Windows (binary before script), the bare name on Unix.
pub fn bin_candidates(name: &str) -> Vec<String> {
    if cfg!(windows) {
        [".exe", ".cmd", ".bat", ".ps1"].iter().map(|ext| format!("{name}{ext}")).collect()
    } else {
        vec![name.to_string()]
    }
}

/// First existing file among the candidates: the version dir root first (env
/// puts it ahead of `bin/` on PATH), then the `bin/` subdir.
pub fn find_executable(version_dir: &Path, name: &str) -> Option<PathBuf> {
    let bin_dir = version_dir.join("bin");
    for dir in [version_dir, bin_dir.as_path()] {
        for candidate in bin_candidates(name) {
            let candidate = dir.join(candidate);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Version-dir half of the resolution chain: active version → deploy dir →
/// executable. `tools_root` is a parameter so tests can use a scratch dir
/// (same pattern as `env`).
pub fn locate(tools_root: &Path, tool: &str, version: &str) -> Result<PathBuf, UError> {
    let version_dir = absolute(tools_root.join(tool).join(version));
    // An active version whose dir was deleted by hand counts as "not
    // installed": read-only commands report, never repair state.
    if !version_dir.is_dir() {
        return Err(UError::VersionNotFound {
            tool: tool.to_string(),
            version: version.to_string(),
        });
    }
    find_executable(&version_dir, tool).ok_or_else(|| UError::ExecutableNotFound {
        tool: tool.to_string(),
        version: version.to_string(),
        path: version_dir,
    })
}

/// Whether a resolved version is actually deployed: its version dir exists
/// under the tools root.
pub fn deployed(tools_root: &Path, tool: &str, version: &str) -> bool {
    tools_root.join(tool).join(version).is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn test_bin_candidates_extension_rules() {
        // Windows probes the deploy extensions in order; Unix the bare name
        if cfg!(windows) {
            assert_eq!(
                bin_candidates("node"),
                vec!["node.exe", "node.cmd", "node.bat", "node.ps1"]
            );
        } else {
            assert_eq!(bin_candidates("node"), vec!["node"]);
        }
    }

    #[test]
    fn test_find_executable_prefers_binary_extension() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("node"), "script");
        write_file(&dir.path().join("node.cmd"), "script");
        write_file(&dir.path().join("node.exe"), "binary");

        let found = find_executable(dir.path(), "node").unwrap();
        if cfg!(windows) {
            assert_eq!(found, dir.path().join("node.exe"));
        } else {
            assert_eq!(found, dir.path().join("node"));
        }
    }

    #[test]
    fn test_find_executable_windows_falls_through_extensions() {
        let dir = tempfile::tempdir().unwrap();
        // No .exe present: the next extension (.cmd) matches; on Unix the
        // bare name is the only candidate, so nothing matches here
        write_file(&dir.path().join("node.cmd"), "script");
        let found = find_executable(dir.path(), "node");
        if cfg!(windows) {
            assert_eq!(found, Some(dir.path().join("node.cmd")));
        } else {
            assert_eq!(found, None, "Unix only probes the bare name");
        }
    }

    #[test]
    fn test_find_executable_falls_back_to_bin_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let name = if cfg!(windows) { "node.exe" } else { "node" };
        write_file(&bin.join(name), "binary");
        assert_eq!(find_executable(dir.path(), "node"), Some(bin.join(name)));
    }

    #[test]
    fn test_locate_finds_deployed_binary() {
        let root = tempfile::tempdir().unwrap();
        let version_dir = root.path().join("node").join("22.19.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        let name = if cfg!(windows) { "node.exe" } else { "node" };
        write_file(&version_dir.join(name), "binary");

        let path = locate(root.path(), "node", "22.19.0").unwrap();
        assert_eq!(path, version_dir.join(name));
        assert!(path.is_absolute(), "which must report an absolute path");
    }

    #[test]
    fn test_locate_version_dir_deleted_by_hand() {
        // Active version dir missing on disk → "version not found" (report,
        // never repair state)
        let root = tempfile::tempdir().unwrap();
        let err = locate(root.path(), "node", "22.19.0").unwrap_err();
        match err {
            UError::VersionNotFound { tool, version } => {
                assert_eq!(tool, "node");
                assert_eq!(version, "22.19.0");
            },
            other => panic!("expected VersionNotFound, got {other:?}"),
        }
    }

    #[test]
    fn test_locate_empty_deploy_dir_errors_with_redeploy_hint() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("node").join("22.19.0")).unwrap();

        let err = locate(root.path(), "node", "22.19.0").unwrap_err();
        match &err {
            UError::ExecutableNotFound { tool, version, path } => {
                assert_eq!(tool, "node");
                assert_eq!(version, "22.19.0");
                assert!(path.ends_with(std::path::Path::new("node").join("22.19.0").as_path()));
            },
            other => panic!("expected ExecutableNotFound, got {other:?}"),
        }
        // The hint names the redeploy command
        let hint = err.hint().expect("redeploy hint");
        assert!(
            hint.commands.iter().any(|c| c.contains("uvman install node@22.19.0")),
            "hint should suggest redeploying, got {:?}",
            hint.commands
        );
    }

    #[test]
    fn test_deployed_checks_version_dir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("node").join("22.19.0")).unwrap();
        assert!(deployed(root.path(), "node", "22.19.0"));
        assert!(!deployed(root.path(), "node", "20.0.0"));
    }
}
