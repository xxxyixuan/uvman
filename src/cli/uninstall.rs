//! `uvman uninstall <tool[@version]>`: remove one installed version or an
//! entire tool, rolling back the active-version record when the removal
//! touches the currently active version.

use std::fs;
use std::path::Path;

use super::install::parse_spec;
use crate::Result;
use crate::core::current;
use crate::core::error::UError;
use crate::core::paths;
use crate::toolset::{installed_versions, resolve_installed_version};
use crate::ui::report::print_hint;
use crate::ui::style::ogreen;

/// Uninstall a tool version or the whole tool
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct Uninstall {
    /// Tool and version to uninstall, in the form `tool@version`
    ///
    /// Omit the version to remove the whole tool with all of its versions.
    /// The version is resolved against installed versions, so partial
    /// versions and aliases work: e.g. `node@22`, `node@latest`, `node`
    pub tool_spec: String,
}

impl Uninstall {
    pub async fn run(&self) -> Result<()> {
        let (tool, version) = parse_spec(&self.tool_spec)?;
        ensure_valid_name(&tool)?;

        // Read the active record up front: it decides the rollback and the
        // notice after the removal
        let active = current::current_version(&tool);

        let mut rolled_back: Option<String> = None;
        let message = match version.as_deref() {
            // `tool@version` / partial / alias: one installed version
            Some(request) => {
                let resolved = resolve_installed_version(&tool, Some(request)).await?;
                uninstall_version_at(&paths::tools_dir(), &tool, &resolved)?;
                let message = uninstalled_version_message(&tool, &resolved);
                if active.as_deref() == Some(resolved.as_str()) {
                    current::remove_current(&tool)?;
                    rolled_back = Some(resolved);
                }
                message
            },
            // `tool`: the whole tool with all of its versions
            None => {
                let count = uninstall_tool_at(&paths::tools_dir(), &tool)?;
                let message = uninstalled_tool_message(&tool, count);
                if let Some(active) = &active {
                    current::remove_current(&tool)?;
                    rolled_back = Some(active.clone());
                }
                message
            },
        };

        println!("{}", ogreen(message));

        // The uninstalled version was the active one: its record is gone;
        // point back to the newest remaining version when there is one
        if let Some(removed) = &rolled_back {
            let remaining = installed_versions(&tool);
            print_hint(&active_removed_message(&tool, removed), &use_suggestion(&tool, &remaining));
        }
        Ok(())
    }
}

/// Remove one installed version directory; drops the tool dir too when the
/// last version leaves (an empty dir would ghost `uvman list`)
fn uninstall_version_at(tools_root: &Path, tool: &str, version: &str) -> Result<(), UError> {
    remove_dir(&tools_root.join(tool).join(version))?;
    let tool_dir = tools_root.join(tool);
    if fs::read_dir(&tool_dir).map(|mut entries| entries.next().is_none()).unwrap_or(false) {
        let _ = fs::remove_dir(&tool_dir);
    }
    Ok(())
}

/// Remove a tool's whole directory (all versions at once). Returns the number
/// of versions removed; errors when nothing is installed.
fn uninstall_tool_at(tools_root: &Path, tool: &str) -> Result<usize, UError> {
    let tool_dir = tools_root.join(tool);
    let versions = list_versions(&tool_dir);
    if versions.is_empty() {
        return Err(UError::SimpleError(format!("no local version of '{tool}' is installed")));
    }
    remove_dir(&tool_dir)?;
    Ok(versions.len())
}

/// Delete a directory, erroring when it doesn't exist (the caller is expected
/// to have resolved it against installed versions first)
fn remove_dir(dir: &Path) -> Result<(), UError> {
    if !dir.exists() {
        return Err(UError::PathNotFound { path: dir.to_path_buf() });
    }
    fs::remove_dir_all(dir).map_err(|source| UError::FileError { path: dir.to_path_buf(), source })
}

/// Version dirs directly under a tool dir; empty when the tool dir is missing
fn list_versions(tool_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(tool_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect()
}

/// Only plain names may be deleted: the name is joined under `tools/`, so
/// separators or `..` would escape the install root
fn ensure_valid_name(name: &str) -> Result<(), UError> {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        Ok(())
    } else {
        Err(UError::InvalidToolName { name: name.to_string() })
    }
}

