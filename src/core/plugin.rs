use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::error::UError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPlugin {
    pub tool: ToolMeta,

    #[serde(default)]
    pub aliases: HashMap<String, String>,

    pub registry: Registry,

    pub release: Release,

    pub platform: Option<Platform>,

    pub install: Install,
}

impl ToolPlugin {
    pub fn load_from(path: &Path) -> Result<ToolPlugin, UError> {
        let content = std::fs::read_to_string(path).map_err(|source| {
            UError::FileError { path: path.to_path_buf(), source }
        })?;
        toml::from_str(&content).map_err(|source| UError::TomlError {
            path: path.to_path_buf(),
            source,
        })
    }

    /// 将系统 OS/ARCH 常量经 platform.os_map / arch_map 映射为下载标识（如
    /// windows -> win、x86_64 -> x64）
    pub fn resolve_platform(&self) -> Result<(String, String), UError> {
        let sys_os = crate::core::platform::OS.as_str();
        let sys_arch = crate::core::platform::ARCH.as_str();

        let Some(platform) = &self.platform else {
            return Err(UError::PlatformNotSupported {
                os: sys_os.to_string(),
                arch: sys_arch.to_string(),
            });
        };

        let os = platform.os_map.get(sys_os).cloned().ok_or_else(|| {
            UError::PlatformNotSupported {
                os: sys_os.to_string(),
                arch: sys_arch.to_string(),
            }
        })?;
        let arch =
            platform.arch_map.get(sys_arch).cloned().ok_or_else(|| {
                UError::PlatformNotSupported {
                    os: os.clone(),
                    arch: sys_arch.to_string(),
                }
            })?;

        Ok((os, arch))
    }

