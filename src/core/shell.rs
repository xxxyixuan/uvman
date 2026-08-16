//! 目标 Shell 渲染策略（设计见 .tmp/dev-docs/uvman-env-design.md）。
//!
//! 职责：把平台无关的键值输出为指定 Shell 可直接求值的语句，
//! 以及生成 activate 激活脚本（设计见 uvman-activate-design.md）。
//! 无状态、无 IO；检测（detect）只读环境变量。

use std::path::Path;

/// `uvman env` 的输出目标 Shell
// PowerShell 是领域固有名，无法避开 Shell 后缀
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Shell {
    /// bash / Git Bash / 兼容 POSIX sh
    #[value(alias = "sh")]
    Bash,
    /// zsh
    Zsh,
    /// fish
    Fish,
    /// Windows PowerShell（含 pwsh 7+）
    #[value(name = "pwsh", alias = "powershell")]
    PowerShell,
    /// Windows cmd.exe
    #[value(alias = "cmd.exe", alias = "bat")]
    Cmd,
}

impl Shell {
    /// 环境推断：显式 --shell 之外的缺省来源。
    ///
    /// Windows：pwsh 系始终设置 PSModulePath，cmd 不会；
    /// Unix：$SHELL 取 basename，未知（dash/sh 等）归一 Bash。
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

    /// 渲染单条变量设置（含转义与路径风格处理由调用方决定：
    /// value 已是最终字符串，路径请先用 fmt_path 转换）
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
                // 引号形式避免尾随空格与 & 等特殊字符问题
                format!("set \"{key}={value}\"")
            }
        }
    }

    /// 渲染 PATH 前插语句（uvman 条目优先于系统版本）。
    /// 对已有 PATH 的引用（$PATH 等）由各 Shell 语法自身展开，不参与转义。
    pub fn prepend_path(&self, entries: &[String]) -> String {
        if entries.is_empty() {
            return String::new();
        }
        match self {
            Shell::Bash | Shell::Zsh => {
                format!("export PATH=\"{}:$PATH\"", sh_escape(&entries.join(":")))
            }
            Shell::Fish => {
                // fish 的 PATH 是 list：元素独立引用，空格安全
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

    /// `use` 命令成功后的刷新指引（按检测 Shell 给出可复制命令）
    pub fn inject_hint(&self) -> Vec<String> {
        match self {
            Shell::Bash | Shell::Zsh => {
                vec!["eval \"$(uvman env)\"".to_string()]
            }
            Shell::Fish => vec!["uvman env | source".to_string()],
            Shell::PowerShell => vec!["uvman env | iex".to_string()],
            Shell::Cmd => {
                // for /f 逐行执行输出；交互式用 %i（写入 .bat 时才需 %%i）
                vec!["for /f \"delims=\" %i in ('uvman env --shell cmd') do %i"
                    .to_string()]
            }
        }
    }

    /// PATH 列表分隔符
    fn path_sep(&self) -> char {
        match self {
            Shell::Bash | Shell::Zsh | Shell::Fish => ':',
            // pwsh 跨平台存在，跟随宿主 OS
            Shell::PowerShell | Shell::Cmd => {
                if cfg!(windows) {
                    ';'
                } else {
                    ':'
                }
            }
        }
    }

    /// 路径风格：POSIX 家族恒用 `/`（Git Bash 接受 E:/x 且避免 \ 转义歧义）；
    /// pwsh/cmd 跟随 OS 原生
    pub fn fmt_path(&self, path: &Path) -> String {
        let s = path.to_string_lossy();
        match self {
            Shell::Bash | Shell::Zsh | Shell::Fish => s.replace('\\', "/"),
            Shell::PowerShell | Shell::Cmd => s.to_string(),
        }
    }

    /// 渲染 unset 语句（清理已失效的 UVMAN_* 变量）
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

    /// 生成 activate 激活脚本（设计见 uvman-activate-design.md 4.x）。
    ///
    /// 脚本烘焙 state/tools 的绝对路径；结构：烘焙常量 →
    /// 刷新函数（strip-then-eval）→ prompt 钩子（mtime 快路径）→
    /// 注册（幂等守卫）→ 激活时立即刷新一次。
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
            // cmd 无 prompt 钩子点，由 activate 命令层拦截并说明替代方案
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

/// POSIX 双引号内转义（`\` `"` `` ` `` `$`），保证 eval 时值不被二次展开
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
        // 幂等守卫：PROMPT_COMMAND 已含钩子时跳过注册
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
        // 守卫：不二次包裹原 prompt
        assert!(pwsh.contains("if (-not $global:__uvman_orig_prompt)"));

        // cmd 无钩子点，必须报错而非输出脚本
        assert!(Shell::Cmd.activation_script(state, tools).is_err());
    }
}