fn uninstalled_version_message(tool: &str, version: &str) -> String {
    format!("Uninstalled {tool}@{version}")
}

fn uninstalled_tool_message(tool: &str, versions: usize) -> String {
    format!(
        "Uninstalled {tool} ({versions} version{} removed)",
        if versions == 1 { "" } else { "s" }
    )
}

fn active_removed_message(tool: &str, version: &str) -> String {
    format!(
        "{tool}@{version} was the active version; its record in `tool_current.toml` was removed"
    )
}

/// Suggest re-activating the newest remaining version, if any
fn use_suggestion(tool: &str, remaining: &[String]) -> Vec<String> {
    remaining.last().map(|next| vec![format!("uvman use {tool}@{next}")]).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uninstalled_tool_message_singular_and_plural() {
        assert_eq!(uninstalled_tool_message("node", 1), "Uninstalled node (1 version removed)");
        assert_eq!(uninstalled_tool_message("node", 3), "Uninstalled node (3 versions removed)");
        assert_eq!(uninstalled_version_message("node", "22.19.0"), "Uninstalled node@22.19.0");
    }

    #[test]
    fn test_ensure_valid_name() {
        assert!(ensure_valid_name("node").is_ok());
        assert!(ensure_valid_name("node-lts").is_ok());
        assert!(ensure_valid_name("node_2").is_ok());
        // No escaping the install root or empty names
        assert!(ensure_valid_name("").is_err());
        assert!(ensure_valid_name("../..").is_err());
        assert!(ensure_valid_name("a/b").is_err());
        assert!(ensure_valid_name("a b").is_err());
    }

    #[test]
    fn test_uninstall_version_at_cleans_empty_tool_dir() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("node/22.19.0")).unwrap();
        fs::write(root.path().join("node/22.19.0/tool.exe"), b"binary").unwrap();
        fs::create_dir_all(root.path().join("node/20.0.0")).unwrap();
        fs::write(root.path().join("node/20.0.0/tool.exe"), b"binary").unwrap();

        uninstall_version_at(root.path(), "node", "22.19.0").unwrap();
        assert!(!root.path().join("node/22.19.0").exists());
        assert!(root.path().join("node/20.0.0").exists(), "other versions stay");

        // Last version leaves → the empty tool dir goes with it
        uninstall_version_at(root.path(), "node", "20.0.0").unwrap();
        assert!(!root.path().join("node").exists());
    }

    #[test]
    fn test_uninstall_version_at_missing_version_errors() {
        let root = tempfile::tempdir().unwrap();
        let err = uninstall_version_at(root.path(), "node", "9.9.9").unwrap_err();
        assert!(matches!(err, UError::PathNotFound { .. }));
    }

    #[test]
    fn test_uninstall_tool_at_removes_everything() {
        let root = tempfile::tempdir().unwrap();
        for v in ["10.0.0", "22.19.0"] {
            fs::create_dir_all(root.path().join("node").join(v)).unwrap();
        }
        // Stray files under the tool dir aren't versions, but go away with it
        fs::write(root.path().join("node/stray.txt"), b"x").unwrap();

        let count = uninstall_tool_at(root.path(), "node").unwrap();
        assert_eq!(count, 2);
        assert!(!root.path().join("node").exists());
    }

    #[test]
    fn test_uninstall_tool_at_missing_tool_errors() {
        let root = tempfile::tempdir().unwrap();
        let err = uninstall_tool_at(root.path(), "node").unwrap_err();
        assert_eq!(err.to_string(), "no local version of 'node' is installed");
    }

    #[test]
    fn test_use_suggestion_targets_newest_remaining() {
        // installed_versions returns semver-ascending; the newest is last
        let remaining = vec!["20.0.0".to_string(), "22.19.0".to_string()];
        assert_eq!(use_suggestion("node", &remaining), vec!["uvman use node@22.19.0".to_string()]);
        // Nothing left → no suggestion
        assert!(use_suggestion("node", &[]).is_empty());
    }
}
