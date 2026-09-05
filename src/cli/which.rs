//! `uvman which <tool>`: absolute path of the active version's executable.
//!
//! Read-only: the answer comes from the activation table plus the deployed
//! version dir on disk; nothing writes state. Output is a single bare absolute
//! path (no decoration, no styling) so scripts can consume it directly.

use std::path::{Path, PathBuf};

use crate::Result;
use crate::core::current;
use crate::core::error::UError;
use crate::core::paths::{absolute, tools_dir};

/// Print the absolute path of the executable behind a tool's active version
///
/// Resolution follows `current`: the globally active version selects
/// `tools/<tool>/<version>/`, where deploy flattens the plugin's `bin_dir`
/// contents; its `bin/` subdirectory is probed as well (`env` prepends it to
/// PATH when present). The first file named after the tool wins — Windows
/// tries `.exe` / `.cmd` / `.bat` / `.ps1` in order, Unix the bare name.
///
/// Unlike `current`, an unanswerable query is an error (exit code non-zero):
/// no active version, an active version dir deleted by hand, or a deploy
/// without a matching executable.
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct Which {
    /// Tool name to locate
    pub tool: String,
}

impl Which {
    pub fn run(&self) -> Result<()> {
        let Some(version) = current::current_version(&self.tool) else {
            return Err(UError::NoActiveVersion { tool: self.tool.clone() }.into());
        };
        let path = locate_executable(&tools_dir(), &self.tool, &version)?;
        println!("{}", path.display());
        Ok(())
    }
}

/// Version-dir half of the resolution chain: active version → deploy dir →
/// executable. `tools_root` is a parameter so tests can use a scratch dir
/// (same pattern as `env`).
fn locate_executable(tools_root: &Path, tool: &str, version: &str) -> Result<PathBuf, UError> {
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

/// Executable file names to probe for, in priority order: the deploy
/// extensions on Windows (binary before script), the bare name on Unix.
fn bin_candidates(tool: &str) -> Vec<String> {
    if cfg!(windows) {
        [".exe", ".cmd", ".bat", ".ps1"].iter().map(|ext| format!("{tool}{ext}")).collect()
    } else {
        vec![tool.to_string()]
    }
}

/// First existing file among the candidates: the version dir root first (env
/// puts it ahead of `bin/` on PATH), then the `bin/` subdir.
fn find_executable(version_dir: &Path, tool: &str) -> Option<PathBuf> {
    let bin_dir = version_dir.join("bin");
    for dir in [version_dir, bin_dir.as_path()] {
        for name in bin_candidates(tool) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
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
    fn test_locate_executable_finds_deployed_binary() {
        let root = tempfile::tempdir().unwrap();
        let version_dir = root.path().join("node").join("22.19.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        let name = if cfg!(windows) { "node.exe" } else { "node" };
        write_file(&version_dir.join(name), "binary");

        let path = locate_executable(root.path(), "node", "22.19.0").unwrap();
        assert_eq!(path, version_dir.join(name));
        assert!(path.is_absolute(), "which must report an absolute path");
    }

    #[test]
    fn test_locate_executable_version_dir_deleted_by_hand() {
        // Active version dir missing on disk → "version not found" (report,
        // never repair state)
        let root = tempfile::tempdir().unwrap();
        let err = locate_executable(root.path(), "node", "22.19.0").unwrap_err();
        match err {
            UError::VersionNotFound { tool, version } => {
                assert_eq!(tool, "node");
                assert_eq!(version, "22.19.0");
            },
            other => panic!("expected VersionNotFound, got {other:?}"),
        }
    }

    #[test]
    fn test_locate_executable_empty_deploy_dir_errors_with_redeploy_hint() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("node").join("22.19.0")).unwrap();

        let err = locate_executable(root.path(), "node", "22.19.0").unwrap_err();
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
    fn test_new_error_hints_and_exit_codes() {
        let err = UError::NoActiveVersion { tool: "node".into() };
        let hint = err.hint().expect("hint");
        assert!(hint.commands.iter().any(|c| c.contains("uvman use node")));

        // Both new errors are state problems, not usage errors: exit 1
        assert_eq!(err.exit_code(), 1);
        assert_eq!(
            UError::ExecutableNotFound {
                tool: "node".into(),
                version: "22.19.0".into(),
                path: PathBuf::from("tools/node/22.19.0"),
            }
            .exit_code(),
            1
        );
    }
}
