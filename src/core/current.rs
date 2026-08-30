use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::error::UError;
use crate::core::paths::tool_current_path;

/// `config/tool_current.toml`: records the currently active version of each
/// tool.
///
/// Responsibility boundary (per design doc): `use` writes version switches
/// and `uninstall` removes the entry of an uninstalled active version; `env` /
/// `list` only read. A missing or corrupt file is treated as "no active
/// version".
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CurrentTools {
    /// Keyed by tool name; BTreeMap keeps serialized output ordered (idempotent
    /// diff)
    #[serde(flatten)]
    pub tools: BTreeMap<String, CurrentEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CurrentEntry {
    pub version: String,
}

/// Load the current-tools table (default path); returns empty on
/// missing/corrupt
pub fn load() -> CurrentTools {
    load_from(&tool_current_path())
}

/// Query the currently active version of a tool
pub fn current_version(tool: &str) -> Option<String> {
    load().tools.get(tool).map(|e| e.version.clone())
}

/// Set (or switch) the current version of a tool
pub fn set_current(tool: &str, version: &str) -> Result<(), UError> {
    set_current_at(&tool_current_path(), tool, version)
}

/// Remove a tool's active-version entry (its active version was uninstalled);
/// returns whether an entry existed
pub fn remove_current(tool: &str) -> Result<bool, UError> {
    remove_current_at(&tool_current_path(), tool)
}

pub fn load_from(path: &Path) -> CurrentTools {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        // Reader tolerance: a missing/corrupt table must not break read-only commands
        Err(_) => CurrentTools::default(),
    }
}

pub fn set_current_at(path: &Path, tool: &str, version: &str) -> Result<(), UError> {
    let mut table = load_from(path);
    table.tools.insert(tool.to_string(), CurrentEntry { version: version.to_string() });
    write_table(path, &table)
}

pub fn remove_current_at(path: &Path, tool: &str) -> Result<bool, UError> {
    let mut table = load_from(path);
    if table.tools.remove(tool).is_none() {
        return Ok(false);
    }
    write_table(path, &table)?;
    Ok(true)
}

/// Serialize and persist the table (creating parent dirs as needed)
fn write_table(path: &Path, table: &CurrentTools) -> Result<(), UError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|source| UError::FileError { path: parent.to_path_buf(), source })?;
    }
    let text = toml::to_string_pretty(table)
        .map_err(|source| UError::TomlSerializeError { path: path.to_path_buf(), source })?;
    fs::write(path, text).map_err(|source| UError::FileError { path: path.to_path_buf(), source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool_current.toml");

        set_current_at(&path, "node", "22.19.0").unwrap();
        set_current_at(&path, "go", "1.23.0").unwrap();

        let table = load_from(&path);
        assert_eq!(table.tools.get("node").map(|e| e.version.as_str()), Some("22.19.0"));
        assert_eq!(table.tools.get("go").map(|e| e.version.as_str()), Some("1.23.0"));

        // Switching an existing entry overwrites the old version
        set_current_at(&path, "node", "22.23.2").unwrap();
        assert_eq!(
            load_from(&path).tools.get("node").map(|e| e.version.clone()),
            Some("22.23.2".to_string())
        );
    }

    #[test]
    fn test_serialized_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool_current.toml");
        set_current_at(&path, "node", "22.19.0").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, "[node]\nversion = \"22.19.0\"\n");
    }

    #[test]
    fn test_load_tolerates_missing_and_corrupt() {
        // Missing file -> empty table
        assert!(load_from(Path::new("definitely/missing.toml")).tools.is_empty());
        // Corrupt content -> empty table, no panic
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool_current.toml");
        fs::write(&path, "not = [valid").unwrap();
        assert!(load_from(&path).tools.is_empty());
    }

    #[test]
    fn test_remove_current() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/tool_current.toml");
        set_current_at(&path, "node", "22.19.0").unwrap();
        set_current_at(&path, "go", "1.23.0").unwrap();

        assert!(remove_current_at(&path, "node").unwrap());
        // Other tools' entries survive; the removed one is gone
        assert!(load_from(&path).tools.contains_key("go"));
        assert!(!load_from(&path).tools.contains_key("node"));

        // Removing again is a no-op reporting false
        assert!(!remove_current_at(&path, "node").unwrap());
    }
}
