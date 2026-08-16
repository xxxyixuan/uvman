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
        // BTreeMap 按工具名有序，输出确定性
        let mut path_entries: Vec<String> = Vec::new();

        // 先清理继承环境中已失效的 UVMAN_* 变量（工具被移除场景）。
        // 单工具过滤模式是聚焦输出，不越权动其他工具的变量
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
            // 激活版本被手动删除等场景：跳过失效条目而非报错
            if !home.is_dir() {
                continue;
            }

            let var = env_var_fragment(name);
            println!("{}", shell.set_var(&format!("UVMAN_{var}_VERSION"), &entry.version));
            println!(
                "{}",
                shell.set_var(&format!("UVMAN_{var}_HOME"), &shell.fmt_path(&home))
            );

            // PATH 条目：安装根目录 + bin 子目录（存在时）
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

/// 找出继承环境中已失效的 UVMAN_* 变量：形如 `UVMAN_<FRAGMENT>_VERSION`
/// / `UVMAN_<FRAGMENT>_HOME`，但对应工具不在当前状态表内
fn stale_uvman_vars(
    table: &current::CurrentTools,
    env: impl Iterator<Item = (String, String)>,
) -> Vec<String> {
    let live: std::collections::HashSet<String> = table
        .tools
        .keys()
        .flat_map(|name| {
            let f = env_var_fragment(name);
            [format!("UVMAN_{f}_VERSION"), format!("UVMAN_{f}_HOME")]
        })
        .collect();

    let mut stale: Vec<String> = env
        .map(|(k, _)| k)
        .filter(|k| is_uvman_var(k) && !live.contains(k))
        .collect();
    stale.sort();
    stale
}

/// UVMAN_*_VERSION / UVMAN_*_HOME 形态判断（片段非空且仅 A-Z0-9_）；
/// 注意 UVMAN_SHELL（激活标记）与裸 UVMAN_HOME 均不符合
fn is_uvman_var(key: &str) -> bool {
    let Some(mid) = key
        .strip_prefix("UVMAN_")
        .and_then(|r| r.strip_suffix("_VERSION").or_else(|| r.strip_suffix("_HOME")))
    else {
        return false;
    };
    !mid.is_empty()
        && mid
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// 工具名 → 环境变量名片段：非字母数字转 `_` 并大写
/// （`node` → `NODE`，`rust-toolchain` → `RUST_TOOLCHAIN`）
pub(crate) fn env_var_fragment(tool: &str) -> String {
    tool.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::current::CurrentTools;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
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
        table.tools.insert(
            "node".to_string(),
            current::CurrentEntry { version: "22.0.0".to_string() },
        );

        let env = vars(&[
            ("UVMAN_NODE_VERSION", "22.0.0"),   // 在表内：保留
            ("UVMAN_NODE_HOME", "E:/x"),        // 在表内：保留
            ("UVMAN_PYTHON_VERSION", "3.12"),   // 工具已移除：清理
            ("UVMAN_PYTHON_HOME", "E:/y"),      // 工具已移除：清理
            ("UVMAN_SHELL", "pwsh"),            // 激活标记：不属于工具变量
            ("UVMAN_HOME", "E:/z"),             // 裸 HOME：片段为空，跳过
            ("PATH", ";"),                      // 无关变量
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
        table.tools.insert(
            "node".to_string(),
            current::CurrentEntry { version: "22.0.0".to_string() },
        );
        let env = vars(&[("UVMAN_NODE_VERSION", "22.0.0"), ("UVMAN_NODE_HOME", "E:/x")]);
        assert!(stale_uvman_vars(&table, env.into_iter()).is_empty());
    }
}
