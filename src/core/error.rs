use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum UError {
    /// 通用IO错误
    #[error("IO 错误: {source}")]
    IoError {
        #[from]
        source: std::io::Error,
    },

    /// 文件操作错误
    #[error("文件操作失败 {path}: {source}")]
    FileError { path: PathBuf, source: std::io::Error },

    /// TOML 反序列化错误
    #[error("TOML 解析失败 {path}: {source}")]
    TomlError { path: PathBuf, source: toml::de::Error },

    /// TOML 序列化错误
    #[error("TOML 序列化失败 {path}: {source}")]
    TomlSerializeError { path: PathBuf, source: toml::ser::Error },

    /// 配置校验错误
    #[error("配置错误 {path}: {message}")]
    ConfigError { path: PathBuf, message: String },

    /// 插件加载/校验错误
    #[error("插件错误 {name}: {message}")]
    PluginError { name: String, message: String },

    /// HTTP 请求失败
    #[error("网络请求失败 {url}: {source}")]
    NetworkError { url: String, source: reqwest::Error },

    /// HTTP 响应状态码错误
    #[error("HTTP {status} {url}")]
    HttpStatusError { url: String, status: u16 },

    /// 文件校验和错误
    #[error("校验和错误: {message}")]
    ChecksumError { message: String },

    /// 压缩包解压失败（tar/zip/gzip/xz）
    #[error("解压失败 {path}: {source}")]
    ExtractError {
        path: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// 源码编译失败
    #[error("编译失败: {message}")]
    BuildError { message: String },

    /// 当前平台无匹配的安装方案
    #[error("当前平台不支持 {os}-{arch}")]
    PlatformNotSupported { os: String, arch: String },

    /// 请求的版本不存在
    #[error("版本未找到 {tool}@{version}")]
    VersionNotFound { tool: String, version: String },
}
