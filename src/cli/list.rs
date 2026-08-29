use serde::Serialize;

use crate::core::current::{self, CurrentTools};
use crate::core::error::UError;
use crate::core::paths::{plugin_path, tools_dir};
use crate::toolset::{self, RemoteVersion};
use crate::ui::report::print_hint;
use crate::ui::style::{odim, ogreen};

/// Human-friendly cap on `--remote` output (newest first); `--all` lifts it.
/// JSON output is never truncated.
const REMOTE_LIMIT: usize = 30;

/// List installed tools and versions, or a tool's remotely available versions.
///
/// Output is newest-first (human and JSON alike). Local listing marks the
/// active version with `(current)`. Remote listing marks `(latest)` /
/// `(lts: <codename>)`, narrows by version prefix
/// (`uvman list node --remote 22`), and caps the human-readable output at the
/// newest 30 entries unless `--all` is passed; JSON is never truncated.
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, visible_alias = "ls")]
pub struct List {
    /// Tool name; omit to list every installed tool
    pub tool: Option<String>,

    /// Version prefix to filter remote versions by (requires --remote)
    #[clap(requires = "remote")]
    pub filter: Option<String>,

    /// List a tool's remotely available versions (requires TOOL)
    #[clap(short = 'r', long, requires = "tool")]
    pub remote: bool,

    /// Show the full remote list instead of the newest N (requires --remote)
    #[clap(short = 'a', long, requires = "remote")]
    pub all: bool,

    /// Emit machine-readable JSON (always full data, never truncated)
    #[clap(short = 'J', long)]
    pub json: bool,
}

impl List {
    pub async fn run(&self) -> crate::Result<()> {
        if self.remote { self.run_remote().await } else { self.run_local() }
    }

    // ---------------- Local listing ----------------

    fn run_local(&self) -> crate::Result<()> {
        match &self.tool {
            Some(name) => Ok(self.list_one_local(name)?),
            None => Ok(self.list_all_local()?),
        }
    }

    /// One tool: versions (newest first) plus the version `use` recorded as
    /// active; an uninstalled tool gets `(none)` and a next-step hint instead
    /// of a silent empty list
    fn list_one_local(&self, name: &str) -> Result<(), UError> {
        let versions = installed_desc(name);
        let currents = current::load();

        if self.json {
            let value = local_json(&[(name.to_string(), versions)], &currents);
            print_json(&value)?;
        } else {
            print_local_section(name, &versions, current_of(&currents, name));
            if versions.is_empty() {
                hint_not_installed(name);
            }
        }
        Ok(())
    }

    /// Every installed tool as its own block, sorted by name
    fn list_all_local(&self) -> Result<(), UError> {
        let tools = collect_installed_tools()?;
        let currents = current::load();

        if self.json {
            let value = local_json(&tools, &currents);
            print_json(&value)?;
        } else if tools.is_empty() {
            println!("{}", odim("no tools installed yet"));
            print_hint("install a tool to get started", &["uvman install node@lts".to_string()]);
        } else {
            for (name, versions) in &tools {
                print_local_section(name, versions, current_of(&currents, name));
            }
        }
        Ok(())
    }

    // ---------------- Remote listing ----------------

    async fn run_remote(&self) -> crate::Result<()> {
        // clap(requires = "tool") normally catches a missing tool; this is a
        // defensive fallback
        let Some(tool) = self.tool.as_deref() else {
            return Err(UError::SimpleError(
                "remote listing requires a tool name: uvman list <tool> --remote".into(),
            )
            .into());
        };

        let mut versions = toolset::remote_versions(tool).await?;
        sort_remote_desc(&mut versions);
        let currents = current::load();
        let current = currents.tools.get(tool).map(|e| e.version.clone());

        if self.json {
            let value = serde_json::json!({ "tool": tool, "versions": versions });
            print_json(&value)?;
            return Ok(());
        }

        // latest/lts are resolved on the full set so they stay correct after
        // prefix filtering
        let latest = versions.first().map(|v| v.version.clone());
        let lts = newest_lts(&versions);

        let selected: Vec<&RemoteVersion> = match &self.filter {
            Some(prefix) => {
                versions.iter().filter(|v| matches_prefix(&v.version, prefix)).collect()
            },
            None => versions.iter().collect(),
        };

        println!("{}", ogreen(format!("{tool}:")));
        if selected.is_empty() {
            match &self.filter {
                Some(prefix) => println!("{}", odim(format!("no versions match '{prefix}'"))),
                None => println!("  (none)"),
            }
            return Ok(());
        }

        let (shown, hidden) = cap_for_display(&selected, self.all);
        for v in shown {
            let markers = markers_for(
                &v.version,
                current.as_deref(),
                latest.as_deref(),
                lts.as_ref().map(|(version, codename)| (version.as_str(), codename.as_str())),
            );
            println!("{}", render_version_line(&v.version, &markers));
        }
        if hidden > 0 {
            let tip = match &self.filter {
                Some(_) => format!("… {hidden} more versions — use --all to show everything"),
                None => format!(
                    "… {hidden} more versions — filter by prefix (uvman list {tool} --remote 22) \
                     or use --all"
                ),
            };
            println!("{}", odim(tip));
        }
        Ok(())
    }
}

