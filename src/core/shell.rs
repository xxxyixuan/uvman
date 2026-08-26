//! Target shell rendering strategy (design in .tmp/dev-docs/uvman-env-design.md).
//!
//! Renders platform-neutral key-values into statements each shell can evaluate
//! directly, and generates the activate script (design in uvman-activate-design.md).
//! Stateless, no IO; detection (detect) only reads environment variables.

use std::path::Path;

/// Target shell for `uvman env` output
// "PowerShell" is a domain term; the Shell suffix cannot be avoided
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Shell {
    /// bash / Git Bash / POSIX sh compatible
    #[value(alias = "sh")]
    Bash,
    /// zsh
    Zsh,
    /// fish
    Fish,
    /// Windows PowerShell (incl. pwsh 7+)
    #[value(name = "pwsh", alias = "powershell")]
    PowerShell,
    /// Windows cmd.exe
    #[value(alias = "cmd.exe", alias = "bat")]
    Cmd,
}

impl Shell {
    /// Environment inference: the default source aside from explicit --shell.
    ///
    /// Windows: pwsh-family always sets PSModulePath, cmd does not;
    /// Unix: $SHELL basename, unknowns (dash/sh etc.) normalize to Bash.
    pub fn detect() -> Shell {
        if cfg!(windows) {
            if std::env::var_os("PSModulePath").is_some() {
                Shell::PowerShell
            } else {
                Shell::Cmd
            }
        } else {
            match std::env::var("SHELL") {
                Ok(s) => match s.rsplit('/').next().unwrap_or_default() {
                    "zsh" => Shell::Zsh,
                    "fish" => Shell::Fish,
                    _ => Shell::Bash,
                },
                Err(_) => Shell::Bash,
            }
        }
    }

    /// Render a single variable set (calling code owns escaping and path style:
    /// value is the final string; convert paths with fmt_path first)
    pub fn set_var(&self, key: &str, value: &str) -> String {
        match self {
            Shell::Bash | Shell::Zsh => {
                format!("export {key}={}", sh_quote(value))
            }
            Shell::Fish => {
                format!("set -gx {key} {}", sh_quote(value))
            }
            Shell::PowerShell => {
                format!("$env:{key} = '{}'", value.replace('\'', "''"))
            }
            Shell::Cmd => {
                // Quoted form avoids trailing-space and special char (&) issues
                format!("set \"{key}={value}\"")
            }
        }
    }

    /// Render a PATH prepend statement (uvman entries take priority over system versions).
    /// References to existing PATH ($PATH etc.) are expanded by each shell, not escaped here.
    pub fn prepend_path(&self, entries: &[String]) -> String {
        if entries.is_empty() {
            return String::new();
        }
        match self {
            Shell::Bash | Shell::Zsh => {
                format!("export PATH=\"{}:$PATH\"", sh_escape(&entries.join(":")))
            }
            Shell::Fish => {
                // fish PATH is a list: each element quoted independently, spaces safe
                let items: Vec<String> =
                    entries.iter().map(|e| sh_quote(e)).collect();
                format!("set -gx PATH {} $PATH", items.join(" "))
            }
            Shell::PowerShell => {
                let sep = self.path_sep();
                let joined = entries.join(&sep.to_string());
                format!("$env:PATH = '{joined}{sep}' + $env:PATH")
            }
            Shell::Cmd => {
                let sep = self.path_sep();
                let joined = entries.join(&sep.to_string());
                format!("set \"PATH={joined}{sep}%PATH%\"")
            }
        }
    }

    /// Refresh hint after a successful `use` (copyable commands for the detected shell)
    pub fn inject_hint(&self) -> Vec<String> {
        match self {
            Shell::Bash | Shell::Zsh => {
                vec!["eval \"$(uvman env)\"".to_string()]
            }
            Shell::Fish => vec!["uvman env | source".to_string()],
            Shell::PowerShell => vec!["uvman env | iex".to_string()],
            Shell::Cmd => {
                // for /f runs output line-by-line; interactive uses %i (%%i only inside a .bat)
                vec!["for /f \"delims=\" %i in ('uvman env --shell cmd') do %i"
                    .to_string()]
            }
        }
    }

