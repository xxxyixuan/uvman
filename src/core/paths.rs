use std::fs;
use std::path::PathBuf;

use crate::core::error::UError;

fn user_home() -> PathBuf {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(key).map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

pub fn uvman_home() -> PathBuf {
    // test & dbg 环境下使用 test/ 作为 home 目录，避免污染用户真实数据
    if cfg!(test) || cfg!(debug_assertions) {
        return PathBuf::from("test");
    }

    // 便携/自定义安装通过 UVMAN_HOME 指定数据目录
    if let Ok(p) = std::env::var("UVMAN_HOME") {
        return PathBuf::from(p);
    }

    // 默认数据目录为 ~/.uvman（与设计文档一致，独立于可执行文件位置）
    user_home().join(".uvman")
}

pub fn plugins_dir() -> PathBuf {
    uvman_home().join("plugins")
}

pub fn tools_dir() -> PathBuf {
    uvman_home().join("tools")
}

pub fn cache_dir() -> PathBuf {
    uvman_home().join("cache")
}

pub fn cache_tools_dir() -> PathBuf {
    uvman_home().join("cache").join("tools")
}

pub fn src_build_dir() -> PathBuf {
    uvman_home().join("cache").join("builds")
}

pub fn config_dir() -> PathBuf {
    uvman_home().join("config")
}

pub fn logs_dir() -> PathBuf {
    uvman_home().join("logs")
}

pub fn plugin_path(name: &str) -> PathBuf {
    plugins_dir().join(format!("{name}.toml"))
}

pub fn global_config_path() -> PathBuf {
    uvman_home().join("config").join("uvman.toml")
}

pub fn plugin_index_path() -> PathBuf {
    uvman_home().join("cache").join("plugins.json")
}

pub fn tool_current_path() -> PathBuf {
    config_dir().join("tool_current.toml")
}

pub fn layout_dirs() -> Vec<PathBuf> {
    vec![config_dir(), plugins_dir(), tools_dir(), cache_dir(), logs_dir()]
}

/// 幂等创建 UVMAN_HOME 目录结构；已存在时静默跳过
pub fn ensure_layout() -> Result<(), UError> {
    for dir in layout_dirs() {
        if !dir.exists() {
            fs::create_dir_all(&dir).map_err(|source| UError::FileError {
                path: dir.clone(),
                source,
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_path_under_test() {
        // 测试环境下 uvman_home 默认为 test/
        let path = global_config_path();
        assert!(path.ends_with("test/config/uvman.toml"));
    }

    #[test]
    fn test_plugin_path_under_test() {
        let path = plugin_path("node");
        assert!(path.ends_with("test/plugins/node.toml"));
    }

    #[test]
    fn test_layout_dirs_under_test() {
        let dirs = layout_dirs();
        let names: Vec<&str> = dirs
            .iter()
            .filter_map(|d| d.file_name().and_then(|n| n.to_str()))
            .collect();
        for sub in ["config", "plugins", "tools", "cache", "logs"] {
            assert!(names.contains(&sub), "layout must include {sub}");
        }
        // 全部目录都应位于测试 home（test/）之下
        for dir in &dirs {
            assert!(
                dir.starts_with("test"),
                "{} must be under test/",
                dir.display()
            );
        }
    }
}
