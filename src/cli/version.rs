//! `uvman version`: print uvman's own version, plus a best-effort upgrade hint.

use std::time::Duration;

use indoc::indoc;
use semver::Version as Semver;

use crate::core::http::HTTP_CLIENT;
use crate::core::platform::{ARCH, OS};
use crate::core::VERSION;
use crate::ui::report;
use crate::ui::style;
use crate::Result;

/// GitHub Releases API (uvman is only published to GitHub Releases)
const RELEASES_API_URL: &str = "https://api.github.com/repos/xxxyixuan/uvman/releases/latest";

/// Short independent timeout for the upgrade check: `uvman version` must never
/// be blocked by a slow network (the shared client default is far longer).
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// ASCII banner, printed only on an interactive terminal
const LOGO: &str = indoc! {r#"
   __  __ _    __ __  ___ ___     _   __
  / / / /| |  / //  |/  //   |   / | / /
 / / / / | | / // /|_/ // /| |  /  |/ /
/ /_/ /  | |/ // /  / // ___ | / /|  /
\____/   |___//_/  /_//_/  |_|/_/ |_/
"#};

/// Print the version of uvman
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, visible_alias = "v")]
pub struct Version {
    /// Print the version information in JSON format
    #[clap(short = 'J', long)]
    pub(crate) json: bool,
}

impl Version {
    pub async fn run(&self) -> Result<()> {
        if self.json { self.print_json().await } else { self.print_human().await }
    }

    /// Machine-readable output: the document is always complete and parseable;
    /// `latest` is left empty when the upgrade check fails.
    async fn print_json(&self) -> Result<()> {
        let latest = latest_release().await.map(|version| version.to_string()).unwrap_or_default();
        let json = serde_json::json!({
            "version": VERSION.to_string(),
            "latest": latest,
            "os": OS.as_str(),
            "arch": ARCH.as_str(),
        });
        println!("{}", serde_json::to_string_pretty(&json)?);
        Ok(())
    }

    /// Human output: banner + version line, then an upgrade hint. The network
    /// check runs only on an interactive terminal (piped/scripted calls stay
    /// instant), and `--quiet` suppresses the hint as a non-essential message.
    async fn print_human(&self) -> Result<()> {
        let interactive = console::user_attended();
        if interactive {
            println!("{}", style::ocyan(LOGO));
        }
        println!(
            "{version}      {os}-{arch}",
            version = *VERSION,
            os = OS.as_str(),
            arch = ARCH.as_str(),
        );
        if interactive && !report::quiet() {
            print_upgrade_hint().await;
        }
        Ok(())
    }
}

/// Print a hint when a newer GitHub release exists; all failures are
/// suppressed (best-effort by design — never break `uvman version`)
async fn print_upgrade_hint() {
    let Some(latest) = latest_release().await else {
        return;
    };
    let Some(current) = parse_version(&VERSION.to_string()) else {
        return;
    };
    if latest > current {
        println!(
            "{} A new release is available: {} (current: {current})",
            style::oyellow("!"),
            latest
        );
    }
}

/// Best-effort fetch of the latest release tag, parsed as a semver version.
///
/// Every failure mode — timeout, network error, unexpected JSON, unparsable
/// tag — collapses to `None` so callers never handle errors.
async fn latest_release() -> Option<Semver> {
    let text = tokio::time::timeout(
        CHECK_TIMEOUT,
        // no retries: the check is best-effort and must stay fast
        HTTP_CLIENT.fetch_text(RELEASES_API_URL, 0, 0),
    )
    .await
    .ok()? // timed out
    .ok()?; // network / HTTP failure
    let release: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tag = release.get("tag_name")?.as_str()?;
    parse_version(tag)
}

/// Parse a release tag or version string, ignoring an optional `v` prefix
fn parse_version(tag: &str) -> Option<Semver> {
    Semver::parse(tag.trim_start_matches(['v', 'V'])).ok()
}
