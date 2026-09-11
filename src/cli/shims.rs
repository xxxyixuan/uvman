//! `uvman shims`: manage the command forwarders that serve GUI/IDE processes.
//!
//! GUI processes inherit the Explorer (registry) environment and can never see
//! `activate`'s session PATH. The plans solves this with shims: a stable
//! `<UVMAN_HOME>\shims` directory on the *user* PATH whose entries forward to
//! whatever version the resolution chain picks — `use` never touches the
//! registry. This command owns that PATH wiring (`enable` / `disable`), the
//! health report (`status`) and manual regeneration (`rehash`).

use crate::Result;
use crate::core::error::UError;
use crate::core::paths;
use crate::core::pathstore::{native_store, EnableReport, PathEditor};
use crate::core::shims;
use crate::ui::report::print_hint;
use crate::ui::style::{odim, ored, oyellow};

/// Manage the shims directory (GUI/IDE tool resolution via user PATH)
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct Shims {
    #[clap(subcommand)]
    pub command: ShimsCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ShimsCommand {
    /// Add `<UVMAN_HOME>\shims` to the user PATH (Windows registry) so GUI
    /// apps resolve uvman-managed tools; on Unix, print the manual shell
    /// profile line instead of editing your profile
    Enable,
    /// Remove uvman's shims entry from the user PATH
    Disable,
    /// Report whether shims are wired into the user PATH, consistent with the
    /// active tools, and warn about shadowing system tools
    Status,
    /// Regenerate the shim forwarders from the active tools (idempotent)
    Rehash,
}

impl Shims {
    pub fn run(&self) -> Result<()> {
        match &self.command {
            ShimsCommand::Enable => enable(),
            ShimsCommand::Disable => disable(),
            ShimsCommand::Status => status(),
            ShimsCommand::Rehash => rehash_cmd(),
        }
    }
}

/// User PATH entry this release owns: `<home>\shims`
fn shims_entry() -> String {
    paths::shims_dir().to_string_lossy().into_owned()
}

/// Windows: write the shims dir into the user PATH (backup → type-faithful
/// write → broadcast). Unix: PATH is shell-owned, so only print the manual
/// step — never edit the user's profile.
fn enable() -> Result<()> {
    if !cfg!(windows) {
        println!("{}", crate::ui::style::ogreen("shims are generated, but the user PATH is shell-owned here"));
        print_hint(
            "add the shims dir to your shell profile once (or keep using `uvman activate`)",
            &[format!("export PATH=\"{}\"${{PATH:+:${{PATH}}}}", shims_entry())],
        );
        return Ok(());
    }

    let store = native_store();
    let shims = paths::shims_dir();
    let backup = paths::backup_dir();
    let editor = PathEditor::new(store.as_ref(), &shims, &backup);
    match editor.enable() {
        Ok(EnableReport::AlreadyEnabled) => {
            println!("{}", odim(format!("already enabled: {} is in the user PATH", shims_entry())));
        },
        Ok(EnableReport::Enabled) => {
            println!(
                "{}",
                crate::ui::style::ogreen(format!("enabled: added {} to the user PATH", shims_entry()))
            );
            print_hint(
                "the user PATH is a system setting — restart already-running programs \
                 (including IDEA) for them to pick it up",
                &[],
            );
        },
        Err(e) => {
            // Registry-level failure (no access etc.): state clearly and point
            // at the manual path so a half-configured state can't hide
            return Err(UError::SimpleError(format!(
                "failed to write the user PATH: {e}; add `{}` to your user PATH by hand if needed",
                shims_entry()
            ))
            .into());
        },
    }
    Ok(())
}

/// Windows: strip uvman-owned entries. Unix: nothing is stored in the shell
/// profile by uvman, so report the no-op.
fn disable() -> Result<()> {
    if !cfg!(windows) {
        println!("{}", odim("nothing to disable: uvman never edits the shell profile on this platform"));
        return Ok(());
    }
    let store = native_store();
    let shims = paths::shims_dir();
    let backup = paths::backup_dir();
    let editor = PathEditor::new(store.as_ref(), &shims, &backup);
    match editor.disable() {
        Ok(crate::core::pathstore::DisableReport::AlreadyDisabled) => {
            println!("{}", odim("shims entry is not in the user PATH"));
        },
        Ok(_) => {
            println!(
                "{}",
                crate::ui::style::ogreen(format!("disabled: removed {} from the user PATH", shims_entry()))
            );
        },
        Err(e) => {
            return Err(UError::SimpleError(format!(
                "failed to update the user PATH: {e}; remove `{}` from your user PATH by hand if needed",
                shims_entry()
            ))
            .into());
        },
    }
    Ok(())
}

/// Health report: directory state, PATH wiring, shim↔active consistency and
/// system-tool shadowing. Warnings don't change the exit code (consistent
/// with `doctor`'s warning-vs-fail split).
fn status() -> Result<()> {
    let home = paths::uvman_home();
    let shims_dir = home.join("shims");
    let mut warns = 0usize;

    // 1. The shims directory itself
    match shims_count(&shims_dir) {
        Some(count) if count > 0 => {
            println!("{} {count} shim{} in {}", crate::ui::style::ogreen("✔"), plural(count), shims_dir.display())
        },
        Some(_) => {
            println!("{} shims dir exists but holds no shims", warn_str());
            warns += 1;
        },
        None => {
            println!("{} shims dir is missing (run `uvman shims rehash`)", warn_str());
            warns += 1;
        },
    }

    // 2. User PATH wiring — platform-specific
    #[cfg(windows)]
    {
        let store = native_store();
        let backup = paths::backup_dir();
        let editor = PathEditor::new(store.as_ref(), &shims_dir, &backup);
        match editor.in_path() {
            Ok(true) => println!("{} shims dir is in the user PATH", crate::ui::style::ogreen("✔")),
            Ok(false) => {
                println!("{} shims dir is NOT in the user PATH (GUI apps won't see it)", warn_str());
                warns += 1;
            },
            Err(e) => {
                println!("{} could not read the user PATH: {e}", warn_str());
                warns += 1;
            },
        }
    }
    #[cfg(not(windows))]
    {
        println!("{} user PATH is shell-owned here; shims apply after you add the profile line", odim("·"));
    }

    // 3. Shim ↔ active-tool consistency (stale / missing / unresolvable)
    let desired = shims::active_command_names(&home);
    let existing = shims::manifest_names(&shims_dir);
    let stale: Vec<&String> = existing.iter().filter(|n| !desired.contains(n)).collect();
    let missing: Vec<&String> = desired.iter().filter(|n| !existing.contains(n) || !shims_dir.join(n).exists()).collect();
    for name in &stale {
        println!("{} stale shim `{name}` (no active tool provides it) — run `uvman shims rehash`", warn_str());
        warns += 1;
    }
    for name in &missing {
        println!("{} missing shim `{name}` for an active tool — run `uvman shims rehash`", warn_str());
        warns += 1;
    }
    for name in &existing {
        if shims::locate_forward_target(&home, name).is_none() {
            println!("{} shim `{name}` resolves to nothing — run `uvman shims rehash`", warn_str());
            warns += 1;
        }
    }

    // 4. System-tool shadowing on the process PATH (a real tool may preempt the
    //    shims entry when it sorts earlier)
    for name in &desired {
        if let Some(shadow) = shadowing_path_entry(&shims_dir, name) {
            println!(
                "{} `{name}` also exists in {} (system or other PATH entry); it shadows the uvman shim",
                warn_str(),
                shadow.display()
            );
            warns += 1;
        }
    }

    if warns == 0 {
        println!("{}", crate::ui::style::ogreen("all shims checks passed"));
    } else {
        println!("{}", ored(format!("{warns} warning{s} found", s = if warns == 1 { "" } else { "s" })));
    }
    Ok(())
}

fn warn_str() -> String {
    oyellow("⚠").to_string()
}

fn shims_count(shims_dir: &std::path::Path) -> Option<usize> {
    let entries = std::fs::read_dir(shims_dir).ok()?;
    // Top-level regular files only: the private helper lives in the `.uvman`
    // subdirectory, so every top-level file is a shim
    let n = entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_file())).count();
    Some(n)
}

