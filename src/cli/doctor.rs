//! `uvman doctor`: environment self-check.
//!
//! Verifies the four basics of the environment: UVMAN_HOME layout, global
//! config parseability, plugin dir integrity, and shell activation state.
//! Human output is a check report; `--json` emits a machine-readable document.
//! Exit code is 1 when any check fails; warnings don't affect it.

use std::fs;
use std::path::Path;

use crate::Result;
use crate::core::config::UvmanConfig;
use crate::core::error::UError;
use crate::core::paths;
use crate::core::plugin::ToolPlugin;
use crate::core::shell::{Shell, is_activated};
use crate::ui::style;

/// Check the uvman environment for problems
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct Doctor {
    /// Print the report in JSON format
    #[clap(short = 'J', long)]
    pub(crate) json: bool,
}

impl Doctor {
    pub fn run(&self) -> Result<()> {
        let home = paths::uvman_home();
        let checks = collect(&home, is_activated());

        if self.json {
            let healthy = checks.iter().all(|c| c.status != Status::Fail);
            let doc = serde_json::json!({
                "home": home.display().to_string(),
                "healthy": healthy,
                "checks": checks
                    .iter()
                    .map(|c| serde_json::json!({
                        "name": c.name,
                        "status": c.status.as_str(),
                        "detail": c.detail,
                        "fix": c.fix,
                    }))
                    .collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&doc)?);
        } else {
            print_report(&home, &checks);
        }

        // Diagnostics stay scriptable: only real failures set the exit code
        if checks.iter().any(|c| c.status == Status::Fail) {
            std::process::exit(1);
        }
        Ok(())
    }
}

/// Run the four doctor checks against a home dir (`activated` comes from the
/// live environment)
fn collect(home: &Path, activated: bool) -> Vec<Check> {
    vec![check_layout(home), check_config(home), check_plugins(home), check_shell(activated)]
}

/// Subdirs a healthy UVMAN_HOME must have; must stay in sync with
/// [`paths::layout_dirs`] (guarded by a unit test)
const LAYOUT_SUBDIRS: [&str; 5] = ["config", "plugins", "tools", "cache", "logs"];

fn check_layout(home: &Path) -> Check {
    let missing: Vec<&str> =
        LAYOUT_SUBDIRS.iter().copied().filter(|d| !home.join(d).is_dir()).collect();
    if missing.is_empty() {
        Check {
            name: "layout",
            status: Status::Ok,
            detail: format!("{} ({} dirs present)", home.display(), LAYOUT_SUBDIRS.len()),
            fix: None,
        }
    } else {
        Check {
            name: "layout",
            status: Status::Fail,
            detail: format!("missing: {}", missing.join(", ")),
            fix: None,
        }
    }
}

fn check_config(home: &Path) -> Check {
    let path = home.join("config").join("uvman.toml");
    match UvmanConfig::load_from(&path) {
        Ok(_) => Check {
            name: "config",
            status: Status::Ok,
            detail: "uvman.toml parses".into(),
            fix: None,
        },
        // First-run generation (app::init) should have created it; missing
        // means that failed and the config dir is unwritable
        Err(UError::FileError { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Check {
                name: "config",
                status: Status::Fail,
                detail: "config/uvman.toml is missing".into(),
                fix: None,
            }
        },
        Err(UError::TomlError { source, .. }) => Check {
            name: "config",
            status: Status::Fail,
            detail: format!("failed to parse uvman.toml: {source}"),
            fix: None,
        },
        Err(e) => Check {
            name: "config",
            status: Status::Fail,
            detail: format!("failed to load uvman.toml: {e}"),
            fix: None,
        },
    }
}

fn check_plugins(home: &Path) -> Check {
    let dir = home.join("plugins");
    let mut total = 0;
    let mut broken = Vec::new();
    // A missing plugins dir is already reported by the layout check
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("toml") {
                continue;
            }
            total += 1;
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("plugin");
            if let Err(e) = ToolPlugin::load_from(&path) {
                broken.push(match e {
                    UError::TomlError { source, .. } => format!("{name}.toml: {source}"),
                    e => format!("{name}.toml: {e}"),
                });
            }
        }
    }

    if broken.is_empty() {
        let detail = if total == 0 {
            "no plugins installed".to_string()
        } else {
            format!("{total} installed, all valid")
        };
        Check { name: "plugins", status: Status::Ok, detail, fix: None }
    } else {
        Check {
            name: "plugins",
            status: Status::Fail,
            detail: format!("{} of {} failed to parse: {}", broken.len(), total, broken.join("; ")),
            fix: None,
        }
    }
}

fn check_shell(activated: bool) -> Check {
    if activated {
        Check {
            name: "shell",
            status: Status::Ok,
            detail: "activated; the prompt hook refreshes env automatically".into(),
            fix: None,
        }
    } else {
        let shell = Shell::detect();
        Check {
            name: "shell",
            status: Status::Warn,
            detail: format!(
                "not activated (detected shell: {}); \
                 env changes need a manual refresh until activated",
                shell_name(shell)
            ),
            fix: activate_command(shell),
        }
    }
}

/// The eval form activating uvman in the detected shell (mirrors the usage
/// documented on `uvman activate`); cmd has no activation support
fn activate_command(shell: Shell) -> Option<String> {
    match shell {
        Shell::Bash | Shell::Zsh => Some(r#"eval "$(uvman activate)""#.into()),
        Shell::Fish => Some("uvman activate | source".into()),
        Shell::PowerShell => Some("uvman activate | Out-String | Invoke-Expression".into()),
        Shell::Cmd => None,
    }
}

fn shell_name(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => "bash",
        Shell::Zsh => "zsh",
        Shell::Fish => "fish",
        Shell::PowerShell => "pwsh",
        Shell::Cmd => "cmd",
    }
}

