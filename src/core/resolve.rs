//! Single version-resolution entry point (0.2.0 plan, task 4): the one place
//! that answers "which version of a tool is active". `current` / `which` /
//! `env` all resolve through here, so 0.5.0's project-level scope lands as a
//! change to the lookup body alone — the three commands stay untouched.

use serde::Serialize;

use crate::core::current::{self, CurrentTools};

/// Where a resolved version is active.
///
/// This release resolves from the global activation table only; the type pins
/// the resolution contract (the human `(global)` suffix, the JSON `scope`
/// field) so 0.5.0 can slot the project level ahead of it without changing
/// what commands print.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Global,
}

impl Scope {
    /// Human-facing suffix label: `(global)`
    pub fn label(self) -> &'static str {
        match self {
            Scope::Global => "global",
        }
    }
}

/// Resolve one tool's active version and scope against a loaded table.
///
/// Lookup order this release: global activation state, then none. The disk is
/// deliberately not consulted — an active version whose directory was deleted
/// by hand still resolves here; commands apply their own policy on top
/// (`which` errors, `current` / `env` skip).
pub fn from_table<'a>(table: &'a CurrentTools, tool: &str) -> Option<(&'a str, Scope)> {
    table.tools.get(tool).map(|entry| (entry.version.as_str(), Scope::Global))
}

/// Resolve against the default global table, owning the version — for
/// single-tool callers that don't hold a table.
pub fn current_version(tool: &str) -> Option<(String, Scope)> {
    from_table(&current::load(), tool).map(|(version, scope)| (version.to_string(), scope))
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
    fn test_from_table_reports_global_scope() {
        let table = table_with(&[("node", "22.19.0")]);
        assert_eq!(from_table(&table, "node"), Some(("22.19.0", Scope::Global)));
        // A tool without an entry resolves to none
        assert_eq!(from_table(&table, "go"), None);
    }

    #[test]
    fn test_scope_label_is_the_documented_suffix() {
        assert_eq!(Scope::Global.label(), "global");
    }

    #[test]
    fn test_scope_serializes_lowercase() {
        assert_eq!(serde_json::to_value(Scope::Global).unwrap(), "global");
    }
}
