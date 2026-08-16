use std::path::PathBuf;

use thiserror::Error;

/// 修复建议：一句说明 + 若干可整行复制的命令。
///
/// 设计参考 uv 的 `hint:` 与 mise 的提示格式：
/// 命令独立成行输出（缩进两空格、绿色高亮），方便用户直接选中复制。
pub struct Hint {
    /// 一句话说明（可含反引号标记的片段）
    pub message: String,
    /// 可整行复制的命令，每行渲染一个
    pub commands: Vec<String>,
}

/// uvman 领域错误。
///
/// `Display` 输出面向用户的消息；`hint()` 返回可选的修复建议；
/// `exit_code()` 区分用法错误（2）与一般错误（1），与 uv 惯例一致。
#[derive(Error, Debug)]
pub enum UError {
    /// 插件已存在（可 --force 覆盖）
    #[error("plugin '{name}' already exists")]
    PluginAlreadyExists { name: String },

    /// 插件未安装；similar 携带拼写相近的已安装插件名（did you mean）
    #[error("plugin '{name}' is not installed")]
    PluginNotInstalled { name: String, similar: Vec<String> },

    /// 升级源的插件版本低于当前已安装版本（阻止静默降级）
    #[error(
        "plugin '{name}' is already at {current}; source version {remote} is older"
    )]
    PluginDowngrade { name: String, current: String, remote: String },

    /// 未指定要操作的插件
    #[error("no plugin specified to upgrade")]
    MissingPluginTarget,

    /// 无效的 GitHub 仓库 URL
    #[error("'{url}' is not a valid GitHub repository URL")]
    InvalidGitHubUrl { url: String },

    /// 无效 URL
    #[error("'{url}' is not a valid URL")]
    #[allow(dead_code)] // 预留：通用 URL 校验错误
    InvalidUrl { url: String },

    /// 插件名非法
    #[error(
        "invalid plugin name '{name}': only letters, digits, '_' and '-' are allowed"
    )]
    InvalidPluginName { name: String },

    /// 路径不存在
    #[error("path '{}' does not exist", path.display())]
    PathNotFound { path: PathBuf },

    /// 路径不是文件
    #[error("path '{}' is not a file", path.display())]
    NotAFile { path: PathBuf },

    /// 目标文件已存在
    #[error("file already exists: {}", path.display())]
    FileExists { path: PathBuf },

    /// 兜底错误
    #[error("{0}")]
    SimpleError(String),

    /// 通用IO错误
    #[error("IO error: {source}")]
    IoError {
        #[from]
        source: std::io::Error,
    },

    /// 文件操作错误
    #[error("file operation failed on {}: {source}", path.display())]
    FileError { path: PathBuf, source: std::io::Error },

    /// TOML 反序列化错误
    #[error("failed to parse {}: {source}", path.display())]
    TomlError { path: PathBuf, source: toml::de::Error },

    /// TOML 序列化错误
    #[error("failed to serialize {}: {source}", path.display())]
    TomlSerializeError { path: PathBuf, source: toml::ser::Error },

    /// JSON 序列化/反序列化错误
    #[error("JSON error: {source}")]
    JsonError { source: serde_json::Error },

    /// HTTP 请求失败
    #[error("failed to reach {url}: {source}")]
    NetworkError { url: String, source: reqwest::Error },

    /// 代理配置错误
    #[error("invalid proxy URL '{url}': {source}")]
    ProxyError { url: String, source: reqwest::Error },

    /// HTTP 响应状态码错误
    ///
    /// 不向用户暴露内部下载 URL（如 raw.githubusercontent.com 的完整地址），
    /// 那对自助排查无益；具体地址在 `--verbose` 的详细报告中可见。
    #[error("the remote server returned HTTP {status}")]
    HttpStatusError { url: String, status: u16 },

    /// 文件校验和错误
    #[error("checksum mismatch: {message}")]
    ChecksumError { message: String },

    /// 压缩包解压失败（tar/zip/gzip/xz）
    #[error("failed to extract {}: {source}", path.display())]
    ExtractError {
        path: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// 源码编译失败
    #[error("build failed: {message}")]
    #[allow(dead_code)] // 预留：源码构建错误
    BuildError { message: String },

    /// 当前平台无匹配的安装方案
    #[error("no installation available for {os}-{arch}")]
    PlatformNotSupported { os: String, arch: String },

    /// 请求的版本不存在
    #[error("version {version} not found for {tool}")]
    VersionNotFound { tool: String, version: String },

    /// 目标版本已安装（不 `--force` 时阻止重复安装）
    #[error("{tool}@{version} is already installed")]
    AlreadyInstalled { tool: String, version: String },
}

