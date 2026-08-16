use std::fs;
use std::path::PathBuf;

use crate::core::error::UError;

fn user_home() -> PathBuf {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(key).map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

/// 可执行文件所在目录（Windows 便携安装的默认数据根目录）
#[cfg(windows)]
fn executable_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// uvman 数据根目录（tools/plugins/cache/config/logs 的统一父目录）。
///
/// 平台差异的默认取值：
/// - **Windows**：默认取可执行文件所在目录（便携式），数据跟随二进制存放，
///   移到任意位置即可整体迁移；`UVMAN_HOME` 环境变量可覆盖指定其他位置。
/// - **Linux/macOS**：固定为 `$HOME/.uvman`，不读取 `UVMAN_HOME`，
///   遵循类 Unix 约定（用户级工具数据独立于可执行文件位置）。
pub fn uvman_home() -> PathBuf {
    // 仅单测（cfg(test) 由 libtest 编译注入）用 test/ 隔离，避免污染用户真实数据。
    // 严禁扩展到 debug_assertions：debug 二进制会被加入 PATH 当真实工具用，
    // 相对路径 home 会随 CWD 在任意目录创建 test/（P0 bug）。
    // 开发期需要隔离数据目录时，设置 UVMAN_HOME 环境变量（仅 Windows 生效）。
    if cfg!(test) {
        return PathBuf::from("test");
    }

    #[cfg(windows)]
    {
        // Windows：UVMAN_HOME 优先（便携可覆盖），否则默认二进制目录
        if let Ok(p) = std::env::var("UVMAN_HOME") {
            return PathBuf::from(p);
        }
        if let Some(dir) = executable_dir() {
            return dir;
        }
    }

    // Linux/macOS：固定 ~/.uvman，不读取 UVMAN_HOME
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

/// 远端已发布版本缓存目录（`list <tool> --remote` 的 api 源缓存）
pub fn cache_versions_dir() -> PathBuf {
    uvman_home().join("cache").join("versions")
}

#[allow(dead_code)] // 预留：源码构建缓存目录
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

/// 相对路径锚定为绝对路径：注入/烘焙进 shell 脚本的值不应随 CWD 漂移
/// （UVMAN_HOME 允许配置为相对路径，必须先求值固定）
pub fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
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
