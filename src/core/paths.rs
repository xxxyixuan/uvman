use std::fs;
use std::path::PathBuf;
use std::sync::LazyLock;

use crate::core::error::UError;

fn user_home() -> PathBuf {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(key).map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

/// Directory of the executable (default data root for Windows portable
/// installs)
#[cfg(windows)]
fn executable_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// Compute the uvman data root (common parent of tools/plugins/cache/config/logs).
///
/// Platform defaults:
/// - **Windows**: the executable's directory by default (portable); data lives
///   beside the binary so moving the folder migrates everything. `UVMAN_HOME`
///   overrides.
/// - **Linux/macOS**: fixed to `$HOME/.uvman`, ignoring `UVMAN_HOME`, following
///   Unix conventions (user-level tool data independent of the binary).
fn compute_home() -> PathBuf {
    // Only unit tests (cfg(test)) use test/ to isolate test data from real user
    // data. Do NOT extend to debug_assertions: a debug binary can be added to
    // PATH as a real tool, and a relative home would create test/ anywhere the
    // CWD happens to be (P0 bug). During development, set UVMAN_HOME to isolate
    // the data dir (Windows only).
    if cfg!(test) {
        return PathBuf::from("test");
    }

    #[cfg(windows)]
    {
        // Windows: UVMAN_HOME wins (portable override), else the binary dir
        if let Ok(p) = std::env::var("UVMAN_HOME") {
            return PathBuf::from(p);
        }
        if let Some(dir) = executable_dir() {
            return dir;
        }
    }

    // Linux/macOS: fixed ~/.uvman, ignore UVMAN_HOME
    user_home().join(".uvman")
}

/// Resolved once: nothing in the process mutates `UVMAN_HOME` or re-execs, so
/// every path helper shares one root instead of re-reading env/current_exe.
static HOME: LazyLock<PathBuf> = LazyLock::new(compute_home);

/// uvman data root dir (common parent of tools/plugins/cache/config/logs).
pub fn uvman_home() -> PathBuf {
    HOME.clone()
}

pub fn plugins_dir() -> PathBuf {
    uvman_home().join("plugins")
}

pub fn tools_dir() -> PathBuf {
    uvman_home().join("tools")
}

pub fn cache_dir() -> PathBuf {
    uvman_home().join("cache")
}

pub fn cache_tools_dir() -> PathBuf {
    uvman_home().join("cache").join("tools")
}

/// Cache dir for remote released versions (api source cache for `list <tool>
/// --remote`)
pub fn cache_versions_dir() -> PathBuf {
    uvman_home().join("cache").join("versions")
}

pub fn config_dir() -> PathBuf {
    uvman_home().join("config")
}

pub fn logs_dir() -> PathBuf {
    uvman_home().join("logs")
}

pub fn plugin_path(name: &str) -> PathBuf {
    plugins_dir().join(format!("{name}.toml"))
}

pub fn global_config_path() -> PathBuf {
    uvman_home().join("config").join("uvman.toml")
}

pub fn plugin_index_path() -> PathBuf {
    uvman_home().join("cache").join("plugins.json")
}

pub fn tool_current_path() -> PathBuf {
    config_dir().join("tool_current.toml")
}

/// Anchor a relative path to absolute: values injected/baked into shell scripts
/// must not drift with CWD (UVMAN_HOME may be relative and must be resolved
/// once)
pub fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() { path } else { std::env::current_dir().unwrap_or_default().join(path) }
}

pub fn layout_dirs() -> Vec<PathBuf> {
    vec![config_dir(), plugins_dir(), tools_dir(), cache_dir(), logs_dir()]
}

/// Idempotently create the UVMAN_HOME layout; silently skip existing dirs
pub fn ensure_layout() -> Result<(), UError> {
    layout_dirs().into_iter().try_for_each(|dir| {
        if !dir.exists() {
            fs::create_dir_all(&dir)
                .map_err(|source| UError::FileError { path: dir.clone(), source })?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_path_under_test() {
        // home defaults to test/ in tests
        let path = global_config_path();
        assert!(path.ends_with("test/config/uvman.toml"));
    }

    #[test]
    fn test_plugin_path_under_test() {
        let path = plugin_path("node");
        assert!(path.ends_with("test/plugins/node.toml"));
    }

    #[test]
    fn test_layout_dirs_under_test() {
        let dirs = layout_dirs();
        let names: Vec<&str> =
            dirs.iter().filter_map(|d| d.file_name().and_then(|n| n.to_str())).collect();
        for sub in ["config", "plugins", "tools", "cache", "logs"] {
            assert!(names.contains(&sub), "layout must include {sub}");
        }
        // All dirs must live under the test home (test/)
        for dir in &dirs {
            assert!(dir.starts_with("test"), "{} must be under test/", dir.display());
        }
    }
}
