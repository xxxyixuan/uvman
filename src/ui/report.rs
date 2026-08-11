//! 统一的诊断输出（error / warning / hint）。
//!
//! 设计参考 uv 与 mise：
//! - `error:` 一句话陈述事实
//! - `hint:` 修复建议；可复制的命令独立成行（缩进两空格、绿色高亮）
//! - 底层系统错误（网络/IO 等）追加 dim footer，告知 `--verbose` 自助深挖出口
//! - 用法错误退出码 2，一般错误 1

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use console::StyledObject;

use crate::core::error::UError;
use crate::ui::style::{ecyan, egreen, ered, eyellow};

/// 全局 verbose 级别（由 CLI --verbose / -v 写入）
static VERBOSE: AtomicU8 = AtomicU8::new(0);

/// 全局 quiet 开关（由 CLI --quiet / -q 写入），抑制非错误输出
static QUIET: AtomicBool = AtomicBool::new(false);

/// 颜色总开关（NO_COLOR / 测试环境可关闭）
static COLOR: AtomicBool = AtomicBool::new(true);

pub fn set_verbose(level: u8) {
    VERBOSE.store(level, Ordering::Relaxed);
}

pub fn verbose() -> u8 {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn set_quiet(enabled: bool) {
    QUIET.store(enabled, Ordering::Relaxed);
}

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

pub fn set_color(enabled: bool) {
    COLOR.store(enabled, Ordering::Relaxed);
}

fn color_enabled() -> bool {
    COLOR.load(Ordering::Relaxed)
}

/// 输出错误消息、修复建议与退出码
pub fn print_error(err: &UError) -> u8 {
    print_prefixed(ered("error:"), &err.to_string());

    if let Some(hint) = err.hint() {
        print_prefixed(ecyan("hint:"), &hint.message);
        for cmd in &hint.commands {
            // 命令独立成行、缩进两空格、绿色，方便用户整行选中复制
            eprintln!("  {}", styled_code(cmd));
        }
    }

    // mise 风格 footer：底层错误告知自助调试出口（hint
    // 已能解决的语义错误不打扰）
    if err.has_source() && verbose() == 0 {
        eprintln!(
            "{}",
            crate::ui::style::estyle("run with --verbose for more details")
                .dim()
        );
    }
    err.exit_code()
}

/// 输出裸错误消息（非 UError 场景）
pub fn print_error_message(msg: &str) -> u8 {
    print_prefixed(ered("error:"), msg);
    1
}

/// 输出警告
pub fn print_warning(msg: &str) {
    if quiet() {
        return;
    }
    print_prefixed(eyellow("warning:"), msg);
}

/// 输出提示信息（非错误场景的修复建议，命令可整行复制）
pub fn print_hint(message: &str, commands: &[String]) {
    if quiet() {
        return;
    }
    print_prefixed(ecyan("hint:"), message);
    for cmd in commands {
        eprintln!("  {}", styled_code(cmd));
    }
}

fn print_prefixed(prefix: StyledObject<&str>, msg: &str) {
    eprint!("{} ", prefix.bold());
    render_backticks(msg);
}

/// 按反引号切分：反引号内的片段绿色高亮
fn render_backticks(msg: &str) {
    if !color_enabled() {
        eprintln!("{msg}");
        return;
    }
    let mut in_code = false;
    for seg in msg.split('`') {
        if in_code {
            eprint!("{}", egreen(seg));
        } else {
            eprint!("{seg}");
        }
        in_code = !in_code;
    }
    eprintln!();
}

fn styled_code(cmd: &str) -> String {
    if color_enabled() { egreen(cmd).to_string() } else { cmd.to_string() }
}
