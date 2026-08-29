use crate::core::error::UError;
use crate::core::paths::tools_dir;
use crate::ui::style::ogreen;

/// Semantic-version ascending comparison (newest last); falls back to
/// lexicographic order when a version cannot be parsed
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

    /// Remote listing targets a single tool, so it requires TOOL
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
    // clap(requires = "tool") normally catches a missing tool; this is a
    // defensive fallback
    let Some(tool) = tool else {
        return Err(UError::SimpleError(
            "remote listing requires a tool name: uvman list <tool> --remote".into(),
        ));
    };

    let mut versions = crate::toolset::remote_versions(tool).await?;
    // The API returns newest-first; re-sort ascending so the latest is last
    versions.sort_by(|a, b| cmp_versions(&a.version, &b.version));

    if json {
        let value = serde_json::json!({ "tool": tool, "versions": versions });
        let json =
            serde_json::to_string_pretty(&value).map_err(|source| UError::JsonError { source })?;
        println!("{json}");
    } else {
        println!("{}", ogreen(format!("{tool}:")));
        for v in &versions {
            match &v.lts {
                // LTS-line codename (e.g. node's Iron/Jod) follows the version
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
    // Collect all installed tools by default; otherwise only the given tool
    let tools = match tool {
        Some(t) => vec![(t.to_string(), collect_versions(t)?)],
        None => collect_tools()?,
    };

    if json {
        let value: Vec<serde_json::Value> = tools
            .iter()
            .map(|(name, versions)| serde_json::json!({ "tool": name, "version": versions }))
            .collect();
        let json =
            serde_json::to_string_pretty(&value).map_err(|source| UError::JsonError { source })?;
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

/// Collect every installed tool and its versions from tools/ (sorted by name)
fn collect_tools() -> Result<Vec<(String, Vec<String>)>, UError> {
    let mut tools = Vec::new();
    let entries = std::fs::read_dir(tools_dir()).map_err(|source| UError::IoError { source })?;
    for e in entries {
        let e = e?;
        if e.file_type()?.is_dir()
            && let Some(name) = e.file_name().to_str()
        {
            tools.push((name.to_string(), collect_versions(name)?));
        }
    }
    tools.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(tools)
}

/// Collect the installed version dirs of a tool (sorted by name).
/// Returns empty for an uninstalled tool (missing dir); callers decide
/// how to present it
fn collect_versions(tool: &str) -> Result<Vec<String>, UError> {
    let mut version = Vec::new();
    let entries = match std::fs::read_dir(tools_dir().join(tool)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(version),
        Err(e) => return Err(UError::IoError { source: e }),
    };
    for e in entries {
        let e = e?;
        if e.file_type()?.is_dir()
            && let Some(name) = e.file_name().to_str()
        {
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
        // Semver ascending: 10.x sorts above 9.x (lexicographic would mis-sort)
        let mut v = vec!["10.0.0", "9.1.0", "9.0.1", "v8.0.0"];
        v.sort_by(|a, b| cmp_versions(a, b));
        assert_eq!(v, vec!["v8.0.0", "9.0.1", "9.1.0", "10.0.0"]);
    }

    #[test]
    fn test_cmp_versions_falls_back_to_string() {
        // Non-semver aliases fall back to lexicographic order, staying stable
        let mut v = vec!["beta", "alpha"];
        v.sort_by(|a, b| cmp_versions(a, b));
        assert_eq!(v, vec!["alpha", "beta"]);
        // Mixed valid/invalid still fall back to lexicographic, no panic
        assert_eq!(cmp_versions("1.0.0", "alpha"), "1.0.0".cmp("alpha"));
    }
}