/// First PATH entry (other than the shims dir) containing a same-named
/// command; used for the shadow warning.
fn shadowing_path_entry(shims_dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let candidates: Vec<std::path::PathBuf> = if cfg!(windows) {
        [".exe", ".cmd", ".bat", ".ps1"].iter().map(|e| std::path::PathBuf::from(format!("{name}{e}"))).collect()
    } else {
        vec![std::path::PathBuf::from(name)]
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for item in std::env::split_paths(&path_var) {
        if item == *shims_dir {
            continue;
        }
        if candidates.iter().any(|c| item.join(c).is_file()) {
            return Some(item);
        }
    }
    None
}

fn rehash_cmd() -> Result<()> {
    let home = paths::uvman_home();
    match shims::rehash(&home) {
        Ok(report) => {
            for name in &report.removed {
                println!("{}", odim(format!("removed {name}")));
            }
            for name in &report.generated {
                println!("{}", crate::ui::style::ogreen(format!("shim {name}")));
            }
            let summary = if report.generated.is_empty() && report.removed.is_empty() {
                "shims are up to date".to_string()
            } else {
                format!("{} shim{}, {} removed", report.generated.len(), plural(report.generated.len()), report.removed.len())
            };
            println!("{summary}");
        },
        Err(e) => {
            // The most helpful failure is a missing forwarding binary: physical
            // reinstall fixes it, so name it explicitly
            return Err(e.into());
        },
    }
    Ok(())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plural() {
        assert_eq!(plural(1), "");
        assert_eq!(plural(2), "s");
    }

    #[test]
    fn test_shadowing_skips_shims_dir() {
        let dir = tempfile::tempdir().unwrap();
        let shims = dir.path().join("shims");
        std::fs::create_dir_all(&shims).unwrap();
        let name = if cfg!(windows) { "node.exe" } else { "node" };
        std::fs::write(shims.join(name), b"x").unwrap();
        // Only the shims dir holds it: no shadow
        assert_eq!(shadowing_path_entry(&shims, name), None);
    }
}