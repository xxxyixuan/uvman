//! Internal refresh command behind `uvman activate`.
//!
//! Hidden from `--help`: users enable environment sync once via `uvman
//! activate`, and the generated scripts drive `uvman env --shell <shell>` on
//! every prompt refresh, eval-ing its stdout. The contract is therefore to
//! print nothing but shell statements to stdout — diagnostics go to stderr
//! and must stay quiet while a hook is driving the command.

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::Result;
use crate::core::current::{self, CurrentTools};
use crate::core::error::UError;
use crate::core::paths::{absolute, tools_dir};
use crate::core::shell::{Shell, is_activated};
use crate::ui::report::print_hint;

/// Emit shell statements that sync the live environment with the current
/// state of `uvman use`.
///
/// Output reflects the state table at invocation time: stale `UVMAN_*` vars
/// inherited for tools no longer active are unset first, then each active
/// tool exports `UVMAN_<TOOL>_VERSION` / `UVMAN_<TOOL>_HOME` and prepends its
/// install root + `bin/` to PATH.
#[derive(Debug, clap::Args)]
pub struct Env {
    /// Target shell syntax; defaults to auto-detection
    #[clap(short = 's', long, value_enum)]
    pub shell: Option<Shell>,
}

impl Env {
    pub fn run(&self) -> Result<()> {
        let shell = self.shell.unwrap_or_else(Shell::detect);
        let plan = build_plan(&current::load(), &tools_dir());

        let stdout = io::stdout();
        let mut out = BufWriter::new(stdout.lock());
        render(&plan, shell, &mut out).map_err(UError::from)?;
        out.flush().map_err(UError::from)?;

        // Explain a silent result to a human probing the hidden command; a
        // hook-driven invocation (see is_activated) must stay silent to avoid
        // spamming every prompt.
        if plan.tools.is_empty() && !is_activated() {
            print_hint(
                "nothing to export: no tools are active yet (`uvman use <tool>@<version>` \
                 activates an installed one; `uvman activate` keeps your shell in sync)",
                &[],
            );
        }
        Ok(())
    }
}

/// Shell-neutral work plan for one refresh, gathered before any rendering
struct RefreshPlan {
    /// `UVMAN_*` vars inherited for tools no longer active; unset first
    stale_keys: Vec<String>,
    /// Active tools with an existing install dir, name-sorted
    tools: Vec<ToolEnv>,
}

/// One active tool's install layout
struct ToolEnv {
    /// Env-var fragment, e.g. `NODE` for tool `node`
    fragment: String,
    version: String,
    /// Absolute install root
    home: PathBuf,
    /// `bin/` subdir, only when it exists
    bin: Option<PathBuf>,
}

/// Gather the plan from the state table, the tools dir and the inherited
/// environment. The IO-reading half of the command; `tools_root` is a
/// parameter so tests can use a scratch dir.
fn build_plan(table: &CurrentTools, tools_root: &Path) -> RefreshPlan {
    // BTreeMap iteration is name-sorted, so output order is deterministic
    let tools = table
        .tools
        .iter()
        .filter_map(|(name, entry)| {
            let home = absolute(tools_root.join(name).join(&entry.version));
            // Skip missing installs (e.g. an active version deleted manually)
            if !home.is_dir() {
                return None;
            }
            let bin = home.join("bin");
            Some(ToolEnv {
                fragment: env_var_fragment(name),
                version: entry.version.clone(),
                bin: bin.is_dir().then_some(bin),
                home,
            })
        })
        .collect();
    RefreshPlan { stale_keys: stale_uvman_vars(table, std::env::vars()), tools }
}

/// Render the plan into statements the target shell evaluates directly. Pure:
/// reads neither the environment nor the filesystem.
fn render(plan: &RefreshPlan, shell: Shell, out: &mut impl Write) -> io::Result<()> {
    for key in &plan.stale_keys {
        writeln!(out, "{}", shell.unset_var(key))?;
    }

    let mut path_entries = Vec::with_capacity(plan.tools.len() * 2);
    for tool in &plan.tools {
        writeln!(
            out,
            "{}",
            shell.set_var(&format!("UVMAN_{}_VERSION", tool.fragment), &tool.version)
        )?;
        writeln!(
            out,
            "{}",
            shell.set_var(&format!("UVMAN_{}_HOME", tool.fragment), &shell.fmt_path(&tool.home))
        )?;
        path_entries.push(shell.fmt_path(&tool.home));
        if let Some(bin) = &tool.bin {
            path_entries.push(shell.fmt_path(bin));
        }
    }

    // Single PATH prepend: uvman entries take priority over system versions
    let path_stmt = shell.prepend_path(&path_entries);
    if !path_stmt.is_empty() {
        writeln!(out, "{path_stmt}")?;
    }
    Ok(())
}

