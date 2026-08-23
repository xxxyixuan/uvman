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

/// Default timeout (s) when `[network] timeout` is not set
const DEFAULT_TIMEOUT_SECS: u64 = 30;

pub static HTTP_CLIENT: Lazy<HttpClient> = Lazy::new(|| {
    // Timeout follows global config ([network] timeout), defaulting to 30s
    let timeout = crate::core::config::GLOBAL_CONFIG
        .network
        .timeout
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    HttpClient::new(timeout).expect("Failed to start HTTP Client")
});

#[derive(Debug, Clone)]
pub struct HttpClient {
    client: Client,
}

impl HttpClient {
    pub fn new(timeout: u64) -> Result<Self, UError> {
        Self::with_proxy(timeout, None)
    }

    /// Build an HTTP client; an explicit proxy wins, else falls back to the global config proxy
    pub fn with_proxy(
        timeout: u64, proxy: Option<&str>,
    ) -> Result<Self, UError> {
        let user_agent = "uvman/1.0";
        let mut builder = Client::builder()
            .timeout(Duration::from_secs(timeout))
            .user_agent(user_agent);

        // Explicit proxy wins, else fall back to the global config proxy
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

    #[allow(dead_code)] // reserved: infer filename from URL and download into a directory
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

    /// Download to a target path (with retries and atomic write)
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
        let temp_path = tmp_sibling(dest_path);
        let result = self
            .try_download_to_temp(url, dest_path, &temp_path)
            .await;
        // Clean up the partial .tmp on failure to avoid leftover garbage in cache
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temp_path).await;
        }
        result
    }

    async fn try_download_to_temp(
        &self, url: &str, dest_path: &Path, temp_path: &Path,
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

        // Progress bar when Content-Length is known, spinner otherwise; hidden in quiet mode
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

        let mut file = File::create(temp_path).await.map_err(|e| {
            UError::FileError { path: temp_path.to_path_buf(), source: e }
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
                path: temp_path.to_path_buf(),
                source: e,
            })?;
        }
        if let Some(pb) = &progress {
            // Keep the full bar visible (not cleared) so it flows into the install/done message
            pb.finish();
        }

        file.sync_all().await.map_err(|e| UError::FileError {
            path: temp_path.to_path_buf(),
            source: e,
        })?;
        drop(file);

        tokio::fs::rename(temp_path, dest_path).await.map_err(|e| {
            UError::FileError { path: temp_path.to_path_buf(), source: e }
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

    /// Fetch URL content as text (with retries) for small payloads (plugin TOML, templates)
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

/// Read the proxy address from global config (plugin.proxy first, then network.proxy)
fn config_proxy() -> Option<&'static str> {
    let config = &crate::core::config::GLOBAL_CONFIG;
    config.plugin.proxy.as_deref().or_else(|| config.network.proxy.as_deref())
}

/// Temp path for a half-downloaded file (same dir as target, prefixed .tmp_ + pid)
fn tmp_sibling(dest_path: &Path) -> PathBuf {
    let temp_dir = dest_path.parent().unwrap_or(Path::new("."));
    temp_dir.join(format!(".tmp_{}", std::process::id()))
}

/// Build download progress: a progress bar when total is known, else a spinner
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

    /// Test client does not inherit the global proxy so localhost mock requests stay local
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
            .expect(3) // expect at most 3 hits
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
