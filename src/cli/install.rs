//! `uvman install <tool@version>`: resolve the spec into an install plan and
//! execute it (download → verify → extract → deploy, in toolset), with a
//! rollback-safe replacement of an already-installed version.

use std::fs;
use std::path::{Path, PathBuf};

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

        // Reinstalling over an existing version needs --force
        if plan.install_dir.exists() && !self.force {
            return Err(UError::AlreadyInstalled {
                tool: plan.name.clone(),
                version: plan.version.clone(),
            }
            .into());
        }

        println!("{}", style::ogreen(format!("Installing {}@{} ...", plan.name, plan.version)));

        let replace = ReplaceGuard::set_aside(&plan.install_dir)?;

        if let Err(err) = crate::toolset::execute(&plan).await {
            if let Err(aside) = replace.rollback() {
                crate::ui::report::print_warning(&format!(
                    "failed to restore previous installation from {}; recover it manually",
                    aside.display()
                ));
            }
            return Err(err.into());
        }
        replace.commit();

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
    }
}

/// Rollback-safe replacement of an already-installed version directory.
///
/// Deleting the old install up front would leave the user with nothing when
/// the new install fails midway, so the old dir is renamed aside as
/// `<dir>.uvman_bak` instead. [`ReplaceGuard::commit`] drops the aside copy
/// once the new install landed; [`ReplaceGuard::rollback`] puts it back after
/// a failure, removing the half-written dir first.
struct ReplaceGuard {
    /// Install dir the new version is deployed into
    target: PathBuf,
    /// Where the previous installation was renamed aside; `None` on a fresh
    /// install with nothing to preserve
    aside: Option<PathBuf>,
}

impl ReplaceGuard {
    /// Rename an existing installation at `target` aside so the deploy can
    /// reuse the path. A stale aside copy left by a previous failed run is
    /// removed first, or the rename below would have no free name to use.
    fn set_aside(target: &Path) -> Result<Self> {
        let aside = if target.exists() {
            let aside = aside_path(target);
            let _ = fs::remove_dir_all(&aside);
            fs::rename(target, &aside)
                .map_err(|source| UError::FileError { path: target.to_path_buf(), source })?;
            Some(aside)
        } else {
            None
        };
        Ok(Self { target: target.to_path_buf(), aside })
    }

    /// The new install landed: the aside copy is obsolete.
    fn commit(self) {
        if let Some(aside) = &self.aside {
            let _ = fs::remove_dir_all(aside);
        }
    }

    /// The new install failed: clear the half-written dir and move the aside
    /// copy back. `Err` holds the aside path when the restore itself failed —
    /// the previous installation is still recoverable there by hand.
    fn rollback(self) -> std::result::Result<(), PathBuf> {
        let _ = fs::remove_dir_all(&self.target);
        match &self.aside {
            Some(aside) => fs::rename(aside, &self.target).map_err(|_| aside.clone()),
            None => Ok(()),
        }
    }
}

/// Aside copy path: sibling of `dir` with a `.uvman_bak` suffix
/// (`tools/node/22.0.0` → `tools/node/22.0.0.uvman_bak`)
fn aside_path(dir: &Path) -> PathBuf {
    let mut name = dir.as_os_str().to_os_string();
    name.push(".uvman_bak");
    PathBuf::from(name)
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
        return Err(UError::SimpleError(format!("invalid tool spec '{spec}': missing tool name")));
    }
    let version = if version.is_empty() { None } else { Some(version.to_string()) };
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

    /// Create a fake installed version with one binary file; returns the
    /// install dir
    fn make_install(parent: &Path, name: &str) -> PathBuf {
        let target = parent.join(name);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("tool.exe"), b"binary").unwrap();
        target
    }

    #[test]
    fn test_replace_guard_rolls_back_failed_install() {
        let dir = tempfile::tempdir().unwrap();
        let target = make_install(dir.path(), "22.0.0");

        let guard = ReplaceGuard::set_aside(&target).unwrap();
        assert!(!target.exists(), "old install should be renamed aside");
        assert_eq!(fs::read(aside_path(&target).join("tool.exe")).unwrap(), b"binary");

        // Simulate a half-written new install, then roll it back
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("partial.exe"), b"junk").unwrap();
        guard.rollback().unwrap();

        assert_eq!(fs::read(target.join("tool.exe")).unwrap(), b"binary");
        assert!(!target.join("partial.exe").exists(), "half-written files must go");
        assert!(!aside_path(&target).exists(), "aside copy should be moved back");
    }

    #[test]
    fn test_replace_guard_commit_drops_aside_copy() {
        let dir = tempfile::tempdir().unwrap();
        let target = make_install(dir.path(), "22.0.0");

        let guard = ReplaceGuard::set_aside(&target).unwrap();
        fs::create_dir_all(&target).unwrap(); // the new install lands
        guard.commit();

        assert!(target.exists());
        assert!(!aside_path(&target).exists(), "obsolete aside copy should be dropped");
    }

    #[test]
    fn test_replace_guard_without_previous_install_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("22.0.0");

        let guard = ReplaceGuard::set_aside(&target).unwrap();
        guard.commit();
        assert!(!aside_path(&target).exists());

        let guard = ReplaceGuard::set_aside(&target).unwrap();
        guard.rollback().unwrap();
        assert!(!target.exists());
        assert!(!aside_path(&target).exists());
    }

    #[test]
    fn test_replace_guard_clears_stale_aside() {
        // A leftover aside copy from a previous failed run must not block the
        // rename; the current install replaces its content
        let dir = tempfile::tempdir().unwrap();
        let target = make_install(dir.path(), "22.0.0");
        let stale = aside_path(&target);
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("old.exe"), b"stale").unwrap();

        ReplaceGuard::set_aside(&target).unwrap();

        assert!(!stale.join("old.exe").exists(), "stale aside content should be replaced");
        assert_eq!(fs::read(aside_path(&target).join("tool.exe")).unwrap(), b"binary");
    }
}
