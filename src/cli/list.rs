use crate::core::error::UError;
use crate::core::paths::tools_dir;
use crate::ui::style::ogreen;

/// 语义化版本升序比较（最新在最后）；无法解析时回退字符串序
fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    match (parse_semver(a), parse_semver(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

fn parse_semver(s: &str) -> Option<semver::Version> {
    semver::Version::parse(s.trim_start_matches(['v', 'V'])).ok()
}
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, visible_alias = "ls")]
pub struct List {
    pub tool: Option<String>,

    /// 远端版本针对单个工具，必须搭配 TOOL 使用
    #[clap(short = 'r', long, requires = "tool")]
    pub remote: bool,

    #[clap(short = 'J', long)]
    pub json: bool,
}

impl List {
    pub async fn run(&self) -> crate::Result<()> {
        if self.remote {
            list_remote(self.tool.as_deref(), self.json).await?;
        } else {
            list_local(self.tool.as_deref(), self.json)?;
        }
        Ok(())
    }
}

async fn list_remote(tool: Option<&str>, json: bool) -> Result<(), UError> {
    // 正常情况下 clap(requires = "tool") 已拦截缺省，此处为防御性兜底
    let Some(tool) = tool else {
        return Err(UError::SimpleError(
            "remote listing requires a tool name: uvman list <tool> --remote"
                .into(),
        ));
    };

    let mut versions = crate::toolset::remote_versions(tool).await?;
    // API 原始顺序通常为新版在前，统一改为升序：最新版本最后输出
    versions.sort_by(|a, b| cmp_versions(&a.version, &b.version));

    if json {
        let value = serde_json::json!({ "tool": tool, "versions": versions });
        let json = serde_json::to_string_pretty(&value)
            .map_err(|source| UError::JsonError { source })?;
        println!("{json}");
    } else {
        println!("{}", ogreen(format!("{tool}:")));
        for v in &versions {
            match &v.lts {
                // LTS 线代号（如 node 的 Iron/Jod）标注在版本号后
                Some(codename) => {
                    println!(" - {} (lts: {codename})", v.version)
                },
                None => println!(" - {}", v.version),
            }
        }
    }
    Ok(())
}

fn list_local(tool: Option<&str>, json: bool) -> Result<(), UError> {
    // 缺省时收集全部已安装工具；否则仅收集指定工具
    let tools = match tool {
        Some(t) => vec![(t.to_string(), collect_versions(t)?)],
        None => collect_tools()?,
    };

    if json {
        let value: Vec<serde_json::Value> = tools
            .iter()
            .map(|(name, versions)| serde_json::json!({ "tool": name, "version": versions }))
            .collect();
        let json = serde_json::to_string_pretty(&value)
            .map_err(|source| UError::JsonError { source })?;
        println!("{json}");
    } else {
        for (name, versions) in &tools {
            println!("{}", ogreen(format!("{name}:")));
            for v in versions {
                println!(" - {v}");
            }
        }
    }
    Ok(())
}

/// 收集 tools/ 下所有已安装工具及其版本（按工具名排序）
fn collect_tools() -> Result<Vec<(String, Vec<String>)>, UError> {
    let mut tools = Vec::new();
    let entries = std::fs::read_dir(tools_dir())
        .map_err(|source| UError::IoError { source })?;
    for e in entries {
        let e = e?;
        if e.file_type()?.is_dir()
            && let Some(name) = e.file_name().to_str() {
                tools.push((name.to_string(), collect_versions(name)?));
            }
    }
    tools.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(tools)
}

/// 收集某工具已安装的版本目录（按名排序）。
/// 工具未安装（目录不存在）返回空列表，由调用方决定如何呈现
fn collect_versions(tool: &str) -> Result<Vec<String>, UError> {
    let mut version = Vec::new();
    let entries = match std::fs::read_dir(tools_dir().join(tool)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(version)
        },
        Err(e) => return Err(UError::IoError { source: e }),
    };
    for e in entries {
        let e = e?;
        if e.file_type()?.is_dir()
            && let Some(name) = e.file_name().to_str() {
                version.push(name.to_string());
            }
    }
    version.sort_by(|a, b| cmp_versions(a, b));
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cmp_versions_semver_ascending() {
        // 语义化版本升序：10.x 大于 9.x（字典序会排错）
        let mut v = vec!["10.0.0", "9.1.0", "9.0.1", "v8.0.0"];
        v.sort_by(|a, b| cmp_versions(a, b));
        assert_eq!(v, vec!["v8.0.0", "9.0.1", "9.1.0", "10.0.0"]);
    }

    #[test]
    fn test_cmp_versions_falls_back_to_string() {
        // 非 semver 代号回退字符串序，保持稳定
        let mut v = vec!["beta", "alpha"];
        v.sort_by(|a, b| cmp_versions(a, b));
        assert_eq!(v, vec!["alpha", "beta"]);
        // 混合时非法版本与合法版本间也回退字符串序，不 panic
        assert_eq!(cmp_versions("1.0.0", "alpha"), "1.0.0".cmp("alpha"));
    }
}