    /// PATH list separator
    fn path_sep(&self) -> char {
        match self {
            Shell::Bash | Shell::Zsh | Shell::Fish => ':',
            // pwsh is cross-platform; follow the host OS
            Shell::PowerShell | Shell::Cmd => {
                if cfg!(windows) {
                    ';'
                } else {
                    ':'
                }
            }
        }
    }

    /// Path style: POSIX family always uses / (Git Bash accepts E:/x and avoids \ ambiguity);
    /// pwsh/cmd follow the native OS
    pub fn fmt_path(&self, path: &Path) -> String {
        let s = path.to_string_lossy();
        match self {
            Shell::Bash | Shell::Zsh | Shell::Fish => s.replace('\\', "/"),
            Shell::PowerShell | Shell::Cmd => s.to_string(),
        }
    }

    /// Render an unset statement (clears stale UVMAN_* vars)
    pub fn unset_var(&self, key: &str) -> String {
        match self {
            Shell::Bash | Shell::Zsh => format!("unset {key}"),
            Shell::Fish => format!("set -e {key}"),
            Shell::PowerShell => {
                format!("Remove-Item Env:{key} -ErrorAction SilentlyContinue")
            }
            Shell::Cmd => format!("set \"{key}=\""),
        }
    }

    /// Generate the activate script (design in uvman-activate-design.md 4.x).
    ///
    /// Bakes absolute state/tools paths; structure: baked constants →
    /// refresh fn (strip-then-eval) → prompt hook (mtime fast path) →
    /// registration (idempotent guard) → one refresh on activation.
    pub fn activation_script(&self, state: &Path, tools_root: &Path) -> Result<String, String> {
        let render = |tmpl: &str| {
            tmpl.replace("@STATE@", &self.fmt_path(state))
                .replace("@TOOLS@", &self.fmt_path(tools_root))
        };
        let script = match self {
            Shell::Bash => render(BASH_TMPL),
            Shell::Zsh => render(ZSH_TMPL),
            Shell::Fish => render(FISH_TMPL),
            Shell::PowerShell => render(PWSH_TMPL),
            // cmd has no prompt hook; the activate command layer intercepts and explains alternatives
            Shell::Cmd => {
                return Err("cmd has no prompt hook".to_string());
            }
        };
        Ok(script)
    }
}

const BASH_TMPL: &str = r#"# --- uvman activate bash ---
export UVMAN_SHELL=bash
__UVMAN_STATE='@STATE@'
__UVMAN_TOOLS='@TOOLS@'
__UVMAN_STAMP="$(mktemp)"

# 刷新：剥离旧 uvman PATH 条目后重新求值，保证幂等
__uvman_refresh() {
    local __e __new="" IFS=:
    for __e in $PATH; do
        case "$__e" in "$__UVMAN_TOOLS"*) ;; *) __new="${__new:+$__new:}$__e" ;; esac
    done
    PATH="$__new"
    eval "$(uvman env --shell bash)"
    touch "$__UVMAN_STAMP"
}

# mtime 快路径：未变更时零进程派生
__uvman_hook() {
    local __s=$?
    [ -f "$__UVMAN_STATE" ] && [ "$__UVMAN_STATE" -nt "$__UVMAN_STAMP" ] && __uvman_refresh
    return $__s
}

case ";${PROMPT_COMMAND:-};" in
    *";__uvman_hook;"*) ;;
    *) PROMPT_COMMAND="__uvman_hook${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac

__uvman_refresh
"#;

const ZSH_TMPL: &str = r#"# --- uvman activate zsh ---
export UVMAN_SHELL=zsh
__UVMAN_STATE='@STATE@'
__UVMAN_TOOLS='@TOOLS@'
__UVMAN_STAMP="$(mktemp)"

__uvman_refresh() {
    local __e __new=""
    # zsh 的 path 数组与 PATH 联动，直接遍历无需 IFS 拆分
    for __e in $path; do
        case "$__e" in "$__UVMAN_TOOLS"*) ;; *) __new="${__new:+$__new:}$__e" ;; esac
    done
    PATH="$__new"
    eval "$(uvman env --shell zsh)"
    touch "$__UVMAN_STAMP"
}