/// Find UVMAN_* vars from the inherited environment (keys like
/// `UVMAN_<FRAGMENT>_VERSION` / `UVMAN_<FRAGMENT>_HOME`) whose tool is no
/// longer present in the current state table
fn stale_uvman_vars(
    table: &CurrentTools, env: impl Iterator<Item = (String, String)>,
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
fn env_var_fragment(tool: &str) -> String {
    tool.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::current::CurrentEntry;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn tool_env(fragment: &str, version: &str, home: &str, bin: Option<&str>) -> ToolEnv {
        ToolEnv {
            fragment: fragment.to_string(),
            version: version.to_string(),
            home: PathBuf::from(home),
            bin: bin.map(PathBuf::from),
        }
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
        table.tools.insert("node".to_string(), CurrentEntry { version: "22.0.0".to_string() });

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
        table.tools.insert("node".to_string(), CurrentEntry { version: "22.0.0".to_string() });
        let env = vars(&[("UVMAN_NODE_VERSION", "22.0.0"), ("UVMAN_NODE_HOME", "E:/x")]);
        assert!(stale_uvman_vars(&table, env.into_iter()).is_empty());
    }

    #[test]
    fn test_render_bash_plan() {
        let plan = RefreshPlan {
            stale_keys: vec!["UVMAN_PYTHON_VERSION".to_string()],
            tools: vec![
                tool_env(
                    "NODE",
                    "22.19.0",
                    r"E:\uvman\tools\node\22.19.0",
                    Some(r"E:\uvman\tools\node\22.19.0\bin"),
                ),
                // no bin dir → only the install root goes on PATH
                tool_env("GO", "1.23.0", r"E:\uvman\tools\go\1.23.0", None),
            ],
        };

        let mut out = Vec::new();
        render(&plan, Shell::Bash, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "unset UVMAN_PYTHON_VERSION\n\
             export UVMAN_NODE_VERSION=\"22.19.0\"\n\
             export UVMAN_NODE_HOME=\"E:/uvman/tools/node/22.19.0\"\n\
             export UVMAN_GO_VERSION=\"1.23.0\"\n\
             export UVMAN_GO_HOME=\"E:/uvman/tools/go/1.23.0\"\n\
             export PATH=\"E:/uvman/tools/node/22.19.0:E:/uvman/tools/node/22.19.0/bin:\
             E:/uvman/tools/go/1.23.0:$PATH\"\n"
        );
    }

    #[test]
    fn test_render_empty_plan_is_silent() {
        let plan = RefreshPlan { stale_keys: vec![], tools: vec![] };
        let mut out = Vec::new();
        render(&plan, Shell::Bash, &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn test_render_pwsh_paths() {
        let plan = RefreshPlan {
            stale_keys: vec![],
            tools: vec![tool_env("NODE", "22.19.0", r"E:\uvman\tools\node\22.19.0", None)],
        };

        let mut out = Vec::new();
        render(&plan, Shell::PowerShell, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("$env:UVMAN_NODE_VERSION = '22.19.0'"));
        assert!(text.contains(r"$env:UVMAN_NODE_HOME = 'E:\uvman\tools\node\22.19.0'"));
        let sep = if cfg!(windows) { ';' } else { ':' };
        assert!(
            text.contains(&format!(
                "$env:PATH = 'E:\\uvman\\tools\\node\\22.19.0{sep}' + $env:PATH"
            )),
            "pwsh PATH prepend missing: {text}"
        );
    }

    #[test]
    fn test_build_plan_skips_missing_and_detects_bin() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let node = root.join("node").join("22.19.0");
        std::fs::create_dir_all(node.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("go").join("1.23.0")).unwrap();

        let mut table = CurrentTools::default();
        table.tools.insert("node".to_string(), CurrentEntry { version: "22.19.0".to_string() });
        table.tools.insert("go".to_string(), CurrentEntry { version: "1.23.0".to_string() });
        // Install dir missing on disk: excluded from the plan
        table.tools.insert("python".to_string(), CurrentEntry { version: "3.12.0".to_string() });

        let plan = build_plan(&table, root);
        assert_eq!(plan.tools.len(), 2, "missing install skipped");
        assert_eq!(plan.tools[0].fragment, "GO", "name-sorted");
        assert_eq!(plan.tools[1].fragment, "NODE");
        assert!(plan.tools[0].bin.is_none(), "no bin dir");
        assert_eq!(plan.tools[1].bin.as_deref(), Some(node.join("bin").as_path()));
    }
}
