use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::error::UError;
use crate::core::paths::tool_current_path;

/// `config/tool_current.toml`：记录每个工具当前使用的版本。
///
/// 职责边界（与设计文档一致）：`use` 命令唯一写入方；
/// `env` / `list` 只读。读取容错：文件缺失或损坏视为无激活版本。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CurrentTools {
    /// 键为工具名；BTreeMap 保证序列化输出按工具名有序（幂等 diff）
    #[serde(flatten)]
    pub tools: BTreeMap<String, CurrentEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CurrentEntry {
    pub version: String,
}

/// 读取当前激活表（缺省路径）；缺失或损坏返回空表
pub fn load() -> CurrentTools {
    load_from(&tool_current_path())
}

/// 查询某工具当前使用的版本
pub fn current_version(tool: &str) -> Option<String> {
    load().tools.get(tool).map(|e| e.version.clone())
}

/// 设置（或切换）某工具的当前版本
pub fn set_current(tool: &str, version: &str) -> Result<(), UError> {
    set_current_at(&tool_current_path(), tool, version)
}

pub fn load_from(path: &Path) -> CurrentTools {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        // 读取方容错：缺失或损坏的激活表不应让只读命令失败
        Err(_) => CurrentTools::default(),
    }
}

pub fn set_current_at(
    path: &Path,
    tool: &str,
    version: &str,
) -> Result<(), UError> {
    let mut table = load_from(path);
    table
        .tools
        .insert(tool.to_string(), CurrentEntry { version: version.to_string() });

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| UError::FileError {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let text = toml::to_string_pretty(&table).map_err(|source| {
        UError::TomlSerializeError { path: path.to_path_buf(), source }
    })?;
    fs::write(path, text).map_err(|source| UError::FileError {
        path: path.to_path_buf(),
        source,
    })
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
        assert_eq!(
            table.tools.get("node").map(|e| e.version.as_str()),
            Some("22.19.0")
        );
        assert_eq!(
            table.tools.get("go").map(|e| e.version.as_str()),
            Some("1.23.0")
        );

        // 切换已有条目覆盖旧版本
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
        // 文件不存在 → 空表
        assert!(load_from(Path::new("definitely/missing.toml")).tools.is_empty());
        // 内容损坏 → 空表，不 panic
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool_current.toml");
        fs::write(&path, "not = [valid").unwrap();
        assert!(load_from(&path).tools.is_empty());
    }
}