// ---------------- Local helpers ----------------

/// One tool's human-readable block: green `name:` header, one line per
/// version, `(none)` when there is nothing installed
fn print_local_section(name: &str, versions: &[String], current: Option<&str>) {
    println!("{}", ogreen(format!("{name}:")));
    if versions.is_empty() {
        println!("  (none)");
        return;
    }
    for version in versions {
        let markers = markers_for(version, current, None, None);
        println!("{}", render_version_line(version, &markers));
    }
}

/// Next-step hint for a tool with nothing installed; the suggestion depends
/// on whether its plugin is available
fn hint_not_installed(name: &str) {
    if plugin_path(name).exists() {
        print_hint(
            &format!("no local version of '{name}' is installed"),
            &[format!("uvman list {name} --remote"), format!("uvman install {name}@lts")],
        );
    } else {
        print_hint(
            &format!("plugin '{name}' is not installed"),
            &[format!("uvman plugin install {name}")],
        );
    }
}

/// All installed tools with their versions (newest first), sorted by name.
/// A missing tools/ dir counts as "nothing installed" — read-only commands
/// must not fail on an uninitialized layout.
fn collect_installed_tools() -> Result<Vec<(String, Vec<String>)>, UError> {
    let entries = match std::fs::read_dir(tools_dir()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(UError::IoError { source }),
    };
    let mut tools = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            tools.push((name.to_string(), installed_desc(name)));
        }
    }
    tools.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(tools)
}

/// Installed versions of one tool, newest first. The directory scan is
/// reused from toolset; re-sorted here so local and remote share one
/// "newest first" definition.
fn installed_desc(name: &str) -> Vec<String> {
    let mut versions = toolset::installed_versions(name);
    versions.sort_by_key(|v| std::cmp::Reverse(desc_key(v)));
    versions
}

// ---------------- Remote helpers ----------------

/// Split the selected entries into (shown, hidden) for human output; the
/// cap applies unless --all. JSON bypasses this entirely.
fn cap_for_display<'a>(
    selected: &'a [&'a RemoteVersion], show_all: bool,
) -> (&'a [&'a RemoteVersion], usize) {
    if show_all || selected.len() <= REMOTE_LIMIT {
        (selected, 0)
    } else {
        (&selected[..REMOTE_LIMIT], selected.len() - REMOTE_LIMIT)
    }
}

/// The `@lts` target: newest entry carrying LTS metadata, as (version,
/// codename). Entries are expected newest-first (see [`sort_remote_desc`]).
fn newest_lts(versions: &[RemoteVersion]) -> Option<(String, String)> {
    versions
        .iter()
        .find_map(|v| v.lts.as_ref().map(|codename| (v.version.clone(), codename.clone())))
}

// ---------------- Shared display helpers ----------------

/// Whether a remote version matches a prefix request (`22` / `22.1`), using
/// the same semantics as `use <tool>@<prefix>`: the request must continue
/// past the version's segments, so request `22` doesn't match version `2`,
/// while `2.0` matches a bare `2`. A leading `v`/`V` is ignored on both
/// sides, so a raw `v20.19.2` (plugins without version_pattern) matches `20`.
fn matches_prefix(version: &str, prefix: &str) -> bool {
    let (version, prefix) = (bare_version(version), bare_version(prefix));
    version == prefix
        || version.starts_with(&format!("{prefix}."))
        || prefix.starts_with(&format!("{version}."))
}

/// Version-line annotations, rendered after the version in parentheses
#[derive(Debug, PartialEq, Eq)]
enum Marker {
    /// Version recorded as active by `use`
    Current,
    /// Newest remote version (the `@latest` target)
    Latest,
    /// Newest LTS release, with its codename
    Lts(String),
}