/// Outcome of one self-check: ok passes, warn is suspicious but workable,
/// fail is a real problem (sets the exit code)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

/// One self-check result (pure data; rendering happens at the end)
struct Check {
    name: &'static str,
    status: Status,
    detail: String,
    /// Copyable fix command for warn/fail results, if one exists
    fix: Option<String>,
}

fn print_report(home: &Path, checks: &[Check]) {
    println!("{}", style::odim(format!("uvman home: {}", home.display())));
    for check in checks {
        println!("{} {:<8} {}", mark(check.status), check.name, check.detail);
        if let Some(fix) = &check.fix {
            println!("  fix: {}", style::ogreen(fix));
        }
    }

    let warns = checks.iter().filter(|c| c.status == Status::Warn).count();
    let fails = checks.iter().filter(|c| c.status == Status::Fail).count();
    let summary = if fails > 0 {
        style::ored(format!("{fails} problem{}, {warns} warning{}", plural(fails), plural(warns)))
            .to_string()
    } else if warns > 0 {
        style::oyellow(format!("{warns} warning{}, otherwise healthy", plural(warns))).to_string()
    } else {
        style::ogreen("all checks passed").to_string()
    };
    println!("{summary}");
}

fn mark(status: Status) -> String {
    match status {
        Status::Ok => style::ogreen("✔").to_string(),
        Status::Warn => style::oyellow("⚠").to_string(),
        Status::Fail => style::ored("✖").to_string(),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::DEFAULT_CONFIG;

    /// A minimal valid plugin toml (same shape toolset tests use)
    const VALID_PLUGIN: &str = r#"
[tool]
name = "fakey"
[registry]
default = "https://example.com/dist"
[release]
source = "static"
versions = []
[platform]
os_map = { windows = "win" }
arch_map = { x86_64 = "x64" }
[install.defaults]
version = "latest"
mode = "bin"
[[install.bin]]
os = ["windows"]
arch = ["x86_64"]
[install.bin.download]
path = "{registry}/fakey-{version}-{os}-{arch}.{ext}"
[install.bin.download.ext]
windows = "zip"
[install.bin.download.hash]
enabled = false
[install.bin.extract]
strip = 1
[install.bin.deploy]
bin_dir = "bin"
"#;

    /// Doctor scans by subdir name; keep the list in sync with paths::layout_dirs
    #[test]
    fn test_layout_subdirs_match_paths() {
        let names: Vec<String> = paths::layout_dirs()
            .iter()
            .filter_map(|d| d.file_name().and_then(|n| n.to_str()).map(str::to_string))
            .collect();
        assert_eq!(names, LAYOUT_SUBDIRS);
    }

    fn make_home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn make_layout(home: &Path) {
        for sub in LAYOUT_SUBDIRS {
            fs::create_dir_all(home.join(sub)).unwrap();
        }
    }

    #[test]
    fn test_check_layout() {
        let home = make_home();
        // Nothing created yet → all 5 missing
        let check = check_layout(home.path());
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("config") && check.detail.contains("logs"));

        make_layout(home.path());
        let check = check_layout(home.path());
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("5 dirs present"));
    }

    #[test]
    fn test_check_config() {
        let home = make_home();
        // Missing config fails (first-run generation should have written it)
        assert_eq!(check_config(home.path()).status, Status::Fail);

        fs::create_dir_all(home.path().join("config")).unwrap();
        fs::write(home.path().join("config/uvman.toml"), DEFAULT_CONFIG).unwrap();
        assert_eq!(check_config(home.path()).status, Status::Ok);

        fs::write(home.path().join("config/uvman.toml"), "not = [valid").unwrap();
        let check = check_config(home.path());
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("failed to parse"));
    }

    #[test]
    fn test_check_plugins() {
        let home = make_home();
        // Missing plugins dir: caught by layout; plugins check stays neutral
        assert_eq!(check_plugins(home.path()).detail, "no plugins installed");

        let plugins = home.path().join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        assert_eq!(check_plugins(home.path()).status, Status::Ok);

        fs::write(plugins.join("fakey.toml"), VALID_PLUGIN).unwrap();
        let check = check_plugins(home.path());
        assert_eq!(check.status, Status::Ok);
        assert_eq!(check.detail, "1 installed, all valid");

        fs::write(plugins.join("broken.toml"), "not = [valid").unwrap();
        let check = check_plugins(home.path());
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("1 of 2"));
        assert!(check.detail.contains("broken.toml"));
    }

    #[test]
    fn test_check_shell() {
        let check = check_shell(true);
        assert_eq!(check.status, Status::Ok);
        assert!(check.fix.is_none());

        // Not activated is a warning with an actionable fix (cmd has none)
        let check = check_shell(false);
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("not activated"));
        assert!(check.fix.as_deref().is_none_or(|f| f.contains("uvman activate")));
    }

    #[test]
    fn test_collect_covers_four_checks() {
        let home = make_home();
        let checks = collect(home.path(), true);
        let names: Vec<&str> = checks.iter().map(|c| c.name).collect();
        assert_eq!(names, ["layout", "config", "plugins", "shell"]);
        // Unhealthy home propagates to the check results
        assert_eq!(checks.iter().filter(|c| c.status == Status::Fail).count(), 2);
    }
}
