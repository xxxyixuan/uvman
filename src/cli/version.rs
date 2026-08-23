use indoc::indoc;
use semver::Version as Semver;
use std::time::Duration;

use crate::Result;
use crate::core::VERSION;
use crate::core::error::UError;
use crate::core::platform::{ARCH, OS};
use crate::ui::report;
use crate::ui::style;

/// GitHub Releases API (uvman is only published to GitHub Releases, not crates.io)
const RELEASES_API_URL: &str =
    "https://api.github.com/repos/xxxyixuan/uvman/releases/latest";

/// Display the version of uvman
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, visible_alias = "v")]
pub struct Version {
    /// Print the version information in JSON format
    #[clap(short = 'J', long)]
    pub(crate) json: bool,
}

impl Version {
    pub async fn run(&self) -> Result<()> {
        if self.json {
            self.print_json().await?;
        } else {
            self.print_normal().await?;
        }
        Ok(())
    }

    async fn print_json(&self) -> Result<()> {
        let version = VERSION.to_string();
        // Best-effort latest check; blank on failure so it never blocks JSON out
        let latest = fetch_latest_tag()
            .await
            .unwrap_or_else(|_| String::new())
            .trim_start_matches(['v', 'V'])
            .to_string();
        let json = serde_json::json!({
            "version": version,
            "latest": latest,
            "os": OS.as_str(),
            "arch": ARCH.as_str(),
        });
        println!("{}", serde_json::to_string_pretty(&json)?);
        Ok(())
    }

    async fn print_normal(&self) -> Result<()> {
        show_version()?;
        show_latest().await;
        Ok(())
    }
}

fn show_version() -> Result<()> {
    if console::user_attended() {
        let logo: &str = indoc! {r#"
           __  __ _    __ __  ___ ___     _   __
          / / / /| |  / //  |/  //   |   / | / /
         / / / / | | / // /|_/ // /| |  /  |/ /
        / /_/ /  | |/ // /  / // ___ | / /|  /
        \____/   |___//_/  /_//_/  |_|/_/ |_/
        "#};
        println!("{}", style::ocyan(logo));
    }
    let version = VERSION.to_string();
    println!(
        "{version}      {os}-{arch}",
        os = OS.as_str(),
        arch = ARCH.as_str(),
    );
    Ok(())
}

/// Best-effort check for a newer GitHub release; failures are suppressed so
/// they never break `uvman version`. When not quiet, prints an upgrade hint
/// only on an interactive terminal.
async fn show_latest() {
    if report::quiet() || !console::user_attended() {
        return;
    }
    let Ok(tag) = fetch_latest_tag().await else {
        return;
    };
    // strip the leading `v` from the remote tag before comparing
    let Ok(latest) = Semver::parse(tag.trim_start_matches(['v', 'V'])) else {
        return;
    };
    let Ok(current) = Semver::parse(&VERSION.to_string()) else {
        return;
    };
    if latest <= current {
        return;
    }
    println!(
        "{}  A new release is available: {} (current: {current})",
        style::oyellow("!".to_string()),
        latest
    );
}

/// Fetch the tag of the latest GitHub release. Uses a short independent
/// timeout (3s); any failure (including timeout) returns Err and is
/// suppressed by callers so it never blocks version output.
async fn fetch_latest_tag() -> Result<String, UError> {
    const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
    match tokio::time::timeout(CHECK_TIMEOUT, fetch_latest_tag_inner()).await {
        Ok(result) => result,
        Err(_) => Err(UError::SimpleError(
            "latest version check timed out".into(),
        )),
    }
}

async fn fetch_latest_tag_inner() -> Result<String, UError> {
    let response = crate::core::http::HTTP_CLIENT.get(RELEASES_API_URL).await?;
    let text = response.text().await.map_err(|source| {
        UError::NetworkError { url: RELEASES_API_URL.to_string(), source }
    })?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|source| UError::JsonError { source })?;
    json.get("tag_name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| UError::SimpleError("missing tag_name in release response".into()))
}
