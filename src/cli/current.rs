//! `uvman current`: read-only query of the globally active tool versions.
//!
//! Versions resolve through the shared core entry (`core::resolve`), the same
//! lookup `which` and `env` use; a version whose deploy dir was deleted by
//! hand reads back as not current. Nothing here writes state, so a
//! missing/corrupt table or an absent tool degrades to `none` with exit 0
//! instead of an error.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::Result;
use crate::core::current::{self, CurrentTools};
use crate::core::error::UError;
use crate::core::paths::tools_dir;
use crate::core::resolve::{self, Scope};
use crate::ui::report::print_hint;
use crate::ui::style::odim;

/// Print the currently active version of each tool, or of one tool.
///
/// With no argument every active tool is listed (name-sorted, columns
/// aligned); naming a tool prints only that one. A tool without an active
/// version — or nothing active at all — prints `none` and exits 0: a read-only
/// query must not fail. `--json` prints the same data as
/// `{ "<tool>": { "version": "…", "scope": "global" } }`, an empty object when
/// nothing is active.
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct Current {
    /// Tool name; omit to show every active tool
    pub tool: Option<String>,

    /// Emit machine-readable JSON
    #[clap(short = 'J', long)]
    pub json: bool,
}

impl Current {
    pub fn run(&self) -> Result<()> {
        let table = current::load();
        let tools_root = tools_dir();
        if self.json {
            self.print_json(&table, &tools_root)
        } else {
            self.print_human(&table, &tools_root);
            Ok(())
        }
    }

    /// Human mode: one aligned line per active tool, `none` plus a next-step
    /// hint when the answer is empty (hints go to stderr and honor --quiet)
    fn print_human(&self, table: &CurrentTools, tools_root: &Path) {
        let rows = report_rows(table, tools_root, self.tool.as_deref());
        if rows.is_empty() {
            println!("{}", odim("none"));
            match &self.tool {
                Some(name) => {
                    print_hint(
                        &format!("no active version of '{name}'"),
                        &[format!("uvman list {name}")],
                    );
                },
                None => {
                    print_hint(
                        "no tools are active yet",
                        &["uvman install node@lts".to_string(), "uvman use node@lts".to_string()],
                    );
                },
            }
            return;
        }
        let width = rows.iter().map(|(name, ..)| name.len()).max().unwrap_or(0);
        for (name, version, scope) in rows {
            println!("{}", render_line(name, version, scope, width));
        }
    }

    /// JSON mode: stdout carries only the document; nothing else is emitted
    fn print_json(&self, table: &CurrentTools, tools_root: &Path) -> Result<()> {
        let document = json_document(table, tools_root, self.tool.as_deref());
        let json = serde_json::to_string_pretty(&document)
            .map_err(|source| UError::JsonError { source })?;
        println!("{json}");
        Ok(())
    }
}

/// Whether a resolved version is actually deployed: its version dir exists
/// under the tools root. A dir deleted by hand counts as "no such version" —
/// read-only commands report, never repair state (`which` errors, `env` and
/// `current` skip) — so the stale activation record is not surfaced.
fn deployed(tools_root: &Path, tool: &str, version: &str) -> bool {
    tools_root.join(tool).join(version).is_dir()
}

/// The rows to report, already narrowed by the tool argument and name-sorted
/// (BTreeMap iteration); every row resolves through the shared core entry and
/// drops versions missing on disk. Empty = nothing active for the requested
/// view.
fn report_rows<'a>(
    table: &'a CurrentTools, tools_root: &Path, tool: Option<&'a str>,
) -> Vec<(&'a str, &'a str, Scope)> {
    match tool {
        Some(name) => resolve::from_table(table, name)
            .filter(|(version, _)| deployed(tools_root, name, version))
            .map(|(version, scope)| vec![(name, version, scope)])
            .unwrap_or_default(),
        None => table
            .tools
            .keys()
            .filter_map(|name| {
                resolve::from_table(table, name)
                    .filter(|(version, _)| deployed(tools_root, name, version))
                    .map(|(version, scope)| (name.as_str(), version, scope))
            })
            .collect(),
    }
}

/// One human line: name padded to `width` so a multi-tool listing aligns —
/// `node  22.19.0 (global)`
fn render_line(name: &str, version: &str, scope: Scope, width: usize) -> String {
    format!("{:<width$}  {} {}", name, version, odim(format!("({})", scope.label())))
}

