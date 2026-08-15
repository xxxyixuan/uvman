use crate::core::error::UError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

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

    /// 将系统 OS/ARCH 常量经 platform.os_map / arch_map 映射为下载标识（如 windows -> win、x86_64 -> x64）
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

    /// 将链接/路径模板（download.path、hash.path 等）中的 {key} 占位符替换为实际值；
    /// 未提供的占位符保留原样
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    /// value：api / static
    pub source: String,
    pub url: String,
    #[serde(default)]
    pub version_path: Option<String>,
    #[serde(default)]
    pub version_pattern: Option<String>,
    /// used when source is static defaults to none
    #[serde(default)]
    pub versions: Option<Vec<String>>,
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
    pub bin_dir: String,
    pub copy_extra: Option<Vec<String>>,
    pub post_install: Option<HashMap<String, Vec<String>>>,
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
    use crate::core::paths::plugin_path;

    #[test]
    fn test_load_plugin() {
        let node_plugin_path = plugin_path("node");
        let content = std::fs::read_to_string(node_plugin_path).unwrap();
        let plugin: ToolPlugin = toml::from_str(&content).unwrap();

        println!("{:#?}", plugin);
        println!("{:#?}", plugin.registry.sources());
    }

    #[test]
    fn test_resolve_and_render_node() {
        let plugin = ToolPlugin::load_from(&plugin_path("node")).unwrap();

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
        println!("resolved url: {url}");
        assert!(url.starts_with(&plugin.registry.default));
        assert!(url.ends_with(&format!("node-v20.11.0-{os}-{arch}.zip")));
    }
}
