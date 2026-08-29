//! Self-update version checking: locate the latest GitHub release and build
//! the download URL for the current platform's release asset.

use semver::Version as Semver;

use crate::core::error::UError;
use crate::core::http::HTTP_CLIENT;

/// GitHub repo uvman is published to (GitHub Releases only, no crates.io)
pub const REPO: &str = "xxxyixuan/uvman";

/// Latest stable release (excludes pre-releases by GitHub's semantics)
const RELEASES_LATEST_API_URL: &str = "https://api.github.com/repos/xxxyixuan/uvman/releases/latest";

/// Release list (needed for --prerelease, where /latest is not enough)
const RELEASES_LIST_API_URL: &str = "https://api.github.com/repos/xxxyixuan/uvman/releases?per_page=100";

/// Retries for the self-update check: unlike the best-effort hint in
/// `uvman version`, self-update reports fetch failures to the user
const FETCH_RETRIES: u64 = 1;
const RETRY_DELAY_SECS: u64 = 1;

/// A resolved remote release: the parsed version plus the raw tag (the tag is
/// what release asset names are built from).
#[derive(Debug, Clone)]
pub struct LatestRelease {
    pub version: Semver,
    pub tag: String,
}

/// Fetch the newest release, optionally including pre-releases.
pub async fn fetch_latest_release(prerelease: bool) -> Result<LatestRelease, UError> {
    if prerelease {
        fetch_latest_including_prereleases().await
    } else {
        fetch_latest_stable().await
    }
}

async fn fetch_latest_stable() -> Result<LatestRelease, UError> {
    let text = HTTP_CLIENT.fetch_text(RELEASES_LATEST_API_URL, FETCH_RETRIES, RETRY_DELAY_SECS).await?;
    let release: serde_json::Value =
        serde_json::from_str(&text).map_err(|source| UError::JsonError { source })?;
    let tag = release
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| UError::SimpleError("release payload has no 'tag_name'".into()))?
        .to_string();
    let version = parse_version(&tag)
        .ok_or_else(|| UError::SimpleError(format!("cannot parse release tag '{tag}'")))?;
    Ok(LatestRelease { version, tag })
}

/// With --prerelease the /latest endpoint is not enough (it never returns
/// pre-releases), so list all releases and pick the highest semver.
async fn fetch_latest_including_prereleases() -> Result<LatestRelease, UError> {
    let text = HTTP_CLIENT.fetch_text(RELEASES_LIST_API_URL, FETCH_RETRIES, RETRY_DELAY_SECS).await?;
    let releases: Vec<serde_json::Value> =
        serde_json::from_str(&text).map_err(|source| UError::JsonError { source })?;
    releases
        .iter()
        // drafts are unpublished; treat a missing field as draft to be safe
        .filter(|r| !r.get("draft").and_then(|d| d.as_bool()).unwrap_or(true))
        .filter_map(|r| {
            let tag = r.get("tag_name")?.as_str()?.to_string();
            let version = parse_version(&tag)?;
            Some(LatestRelease { version, tag })
        })
        .max_by(|a, b| a.version.cmp(&b.version))
        .ok_or_else(|| UError::SimpleError("no published releases found".into()))
}

/// Parse a release tag or version string, ignoring an optional `v` prefix
pub fn parse_version(tag: &str) -> Option<Semver> {
    Semver::parse(tag.trim_start_matches(['v', 'V'])).ok()
}

/// Release asset name for this platform, matching the naming scheme produced
/// by the release workflow (`uvman-<tag>-<target>.<ext>`).
pub fn asset_filename(tag: &str) -> String {
    // cfg! is evaluated for the target platform, so cross-compiled release
    // builds still pick the right archive format
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    format!("uvman-{tag}-{}.{ext}", env!("UVMAN_TARGET"))
}

/// Direct download URL for the release asset of this platform
pub fn asset_url(tag: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/{}", asset_filename(tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_strips_v_prefix() {
        assert_eq!(parse_version("v1.2.3").unwrap().to_string(), "1.2.3");
        assert_eq!(parse_version("1.2.3").unwrap().to_string(), "1.2.3");
        assert_eq!(parse_version("v1.0.0-rc.1").unwrap().to_string(), "1.0.0-rc.1");
        assert!(parse_version("not-a-version").is_none());
    }

    #[test]
    fn prerelease_orders_after_stable() {
        // semver: 1.0.0-rc.2 > 1.0.0-rc.1, and 1.0.0 > 1.0.0-rc.2
        let rc1 = parse_version("v1.0.0-rc.1").unwrap();
        let rc2 = parse_version("v1.0.0-rc.2").unwrap();
        let stable = parse_version("v1.0.0").unwrap();
        assert!(rc2 > rc1);
        assert!(stable > rc2);
    }

    #[test]
    fn asset_filename_matches_workflow_scheme() {
        let name = asset_filename("v0.2.0");
        // both extensions keep the shared prefix; only the suffix differs
        let prefix = format!("uvman-v0.2.0-{}", env!("UVMAN_TARGET"));
        assert!(name == format!("{prefix}.zip") || name == format!("{prefix}.tar.gz"));
    }

    #[test]
    fn asset_url_contains_tag_and_filename() {
        let url = asset_url("v0.2.0");
        assert!(url.starts_with("https://github.com/xxxyixuan/uvman/releases/download/v0.2.0/"));
        assert!(url.ends_with(&asset_filename("v0.2.0")));
    }
}
