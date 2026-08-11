use crate::core;
use crate::core::error::UError;
use futures_util::StreamExt;
use reqwest::Client;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

#[derive(Debug, Clone)]
pub struct HttpClient {
    client: Client,
}

impl HttpClient {
    pub fn new(timeout: usize) -> Result<Self, UError> {
        let user_agent = "uvman/1.0";
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout as u64))
            .user_agent(user_agent)
            .build()
            .map_err(|e| UError::NetworkError {
                url: "builder".to_string(),
                source: e,
            })?;
        Ok(Self { client })
    }

    pub fn default() -> Result<Self, UError> {
        Self::new(30)
    }

    /// Download the file to the specified directory and return the downloaded file path.
    ///
    /// # Arguments
    /// * `url` - The URL of the file to download
    /// * `dest_dir` - The directory to download the file to
    /// * `retries` - The number of times to retry the download
    /// * `retry_delay` - The delay between retries in seconds
    /// # Returns
    /// * `Ok(PathBuf)` - The path of the downloaded file
    /// * `Err(UError)` - An error if the download fails
    pub async fn download(
        &self, url: &str, dest_dir: &str, retries: usize, retry_delay: usize,
    ) -> Result<PathBuf, UError> {
        let filename = url
            .split("/")
            .last()
            .and_then(|s| if s.is_empty() { None } else { Some(s) })
            .unwrap_or("download")
            .to_string();

        let dest_path = Path::new(dest_dir).join(&filename);
        let dest_dir_path = Path::new(dest_dir);

        core::file::ensure_dir(dest_dir_path)?;

        // retry loop
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.try_download_once(url, &dest_path).await {
                Ok(path) => return Ok(path),
                Err(e) => {
                    if attempt > retries {
                        return Err(e);
                    }
                    sleep(Duration::from_secs(retry_delay as u64)).await;
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

        let mut file = File::create(&temp_path).await.map_err(|e| {
            UError::FileError { path: temp_path.clone(), source: e }
        })?;

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| UError::NetworkError {
                url: url.to_string(),
                source: e,
            })?;
            file.write_all(&chunk).await.map_err(|e| UError::FileError {
                path: temp_path.clone(),
                source: e,
            })?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn create_client() -> HttpClient {
        HttpClient::default().unwrap()
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

        let client = HttpClient::default().unwrap();
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
        let err = client.download(&url, dest_dir, 1, 0).await.unwrap_err();

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