impl Marker {
    fn label(&self) -> String {
        match self {
            Marker::Current => "current".into(),
            Marker::Latest => "latest".into(),
            Marker::Lts(codename) => format!("lts: {codename}"),
        }
    }
}

/// Markers applying to `version`, given the active/latest/lts references.
/// The latest/lts references are resolved on the full version set before
/// filtering, so a filtered view only annotates versions it actually shows.
/// All comparisons ignore a leading `v`/`V`, so a `20.19.2` current entry
/// still marks a raw `v20.19.2` remote line. Order: user state first, then
/// release-line info.
fn markers_for(
    version: &str, current: Option<&str>, latest: Option<&str>, lts: Option<(&str, &str)>,
) -> Vec<Marker> {
    let bare = bare_version(version);
    let same = |other: &str| bare_version(other) == bare;
    let mut markers = Vec::new();
    if current.is_some_and(same) {
        markers.push(Marker::Current);
    }
    if latest.is_some_and(same) {
        markers.push(Marker::Latest);
    }
    if let Some((lts_version, codename)) = lts
        && same(lts_version)
    {
        markers.push(Marker::Lts(codename.to_string()));
    }
    markers
}

/// One display line: ` - <version>` plus markers (current green, release-line
/// info dim)
fn render_version_line(version: &str, markers: &[Marker]) -> String {
    if markers.is_empty() {
        return format!(" - {version}");
    }
    let labels: Vec<String> = markers
        .iter()
        .map(|m| match m {
            Marker::Current => ogreen(m.label()).to_string(),
            _ => odim(m.label()).to_string(),
        })
        .collect();
    format!(" - {version} ({})", labels.join(", "))
}

/// Version string without its leading v/V; the canonical form for
/// comparisons (prefix filter, markers)
fn bare_version(s: &str) -> &str {
    s.trim_start_matches(['v', 'V'])
}

/// Parse a version string (v/V prefix tolerated); None on failure
fn parse_version(s: &str) -> Option<semver::Version> {
    semver::Version::parse(bare_version(s)).ok()
}

/// Sort remote entries newest-first: parsed semver (prerelease-aware) ranks
/// before unparsable aliases, ties broken by the raw string. Descending so
/// the newest release is the first line.
fn sort_remote_desc(versions: &mut [RemoteVersion]) {
    versions.sort_by_key(|v| std::cmp::Reverse(desc_key(&v.version)));
}

/// Descending sort key: (semver-parseable, parsed version, raw string)
fn desc_key(s: &str) -> (u8, Option<semver::Version>, String) {
    match parse_version(s) {
        Some(v) => (1, Some(v), s.to_string()),
        None => (0, None, s.to_string()),
    }
}

// ---------------- JSON output ----------------

/// JSON payload of one tool: the `versions` array (newest first) plus the
/// version `use` recorded as active (null when none)
#[derive(Serialize)]
struct LocalToolJson {
    tool: String,
    versions: Vec<String>,
    current: Option<String>,
}

/// Build the local-listing JSON from collected tools + the current table
fn local_json(tools: &[(String, Vec<String>)], currents: &CurrentTools) -> serde_json::Value {
    let value: Vec<LocalToolJson> = tools
        .iter()
        .map(|(name, versions)| LocalToolJson {
            tool: name.clone(),
            versions: versions.clone(),
            current: currents.tools.get(name).map(|e| e.version.clone()),
        })
        .collect();
    serde_json::json!(value)
}

fn print_json(value: &serde_json::Value) -> Result<(), UError> {
    let json =
        serde_json::to_string_pretty(value).map_err(|source| UError::JsonError { source })?;
    println!("{json}");
    Ok(())
}

