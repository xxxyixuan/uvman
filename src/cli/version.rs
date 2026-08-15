use crate::Result;
use crate::core::VERSION;
use crate::core::platform::{ARCH, OS};
use crate::ui::style;
use indoc::indoc;

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
        let json = serde_json::json!({
            "version": version,
            "latest": "",
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
    println!(
        "{}",
        format!(
            "{version}      {os}-{arch}",
            version = VERSION.to_string(),
            os = OS.as_str(),
            arch = ARCH.as_str(),
        )
    );
    Ok(())
}

async fn show_latest() {
    // todo: implement latest version check
}