/// One active version as JSON: `{ "version": …, "scope": … }`
#[derive(Serialize)]
struct ActiveVersion<'a> {
    version: &'a str,
    scope: Scope,
}

/// The `--json` document: every active tool keyed by name; a named tool
/// narrows it to that entry, and nothing active is an empty object
fn json_document<'a>(
    table: &'a CurrentTools, tools_root: &Path, tool: Option<&'a str>,
) -> BTreeMap<&'a str, ActiveVersion<'a>> {
    match tool {
        Some(name) => resolve::from_table(table, name)
            .filter(|(version, _)| deployed(tools_root, name, version))
            .map(|(version, scope)| (name, ActiveVersion { version, scope }))
            .into_iter()
            .collect(),
        None => table
            .tools
            .keys()
            .filter_map(|name| {
                resolve::from_table(table, name)
                    .filter(|(version, _)| deployed(tools_root, name, version))
                    .map(|(version, scope)| (name.as_str(), ActiveVersion { version, scope }))
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::current::CurrentEntry;

    fn table_with(pairs: &[(&str, &str)]) -> CurrentTools {
        let mut table = CurrentTools::default();
        for (name, version) in pairs {
            table.tools.insert(name.to_string(), CurrentEntry { version: version.to_string() });
        }
        table
    }

    #[test]
    fn test_report_rows_narrows_and_sorts() {
        let root = tempfile::tempdir().unwrap();
        for dir in ["go/1.23.0", "node/22.19.0"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let table = table_with(&[("node", "22.19.0"), ("go", "1.23.0")]);

        // No argument: every active tool, name-sorted
        let rows = report_rows(&table, root.path(), None);
        assert_eq!(rows, vec![("go", "1.23.0", Scope::Global), ("node", "22.19.0", Scope::Global)]);

        // A named tool narrows to exactly that row
        assert_eq!(
            report_rows(&table, root.path(), Some("node")),
            vec![("node", "22.19.0", Scope::Global)]
        );

        // Unknown tool / empty table: nothing to report
        assert!(report_rows(&table, root.path(), Some("python")).is_empty());
        assert!(report_rows(&CurrentTools::default(), root.path(), None).is_empty());
    }

    #[test]
    fn test_report_rows_skip_deleted_version_dir() {
        // `go` is active in the table but its deploy dir was deleted by hand:
        // read-only commands report "no such version", they never repair state
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("node/22.19.0")).unwrap();
        let table = table_with(&[("node", "22.19.0"), ("go", "1.23.0")]);

        // The listing skips it; the named query reads back as none / empty JSON
        let rows = report_rows(&table, root.path(), None);
        assert_eq!(rows, vec![("node", "22.19.0", Scope::Global)]);
        assert!(report_rows(&table, root.path(), Some("go")).is_empty());

        let json = serde_json::to_value(json_document(&table, root.path(), Some("go"))).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 0);
    }

    #[test]
    fn test_render_line_matches_contract() {
        // The documented example: two spaces between name and version
        assert_eq!(render_line("node", "22.19.0", Scope::Global, 4), "node  22.19.0 (global)");
        // The name column pads to `width` so listings columns-align
        assert_eq!(render_line("go", "1.23.0", Scope::Global, 4), "go    1.23.0 (global)");
    }

    #[test]
    fn test_json_document_shape() {
        let root = tempfile::tempdir().unwrap();
        for dir in ["go/1.23.0", "node/22.19.0"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let table = table_with(&[("go", "1.23.0"), ("node", "22.19.0")]);

        // All tools: one object per active tool carrying version + scope
        let value = serde_json::to_value(json_document(&table, root.path(), None)).unwrap();
        assert_eq!(value["node"]["version"], "22.19.0");
        assert_eq!(value["node"]["scope"], "global");
        assert_eq!(value["go"]["version"], "1.23.0");

        // A named tool narrows the document to that entry
        let value = serde_json::to_value(json_document(&table, root.path(), Some("node"))).unwrap();
        assert_eq!(value["node"]["version"], "22.19.0");
        assert!(value.get("go").is_none());

        // Nothing active is an empty object, never an error
        let empty = serde_json::to_value(json_document(&table, root.path(), Some("python")))
            .unwrap()
            .as_object()
            .unwrap()
            .len();
        assert_eq!(empty, 0);
        let empty = serde_json::to_value(json_document(&CurrentTools::default(), root.path(), None))
            .unwrap()
            .as_object()
            .unwrap()
            .len();
        assert_eq!(empty, 0);
    }
}