fn current_of<'a>(currents: &'a CurrentTools, tool: &str) -> Option<&'a str> {
    currents.tools.get(tool).map(|e| e.version.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::current::{CurrentEntry, CurrentTools};

    fn rv(version: &str, lts: Option<&str>) -> RemoteVersion {
        RemoteVersion { version: version.into(), lts: lts.map(Into::into) }
    }

    #[test]
    fn test_sort_remote_desc_semver_first() {
        // Newest first; v-prefix parses; prerelease ranks below its release
        let mut v = vec![
            rv("9.0.1", None),
            rv("v8.0.0", None),
            rv("22.0.0-nightly20260101", None),
            rv("22.0.0", None),
            rv("10.0.0", None),
        ];
        sort_remote_desc(&mut v);
        let names: Vec<&str> = v.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(names, vec!["22.0.0", "22.0.0-nightly20260101", "10.0.0", "9.0.1", "v8.0.0"]);
    }

    #[test]
    fn test_sort_remote_desc_unparsable_last() {
        // Aliases that don't parse (beta/alpha) rank after real versions;
        // among themselves they fall back to lexicographic order
        let mut v = vec![rv("beta", None), rv("1.0.0", None), rv("alpha", None)];
        sort_remote_desc(&mut v);
        let names: Vec<&str> = v.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(names, vec!["1.0.0", "beta", "alpha"]);
    }

    #[test]
    fn test_latest_and_lts_after_desc_sort() {
        let mut v =
            vec![rv("26.7.0", None), rv("22.12.0", Some("Jod")), rv("20.11.0", Some("Iron"))];
        sort_remote_desc(&mut v);
        // Newest overall = first entry of the descending list (`@latest`)
        assert_eq!(v.first().map(|r| r.version.as_str()), Some("26.7.0"));
        let (version, codename) = newest_lts(&v).unwrap();
        assert_eq!((version.as_str(), codename.as_str()), ("22.12.0", "Jod"));
    }

    #[test]
    fn test_matches_prefix() {
        assert!(matches_prefix("22.0.0", "22"));
        assert!(matches_prefix("22.19.0", "22.19"));
        assert!(matches_prefix("22", "22"));
        // Request continues past the version: 2.0 matches a bare 2
        assert!(matches_prefix("2", "2.0"));
        // Request 22 must not match version 2
        assert!(!matches_prefix("2.0.0", "22"));
        assert!(!matches_prefix("22.0.0", "23"));
        // A leading v/V on either side is ignored (raw plugin data)
        assert!(matches_prefix("v20.19.2", "20"));
        assert!(matches_prefix("20.19.2", "v20"));
    }

    #[test]
    fn test_markers_for_order_and_content() {
        // User state first, then release-line info
        let m = markers_for("22.19.0", Some("22.19.0"), Some("22.19.0"), Some(("22.19.0", "Jod")));
        assert_eq!(m, vec![Marker::Current, Marker::Latest, Marker::Lts("Jod".into())]);

        // v/V prefix differences don't hide a marker (raw remote data)
        let m = markers_for("v20.19.2", Some("20.19.2"), None, None);
        assert_eq!(m, vec![Marker::Current]);

        // A plain version gets no markers
        let m = markers_for("20.11.0", Some("22.19.0"), Some("24.19.0"), Some(("22.19.0", "Jod")));
        assert!(m.is_empty());
    }

    #[test]
    fn test_marker_labels() {
        assert_eq!(Marker::Current.label(), "current");
        assert_eq!(Marker::Latest.label(), "latest");
        assert_eq!(Marker::Lts("Jod".into()).label(), "lts: Jod");
    }

    #[test]
    fn test_cap_for_display_truncates_newest() {
        let mut owned: Vec<RemoteVersion> =
            (0..REMOTE_LIMIT + 5).map(|i| rv(&format!("1.0.{i}"), None)).collect();
        sort_remote_desc(&mut owned);
        let selected: Vec<&RemoteVersion> = owned.iter().collect();

        let (shown, hidden) = cap_for_display(&selected, false);
        // Newest first: the head of the list is kept, the tail is hidden
        assert_eq!(shown.len(), REMOTE_LIMIT);
        assert_eq!(hidden, 5);
        assert_eq!(shown[0].version, "1.0.34");

        // --all lifts the cap
        let (shown, hidden) = cap_for_display(&selected, true);
        assert_eq!(shown.len(), REMOTE_LIMIT + 5);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn test_local_json_shape() {
        // The array field is `versions` (plural), newest first, with the
        // active version alongside; a tool without an active version is null
        let tools = vec![
            ("node".to_string(), vec!["22.19.0".to_string(), "20.19.2".to_string()]),
            ("go".to_string(), vec!["1.23.4".to_string()]),
        ];
        let mut currents = CurrentTools::default();
        currents.tools.insert("node".into(), CurrentEntry { version: "22.19.0".into() });

        let value = local_json(&tools, &currents);
        assert_eq!(value[0]["tool"], "node");
        assert_eq!(value[0]["versions"][0], "22.19.0");
        assert_eq!(value[0]["versions"][1], "20.19.2");
        assert_eq!(value[0]["current"], "22.19.0");
        assert_eq!(value[1]["tool"], "go");
        assert!(value[1]["current"].is_null());
    }
}