    /// 将链接/路径模板（download.path、hash.path 等）中的 {key}
    /// 占位符替换为实际值； 未提供的占位符保留原样
    pub fn render(&self, template: &str, vars: &HashMap<&str, &str>) -> String {
        let mut result = template.to_string();
        for (key, value) in vars {
            result = result.replace(&format!("{{{key}}}"), value);
        }
        result
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMeta {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,

    #[serde(default)]
    pub homepage: Option<String>,

    #[serde(default)]
    pub version: Option<String>,

    #[serde(default)]
    pub license: Option<String>,

    #[serde(default)]
    pub author: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    pub default: String,

    pub mirrors: Option<Vec<String>>,
}

impl Registry {
    pub fn sources(&self) -> Vec<String> {
        let mirrors_iter = self.mirrors.iter().flat_map(|v| v.iter()).cloned();
        let default_iter = std::iter::once(self.default.clone());
        mirrors_iter.chain(default_iter).collect()
    }
}

/// 版本发布源，`source` 字段决定变体（内部标签，TOML 中始终有 `source` 键）：
///
/// ```toml
/// [release]                    # api：请求 url 拉取 JSON，
/// source = "api"               # 经 version_path 定位、version_pattern 清洗
/// url = "https://..."
/// version_path = "data.tags"   # 可选
/// version_pattern = "..."      # 可选
///
/// [release]                    # static：直接使用插件内固定列表
/// source = "static"
/// versions = ["1.0.0", "1.1.0"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum Release {
    Api {
        url: String,
        #[serde(default)]
        version_path: Option<String>,
        #[serde(default)]
        version_pattern: Option<String>,
    },
    Static {
        #[serde(default)]
        versions: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Platform {
    pub os_map: HashMap<String, String>,
    pub arch_map: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Install {
    pub defaults: InstallDefaults,

    #[serde(default)]
    pub bin: Option<Vec<InstallBin>>,

    #[serde(default)]
    pub src: Option<Vec<InstallSrc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallDefaults {
    pub version: String,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallBin {
    pub os: Vec<String>,
    pub arch: Vec<String>,
    pub download: DownloadConfig,
    pub extract: ExtractConfig,
    pub deploy: DeployConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadConfig {
    pub path: String,
    pub ext: HashMap<String, String>,
    pub hash: HashConfig,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashConfig {
    pub enabled: bool,
    #[serde(default)]
    pub algorithm: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractConfig {
    pub strip: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployConfig {
    pub bin_dir: PerOs<String>,
    #[serde(default)]
    pub copy_extra: Option<Vec<String>>,
    pub post_install: Option<HashMap<String, Vec<String>>>,
}

/// 平台差异化取值：纯值对所有 OS 生效，或按 OS 显式映射。
///
/// 同一工具族不同 OS 的发布物布局可能不同（如 Node 的 Windows zip
/// 可执行文件在根目录，linux/macos 的 tar 包在 bin/ 下），
/// 因此 deploy 字段需要与 ext/post_install 一致的 per-OS 表达能力。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PerOs<T> {
    /// 纯值形态：`bin_dir = "bin/"`，对全部 OS 生效
    All(T),
    /// 按 OS 映射形态：`bin_dir = { linux = "bin/", windows = "" }`
    ByOs(HashMap<String, T>),
}

impl<T> PerOs<T> {
    /// 解析当前 OS 的取值；ByOs 缺失该 OS 条目时明确报错
    /// （而非静默回退，避免把布局错误推迟到安装期才暴露）
    pub fn resolve(&self, os: &str) -> Result<&T, UError> {
        match self {
            PerOs::All(v) => Ok(v),
            PerOs::ByOs(map) => map.get(os).ok_or_else(|| {
                UError::SimpleError(format!(
                    "plugin config has no '{os}' entry for this field \
                     (expected one of: {})",
                    map.keys().cloned().collect::<Vec<_>>().join(", ")
                ))
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallSrc {
    pub os: Vec<String>,
    pub arch: Vec<String>,
    pub dependencies: InstallSrcDependencies,
    pub download: DownloadConfig,
    pub extract: ExtractConfig,
    pub build: InstallSrcBuild,
    pub deploy: DeployConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallSrcDependencies {
    pub tools: HashMap<String, Vec<String>>,
    pub system_libs: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallSrcBuild {
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub command: HashMap<String, Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_per_os_deserialize_all_form() {
        #[derive(Deserialize)]
        struct Cfg {
            bin_dir: PerOs<String>,
        }
        let cfg: Cfg = toml::from_str(r#"bin_dir = "bin/""#).unwrap();
        assert_eq!(cfg.bin_dir, PerOs::All("bin/".to_string()));
        assert_eq!(cfg.bin_dir.resolve("windows").unwrap(), "bin/");
        assert_eq!(cfg.bin_dir.resolve("linux").unwrap(), "bin/");
    }

    #[test]
    fn test_per_os_deserialize_by_os_form() {
        #[derive(Deserialize)]
        struct Cfg {
            bin_dir: PerOs<String>,
        }
        let cfg: Cfg = toml::from_str(
            r#"
bin_dir = { linux = "bin/", macos = "bin/", windows = "" }
"#,
        )
        .unwrap();
        assert_eq!(cfg.bin_dir.resolve("windows").unwrap(), "");
        assert_eq!(cfg.bin_dir.resolve("linux").unwrap(), "bin/");
        // 缺失 OS 条目必须报错而非静默回退
        assert!(cfg.bin_dir.resolve("freebsd").is_err());
    }

    /// 内联 node 插件样例（与插件仓库 node.toml 同构）。
    /// 测试自包含夹具，不依赖 test/ 目录下的外部文件
    fn node_plugin_toml() -> &'static str {
        r#"
[tool]
name = "node"
description = "Node.js JavaScript runtime"
version = "1.0.0"

[registry]
default = "https://nodejs.org/dist"
mirrors = ["https://npmmirror.com/mirrors/node"]

[release]
source = "api"
url = "https://nodejs.org/dist/index.json"
version_pattern = '^v(.*)$'

[platform]
os_map = { windows = "win", linux = "linux", macos = "darwin" }
arch_map = { x86_64 = "x64", aarch64 = "arm64" }

[install.defaults]
version = "latest"
mode = "bin"

[[install.bin]]
os = ["windows", "linux", "macos"]
arch = ["x86_64", "aarch64"]

[install.bin.download]
path = "{registry}/v{version}/node-v{version}-{os}-{arch}.{ext}"

[install.bin.download.ext]
windows = "zip"
linux = "tar.gz"
macos = "tar.gz"

[install.bin.download.hash]
enabled = true
algorithm = "sha256"
path = "{registry}/v{version}/SHASUMS256.txt"
pattern = '^(?P<hash>[0-9a-f]{64})\s+.*node-v{version}-{os}-{arch}.{ext}$'

[install.bin.extract]
strip = 1

[install.bin.deploy]
bin_dir = { windows = "", linux = "bin", macos = "bin" }
"#
    }

    #[test]
    fn test_load_plugin() {
        let plugin: ToolPlugin = toml::from_str(node_plugin_toml()).unwrap();
        assert_eq!(plugin.tool.name, "node");
        // 候选源：mirrors 在前、default 兜底
        let sources = plugin.registry.sources();
        assert_eq!(sources[0], "https://npmmirror.com/mirrors/node");
        assert!(sources.contains(&"https://nodejs.org/dist".to_string()));
    }

    #[test]
    fn test_resolve_and_render_node() {
        let plugin: ToolPlugin = toml::from_str(node_plugin_toml()).unwrap();

        let (os, arch) = plugin.resolve_platform().unwrap();
        let platform = plugin.platform.as_ref().unwrap();
        // 映射结果必须与系统常量在 os_map/arch_map 中的目标一致
        assert_eq!(os, platform.os_map[crate::core::platform::OS.as_str()]);
        assert_eq!(
            arch,
            platform.arch_map[crate::core::platform::ARCH.as_str()]
        );

        let mut vars: HashMap<&str, &str> = HashMap::new();
        vars.insert("registry", plugin.registry.default.as_str());
        vars.insert("version", "20.11.0");
        vars.insert("os", os.as_str());
        vars.insert("arch", arch.as_str());
        vars.insert("ext", "zip");

        let download = &plugin.install.bin.as_ref().unwrap()[0].download;
        let url = plugin.render(&download.path, &vars);
        assert!(url.starts_with(&plugin.registry.default));
        assert!(url.ends_with(&format!("node-v20.11.0-{os}-{arch}.zip")));
    }

    #[test]
    fn test_release_enum_parsing() {
        // api 变体：source 作内部标签，其余字段映射到变体成员
        let api: Release = toml::from_str(
            r#"
            source = "api"
            url = "https://nodejs.org/dist/index.json"
            version_pattern = "^v(.*)$"
            "#,
        )
        .unwrap();
        match api {
            Release::Api { url, version_path, version_pattern } => {
                assert_eq!(url, "https://nodejs.org/dist/index.json");
                assert!(version_path.is_none(), "未写的字段应缺省为 None");
                assert_eq!(version_pattern.as_deref(), Some("^v(.*)$"));
            },
            other => panic!("应解析为 Api 变体，实际 {other:?}"),
        }

        // static 变体：直接使用固定列表
        let fixed: Release = toml::from_str(
            r#"
            source = "static"
            versions = ["1.0.0", "1.1.0"]
            "#,
        )
        .unwrap();
        match fixed {
            Release::Static { versions } => {
                assert_eq!(
                    versions,
                    vec!["1.0.0".to_string(), "1.1.0".to_string()]
                );
            },
            other => panic!("应解析为 Static 变体，实际 {other:?}"),
        }

        // 未知 source 应报错（原设计静默通过，无法提前暴露插件配置错误）
        assert!(toml::from_str::<Release>(r#"source = "unknown""#).is_err());
    }

    #[test]
    fn test_load_node_plugin_release_is_api() {
        let plugin: ToolPlugin = toml::from_str(node_plugin_toml()).unwrap();
        assert!(matches!(plugin.release, Release::Api { .. }));
    }
}
