//! `uvman self-update`: upgrade the uvman binary itself from GitHub Releases.
//!
//! Flow: check latest release → (optional confirm) → download asset + sha256
//! sidecar → verify checksum → extract → replace the running executable.

use std::io::Write;

use clap::Args;

use crate::Result;
use crate::core::VERSION;
use crate::core::http::HTTP_CLIENT;
use crate::core::install::{self, ReplaceOutcome};
use crate::core::upgrade;
use crate::ui::report;
use crate::ui::style;

/// Download retries for the asset and its checksum file
const DOWNLOAD_RETRIES: u64 = 1;
const RETRY_DELAY_SECS: u64 = 1;

/// `uvman self-update`
#[derive(Debug, Args)]
pub struct SelfUpdate {
    /// Only check for a newer release; never download or install
    #[clap(long)]
    pub(crate) check: bool,

    /// Skip the confirmation prompt
    #[clap(short = 'y', long)]
    pub(crate) yes: bool,

    /// Consider pre-release versions as upgrade targets
    #[clap(long)]
    pub(crate) prerelease: bool,

    /// Print the check result in JSON format (implies --check)
    #[clap(short = 'J', long)]
    pub(crate) json: bool,
}

impl SelfUpdate {
    pub async fn run(&self) -> Result<()> {
        // The check itself is always needed; a fetch failure is fatal for
        // --check/--json but only a warning for the default flow
        let latest = match upgrade::fetch_latest_release(self.prerelease).await {
            Ok(latest) => latest,
            Err(e) => {
                if self.check || self.json {
                    return Err(e.into());
                }
                report::print_warning(&format!("failed to check for updates: {e}"));
                return Ok(());
            },
        };

        let current = upgrade::parse_version(&VERSION.to_string()).ok_or_else(|| {
            crate::core::error::UError::SimpleError(format!(
                "cannot parse the running version '{}'",
                *VERSION
            ))
        })?;

        let up_to_date = latest.version <= current;
        if self.json {
            self.print_json(&current, &latest, up_to_date);
            return Ok(());
        }

        if up_to_date {
            if !report::quiet() {
                println!("{} uvman is up to date ({})", style::ogreen("✔"), latest.tag);
            }
            return Ok(());
        }

        if self.check {
            println!(
                "{} update available: {} (current: {current})",
                style::oyellow("!"),
                latest.tag
            );
            report::print_hint("install the update", &["uvman self-update".into()]);
            return Ok(());
        }

        if !self.yes
            && console::user_attended()
            && !confirm(&format!("update uvman {current} → {}?", latest.version))
        {
            println!("aborted");
            return Ok(());
        }

        self.install(&latest, &current).await
    }

    /// Full upgrade: download → verify → extract → replace
    async fn install(
        &self, latest: &upgrade::LatestRelease, current: &semver::Version,
    ) -> Result<()> {
        let url = upgrade::asset_url(&latest.tag);
        let filename = upgrade::asset_filename(&latest.tag);

        // Download the archive and its checksum sidecar into an isolated temp
        // dir (never the shared cache: this is uvman itself, not a tool)
        let download_dir = tempfile::tempdir()?;
        let archive = download_dir.path().join(&filename);
        HTTP_CLIENT.download_to(&url, &archive, DOWNLOAD_RETRIES, RETRY_DELAY_SECS).await?;

        let sha_text = HTTP_CLIENT
            .fetch_text(&format!("{url}.sha256"), DOWNLOAD_RETRIES, RETRY_DELAY_SECS)
            .await?;
        let expected = install::parse_sha256(&sha_text).ok_or_else(|| {
            crate::core::error::UError::ChecksumError {
                message: format!("no valid sha256 digest found for '{filename}'"),
            }
        })?;
        install::verify_sha256(&archive, &expected)?;

        // Release archive layouts differ: tar.gz wraps the binary in a
        // top-level dir, the Windows zip stores it at the root — extract
        // unstripped and locate the binary by name instead.
        let extract_dir = tempfile::tempdir()?;
        let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
        crate::toolset::extract_archive(&archive, extract_dir.path(), ext, 0)?;
        let bin_name = format!("uvman{}", std::env::consts::EXE_SUFFIX);
        let new_bin = install::find_binary(extract_dir.path(), &bin_name).ok_or_else(|| {
            crate::core::error::UError::SimpleError(format!(
                "binary '{bin_name}' not found in the downloaded archive"
            ))
        })?;

        let target = install::install_target()?;
        match install::replace_executable(&target, &new_bin)? {
            ReplaceOutcome::Replaced => {},
            ReplaceOutcome::OldPending(old) => report::print_warning(&format!(
                "the old binary is still in use; '{}' will be removed on the next launch",
                old.display()
            )),
        }

        println!("{} uvman updated to {} (was {current})", style::ogreen("✔"), latest.tag,);
        report::print_hint("restart your terminal, then check the new version", &["uvman version".into()]);
        Ok(())
    }

    fn print_json(
        &self, current: &semver::Version, latest: &upgrade::LatestRelease, up_to_date: bool,
    ) {
        let json = serde_json::json!({
            "current": current.to_string(),
            "latest": latest.version.to_string(),
            "tag": latest.tag,
            "update_available": !up_to_date,
        });
        println!("{}", serde_json::to_string_pretty(&json).expect("serializable"));
    }
}

/// Interactive y/N confirmation on stdout (caller checks tty + --yes first)
fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}
