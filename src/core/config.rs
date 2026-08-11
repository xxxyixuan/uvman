use std::collections::HashMap;
use std::fs;
use std::path::Path;

use indoc::indoc;
use serde::{Deserialize, Serialize};

use crate::Lazy;
use crate::core::error::UError;
use crate::core::{SingleOrArray, paths};

pub static GLOBAL_CONFIG: Lazy<UvmanConfig> = Lazy::new(|| {
    let path = paths::global_config_path();
    UvmanConfig::load_from(&path).unwrap_or_else(|e| {
        if !is_missing_config(&e) {
            crate::ui::report::print_warning(&format!(
                "failed to load config {}, using defaults: {e}",
                path.display()
            ));
        }
        UvmanConfig::default()
    })
});

/// 是否为「配置文件不存在」（首次运行，非错误）
fn is_missing_config(err: &UError) -> bool {
    matches!(
        err,
        UError::FileError { source, .. }
            if source.kind() == std::io::ErrorKind::NotFound
    )
}

/// 默认全局配置模板（首次运行生成，用户可自行修改）
pub const DEFAULT_CONFIG: &str = indoc!(
    r#"
# UVMAN 全局配置文件

# 插件仓库配置
[plugin]
repo = "https://github.com/xxxyixuan/uvman-plugin"
# proxy = "http://127.0.0.1:7890"

# 全局下载镜像源
# 优先级高于插件自带的镜像配置
# e.g.:
# node = [
#   "https://npmmirror.com/mirrors/node",
#   "http://mirrors.cloud.tencent.com/npm/"
# ]
[registry]

# 全局网络设置
[network]
# timeout = 300
# retries = 3
# retry_delay = 2
# proxy = "http://proxy.example.com:8080"

# 缓存设置
[cache]
# 下载档案在 cache/tools/ 中的保留时长（小时），超期后随下次安装自动清理
# 0 表示完全不保留缓存（安装完成后立即删除压缩包）
# 未配置时默认 24
# ttl = 24

# 激活后注入 Shell 的环境变量
# e.g.:
# JAVA_HOME = "{UVMAN_HOME}/tools/java/{current}"
[env]
"#
);

/// 生成默认全局配置文件（仅当文件不存在时，不覆盖用户已有配置）
pub fn ensure_default_config() -> Result<(), UError> {
    let path = paths::global_config_path();
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|source| UError::FileError {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    fs::write(&path, DEFAULT_CONFIG)
        .map_err(|source| UError::FileError { path: path.clone(), source })?;
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UvmanConfig {
    #[serde(default)]
    pub plugin: PluginConfig,

    #[serde(default)]
    pub registry: HashMap<String, SingleOrArray<String>>,

    #[serde(default)]
    pub network: NetworkConfig,

    #[serde(default)]
    pub cache: CacheConfig,

    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl UvmanConfig {
    pub fn load_from(path: &Path) -> Result<UvmanConfig, UError> {
        let content = std::fs::read_to_string(path).map_err(|source| {
            UError::FileError { path: path.to_path_buf(), source }
        })?;
        toml::from_str(&content).map_err(|source| UError::TomlError {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginConfig {
    pub repo: String,
    #[serde(default)]
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(default)]
    pub timeout: Option<u64>,

    #[serde(default)]
    pub retries: Option<u64>,

    #[serde(default)]
    pub retry_delay: Option<u64>,

    #[serde(default)]
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheConfig {
    /// 下载档案保留时长（小时）；0 表示安装完成后立即删除
    #[serde(default)]
    pub ttl: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_default_config() {
        // 测试环境下 home 为 test/，先自举生成默认配置再加载
        ensure_default_config().unwrap();
        let config_path = paths::global_config_path();
        assert!(config_path.exists());
        let config = UvmanConfig::load_from(&config_path).unwrap();
        // 默认配置应给出可解析的插件仓库地址
        assert!(config.plugin.repo.starts_with("https://"));
        println!("Loaded config: {:#?}", config);
    }

    #[test]
    fn test_default_config_is_valid_toml() {
        // 模板本身必须可被反序列化
        let config: UvmanConfig = toml::from_str(DEFAULT_CONFIG)
            .expect("default config must be valid TOML");
        assert!(config.plugin.repo.starts_with("https://"));
    }
}
