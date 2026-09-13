//! `uvman which <tool>`: absolute path of the active version's executable.
//!
//! Read-only: the answer comes from the activation table plus the deployed
//! version dir on disk; nothing writes state. Output is a single bare absolute
//! path (no decoration, no styling) so scripts can consume it directly.

use crate::Result;
use crate::core::error::UError;
use crate::core::executable;
use crate::core::paths::tools_dir;
use crate::core::resolve;

/// Print the absolute path of an active tool's executable
///
/// Same lookup `uvman-shim` forwards by; an unanswerable query is an error
/// (non-zero exit).
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct Which {
    /// Tool name to locate
    pub tool: String,
}

impl Which {
    pub fn run(&self) -> Result<()> {
        let Some((version, _scope)) = resolve::current_version(&self.tool) else {
            return Err(UError::NoActiveVersion { tool: self.tool.clone() }.into());
        };
        let path = executable::locate(&tools_dir(), &self.tool, &version)?;
        println!("{}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_new_error_hints_and_exit_codes() {
        let err = UError::NoActiveVersion { tool: "node".into() };
        let hint = err.hint().expect("hint");
        assert!(hint.commands.iter().any(|c| c.contains("uvman use node")));

        // Both new errors are state problems, not usage errors: exit 1
        assert_eq!(err.exit_code(), 1);
        assert_eq!(
            UError::ExecutableNotFound {
                tool: "node".into(),
                version: "22.19.0".into(),
                path: PathBuf::from("tools/node/22.19.0"),
            }
            .exit_code(),
            1
        );
    }
}