impl UError {
    /// 修复建议：一句说明 + 可整行复制的命令。
    ///
    /// 仅在用户无法从错误本身推断下一步时给出（uv 的 hint 设计原则）。
    pub fn hint(&self) -> Option<Hint> {
        let hint = match self {
            Self::PluginAlreadyExists { name } => Hint {
                message: "to overwrite the existing plugin, run:".into(),
                commands: vec![format!(
                    "uvman plugin install {name} --force"
                )],
            },
            Self::PluginNotInstalled { name, similar } => {
                if let Some(first) = similar.first() {
                    let more = similar.len().saturating_sub(1);
                    Hint {
                        message: if more > 0 {
                            format!(
                                "did you mean '{first}'? ({more} more similar name(s) found)"
                            )
                        } else {
                            format!("did you mean '{first}'?")
                        },
                        commands: vec![],
                    }
                } else {
                    Hint {
                        message: "to install it, run:".into(),
                        commands: vec![format!(
                            "uvman plugin install {name}"
                        )],
                    }
                }
            }
            Self::MissingPluginTarget => Hint {
                message: "specify a plugin name, or upgrade all installed plugins:".into(),
                commands: vec!["uvman plugin upgrade --all".into()],
            },
            Self::PluginDowngrade { name, .. } => Hint {
                message: "upgrade does not downgrade; to overwrite the installed plugin, run:"
                    .into(),
                commands: vec![format!("uvman plugin install {name} --force")],
            },
            Self::InvalidGitHubUrl { .. } => Hint {
                message: "the repository URL must look like:".into(),
                commands: vec!["https://github.com/<owner>/<repo>".into()],
            },
            Self::InvalidUrl { url } => Hint {
                message: format!(
                    "'{url}' is missing a scheme (e.g. `https://`) or is malformed"
                ),
                commands: vec![],
            },
            Self::FileExists { .. } => Hint {
                message: "choose another name, or remove the existing file first".into(),
                commands: vec![],
            },
            Self::VersionNotFound { tool, .. } => Hint {
                message: format!(
                    "list installed versions with `uvman list {tool}`, \
                     or published ones with:"
                ),
                commands: vec![format!("uvman list {tool} --remote")],
            },
            Self::AlreadyInstalled { tool, version } => Hint {
                message: "to reinstall and overwrite the existing files, run:".into(),
                commands: vec![format!("uvman install {tool}@{version} --force")],
            },
            // 网络层失败统一提示代理配置（GitHub 相关域名在部分网络需代理访问）
            Self::NetworkError { .. } | Self::ProxyError { .. } => Hint {
                message: "check your network, or sync the index through a proxy:".into(),
                commands: vec!["uvman plugin sync --proxy <proxy-url>".into()],
            },
            Self::HttpStatusError { status, .. } => match status {
                404 => Hint {
                    message: "the plugin was not found in the remote registry; \
                              double-check the name, or list available plugins:"
                        .into(),
                    commands: vec!["uvman plugin list --remote".into()],
                },
                403 => Hint {
                    message: "GitHub API rate limit exceeded; wait a while or use a proxy".into(),
                    commands: vec![],
                },
                _ => return None,
            },
            Self::TomlError { .. } => Hint {
                message: "fix the TOML syntax error at the location shown above".into(),
                commands: vec![],
            },
            Self::IoError { source, .. }
                if source.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                Hint {
                    message: "permission denied; check the file/folder ownership and access rights".into(),
                    commands: vec![],
                }
            }
            Self::FileError { source, .. }
                if source.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                Hint {
                    message: "permission denied; check the file/folder ownership and access rights".into(),
                    commands: vec![],
                }
            }
            _ => return None,
        };
        Some(hint)
    }

    /// 进程退出码：用法/输入错误为 2（与 uv、clap 惯例一致），其余为 1
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::MissingPluginTarget
            | Self::InvalidPluginName { .. }
            | Self::InvalidUrl { .. }
            | Self::InvalidGitHubUrl { .. }
            | Self::PathNotFound { .. }
            | Self::NotAFile { .. } => 2,
            _ => 1,
        }
    }

    /// 是否为携带底层 source 的系统级错误（用于决定是否显示 --verbose footer）
    pub fn has_source(&self) -> bool {
        matches!(
            self,
            Self::IoError { .. }
                | Self::FileError { .. }
                | Self::TomlError { .. }
                | Self::TomlSerializeError { .. }
                | Self::JsonError { .. }
                | Self::NetworkError { .. }
                | Self::ProxyError { .. }
                | Self::HttpStatusError { .. }
                | Self::ExtractError { .. }
        )
    }
}
