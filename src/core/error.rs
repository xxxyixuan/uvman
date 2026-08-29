use std::path::PathBuf;

use thiserror::Error;

/// Fix suggestion: a one-line message plus commands copyable whole-line.
///
/// Mirrors uv's `hint:` and mise's prompt format:
/// commands are printed one per line (two-space indent, green highlight) for
/// direct select-copy.
pub struct Hint {
    /// One-line explanation (may contain backtick-marked fragments)
    pub message: String,
    /// Commands copyable whole-line, rendered one per line
    pub commands: Vec<String>,
}

/// uvman domain error.
///
/// `Display` outputs user-facing messages; `hint()` returns an optional fix;
/// `exit_code()` distinguishes usage errors (2) from general errors (1),
/// matching uv.
#[derive(Error, Debug)]
pub enum UError {
    /// Plugin already exists (overwrite with --force)
    #[error("plugin '{name}' already exists")]
    PluginAlreadyExists { name: String },

    /// Plugin not installed; similar holds near-miss installed names (did you
    /// mean)
    #[error("plugin '{name}' is not installed")]
    PluginNotInstalled { name: String, similar: Vec<String> },

    /// Invalid GitHub repository URL
    #[error("'{url}' is not a valid GitHub repository URL")]
    InvalidGitHubUrl { url: String },

    /// Invalid URL
    #[error("'{url}' is not a valid URL")]
    #[allow(dead_code)] // reserved: generic URL validation error
    InvalidUrl { url: String },

    /// Invalid plugin name
    #[error("invalid plugin name '{name}': only letters, digits, '_' and '-' are allowed")]
    InvalidPluginName { name: String },

    /// Path does not exist
    #[error("path '{}' does not exist", path.display())]
    PathNotFound { path: PathBuf },

    /// Path is not a file
    #[error("path '{}' is not a file", path.display())]
    NotAFile { path: PathBuf },

    /// Fallback error
    #[error("{0}")]
    SimpleError(String),

    /// Generic IO error
    #[error("IO error: {source}")]
    IoError {
        #[from]
        source: std::io::Error,
    },

    /// File operation error
    #[error("file operation failed on {}: {source}", path.display())]
    FileError { path: PathBuf, source: std::io::Error },

    /// TOML deserialization error
    #[error("failed to parse {}: {source}", path.display())]
    TomlError { path: PathBuf, source: toml::de::Error },

    /// TOML serialization error
    #[error("failed to serialize {}: {source}", path.display())]
    TomlSerializeError { path: PathBuf, source: toml::ser::Error },

    /// JSON serialize/deserialize error
    #[error("JSON error: {source}")]
    JsonError { source: serde_json::Error },

    /// HTTP request failed
    #[error("failed to reach {url}: {source}")]
    NetworkError { url: String, source: reqwest::Error },

    /// Invalid proxy configuration
    #[error("invalid proxy URL '{url}': {source}")]
    ProxyError { url: String, source: reqwest::Error },

    /// HTTP response status error
    ///
    /// Do not expose internal download URLs (e.g. the full
    /// raw.githubusercontent.com address); it does not help self-diagnosis.
    /// The exact URL is visible in `--verbose`.
    #[error("the remote server returned HTTP {status}")]
    HttpStatusError { url: String, status: u16 },

    /// File checksum error
    #[error("checksum mismatch: {message}")]
    ChecksumError { message: String },

    /// Archive extraction failed (tar/zip/gzip/xz)
    #[error("failed to extract {}: {source}", path.display())]
    ExtractError { path: PathBuf, source: Box<dyn std::error::Error + Send + Sync> },

    /// Source build failed
    #[error("build failed: {message}")]
    #[allow(dead_code)] // reserved: source build error
    BuildError { message: String },

    /// No installation matches the current platform
    #[error("no installation available for {os}-{arch}")]
    PlatformNotSupported { os: String, arch: String },

    /// Requested version not found
    #[error("version {version} not found for {tool}")]
    VersionNotFound { tool: String, version: String },

    /// Target version already installed (blocks reinstall without --force)
    #[error("{tool}@{version} is already installed")]
    AlreadyInstalled { tool: String, version: String },
}

impl UError {
    /// Fix suggestion: a one-line message plus whole-line-copyable commands.
    ///
    /// Only given when the error alone cannot suggest the next step (uv's hint
    /// principle).
    pub fn hint(&self) -> Option<Hint> {
        let hint = match self {
            Self::PluginAlreadyExists { name } => Hint {
                message: "to overwrite the existing plugin, run:".into(),
                commands: vec![format!("uvman plugin install {name} --force")],
            },
            Self::PluginNotInstalled { name, similar } => {
                if let Some(first) = similar.first() {
                    let more = similar.len().saturating_sub(1);
                    Hint {
                        message: if more > 0 {
                            format!("did you mean '{first}'? ({more} more similar name(s) found)")
                        } else {
                            format!("did you mean '{first}'?")
                        },
                        commands: vec![],
                    }
                } else {
                    Hint {
                        message: "to install it, run:".into(),
                        commands: vec![format!("uvman plugin install {name}")],
                    }
                }
            },
            Self::InvalidGitHubUrl { .. } => Hint {
                message: "the repository URL must look like:".into(),
                commands: vec!["https://github.com/<owner>/<repo>".into()],
            },
            Self::InvalidUrl { url } => Hint {
                message: format!("'{url}' is missing a scheme (e.g. `https://`) or is malformed"),
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
            // Any network failure points at the config proxy (GitHub domains need it on some
            // networks)
            Self::NetworkError { .. } | Self::ProxyError { .. } => Hint {
                message: "check your network, or set `[plugin] proxy` / `[network] proxy` \
                          in config/uvman.toml"
                    .into(),
                commands: vec![],
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
                    message: "permission denied; check the file/folder ownership and access rights"
                        .into(),
                    commands: vec![],
                }
            },
            Self::FileError { source, .. }
                if source.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                Hint {
                    message: "permission denied; check the file/folder ownership and access rights"
                        .into(),
                    commands: vec![],
                }
            },
            _ => return None,
        };
        Some(hint)
    }

    /// Process exit code: usage/input errors are 2 (matching uv, clap), others
    /// 1
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::InvalidPluginName { .. }
            | Self::InvalidUrl { .. }
            | Self::InvalidGitHubUrl { .. }
            | Self::PathNotFound { .. }
            | Self::NotAFile { .. } => 2,
            _ => 1,
        }
    }

    /// Whether this error carries an underlying source (decides showing the
    /// --verbose footer)
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
