//! `uvman current`: read-only query of the globally active tool versions.
//!
//! The answer comes straight from the activation table `use` maintains;
//! nothing here writes state, so a missing/corrupt table or an absent tool
//! degrades to `none` with exit 0 instead of an error.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::Result;
use crate::core::current::{self, CurrentTools};
use crate::core::error::UError;
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
        if self.json {
            self.print_json(&table)
        } else {
            self.print_human(&table);
            Ok(())
        }
    }

    /// Human mode: one aligned line per active tool, `none` plus a next-step
    /// hint when the answer is empty (hints go to stderr and honor --quiet)
    fn print_human(&self, table: &CurrentTools) {
        let rows = report_rows(table, self.tool.as_deref());
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
    fn print_json(&self, table: &CurrentTools) -> Result<()> {
        let document = json_document(table, self.tool.as_deref());
        let json = serde_json::to_string_pretty(&document)
            .map_err(|source| UError::JsonError { source })?;
        println!("{json}");
        Ok(())
    }
}

/// Where a reported active version comes from. 0.2.0 only has the global
/// activation table; the type pins the output contract (the human `(global)`
/// suffix and the JSON `scope` field) so 0.5.0 can introduce project scope
/// without changing what commands print.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Scope {
    Global,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Scope::Global => "global",
        }
    }
}

/// One tool's active version and its scope — the resolution layer of this
/// release: global activation state, then "none". 0.5.0 inserts the project
/// level ahead of it and hoists this into a `core` entry point shared with
/// `which` / `env` (0.2.0 plan, task 5).
fn resolve<'a>(table: &'a CurrentTools, tool: &str) -> Option<(&'a str, Scope)> {
    table.tools.get(tool).map(|entry| (entry.version.as_str(), Scope::Global))
}

/// The rows to report, already narrowed by the tool argument and name-sorted
/// (BTreeMap iteration). Empty = nothing active for the requested view.
fn report_rows<'a>(
    table: &'a CurrentTools, tool: Option<&'a str>,
) -> Vec<(&'a str, &'a str, Scope)> {
    match tool {
        Some(name) => resolve(table, name)
            .map(|(version, scope)| vec![(name, version, scope)])
            .unwrap_or_default(),
        None => table
            .tools
            .iter()
            .map(|(name, entry)| (name.as_str(), entry.version.as_str(), Scope::Global))
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
    table: &'a CurrentTools, tool: Option<&'a str>,
) -> BTreeMap<&'a str, ActiveVersion<'a>> {
    match tool {
        Some(name) => resolve(table, name)
            .map(|(version, scope)| (name, ActiveVersion { version, scope }))
            .into_iter()
            .collect(),
        None => table
            .tools
            .iter()
            .map(|(name, entry)| {
                (
                    name.as_str(),
                    ActiveVersion { version: entry.version.as_str(), scope: Scope::Global },
                )
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
    fn test_resolve_reports_global_scope() {
        let table = table_with(&[("node", "22.19.0")]);
        assert_eq!(resolve(&table, "node"), Some(("22.19.0", Scope::Global)));
        // A tool without an entry resolves to "none"
        assert_eq!(resolve(&table, "go"), None);
    }

    #[test]
    fn test_report_rows_narrows_and_sorts() {
        let table = table_with(&[("node", "22.19.0"), ("go", "1.23.0")]);

        // No argument: every active tool, name-sorted
        let rows = report_rows(&table, None);
        assert_eq!(rows, vec![("go", "1.23.0", Scope::Global), ("node", "22.19.0", Scope::Global)]);

        // A named tool narrows to exactly that row
        assert_eq!(report_rows(&table, Some("node")), vec![("node", "22.19.0", Scope::Global)]);

        // Unknown tool / empty table: nothing to report
        assert!(report_rows(&table, Some("python")).is_empty());
        assert!(report_rows(&CurrentTools::default(), None).is_empty());
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
        let table = table_with(&[("go", "1.23.0"), ("node", "22.19.0")]);

        // All tools: one object per active tool carrying version + scope
        let value = serde_json::to_value(json_document(&table, None)).unwrap();
        assert_eq!(value["node"]["version"], "22.19.0");
        assert_eq!(value["node"]["scope"], "global");
        assert_eq!(value["go"]["version"], "1.23.0");

        // A named tool narrows the document to that entry
        let value = serde_json::to_value(json_document(&table, Some("node"))).unwrap();
        assert_eq!(value["node"]["version"], "22.19.0");
        assert!(value.get("go").is_none());

        // Nothing active is an empty object, never an error
        let empty = serde_json::to_value(json_document(&table, Some("python")))
            .unwrap()
            .as_object()
            .unwrap()
            .len();
        assert_eq!(empty, 0);
        let empty = serde_json::to_value(json_document(&CurrentTools::default(), None))
            .unwrap()
            .as_object()
            .unwrap()
            .len();
        assert_eq!(empty, 0);
    }
}
