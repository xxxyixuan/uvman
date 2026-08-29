use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::error::UError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPlugin {
    pub tool: ToolMeta,

    pub registry: Registry,

    pub release: Release,

    pub platform: Option<Platform>,

    pub install: Install,
}

impl ToolPlugin {
    pub fn load_from(path: &Path) -> Result<ToolPlugin, UError> {
        let content = std::fs::read_to_string(path)
            .map_err(|source| UError::FileError { path: path.to_path_buf(), source })?;
        toml::from_str(&content)
            .map_err(|source| UError::TomlError { path: path.to_path_buf(), source })
    }

    /// Map system OS/ARCH constants via platform.os_map / arch_map to download
    /// tokens (e.g. windows -> win, x86_64 -> x64)
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
            UError::PlatformNotSupported { os: sys_os.to_string(), arch: sys_arch.to_string() }
        })?;
        let arch = platform.arch_map.get(sys_arch).cloned().ok_or_else(|| {
            UError::PlatformNotSupported { os: os.clone(), arch: sys_arch.to_string() }
        })?;

        Ok((os, arch))
    }

    /// Replace {key} placeholders in link/path templates (download.path,
    /// hash.path, etc.) with real values; missing ones are left as-is
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

    /// Plugin's own version (displayed by `plugin info`; consumed by the
    /// deferred `plugin upgrade`, see .tmp/feat design doc)
    #[serde(default)]
    pub version: Option<String>,

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

/// Version release source; the `source` field selects the variant (internal
/// tag; TOML always has `source`):
///
/// ```toml
/// [release]                    # api: fetch JSON from url,
/// source = "api"               # locate via version_path, clean via version_pattern
/// url = "https://..."
/// version_path = "data.tags"   # optional
/// version_pattern = "..."      # optional
///
/// [release]                    # static: use a fixed list in the plugin
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallDefaults {
    /// Version installed when the user omits one (`uvman install node`)
    pub version: String,
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
}

/// Per-OS value: a plain value applies to all OS, or an explicit OS mapping.
///
/// Different OS releases of the same tool may differ in layout (e.g. Node's
/// Windows zip keeps the binary at the root, linux/macos tarballs under bin/),
/// so deploy fields need the same per-OS expressiveness as ext/post_install.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PerOs<T> {
    /// Plain form: `bin_dir = "bin/"`, applies to all OS
    All(T),
    /// Per-OS mapping: `bin_dir = { linux = "bin/", windows = "" }`
    ByOs(HashMap<String, T>),
}

impl<T> PerOs<T> {
    /// Resolve the value for the current OS; ByOs errors clearly if that OS is
    /// missing (rather than silently falling back, so layout mistakes
    /// surface before install)
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
        // A missing OS entry must error, not silently fall back
        assert!(cfg.bin_dir.resolve("freebsd").is_err());
    }

    /// Inline node plugin sample (isomorphic to node.toml in the plugin repo).
    /// Self-contained fixture; does not depend on external files under test/
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
        // Candidate sources: mirrors first, default as fallback
        let sources = plugin.registry.sources();
        assert_eq!(sources[0], "https://npmmirror.com/mirrors/node");
        assert!(sources.contains(&"https://nodejs.org/dist".to_string()));
    }

    #[test]
    fn test_resolve_and_render_node() {
        let plugin: ToolPlugin = toml::from_str(node_plugin_toml()).unwrap();

        let (os, arch) = plugin.resolve_platform().unwrap();
        let platform = plugin.platform.as_ref().unwrap();
        // Mapped result must match the system constants' targets in os_map/arch_map
        assert_eq!(os, platform.os_map[crate::core::platform::OS.as_str()]);
        assert_eq!(arch, platform.arch_map[crate::core::platform::ARCH.as_str()]);

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
        // api variant: source is the internal tag; remaining fields map to the variant
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

        // static variant: uses the fixed list directly
        let fixed: Release = toml::from_str(
            r#"
            source = "static"
            versions = ["1.0.0", "1.1.0"]
            "#,
        )
        .unwrap();
        match fixed {
            Release::Static { versions } => {
                assert_eq!(versions, vec!["1.0.0".to_string(), "1.1.0".to_string()]);
            },
            other => panic!("应解析为 Static 变体，实际 {other:?}"),
        }

        // An unknown source must error (the original design silently passed, hiding
        // config errors)
        assert!(toml::from_str::<Release>(r#"source = "unknown""#).is_err());
    }

    #[test]
    fn test_load_node_plugin_release_is_api() {
        let plugin: ToolPlugin = toml::from_str(node_plugin_toml()).unwrap();
        assert!(matches!(plugin.release, Release::Api { .. }));
    }
}
