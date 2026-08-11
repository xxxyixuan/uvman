use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{Client, Response};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

use crate::core::error::UError;
use crate::{Lazy, core};

pub static HTTP_CLIENT: Lazy<HttpClient> =
    Lazy::new(|| HttpClient::new(30).expect("Failed to start HTTP Client"));

#[derive(Debug, Clone)]
pub struct HttpClient {
    client: Client,
}

impl HttpClient {
    pub fn new(timeout: u64) -> Result<Self, UError> {
        Self::with_proxy(timeout, None)
    }

    /// 创建 HTTP 客户端；显式指定的 proxy 优先，否则回退到全局配置中的代理
    pub fn with_proxy(
        timeout: u64, proxy: Option<&str>,
    ) -> Result<Self, UError> {
        let user_agent = "uvman/1.0";
        let mut builder = Client::builder()
            .timeout(Duration::from_secs(timeout))
            .user_agent(user_agent);

        // 显式指定的 proxy 优先，否则回退到全局配置中的代理
        let proxy_url: Option<String> = match proxy {
            Some(p) => Some(p.to_string()),
            None => config_proxy().map(str::to_string),
        };
        if let Some(p) = proxy_url {
            let proxy = reqwest::Proxy::all(&p).map_err(|source| {
                UError::ProxyError { url: p.clone(), source }
            })?;
            builder = builder.proxy(proxy);
        }

        let client = builder.build().map_err(|e| {
            UError::SimpleError(format!("failed to build HTTP client: {e}"))
        })?;
        Ok(Self { client })
    }

    pub async fn download(
        &self, url: &str, dest_dir: &str, retries: u64, retry_delay: u64,
    ) -> Result<PathBuf, UError> {
        let filename = url
            .split("/")
            .last()
            .filter(|s| !s.is_empty())
            .unwrap_or("download")
            .to_string();

        let dest_path = Path::new(dest_dir).join(&filename);
        self.download_to(url, &dest_path, retries, retry_delay).await
    }

    /// 下载到指定目标路径（带重试与原子写入）
    pub async fn download_to(
        &self, url: &str, dest_path: &Path, retries: u64, retry_delay: u64,
    ) -> Result<PathBuf, UError> {
        if let Some(dir) = dest_path.parent() {
            core::file::ensure_dir(dir)?;
        }

        // retry loop
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.try_download_once(url, dest_path).await {
                Ok(path) => return Ok(path),
                Err(e) => {
                    if attempt > retries {
                        return Err(e);
                    }
                    sleep(Duration::from_secs(retry_delay)).await;
                },
            }
        }
    }

    async fn try_download_once(
        &self, url: &str, dest_path: &Path,
    ) -> Result<PathBuf, UError> {
        let response = self.client.get(url).send().await.map_err(|e| {
            UError::NetworkError { url: url.to_string(), source: e }
        })?;

        let status = response.status();
        if !status.is_success() {
            return Err(UError::HttpStatusError {
                url: url.to_string(),
                status: status.as_u16(),
            });
        }

        let temp_dir = dest_path.parent().unwrap_or(Path::new("."));
        let temp_filename = format!(".tmp_{}", std::process::id());
        let temp_path = temp_dir.join(temp_filename);

        // 下载进度条：已知 Content-Length 用进度条，未知用 spinner；
        // quiet 模式下不显示
        let progress = if crate::ui::report::quiet() {
            None
        } else {
            Some(new_progress(
                dest_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("download"),
                response.content_length(),
            ))
        };

        let mut file = File::create(&temp_path).await.map_err(|e| {
            UError::FileError { path: temp_path.clone(), source: e }
        })?;

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| UError::NetworkError {
                url: url.to_string(),
                source: e,
            })?;
            if let Some(pb) = &progress {
                pb.inc(chunk.len() as u64);
            }
            file.write_all(&chunk).await.map_err(|e| UError::FileError {
                path: temp_path.clone(),
                source: e,
            })?;
        }
        if let Some(pb) = &progress {
            pb.finish_and_clear();
        }

        file.sync_all().await.map_err(|e| UError::FileError {
            path: temp_path.clone(),
            source: e,
        })?;
        drop(file);

        tokio::fs::rename(&temp_path, dest_path).await.map_err(|e| {
            UError::FileError { path: temp_path.clone(), source: e }
        })?;

        Ok(dest_path.to_path_buf())
    }

    pub async fn get(&self, url: &str) -> Result<Response, UError> {
        let response = self.client.get(url).send().await.map_err(|e| {
            UError::NetworkError { url: url.to_string(), source: e }
        })?;

        let status = response.status();
        if !status.is_success() {
            return Err(UError::HttpStatusError {
                url: url.to_string(),
                status: status.as_u16(),
            });
        }
        Ok(response)
    }

    /// 以文本形式获取 URL 内容（带重试），适用于小体积文本（插件 TOML、模板等）
    pub async fn fetch_text(
        &self, url: &str, retries: u64, retry_delay: u64,
    ) -> Result<String, UError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.try_fetch_text_once(url).await {
                Ok(text) => return Ok(text),
                Err(e) => {
                    if attempt > retries {
                        return Err(e);
                    }
                    sleep(Duration::from_secs(retry_delay)).await;
                },
            }
        }
    }

    async fn try_fetch_text_once(&self, url: &str) -> Result<String, UError> {
        let response = self.get(url).await?;
        response.text().await.map_err(|source| UError::NetworkError {
            url: url.to_string(),
            source,
        })
    }
}