__uvman_hook() {
    local __s=$?
    [[ -f "$__UVMAN_STATE" && "$__UVMAN_STATE" -nt "$__UVMAN_STAMP" ]] && __uvman_refresh
    return $__s
}

if [[ -z "${precmd_functions[(r)__uvman_hook]}" ]]; then
    precmd_functions=(__uvman_hook $precmd_functions)
fi

__uvman_refresh
"#;

const FISH_TMPL: &str = r#"# --- uvman activate fish ---
set -gx UVMAN_SHELL fish
set -g __uvm_state '@STATE@'
set -g __uvm_tools '@TOOLS@'
set -g __uvm_stamp (mktemp)

function __uvman_refresh
    # string 内建剥离旧 uvman PATH 条目，零 spawn
    set -l cleaned (string match --invert --regex "^$__uvm_tools" $PATH)
    set -gx PATH $cleaned
    uvman env --shell fish | source
    touch $__uvm_stamp
end

function __uvman_hook --on-event fish_prompt
    if test -f $__uvm_state; and test $__uvm_state -nt $__uvm_stamp
        __uvman_refresh
    end
end

__uvman_refresh
"#;

const PWSH_TMPL: &str = r#"# --- uvman activate pwsh ---
$Env:UVMAN_SHELL = 'pwsh'
$global:__UVMAN_STATE = '@STATE@'
$global:__UVMAN_TOOLS = '@TOOLS@'
$global:__UVMAN_TICKS = -1L

function global:__uvman_refresh {
    # 剥离旧 uvman PATH 条目（前缀匹配），保证刷新幂等
    $rest = @($Env:PATH -split ';' | Where-Object {
        $_ -and -not $_.StartsWith($global:__UVMAN_TOOLS) })
    $Env:PATH = $rest -join ';'
    uvman env --shell pwsh | Out-String | Invoke-Expression
}

# mtime 快路径：Ticks 会话内缓存，未变更时不派生 uvman 进程
function global:__uvman_hook {
    $f = Get-Item $global:__UVMAN_STATE -ErrorAction SilentlyContinue
    if ($f -and $f.LastWriteTimeUtc.Ticks -ne $global:__UVMAN_TICKS) {
        $global:__UVMAN_TICKS = $f.LastWriteTimeUtc.Ticks
        __uvman_refresh
    }
}

# 守卫：不二次包裹原 prompt
if (-not $global:__uvman_orig_prompt) {
    $global:__uvman_orig_prompt = $function:prompt
    function global:prompt {
        __uvman_hook
        & $global:__uvman_orig_prompt
    }
}

__uvman_hook
"#;

/// POSIX double-quote escaping (\ " $ `), so values are not re-expanded at eval time
fn sh_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

