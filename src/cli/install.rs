use std::fs;

use crate::Result;
use crate::core::error::UError;
use crate::ui::style;

/// Install a tool at a specific version
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, visible_alias = "i")]
pub struct Install {
    /// Tool and version to install, in the form `tool@version`
    ///
    /// Omit the version to install the plugin's default version.
    /// e.g.: `node@20.11.0`, `node`, `node@22`, `node@latest`
    pub tool_spec: String,

    /// Reinstall even if the version is already present
    #[clap(long, short = 'f')]
    pub force: bool,
}

impl Install {
    pub async fn run(&self) -> Result<()> {
        let (tool, version) = parse_spec(&self.tool_spec)?;

        let plan = crate::toolset::plan(&tool, version.as_deref()).await?;

        // 已安装且未 --force 时阻止重复安装
        if plan.install_dir.exists() && !self.force {
            return Err(UError::AlreadyInstalled {
                tool: tool.clone(),
                version: plan.version.clone(),
            }
            .into());
        }

        println!(
            "{}",
            style::ogreen(format!(
                "Installing {}@{} ...",
                plan.name, plan.version
            ))
        );

        // --force 覆盖：旧目录先改名备份而非直接删除；安装失败时
        // 回滚，避免"旧的已删、新的没装"导致机器上什么都不剩
        let backup = if plan.install_dir.exists() {
            let mut backup_name = plan.install_dir.clone().into_os_string();
            backup_name.push(".uvman_bak");
            let backup = std::path::PathBuf::from(backup_name);
            // 上一次失败可能留下残留备份，先清掉
            let _ = fs::remove_dir_all(&backup);
            fs::rename(&plan.install_dir, &backup)?;
            Some(backup)
        } else {
            None
        };

        match crate::toolset::execute(&plan).await {
            Ok(()) => {
                if let Some(backup) = &backup {
                    let _ = fs::remove_dir_all(backup);
                }
                println!(
                    "{}",
                    style::ogreen(format!(
                        "Installed {}@{} to {}",
                        plan.name,
                        plan.version,
                        plan.install_dir.display()
                    ))
                );
                Ok(())
            },
            Err(e) => {
                // 清理半成品安装目录；有备份则回滚旧版本
                let _ = fs::remove_dir_all(&plan.install_dir);
                if let Some(backup) = &backup
                    && fs::rename(backup, &plan.install_dir).is_err()
                {
                    crate::ui::report::print_warning(&format!(
                        "failed to restore previous installation from {}; \
                         recover it manually",
                        backup.display()
                    ));
                }
                Err(e.into())
            },
        }
    }
}

/// 解析 `tool@version` 规范，返回 `(tool, version)`。
///
/// - 未指定版本（`node`）时返回 `None`，由插件默认版本决定
/// - 指定版本可为具体版本号（`20.11.0`）、部分版本（`22`）或代号（`latest`/
///   `lts`/`nightly`）； 代号到具体版本的解析发生在 `toolset::plan`
///   阶段（需要插件 release 数据）
pub(crate) fn parse_spec(spec: &str) -> Result<(String, Option<String>), UError> {
    let (tool, version) = match spec.split_once('@') {
        Some((t, v)) => (t, v),
        None => (spec, ""),
    };
    if tool.is_empty() {
        return Err(UError::SimpleError(format!(
            "invalid tool spec '{spec}': missing tool name"
        )));
    }
    let version =
        if version.is_empty() { None } else { Some(version.to_string()) };
    Ok((tool.to_string(), version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_spec_no_version() {
        let (tool, version) = parse_spec("node").unwrap();
        assert_eq!(tool, "node");
        assert_eq!(version, None);
    }

    #[test]
    fn test_parse_spec_full_version() {
        let (tool, version) = parse_spec("node@20.11.0").unwrap();
        assert_eq!(tool, "node");
        assert_eq!(version.as_deref(), Some("20.11.0"));
    }

    #[test]
    fn test_parse_spec_alias() {
        let (_, version) = parse_spec("node@latest").unwrap();
        assert_eq!(version.as_deref(), Some("latest"));
        let (_, version) = parse_spec("node@lts").unwrap();
        assert_eq!(version.as_deref(), Some("lts"));
        let (_, version) = parse_spec("node@nightly").unwrap();
        assert_eq!(version.as_deref(), Some("nightly"));
    }

    #[test]
    fn test_parse_spec_empty_tool() {
        assert!(parse_spec("@20.0.0").is_err());
        assert!(parse_spec("").is_err());
    }
}
