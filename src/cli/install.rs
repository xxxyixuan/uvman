use std::fs;

use crate::Result;
use crate::core::error::UError;
use crate::ui::style;

/// Install a tool at a specific version
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, visible_alias = "i")]
pub struct Install {
    /// Tool and version to install, in the form `tool@version`
    ///
    /// Omit the version to install the plugin's default version.
    /// e.g.: `node@20.11.0`, `node`, `node@22`, `node@latest`
    pub tool_spec: String,

    /// Reinstall even if the version is already present
    #[clap(long, short = 'f')]
    pub force: bool,
}

impl Install {
    pub async fn run(&self) -> Result<()> {
        let (tool, version) = parse_spec(&self.tool_spec)?;

        let plan = crate::toolset::plan(&tool, version.as_deref()).await?;

        // Block reinstall when already installed and no --force
        if plan.install_dir.exists() && !self.force {
            return Err(UError::AlreadyInstalled {
                tool: tool.clone(),
                version: plan.version.clone(),
            }
            .into());
        }

        println!(
            "{}",
            style::ogreen(format!(
                "Installing {}@{} ...",
                plan.name, plan.version
            ))
        );

        // --force: rename the old dir to a backup rather than deleting it inline, so
        // a failed install can roll back instead of leaving nothing behind
        let backup = if plan.install_dir.exists() {
            let mut backup_name = plan.install_dir.clone().into_os_string();
            backup_name.push(".uvman_bak");
            let backup = std::path::PathBuf::from(backup_name);
            // Clear any stale backup left by a previous failed run
            let _ = fs::remove_dir_all(&backup);
            fs::rename(&plan.install_dir, &backup)?;
            Some(backup)
        } else {
            None
        };

        match crate::toolset::execute(&plan).await {
            Ok(()) => {
                if let Some(backup) = &backup {
                    let _ = fs::remove_dir_all(backup);
                }
                println!(
                    "{}",
                    style::ogreen(format!(
                        "Installed {}@{} to {}",
                        plan.name,
                        plan.version,
                        plan.install_dir.display()
                    ))
                );
                Ok(())
            },
            Err(e) => {
                // Clean up the half-installed dir, restoring the backup on failure
                let _ = fs::remove_dir_all(&plan.install_dir);
                if let Some(backup) = &backup
                    && fs::rename(backup, &plan.install_dir).is_err()
                {
                    crate::ui::report::print_warning(&format!(
                        "failed to restore previous installation from {}; \
                         recover it manually",
                        backup.display()
                    ));
                }
                Err(e.into())
            },
        }
    }
}

/// Parse a `tool@version` spec into `(tool, version)`.
///
/// - Omitted version (`node`) returns `None`, resolved from the plugin default
/// - A version may be a full version (`20.11.0`), a partial version (`22`), or
///   an alias (`latest`/`lts`/`nightly`); aliases are resolved to a concrete
///   version later in `toolset::plan` (needs the plugin release data)
pub(crate) fn parse_spec(spec: &str) -> Result<(String, Option<String>), UError> {
    let (tool, version) = match spec.split_once('@') {
        Some((t, v)) => (t, v),
        None => (spec, ""),
    };
    if tool.is_empty() {
        return Err(UError::SimpleError(format!(
            "invalid tool spec '{spec}': missing tool name"
        )));
    }
    let version =
        if version.is_empty() { None } else { Some(version.to_string()) };
    Ok((tool.to_string(), version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_spec_no_version() {
        let (tool, version) = parse_spec("node").unwrap();
        assert_eq!(tool, "node");
        assert_eq!(version, None);
    }

    #[test]
    fn test_parse_spec_full_version() {
        let (tool, version) = parse_spec("node@20.11.0").unwrap();
        assert_eq!(tool, "node");
        assert_eq!(version.as_deref(), Some("20.11.0"));
    }

    #[test]
    fn test_parse_spec_alias() {
        let (_, version) = parse_spec("node@latest").unwrap();
        assert_eq!(version.as_deref(), Some("latest"));
        let (_, version) = parse_spec("node@lts").unwrap();
        assert_eq!(version.as_deref(), Some("lts"));
        let (_, version) = parse_spec("node@nightly").unwrap();
        assert_eq!(version.as_deref(), Some("nightly"));
    }

    #[test]
    fn test_parse_spec_empty_tool() {
        assert!(parse_spec("@20.0.0").is_err());
        assert!(parse_spec("").is_err());
    }
}