fn sh_quote(value: &str) -> String {
    format!("\"{}\"", sh_escape(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_set_var_all_shells() {
        assert_eq!(
            Shell::Bash.set_var("K", "1.0"),
            "export K=\"1.0\""
        );
        assert_eq!(
            Shell::Fish.set_var("K", "1.0"),
            "set -gx K \"1.0\""
        );
        assert_eq!(
            Shell::PowerShell.set_var("K", "1.0"),
            "$env:K = '1.0'"
        );
        assert_eq!(Shell::Cmd.set_var("K", "1.0"), "set \"K=1.0\"");
    }

    #[test]
    fn test_pwsh_single_quote_escaped() {
        assert_eq!(
            Shell::PowerShell.set_var("K", "a'b"),
            "$env:K = 'a''b'"
        );
    }

    #[test]
    fn test_posix_expansions_escaped() {
        assert_eq!(
            Shell::Bash.set_var("K", "a$b`c\"d"),
            "export K=\"a\\$b\\`c\\\"d\""
        );
    }

    #[test]
    fn test_prepend_path_all_shells() {
        let entries = vec!["E:/x".to_string(), "E:/x/bin".to_string()];
        assert_eq!(
            Shell::Bash.prepend_path(&entries),
            "export PATH=\"E:/x:E:/x/bin:$PATH\""
        );
        assert_eq!(
            Shell::Fish.prepend_path(&entries),
            "set -gx PATH \"E:/x\" \"E:/x/bin\" $PATH"
        );
        if cfg!(windows) {
            assert_eq!(
                Shell::PowerShell.prepend_path(&entries),
                "$env:PATH = 'E:/x;E:/x/bin;' + $env:PATH"
            );
            assert_eq!(
                Shell::Cmd.prepend_path(&entries),
                "set \"PATH=E:/x;E:/x/bin;%PATH%\""
            );
        }
    }

    #[test]
    fn test_prepend_path_empty() {
        assert_eq!(Shell::Bash.prepend_path(&[]), "");
    }

    #[test]
    fn test_fish_path_with_spaces_quoted() {
        let entries = vec!["C:/Users/John Doe/x".to_string()];
        assert_eq!(
            Shell::Fish.prepend_path(&entries),
            "set -gx PATH \"C:/Users/John Doe/x\" $PATH"
        );
    }

    #[test]
    fn test_fmt_path_styles() {
        let p = Path::new(r"E:\a\b");
        assert_eq!(Shell::Bash.fmt_path(p), "E:/a/b");
        assert_eq!(Shell::Zsh.fmt_path(p), "E:/a/b");
        assert_eq!(Shell::Fish.fmt_path(p), "E:/a/b");
        assert_eq!(Shell::Cmd.fmt_path(p), r"E:\a\b");
        assert_eq!(Shell::PowerShell.fmt_path(p), r"E:\a\b");
    }

    #[test]
    fn test_inject_hint_nonempty() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Cmd,
        ] {
            assert!(!shell.inject_hint().is_empty(), "{shell:?}");
        }
    }

    #[test]
    fn test_unset_var_all_shells() {
        assert_eq!(Shell::Bash.unset_var("UVMAN_NODE_VERSION"), "unset UVMAN_NODE_VERSION");
        assert_eq!(Shell::Zsh.unset_var("K"), "unset K");
        assert_eq!(Shell::Fish.unset_var("K"), "set -e K");
        assert_eq!(
            Shell::PowerShell.unset_var("K"),
            "Remove-Item Env:K -ErrorAction SilentlyContinue"
        );
        assert_eq!(Shell::Cmd.unset_var("K"), "set \"K=\"");
    }

    #[test]
    fn test_activation_script_bakes_paths_and_hook() {
        let state = Path::new(r"E:\home\config\tool_current.toml");
        let tools = Path::new(r"E:\home\tools");

        let bash = Shell::Bash.activation_script(state, tools).unwrap();
        assert!(bash.contains("'E:/home/config/tool_current.toml'"), "state baked");
        assert!(bash.contains("'E:/home/tools'"), "tools baked");
        assert!(bash.contains("uvman env --shell bash"));
        // Idempotent guard: skip registration if the hook is already in PROMPT_COMMAND
        assert!(bash.contains("*\";__uvman_hook;\"*)"));

        let zsh = Shell::Zsh.activation_script(state, tools).unwrap();
        assert!(zsh.contains("uvman env --shell zsh"));
        assert!(zsh.contains("precmd_functions[(r)__uvman_hook]"));

        let fish = Shell::Fish.activation_script(state, tools).unwrap();
        assert!(fish.contains("uvman env --shell fish"));
        assert!(fish.contains("--on-event fish_prompt"));

        let pwsh = Shell::PowerShell.activation_script(state, tools).unwrap();
        assert!(pwsh.contains(r"'E:\home\config\tool_current.toml'"), "native path");
        assert!(pwsh.contains("uvman env --shell pwsh"));
        // Guard: don't wrap the original prompt twice
        assert!(pwsh.contains("if (-not $global:__uvman_orig_prompt)"));

        // cmd has no hook point; must error rather than output a script
        assert!(Shell::Cmd.activation_script(state, tools).is_err());
    }
}
