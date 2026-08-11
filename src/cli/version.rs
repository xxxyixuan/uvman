use crate::Lazy;
use crate::Result;
use crate::ui::style;
use indoc::indoc;
use std::convert::Into;
use std::env;
use versions::Versioning;

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
        let json = serde_json::json!({
            "version": VERSION.as_str(),
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
    let version = VERSION.as_str();
    println!("{}", version);
    Ok(())
}

async fn show_latest() {
    // todo: implement latest version check
    
}

pub static OS: Lazy<String> = Lazy::new(|| env::consts::OS.into());
pub static ARCH: Lazy<String> = Lazy::new(|| {
    match env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => env::consts::ARCH,
    }
    .to_string()
});

pub(crate) static V: Lazy<Versioning> =
    Lazy::new(|| Versioning::new(env!("CARGO_PKG_VERSION")).unwrap());

pub static VERSION: Lazy<String> = Lazy::new(|| {
    let mut v = V.to_string();
    if cfg!(debug_assertions) {
        v.push_str("-DEBUG");
    }
    format!(
        "{version}      {os}-{arch}",
        version = v,
        os = OS.as_str(),
        arch = ARCH.as_str(),
    )
});