/// 读取全局配置中的代理地址（plugin.proxy 优先，其次 network.proxy）
fn config_proxy() -> Option<&'static str> {
    let config = &crate::core::config::GLOBAL_CONFIG;
    config.plugin.proxy.as_deref().or_else(|| config.network.proxy.as_deref())
}

/// 构造下载进度指示：已知总大小时用进度条，否则用 spinner
fn new_progress(filename: &str, total: Option<u64>) -> ProgressBar {
    match total {
        Some(len) => {
            let pb = ProgressBar::new(len);
            pb.set_style(
                ProgressStyle::with_template(
                    "{spinner:.green} {msg} [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({eta})",
                )
                .expect("valid progress template")
                .progress_chars("#>-"),
            );
            pb.set_message(filename.to_string());
            pb
        },
        None => {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::with_template("{spinner:.green} {msg} {bytes}")
                    .expect("valid progress template"),
            );
            pb.set_message(format!("downloading {filename}"));
            pb
        },
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// 测试用客户端不继承全局配置中的代理，避免 localhost mock 请求被代理转发
    fn create_client() -> HttpClient {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("uvman/1.0")
            .build()
            .unwrap();
        HttpClient { client }
    }

    #[tokio::test]
    async fn test_download_success() {
        let mock_server = MockServer::start().await;
        let url = format!("{}/testfile.txt", mock_server.uri());

        let content = b"hello, world!";
        Mock::given(method("GET"))
            .and(path("/testfile.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content))
            .mount(&mock_server)
            .await;

        let temp_dir = tempdir().unwrap();
        let dest_dir = temp_dir.path().to_str().unwrap();

        let client = create_client();
        let result_path = client.download(&url, dest_dir, 2, 0).await.unwrap();

        let actual = tokio::fs::read(&result_path).await.unwrap();
        assert_eq!(actual, content);
        assert_eq!(result_path.file_name().unwrap(), "testfile.txt");
    }

    #[tokio::test]
    async fn test_download_http_error() {
        let mock_server = MockServer::start().await;
        let url = format!("{}/missing", mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let temp_dir = tempdir().unwrap();
        let dest_dir = temp_dir.path().to_str().unwrap();

        let client = create_client();
        let err = client.download(&url, dest_dir, 2, 0).await.unwrap_err();

        match err {
            UError::HttpStatusError { url, status } => {
                assert_eq!(status, 404);
                assert!(url.contains("/missing"));
            },
            _ => panic!("Expected HttpStatusError, got {:?}", err),
        }
    }

    #[tokio::test]
    async fn test_download_retry_failure() {
        let mock_server = MockServer::start().await;
        let url = format!("{}/alwaysfail", mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/alwaysfail"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3) // 最多匹配 3 次
            .mount(&mock_server)
            .await;

        let temp_dir = tempdir().unwrap();
        let dest_dir = temp_dir.path().to_str().unwrap();

        let client = create_client();
        let err = client.download(&url, dest_dir, 2, 0).await.unwrap_err();

        match err {
            UError::HttpStatusError { status, .. } => assert_eq!(status, 500),
            _ => panic!("Expected HttpStatusError, got {:?}", err),
        }
    }
}
