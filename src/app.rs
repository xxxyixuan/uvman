use crate::core::config;
use crate::core::paths;
use crate::{Lazy, Result};

pub(crate) fn init() -> Result<()> {
    // Initialize the color_eyre error reporting library
    color_eyre::install()?;

    // Bootstrap: ensure UVMAN_HOME directory layout and default config exist.
    // 创建失败仅告警不阻断，保证只读命令（version/help 等）在只读环境仍可用；
    // 写操作命令自身会对目标目录二次校验并报错。
    if let Err(e) = paths::ensure_layout() {
        crate::ui::report::print_warning(&format!(
            "failed to create UVMAN_HOME directory layout: {e}"
        ));
    }
    if let Err(e) = config::ensure_default_config() {
        crate::ui::report::print_warning(&format!(
            "failed to create default config file: {e}"
        ));
    }

    // Initialize the global configuration
    Lazy::force(&config::GLOBAL_CONFIG);

    Ok(())
}
