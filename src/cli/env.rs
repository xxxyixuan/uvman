use crate::Result;
use crate::core::current;
use crate::core::paths::{absolute, tools_dir};
use crate::core::shell::Shell;

/// Print shell (bash/zsh/fish/pwsh) export statements for active tool versions
///
/// Intended for shell startup or on-demand refresh:
///   bash/zsh:  eval "$(uvman env)"
///   fish:      uvman env | source
///   pwsh:      uvman env | iex
///   cmd:       for /f "delims=" %i in ('uvman env --shell cmd') do %i
///
/// Output reflects the current state of `uvman use` at invocation time.
/// Stale `UVMAN_*` vars inherited from the caller's environment (tools no
/// longer active) are emitted as unset statements first.
#[derive(Debug, clap::Args)]
pub struct Env {
    /// Restrict output to a single tool (optional)
    pub tool: Option<String>,

    /// Target shell syntax; defaults to auto-detection
    #[clap(short = 's', long, value_enum)]
    pub shell: Option<Shell>,
}

impl Env {
    pub fn run(&self) -> Result<()> {
        let shell = self.shell.unwrap_or_else(Shell::detect);
        let table = current::load();
        // BTreeMap keeps tools name-sorted, so output order is deterministic
        let mut path_entries: Vec<String> = Vec::new();

        // Clear UVMAN_* vars inherited for tools that are no longer active.
        // The single-tool filter is a focused view, so leave other tools alone.
        if self.tool.is_none() {
            for key in stale_uvman_vars(&table, std::env::vars()) {
                println!("{}", shell.unset_var(&key));
            }
        }

        for (name, entry) in &table.tools {
            if let Some(want) = &self.tool
                && want != name
            {
                continue;
            }
            let home = absolute(tools_dir().join(name).join(&entry.version));
            // Skip missing installs (e.g. an active version deleted manually)
            if !home.is_dir() {
                continue;
            }

            let var = env_var_fragment(name);
            println!("{}", shell.set_var(&format!("UVMAN_{var}_VERSION"), &entry.version));
            println!("{}", shell.set_var(&format!("UVMAN_{var}_HOME"), &shell.fmt_path(&home)));

            // PATH entries: install root + bin subdir (when present)
            path_entries.push(shell.fmt_path(&home));
            let bin = home.join("bin");
            if bin.is_dir() {
                path_entries.push(shell.fmt_path(&bin));
            }
        }

        let path_stmt = shell.prepend_path(&path_entries);
        if !path_stmt.is_empty() {
            println!("{path_stmt}");
        }
        Ok(())
    }
}

/// Find UVMAN_* vars from the inherited environment (keys like
/// `UVMAN_<FRAGMENT>_VERSION` / `UVMAN_<FRAGMENT>_HOME`) whose tool is no
/// longer present in the current state table
fn stale_uvman_vars(
    table: &current::CurrentTools, env: impl Iterator<Item = (String, String)>,
) -> Vec<String> {
    let live: std::collections::HashSet<String> = table
        .tools
        .keys()
        .flat_map(|name| {
            let f = env_var_fragment(name);
            [format!("UVMAN_{f}_VERSION"), format!("UVMAN_{f}_HOME")]
        })
        .collect();

    let mut stale: Vec<String> =
        env.map(|(k, _)| k).filter(|k| is_uvman_var(k) && !live.contains(k)).collect();
    stale.sort();
    stale
}

/// Whether a key matches the `UVMAN_*_VERSION` / `UVMAN_*_HOME` shape
/// (non-empty fragment of only A-Z0-9_); UVMAN_SHELL and bare UVMAN_HOME
/// deliberately do NOT match
fn is_uvman_var(key: &str) -> bool {
    let Some(mid) = key
        .strip_prefix("UVMAN_")
        .and_then(|r| r.strip_suffix("_VERSION").or_else(|| r.strip_suffix("_HOME")))
    else {
        return false;
    };
    !mid.is_empty() && mid.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Tool name → env-var fragment: non-alphanumeric chars become `_`, then
/// uppercased (`node` → `NODE`, `rust-toolchain` → `RUST_TOOLCHAIN`)
pub(crate) fn env_var_fragment(tool: &str) -> String {
    tool.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::current::CurrentTools;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn test_env_var_fragment() {
        assert_eq!(env_var_fragment("node"), "NODE");
        assert_eq!(env_var_fragment("rust-toolchain"), "RUST_TOOLCHAIN");
        assert_eq!(env_var_fragment("go_1"), "GO_1");
    }

    #[test]
    fn test_stale_uvman_vars_detection() {
        let mut table = CurrentTools::default();
        table
            .tools
            .insert("node".to_string(), current::CurrentEntry { version: "22.0.0".to_string() });

        let env = vars(&[
            ("UVMAN_NODE_VERSION", "22.0.0"), // live: keep
            ("UVMAN_NODE_HOME", "E:/x"),      // live: keep
            ("UVMAN_PYTHON_VERSION", "3.12"), // tool removed: clear
            ("UVMAN_PYTHON_HOME", "E:/y"),    // tool removed: clear
            ("UVMAN_SHELL", "pwsh"),          // activation marker, not tool var
            ("UVMAN_HOME", "E:/z"),           // bare HOME: empty fragment, skip
            ("PATH", ";"),                    // unrelated
        ]);
        let stale = stale_uvman_vars(&table, env.into_iter());
        assert_eq!(
            stale,
            vec!["UVMAN_PYTHON_HOME".to_string(), "UVMAN_PYTHON_VERSION".to_string()]
        );
    }

    #[test]
    fn test_stale_empty_when_all_live() {
        let mut table = CurrentTools::default();
        table
            .tools
            .insert("node".to_string(), current::CurrentEntry { version: "22.0.0".to_string() });
        let env = vars(&[("UVMAN_NODE_VERSION", "22.0.0"), ("UVMAN_NODE_HOME", "E:/x")]);
        assert!(stale_uvman_vars(&table, env.into_iter()).is_empty());
    }
}
