//! Shared shim logic: forward-target lookup and (re)generation.
//!
//! A shim is a command-name entry living in `<UVMAN_HOME>/shims/` — the single
//! stable PATH entry GUI/IDE processes see. `locate_forward_target` resolves a
//! shim invocation to the same executable `which` reports (both go through the
//! shared resolution entry), and `rehash` regenerates the shim set from the
//! active tools' deploy dirs. The main program drives `rehash` and the PATH
//! wiring (plan 0.3.0).
//!
//! # Two shim kinds (`ShimKind`)
//!
//! A deployment contributes two very different things under one command namespace,
//! and each gets the generation strategy that keeps it working from `shims/`:
//!
//! - **Binary entries** (`.exe`, Unix bare binaries) get a *forwarder*: a copy
//!   of `uvman-shim` that resolves the active version at call time and forwards
//!   argv + stdio + exit code. This is what makes `use` version switching
//!   PATH-free.
//! - **Script entries** (`.ps1` / `.cmd` / `.bat`) are *copied with their
//!   self-relative references rebased*. A script has to be read by its
//!   interpreter from `shims/`, so the vendor file itself is the entry — but a
//!   verbatim copy is not enough: bundled scripts locate their siblings through
//!   `%~dp0` / `$PSScriptRoot`, and a copy in `shims/` would resolve those to
//!   `shims/` instead of the deployment dir. `rebase_self_references` rewrites each
//!   self-reference to the *deploy dir* (absolute), so a `shims/npm.cmd` still
//!   finds the `node.exe` and `node_modules/` next to the real npm — while
//!   `--prefix`-style references an explicitly installed global npm (see
//!   [`rebase_self_references`]) keep pointing at their own target.
//!
//! Unix has no script entries: a `#!/bin/sh` script is an executable command
//! name like any other, so it takes the forwarder and `uvman-shim` performs the
//! interpreter indirection (see `src/bin/uvman-shim.rs`).
//!
//! Note: the `uvman-shim` binary no longer calls into this module — it carries
//! its own `std`-only copy of the lookup so it stays dependency-free (see
//! `src/bin/uvman-shim.rs`). This copy is what `doctor` / `shims` use; the
//! `shim_target_matches_core` test in the shim keeps the two in step.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::current;
use crate::core::error::UError;

/// Subdir of `shims/` holding uvman's private helper binary and the manifest;
/// never exposed on PATH itself
pub const HELPER_DIR: &str = ".uvman";

/// Manifest file recording which shims uvman generated (under
/// `shims/<HELPER_DIR>/`); rehash cleans only files it lists, so foreign files
/// a user placed by hand are never deleted
pub const MANIFEST_FILE: &str = "manifest.toml";

/// Windows extensions that count as a shimmable command entry (same set
/// `which` probes); Unix commands are bare names
fn shim_extensions() -> &'static [&'static str] {
    if cfg!(windows) { &[".exe", ".cmd", ".bat", ".ps1"] } else { &[""] }
}

/// How a shim entry is materialized in `shims/`. The kind is derived from the
/// *file type* of the deployment entry, never from the command name, and it drives
/// both generation and the health checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimKind {
    /// Executable program (`.exe` on Windows, bare binaries and shebang
    /// scripts on Unix): a copy of the `uvman-shim` forwarder that resolves
    /// the active version at call time
    Forward,
    /// Windows script (`.ps1` / `.cmd` / `.bat`): the tool's own script, copied
    /// with its self-relative references rebased to the deployment dir
    Script,
}

impl ShimKind {
    /// Classification of a shim file name.
    ///
    /// Windows suffix table — scripts first, everything else that carries a
    /// shimmable extension is a program:
    /// - `.ps1`, `.cmd`, `.bat` → [`ShimKind::Script`]
    /// - `.exe` and Unix bare names → [`ShimKind::Forward`]
    ///
    /// Case-insensitive on Windows: PATHEXT matching ignores case and an
    /// archive may ship `NPM.CMD`, which must still classify as a script.
    /// Scripts (unlike `.bin`/`.psm1`) are the only extensions Windows executes
    /// by name, which is what makes a copied-and-rebased script a drop-in
    /// entry.
    pub fn of(file_name: &str) -> Self {
        if cfg!(windows) {
            let lower = file_name.to_ascii_lowercase();
            if [".ps1", ".cmd", ".bat"].iter().any(|ext| lower.ends_with(ext)) {
                return Self::Script;
            }
        }
        Self::Forward
    }

    /// Whether entries of this kind are materialized from the deployment's own
    /// script instead of being generated as forwarders
    pub fn is_script(self) -> bool {
        matches!(self, Self::Script)
    }
}

fn is_script_entry(file_name: &str) -> bool {
    ShimKind::of(file_name).is_script()
}

fn is_executable_entry(file_name: &str) -> bool {
    shim_extensions()
        .iter()
        .any(|ext| if ext.is_empty() { !file_name.contains('.') } else { file_name.ends_with(ext) })
}

/// Locate the *deploy source* of a shim: the same-named entry in each active
/// tool's version dir (root first, then `bin/`).
///
/// Binary shims use this only for the health check (the forwarder re-resolves
/// at every call); script shims use it as the copy source during `rehash`.
/// `predicate` decides which file type counts, so the two kinds never claim
/// each other's entry: a `node.exe` shim never resolves to a script sibling.
///
/// A tool whose active version is not deployed on disk is skipped, not fatal —
/// one broken tool must not hide a later tool that does provide the command.
pub fn locate_deploy_source(
    home: &Path, shim_file_name: &str, predicate: impl Fn(&str) -> bool,
) -> Option<PathBuf> {
    let table = current::load_from(&home.join("config").join("tool_current.toml"));
    let tools_root = home.join("tools");
    for (tool, entry) in &table.tools {
        let version = entry.version.as_str();
        let version_dir = tools_root.join(tool).join(version);
        if !version_dir.is_dir() {
            continue; // active version deleted by hand: report, never repair
        }
        let bin_dir = version_dir.join("bin");
        for dir in [version_dir.as_path(), bin_dir.as_path()] {
            let candidate = dir.join(shim_file_name);
            if candidate.is_file() && predicate(shim_file_name) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Locate the deploy source of a script shim (the copy source)
pub fn locate_script_source(home: &Path, shim_file_name: &str) -> Option<PathBuf> {
    locate_deploy_source(home, shim_file_name, is_script_entry)
}

/// Locate the forward target for a shim invocation.
///
/// The active-tool table is matched against the shim file name: for each
/// active tool (name order) whose version dir is deployed, look for a
/// same-named executable (root first, then `bin/`). None when no active tool
/// provides the command — the shim then reports an actionable error instead
/// of silently doing nothing.
pub fn locate_forward_target(home: &Path, shim_file_name: &str) -> Option<PathBuf> {
    locate_deploy_source(home, shim_file_name, |_| true)
}

/// Manifest of generated shims (under `shims/.uvman/manifest.toml`)
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ShimManifest {
    /// Command names uvman generated shims for, e.g. `["node", "npm"]`
    #[serde(default)]
    pub generated: Vec<String>,
}

fn manifest_path(shims: &Path) -> PathBuf {
    shims.join(HELPER_DIR).join(MANIFEST_FILE)
}

fn load_manifest(shims: &Path) -> ShimManifest {
    fs::read_to_string(manifest_path(shims))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_manifest(shims: &Path, manifest: &ShimManifest) -> Result<(), UError> {
    let path = manifest_path(shims);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|source| UError::FileError { path: parent.to_path_buf(), source })?;
    }
    let text = toml::to_string_pretty(manifest)
        .map_err(|source| UError::TomlSerializeError { path: path.clone(), source })?;
    fs::write(&path, text).map_err(|source| UError::FileError { path, source })
}

/// Command names the manifest records as generated shims (missing manifest →
/// empty)
pub fn manifest_names(shims: &Path) -> Vec<String> {
    load_manifest(shims).generated
}

/// Every command name the currently active, deployed tools provide (sorted,
/// deduped) — the desired shim set.
pub fn active_command_names(home: &Path) -> Vec<String> {
    let table = current::load_from(&home.join("config").join("tool_current.toml"));
    let mut names = Vec::new();
    for (tool, entry) in &table.tools {
        let version = entry.version.as_str();
        let version_dir = home.join("tools").join(tool).join(version);
        if !version_dir.is_dir() {
            continue; // active version deleted by hand: report, never repair
        }
        names.extend(executables_in(&version_dir));
    }
    names.sort();
    names.dedup();
    names
}

/// Whether an existing shim entry is broken.
///
/// - a **forwarder** is broken when no active tool provides its command (it
///   would print "no actively managed tool provides …" at call time);
/// - a **script copy** is broken when no active version provides that script
///   (stale copy, left behind by a version switch), and additionally when the
///   copy has drifted from the rewrite its deploy source now produces — the
///   deploy moved, or a uvman version changed the rebasing rule.
///
/// Present-but-foreign files (shims a user placed by hand) are never reported:
/// uvman does not own them.
pub fn shim_is_broken(home: &Path, shims: &Path, file_name: &str) -> bool {
    match ShimKind::of(file_name) {
        ShimKind::Forward => locate_forward_target(home, file_name).is_none(),
        ShimKind::Script => match locate_script_source(home, file_name) {
            None => true,
            Some(source) => match rendered_script(&source) {
                // Healthy when the shim IS the current rendering; anything
                // else (an unrebased pre-0.3.5 copy, a deploy that moved under
                // a stale shim) is drifted
                Ok(expected) => !fs::read(shims.join(file_name)).is_ok_and(|have| have == expected),
                // An unreadable deploy source can't be judged; don't report a
                // shim nothing can vouch for either way
                Err(_) => false,
            },
        },
    }
}

/// Outcome of one `rehash` run (consumed by the `shims rehash` command)
#[derive(Debug, Default)]
pub struct RehashReport {
    /// Shims (re)written in this run, sorted
    pub generated: Vec<String>,
    /// Stale shims removed because their tool/version left the active set
    pub removed: Vec<String>,
}

/// File name the `uvman-shim` binary has on this platform
pub fn shim_binary_name() -> &'static str {
    if cfg!(windows) { "uvman-shim.exe" } else { "uvman-shim" }
}

/// Locate the `uvman-shim` binary to copy as the forwarder: next to the
/// running `uvman` executable first (the shipped layout), then beside the home
/// dir (portable folder moves).
fn shim_template(home: &Path) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(shim_binary_name())))
        .into_iter()
        .chain(std::iter::once(home.join(shim_binary_name())))
        .collect();
    candidates.into_iter().find(|p| p.is_file())
}

/// Whether shims can be generated right now (the forwarding binary is present
/// to copy); auto-rehash skips quietly when it is not.
pub fn shim_available(home: &Path) -> bool {
    shim_template(home).is_some()
}

/// Command-entry file names inside a version dir (root first, then `bin/`),
/// deduplicated — the set a deploy contributes to the shim namespace.
fn executables_in(version_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for dir in [version_dir.to_path_buf(), version_dir.join("bin")] {
        if let Ok(entries) = fs::read_dir(&dir) {
            for path in entries.flatten().map(|e| e.path()) {
                let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                    continue;
                };
                if path.is_file() && is_executable_entry(&name) {
                    names.push(name);
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Regenerate the shim set from the active tools' deploy dirs.
///
/// - Removes shims listed in the manifest that no active version provides
///   (never touches files a user placed by hand — they aren't in the
///   manifest);
/// - (re)writes one entry per active command name: a forwarder for binary
///   entries, a rebased copy of the tool's own script for `.cmd` / `.bat` /
///   `.ps1` (see [`ShimKind`]);
/// - copies the private helper binary under `shims/<HELPER_DIR>/` before the
///   forwarders — scripts don't need it, so a tool set made only of scripts
///   still works without it;
/// - records the new set in the manifest. Idempotent: re-running overwrites
///   same-name shims only.
pub fn rehash(home: &Path) -> Result<RehashReport, UError> {
    let shims = home.join("shims");

    // Target set: every command name any active deployed version provides
    let target = active_command_names(home);

    // Stale cleanup is manifest-driven; the manifest does not track the
    // helper dir, so it is safe to leave it untouched here.
    let previous = load_manifest(&shims);
    let removed: Vec<String> =
        previous.generated.iter().filter(|n| !target.contains(n)).cloned().collect();
    for name in &removed {
        let _ = fs::remove_file(shims.join(name));
    }

    // Only forwarders need the uvman-shim binary; resolve it lazily so a
    // script-only tool set still rehashes on a release without the helper.
    let mut helper: Option<PathBuf> = None;
    let mut generated = Vec::new();
    for name in &target {
        if ShimKind::of(name).is_script() {
            let source = locate_script_source(home, name).ok_or_else(|| {
                UError::SimpleError(format!(
                    "no active version provides the script '{name}'; \
                     reinstall the tool or run `uvman shims rehash` after activating one"
                ))
            })?;
            let bytes = rendered_script(&source)?;
            write_script_shim(&shims.join(name), &bytes)?;
        } else {
            let helper = match &helper {
                Some(path) => path.clone(),
                None => {
                    let path = install_helper(&shims, home)?;
                    helper = Some(path.clone());
                    path
                },
            };
            write_forward_shim(&shims, &helper, name)?;
        }
        generated.push(name.clone());
    }

    save_manifest(&shims, &ShimManifest { generated: target.clone() })?;
    Ok(RehashReport { generated, removed })
}

/// Copy the `uvman-shim` forwarder template into the private helper dir and
/// return the copy's path
fn install_helper(shims: &Path, home: &Path) -> Result<PathBuf, UError> {
    let helper_dir = shims.join(HELPER_DIR);
    fs::create_dir_all(&helper_dir)
        .map_err(|source| UError::FileError { path: helper_dir.clone(), source })?;
    let template = shim_template(home).ok_or_else(|| {
        UError::SimpleError(
            "uvman-shim binary not found beside uvman; reinstall uvman to regenerate shims".into(),
        )
    })?;
    let helper = helper_dir.join(shim_binary_name());
    fs::copy(&template, &helper)
        .map_err(|source| UError::FileError { path: helper.clone(), source })?;
    Ok(helper)
}

/// Generate one binary forwarder: a copy of the helper that forwards by its
/// own file name (the helper is never exposed on PATH under its real name).
///
/// Windows shares the copy between concurrent processes; Unix additionally
/// needs the execute bit, which `fs::copy` does not carry over predictably.
fn write_forward_shim(shims: &Path, helper: &Path, name: &str) -> Result<(), UError> {
    let dest = shims.join(name);
    fs::copy(helper, &dest).map_err(|source| UError::FileError { path: dest.clone(), source })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))
            .map_err(|source| UError::FileError { path: dest, source })?;
    }
    Ok(())
}

/// Materialize one script shim: the tool's own script with self-references
/// rebased to the deploy dir (see [`rebase_self_references`]).
///
/// The rewrite is textual and byte-faithful everywhere else (no line-ending
/// normalization, no encoding change): the vendor's CRLF layout and every
/// non-path byte survive untouched, and only the tokens that would otherwise
/// resolve against `shims/` are re-anchored.
fn rendered_script(source: &Path) -> Result<Vec<u8>, UError> {
    let text = fs::read_to_string(source).map_err(|source_err| UError::FileError {
        path: source.to_path_buf(),
        source: source_err,
    })?;
    let base = source.parent().unwrap_or_else(|| Path::new("."));
    Ok(rebase_self_references(&text, base).into_bytes())
}

/// Write a script shim, creating the shims dir on the way
fn write_script_shim(dest: &Path, bytes: &[u8]) -> Result<(), UError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|source| UError::FileError { path: parent.to_path_buf(), source })?;
    }
    fs::write(dest, bytes).map_err(|source| UError::FileError { path: dest.to_path_buf(), source })
}

/// One self-relative reference found in a script, with its position in the
/// original text.
///
/// `start..end` is the byte range of the reference's *leading token* — the
/// engine-specific "directory of this script" idiom. The bytes of the range
/// are replaced; anything the script appends (a path fragment, a quoting
/// prefix) is kept, which is exactly what rebasing means.
///
/// `native_sep` says whether the replacement must be joined with a native
/// separator: true when the script's own bytes carry no separator (so the
/// original spelling was `"%~dp0" + variable` / the token ends right before a
/// `%` jump) and false when the untouched tail already starts with `\`, `/`,
/// a quote, or nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SelfRef {
    start: usize,
    end: usize,
    native_sep: bool,
}

/// Find every self-relative `%~dp0` / `$PSScriptRoot` reference in a script.
///
/// Deliberately **not** a token scan: a `.cmd` may contain the literal text
/// `%~dp0` inside a quoted message and a `.ps1` inside a string, and neither is
/// a doc reference to replace. Instead, each entry is anchored on the shape
/// that actually resolves at runtime:
///
/// - batch: `%~dp0` / `%dp0%` immediately followed by a path fragment
///   (`\`, `/`, `"`, `'`, or an alphanumeric) — so `echo %~dp0` (nothing
///   appended) and a bare mention inside prose are left alone, while
///   `"%~dp0\node_modules\..."`, `%~dp0/node` and `%~dp0%SUFFIX%` are caught;
/// - PowerShell: the *bare* `$PSScriptRoot` token (not preceded by a word
///   character or `$`, not followed by one, so `$env:PSScriptRoot` and
///   `$PSScriptRootIsSet` don't match) and *not* followed by a path fragment:
///   `"$PSScriptRoot\node"` and `"$PSScriptRoot/lib"` are rebased, while
///   `$PSScriptRoot + '\x'` and `Join-Path $PSScriptRoot x` are left alone —
///   those are the shapes npm's own shim uses for an *explicitly installed*
///   global prefix, which must survive at all costs.
fn find_self_refs(text: &str) -> Vec<SelfRef> {
    let bytes = text.as_bytes();
    let mut refs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(token) = batch_token_at(bytes, i) {
            let after = i + token;
            // Followed by a path fragment or a `%VAR%` jump: a genuine reference
            if fragment_follows(bytes, after) || bytes.get(after) == Some(&b'%') {
                // `"%~dp0"` right before a quote means the separator was
                // outside the token (`"%~dp0" + $var`, `%~dp0.x`); a following
                // `\`, `/`, `%` or alphanumeric already supplies the join
                let native_sep = matches!(bytes.get(after), Some(b'"' | b'\''));
                refs.push(SelfRef { start: i, end: after, native_sep });
            }
            i += token;
            continue;
        }
        i += 1;
    }
    refs.extend(pwsh_refs(text));
    refs.sort_by_key(|r| r.start);
    refs
}

/// Length of a batch `%~dp0` / `%dp0%` token starting at `i`, if any.
///
/// Case-insensitive: `CMD` and `cmd` are the same file extension here.
///
/// Byte-based: the caller walks the file byte by byte and may stop inside a
/// multi-byte UTF-8 char (a leading BOM, a CJK comment), and slicing the string
/// at such a position would panic. The token and everything it is matched
/// against is pure ASCII, so byte comparison is exact — and because token bytes
/// (`%`, `$`, `\`, `/`, quotes, alphanumerics) can never appear as UTF-8
/// continuation bytes, a match starts at a real char boundary, so the positions
/// `rebase_self_references` slices at stay valid.
fn batch_token_at(bytes: &[u8], i: usize) -> Option<usize> {
    const TILDE: &[u8] = b"%~dp0";
    const PERCENT: &[u8] = b"%dp0%";
    let rest = &bytes[i..];
    if rest.len() >= TILDE.len() && rest[..TILDE.len()].eq_ignore_ascii_case(TILDE) {
        return Some(TILDE.len());
    }
    if rest.len() >= PERCENT.len() && rest[..PERCENT.len()].eq_ignore_ascii_case(PERCENT) {
        return Some(PERCENT.len());
    }
    None
}

/// Whether the byte at `i` starts a path fragment appended to a script
/// directory reference (`\x`, `/x`, `"x`, `'x`, `x`).
fn fragment_follows(bytes: &[u8], i: usize) -> bool {
    bytes.get(i).is_some_and(|b| {
        matches!(b, b'\\' | b'/' | b'"' | b'\'' | b'$') || b.is_ascii_alphanumeric()
    })
}

/// Bare `$PSScriptRoot` occurrences that must be rebased: the automatic
/// variable used directly as a path prefix, never the `$env:` form and never a
/// `+`/`Join-Path` argument (those are deliberate global-prefix references).
fn pwsh_refs(text: &str) -> Vec<SelfRef> {
    const TOKEN: &str = "$PSScriptRoot";
    let bytes = text.as_bytes();
    let mut refs = Vec::new();
    let mut from = 0;
    while let Some(off) = text[from..].find(TOKEN) {
        let start = from + off;
        let end = start + TOKEN.len();
        let before_ok = start == 0 || {
            let prev = bytes[start - 1];
            !prev.is_ascii_alphanumeric() && prev != b'_' && prev != b'$' && prev != b':'
        };
        // Only a direct directory join is rebased; `+ '\'` / `Join-Path` forms
        // stay as the author wrote them. The tail keeps whatever separator it
        // already had, so `base/node` never becomes a mixed-style `base\node`.
        let after = matches!(bytes.get(end), Some(b'\\' | b'/'));
        if before_ok && after {
            refs.push(SelfRef { start, end, native_sep: false });
        }
        from = end;
    }
    refs
}

/// Rebase a script's self-relative references onto `base` (the deploy dir it
/// was copied from), so a shim in `shims/` still resolves its siblings.
///
/// The result is the byte-identical script except for the leading token of
/// each reference, which is replaced by `base`. Separators are composed
/// per-token so a path never mixes styles:
///
/// | script wrote            | `base` = `E:\…\24.21.0` → |
/// |-------------------------|---------------------------|
/// | `"%~dp0\node.exe"`      | `"E:\…\24.21.0\node.exe"` |
/// | `"%~dp0/node"`          | `"E:\…\24.21.0/node"`     |
/// | `"%~dp0" + $var`        | `"E:\…\24.21.0" + $var`   |
/// | `%~dp0%SUFFIX%`         | `E:\…\24.21.0%SUFFIX%`    |
///
/// Public so the rule is testable in isolation; `rehash` is the only caller.
pub fn rebase_self_references(text: &str, base: &Path) -> String {
    let refs = find_self_refs(text);
    if refs.is_empty() {
        return text.to_string();
    }
    let base = base.to_string_lossy();
    let mut out = String::with_capacity(text.len() + refs.len() * base.len());
    let mut cursor = 0;
    for r in refs {
        out.push_str(&text[cursor..r.start]);
        out.push_str(&base);
        // A reference whose next byte is none of `\`, `/`, a quote or a `%`
        // jump was written as `"%~dp0" + variable`: restore the separator the
        // quote swallowed so the join stays a real path.
        if r.native_sep {
            out.push('\\');
        }
        cursor = r.end;
    }
    out.push_str(&text[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_home_with(dir: &Path, tool: &str, version: &str, files: &[&str]) -> PathBuf {
        let home = dir.to_path_buf();
        let version_dir = home.join("tools").join(tool).join(version);
        for f in files {
            let p = version_dir.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b"binary").unwrap();
        }
        // Record the activation in the internal table file (as `use` would)
        current::set_current_at(&home.join("config").join("tool_current.toml"), tool, version)
            .unwrap();
        home
    }

    /// Windows-only rule: Unix commands are bare names, so scripts never reach
    /// the script branch there.
    fn windows_only() -> bool {
        cfg!(windows)
    }

    #[test]
    fn test_is_executable_entry_classification() {
        if cfg!(windows) {
            assert!(is_executable_entry("node.exe"));
            assert!(is_executable_entry("npm.cmd"));
            assert!(is_executable_entry("run.bat"));
            // Libraries and docs are not shimmable
            assert!(!is_executable_entry("node.dll"));
            assert!(!is_executable_entry("README.md"));
            assert!(!is_executable_entry("node_modules"));
        } else {
            assert!(is_executable_entry("node"));
            assert!(!is_executable_entry("README.md"));
        }
    }

    /// The generation rule hinges on this split: scripts are rebased copies,
    /// programs get a forwarder. Only Windows ever sees scripts (Unix commands
    /// are bare names), and matching must ignore case.
    #[test]
    fn test_shim_kind_classification() {
        // Every file type that gets a script shim, and the boundary cases
        // that must stay forwarders, per platform
        if cfg!(windows) {
            assert_eq!(ShimKind::of("run.ps1"), ShimKind::Script);
            assert_eq!(ShimKind::of("npm.cmd"), ShimKind::Script);
            assert_eq!(ShimKind::of("npx.bat"), ShimKind::Script);
            // Uppercase suffixes come from real archives and mean the same
            assert_eq!(ShimKind::of("NPM.CMD"), ShimKind::Script);
            assert_eq!(ShimKind::of("Build.Ps1"), ShimKind::Script);

            assert_eq!(ShimKind::of("node.exe"), ShimKind::Forward);
            // No-extension entries (Windows-only tools do ship them) forward too
            assert_eq!(ShimKind::of("make"), ShimKind::Forward);
        } else {
            assert_eq!(ShimKind::of("node"), ShimKind::Forward);
            // Unix scripts are shebang files executed by name → forwarder
            assert_eq!(ShimKind::of("run.sh"), ShimKind::Forward);
        }
    }

    #[test]
    fn test_locate_forward_target_hits_active_tool_bin_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "bin/npm.exe"]);
        let tools = home.join("tools").join("node").join("22.19.0");

        if cfg!(windows) {
            assert_eq!(locate_forward_target(&home, "node.exe"), Some(tools.join("node.exe")));
            assert_eq!(
                locate_forward_target(&home, "npm.exe"),
                Some(tools.join("bin").join("npm.exe"))
            );
        } else {
            assert_eq!(locate_forward_target(&home, "node"), Some(tools.join("node")));
        }
    }

    /// A script shim resolves to the very script `rehash` copies — and only
    /// through the script branch.
    #[test]
    fn test_locate_script_source_targets_the_script() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "bin/npm.cmd"]);
        let tools = home.join("tools").join("node").join("22.19.0");
        assert_eq!(locate_script_source(&home, "npm.cmd"), Some(tools.join("bin").join("npm.cmd")));
        // The `.exe` entry is not a script source
        assert_eq!(locate_script_source(&home, "node.exe"), None);
    }

    #[test]
    fn test_locate_forward_target_skips_missing_and_inactive() {
        let dir = tempfile::tempdir().unwrap();
        // `node` is active but its version dir was deleted by hand; `go` is
        // active with a dir but no matching file. Both resolve to None, not
        // panic — the shim's actionable error path.
        let home = make_home_with(dir.path(), "go", "1.23.0", &["go"]);
        current::set_current_at(&home.join("config").join("tool_current.toml"), "node", "22.19.0")
            .unwrap();
        current::set_current_at(&home.join("config").join("tool_current.toml"), "go", "1.23.0")
            .unwrap();

        if cfg!(windows) {
            assert_eq!(locate_forward_target(&home, "node.exe"), None);
            assert_eq!(locate_forward_target(&home, "go.cmd"), None);
        } else {
            assert_eq!(locate_forward_target(&home, "node"), None);
            assert_eq!(locate_forward_target(&home, "go"), Some(home.join("tools/go/1.23.0/go")));
        }
    }

    /// One tool with a hand-deleted version dir must not hide a later tool that
    /// does provide the command (`node` sorts before `python`).
    #[test]
    fn test_locate_forward_target_keeps_scanning_past_undeployed_tool() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "python", "3.13.1", &["python.exe", "python"]);
        // `node` is active but its version dir is gone
        current::set_current_at(&home.join("config").join("tool_current.toml"), "node", "22.19.0")
            .unwrap();

        let expected = home.join("tools/python/3.13.1").join(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        });
        assert_eq!(
            locate_forward_target(&home, if cfg!(windows) { "python.exe" } else { "python" }),
            Some(expected)
        );
    }

    #[test]
    fn test_locate_forward_target_empty_table() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "go", "1.23.0", &["go"]);
        // `go` was installed but never activated: no entry in the table, so
        // nothing resolves (tools on disk alone are not enough)
        current::remove_current_at(&home.join("config").join("tool_current.toml"), "go").unwrap();
        assert_eq!(locate_forward_target(&home, "go"), None);
        assert_eq!(locate_forward_target(&home, "python.exe"), None);
    }

    #[test]
    fn test_manifest_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let shims = dir.path().join("shims");
        let mut manifest = ShimManifest::default();
        manifest.generated.push("node".into());
        save_manifest(&shims, &manifest).unwrap();
        assert_eq!(load_manifest(&shims).generated, vec!["node".to_string()]);
        // Missing manifest reads back empty (list gets regenerated)
        assert!(load_manifest(&dir.path().join("absent")).generated.is_empty());
    }

    /// Same activation state → the shim forwarder lands on exactly the file
    /// `which` reports (plan 0.3.0 completion criterion: same source, no
    /// drift).
    #[test]
    fn test_forward_target_matches_which_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe"]);
        let shim_name = if cfg!(windows) { "node.exe" } else { "node" };
        let via_shim = locate_forward_target(&home, shim_name);
        let via_which =
            crate::core::executable::locate(&home.join("tools"), "node", "22.19.0").ok();
        assert_eq!(via_shim, via_which);
    }

    /// Place a fake `uvman-shim` template beside the home dir (the portable
    /// layout) so rehash can copy it
    fn seed_template(home: &Path) {
        fs::write(home.join(shim_binary_name()), b"template-binary").unwrap();
    }

    #[test]
    fn test_rehash_generates_shims_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "bin/npm.cmd"]);
        seed_template(&home);

        let report = rehash(&home).unwrap();
        assert!(!report.generated.is_empty());
        assert!(report.removed.is_empty());

        let shims = home.join("shims");
        // The helper binary landed in the private subdir
        assert!(shims.join(HELPER_DIR).join(shim_binary_name()).is_file());
        if cfg!(windows) {
            // .exe target → a forwarder copy of the helper binary
            assert_eq!(
                fs::read(shims.join("node.exe")).unwrap(),
                fs::read(shims.join(HELPER_DIR).join(shim_binary_name())).unwrap()
            );
        } else {
            assert!(shims.join("node").is_file());
            assert!(shims.join("npm").is_file());
        }
        // The manifest records exactly the generated set, sorted
        let mut expect: Vec<String> = if cfg!(windows) {
            vec!["npm.cmd".to_string(), "node.exe".to_string()]
        } else {
            vec!["npm".to_string(), "node".to_string()]
        };
        expect.sort();
        assert_eq!(load_manifest(&shims).generated, expect);
    }

    /// The core of the script branch: a `.cmd`/`.ps1` deploy entry lands in
    /// `shims/` as the vendor script with its `%~dp0` / `$PSScriptRoot`
    /// references re-anchored on the deploy dir.
    #[test]
    fn test_rehash_rebases_script_entries() {
        if !windows_only() {
            return; // Windows-only rule: Unix scripts are bare names
        }
        let dir = tempfile::tempdir().unwrap();
        let script = b"@ECHO off\r\n\"%~dp0\\node_modules\\npm\\bin\\npm-cli.js\" %*\r\n";
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "bin/npm.cmd"]);
        let tools = home.join("tools").join("node").join("22.19.0");
        // A vendor script whose exact bytes matter (CRLF + %~dp0), plus a
        // PowerShell entry to cover the second script extension
        fs::write(tools.join("bin").join("npm.cmd"), script).unwrap();
        fs::write(tools.join("bin").join("setup.ps1"), b"& \"$PSScriptRoot\\do.ps1\"\r\n").unwrap();
        seed_template(&home);

        rehash(&home).unwrap();

        let shims = home.join("shims");
        let copied = fs::read_to_string(shims.join("npm.cmd")).unwrap();
        let deploy = tools.join("bin").to_string_lossy().replace('/', "\\");
        // The reference now points at the deploy dir, not at shims/
        assert!(
            copied.contains(&format!("\"{deploy}\\node_modules\\npm\\bin\\npm-cli.js\"")),
            "deploy path rebased: {copied:?}"
        );
        assert!(!copied.contains("%~dp0"), "no self-relative reference may survive: {copied:?}");
        // Everything else is byte-faithful, CRLF layout included
        assert!(copied.starts_with("@ECHO off\r\n"), "bytes outside the rebase survive");

        let ps1 = fs::read_to_string(shims.join("setup.ps1")).unwrap();
        assert!(ps1.contains(&format!("\"{deploy}\\do.ps1\"")), "pwsh rebased: {ps1:?}");
        assert!(!ps1.contains("$PSScriptRoot"), "no self-relative reference may survive: {ps1:?}");
    }

    /// A script with no self-relative reference is copied byte-for-byte.
    #[test]
    fn test_rehash_script_without_self_refs_is_verbatim() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["bin/npm.cmd"]);
        let script = b"@ECHO off\r\necho plain\r\n";
        fs::write(home.join("tools/node/22.19.0/bin/npm.cmd"), script).unwrap();
        rehash(&home).unwrap();
        assert_eq!(fs::read(home.join("shims/npm.cmd")).unwrap(), script);
    }

    // -----------------------------------------------------------------
    // rebase_self_references
    // -----------------------------------------------------------------

    fn rebase(text: &str, base: &str) -> String {
        rebase_self_references(text, Path::new(base))
    }

    #[test]
    fn test_rebase_batch_forms() {
        let base = r"E:\uvman\tools\node\24.21.0";
        assert_eq!(
            rebase(r#""%~dp0\node_modules\npm\bin\npm-cli.js" %*"#, base),
            format!(r#""{base}\node_modules\npm\bin\npm-cli.js" %*"#)
        );
        // A slash join keeps the slash: the injected dir must never make the
        // path mix separators
        assert_eq!(rebase(r#""%~dp0/npm-cli.js""#, base), format!(r#""{base}/npm-cli.js""#));
        // A quote right after the token means the separator lived outside it
        // (`"%~dp0" + $var`) and must be restored
        assert_eq!(
            rebase(r#"Join-Path "%~dp0" $var"#, base),
            format!(r#"Join-Path "{base}\" $var"#)
        );
        // `%dp0%` is the same idiom
        assert_eq!(rebase("call %dp0%\\setup.bat", base), format!(r#"call {base}\setup.bat"#));
        // A `%VAR%` jump right after the dir also counts (and stays glued)
        assert_eq!(rebase("%~dp0%EXTRA%", base), format!("{base}%EXTRA%"));
        // Split percent form with a jump too
        assert_eq!(rebase("%dp0%%EXTRA%", base), format!("{base}%EXTRA%"));
    }

    /// Deliberate non-matches: only shapes that actually resolve relative to
    /// the script are rewritten.
    #[test]
    fn test_rebase_leaves_non_references_alone() {
        let base = r"E:\uvman\tools\node\24.21.0";
        // Nothing appended: the reference is not a path join here
        assert_eq!(rebase(r"echo %~dp0", base), r"echo %~dp0");
        assert_eq!(rebase(r"echo %~dp0 ", base), r"echo %~dp0 ");
        // Non-batch text mentioning the token
        assert_eq!(rebase(r"rem see %dp0x for details", base), r"rem see %dp0x for details");
        // PowerShell: the + / Join-Path forms are global-prefix logic, not a
        // sibling join — npm's shim installs globals through exactly this
        let pwsh_join = "$NPM_PREFIX = Join-Path $PSScriptRoot 'global'\r\n";
        assert_eq!(rebase(pwsh_join, base), pwsh_join);
        let pwsh_concat = "$x = $PSScriptRoot + '\\bin'\r\n";
        assert_eq!(rebase(pwsh_concat, base), pwsh_concat);
        // The $env: form is a different variable entirely
        let env_form = "$env:PSScriptRoot = 'x'\r\n";
        assert_eq!(rebase(env_form, base), env_form);
    }

    #[test]
    fn test_rebase_pwsh_direct_joins() {
        let base = r"E:\uvman\tools\node\24.21.0";
        assert_eq!(
            rebase("& \"$PSScriptRoot\\node.exe\" $args\r\n", base),
            format!("& \"{base}\\node.exe\" $args\r\n")
        );
        assert_eq!(
            rebase("& \"$PSScriptRoot/node\" $args\r\n", base),
            format!("& \"{base}/node\" $args\r\n")
        );
        // A single-quoted join works too
        assert_eq!(
            rebase("$p = '$PSScriptRoot\\lib'\r\n", base),
            format!("$p = '{base}\\lib'\r\n")
        );
    }

    /// Both idioms in one file, order preserved, everything else untouched.
    #[test]
    fn test_rebase_mixed_and_multiple() {
        let base = r"E:\uvman\tools\node\24.21.0";
        let text = "@echo off\r\nset A=\"%~dp0\\a\"\r\nset B=\"%~dp0\\%S\\b\"\r\n";
        let want = format!("@echo off\r\nset A=\"{base}\\a\"\r\nset B=\"{base}\\%S\\b\"\r\n");
        assert_eq!(rebase(text, base), want);
    }

    #[test]
    fn test_rebase_is_idempotent() {
        let base = r"E:\uvman\tools\node\24.21.0";
        let once = rebase(r#""%~dp0\node.exe" %*"#, base);
        assert_eq!(rebase(&once, base), once, "an already-rebased script must not drift");
    }

    /// The scan walks the file byte by byte, so a leading UTF-8 BOM or a CJK
    /// comment must never panic it — and only genuine references are rebased.
    #[test]
    fn test_rebase_is_utf8_safe_with_bom_and_cjk() {
        let base = r"E:\uvman\tools\node\24.21.0";
        let text = "\u{feff}# 中文注释之后才是真实引用\r\n\"%~dp0\\node.exe\" %*\r\n";
        let out = rebase(text, base);
        assert!(out.starts_with('\u{feff}'), "BOM survives: {:?}", out);
        assert!(out.contains(&format!(r#""{base}\node.exe""#)), "ref rebased: {out:?}");
        assert!(out.contains("# 中文注释之后才是真实引用"), "CJK comment survives");

        let pwsh = "\u{feff}# 中文注释\r\n& \"$PSScriptRoot\\tool.ps1\" $args\r\n";
        let out = rebase(pwsh, base);
        assert!(out.contains(r#"& "E:\uvman\tools\node\24.21.0\tool.ps1" $args"#), "{out:?}");
    }

    /// The injected base is passed through as the platform's own path text;
    /// the script's own separator (whichever it is) is never rewritten.
    #[test]
    fn test_rebase_keeps_each_scripts_separator_style() {
        // Forward-slash join stays forward-slash — npm.ps1 relies on this
        assert_eq!(
            rebase(r#""%~dp0/node_modules/npm/bin/npm-cli.js""#, r"E:\uvman\tools\node\24.21.0"),
            r#""E:\uvman\tools\node\24.21.0/node_modules/npm/bin/npm-cli.js""#
        );
        // Backslash join stays backslash
        assert_eq!(
            rebase(r#""%~dp0\node_modules\npm\bin\npm-cli.js""#, r"E:\uvman\tools\node\24.21.0"),
            r#""E:\uvman\tools\node\24.21.0\node_modules\npm\bin\npm-cli.js""#
        );
    }

    /// Script entries are copied, so a rehash must be able to find them in the
    /// version dir root too (deploys differ: root vs `bin/`).
    #[test]
    fn test_locate_script_source_prefers_root_then_bin() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "corepack.cmd"]);
        let tools = home.join("tools").join("node").join("22.19.0");
        fs::create_dir_all(tools.join("bin")).unwrap();

        // Root hit
        assert_eq!(locate_script_source(&home, "corepack.cmd"), Some(tools.join("corepack.cmd")));
        // Root miss → `bin/` hit
        fs::write(tools.join("bin").join("npm.cmd"), b"@echo off\r\n").unwrap();
        assert_eq!(locate_script_source(&home, "npm.cmd"), Some(tools.join("bin").join("npm.cmd")));
        // A binary name never resolves through the script branch
        assert_eq!(locate_script_source(&home, "node.exe"), None);
    }

    /// A script shim must be usable even when no forwarding binary was shipped
    /// — scripts don't need the helper at all.
    #[test]
    fn test_rehash_scripts_do_not_require_template_binary() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["bin/npm.cmd"]);
        let script = b"@ECHO off\r\necho npm\r\n";
        fs::write(home.join("tools/node/22.19.0/bin/npm.cmd"), script).unwrap();
        // No template beside the home
        let report = rehash(&home).unwrap();
        assert_eq!(report.generated, vec!["npm.cmd".to_string()]);
        assert_eq!(fs::read(home.join("shims/npm.cmd")).unwrap(), script);
        // The helper dir is not populated for a script-only set
        assert!(!home.join("shims").join(HELPER_DIR).join(shim_binary_name()).exists());
    }

    #[test]
    fn test_rehash_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe"]);
        seed_template(&home);

        let first = rehash(&home).unwrap();
        let second = rehash(&home).unwrap();
        // Same set both times: nothing new generated, nothing removed
        assert_eq!(first.generated, second.generated);
        assert!(second.removed.is_empty());
    }

    /// Re-running rehash refreshes a stale script copy (version switch).
    #[test]
    fn test_rehash_refreshes_stale_script_copy() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["bin/npm.cmd"]);
        let script = home.join("tools/node/22.19.0/bin/npm.cmd");
        fs::write(&script, b"@ECHO off\r\necho v1\r\n").unwrap();

        rehash(&home).unwrap();
        let shim = home.join("shims/npm.cmd");
        assert_eq!(fs::read(&shim).unwrap(), b"@ECHO off\r\necho v1\r\n");

        // The deploy moves on → the next rehash must replace the copy
        fs::write(&script, b"@ECHO off\r\necho v2\r\n").unwrap();
        rehash(&home).unwrap();
        assert_eq!(fs::read(&shim).unwrap(), b"@ECHO off\r\necho v2\r\n");
    }

    /// Drift detection compares against the *rebased* rendering, so a shim
    /// whose deploy moved (or whose rebasing rule changed) reports stale.
    #[test]
    fn test_shim_is_broken_for_drifted_script_copy() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["bin/npm.cmd"]);
        let source = home.join("tools/node/22.19.0/bin/npm.cmd");
        // A script with a self-reference: the rebased rendering is what rehash
        // writes, and only that rendering counts as healthy
        fs::write(&source, b"@ECHO off\r\ncall \"%~dp0\\npm-cli.cmd\" %*\r\n").unwrap();
        rehash(&home).unwrap();

        let shims = home.join("shims");
        assert!(!shim_is_broken(&home, &shims, "npm.cmd"), "fresh rebased copy is healthy");

        // The deploy moved to another dir → the copy points at the old path
        fs::rename(home.join("tools/node/22.19.0"), home.join("tools/node/22.19.0.moved")).unwrap();
        let moved = home.join("tools/node/22.19.0.moved");
        fs::create_dir_all(&moved).unwrap();
        fs::rename(home.join("tools/node/22.19.0.moved/bin"), moved.join("bin")).ok();
        assert!(
            shim_is_broken(&home, &shims, "npm.cmd"),
            "a copy that no longer matches its deploy is stale"
        );

        // A pre-0.3.5 verbatim copy (self-reference intact) is stale too
        fs::write(shims.join("npm.cmd"), b"@ECHO off\r\ncall \"%~dp0\\npm-cli.cmd\" %*\r\n")
            .unwrap();
        assert!(shim_is_broken(&home, &shims, "npm.cmd"), "unrebased copy is stale");
    }

    #[test]
    fn test_rehash_prunes_stale_shims_only_from_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe"]);
        seed_template(&home);
        rehash(&home).unwrap();

        let shims = home.join("shims");
        // A foreign file the user placed by hand is not in the manifest
        let foreign_needed = shims.join("keep_me.txt");
        fs::write(&foreign_needed, b"user file").unwrap();

        // Tool removed from the active set → its shim goes stale
        current::remove_current_at(&home.join("config").join("tool_current.toml"), "node").unwrap();
        let report = rehash(&home).unwrap();
        assert!(report.removed.contains(&"node.exe".to_string()));
        assert!(!shims.join("node.exe").exists(), "stale shim removed");
        assert!(foreign_needed.exists(), "user-placed file must survive");
        assert!(load_manifest(&shims).generated.is_empty(), "manifest emptied");
    }

    /// Stale script copies are pruned through the manifest exactly like
    /// forwarders (the deletion path is shared).
    #[test]
    fn test_rehash_prunes_stale_script_shim() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["bin/npm.cmd"]);
        fs::write(home.join("tools/node/22.19.0/bin/npm.cmd"), b"@echo off\r\n").unwrap();
        rehash(&home).unwrap();
        assert!(home.join("shims/npm.cmd").exists());

        current::remove_current_at(&home.join("config").join("tool_current.toml"), "node").unwrap();
        let report = rehash(&home).unwrap();
        assert!(report.removed.contains(&"npm.cmd".to_string()));
        assert!(!home.join("shims/npm.cmd").exists());
    }

    #[test]
    fn test_rehash_requires_template_binary() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe"]);
        // No template beside the home: an actionable error, no half-written state
        let err = rehash(&home).unwrap_err();
        assert!(err.to_string().contains("uvman-shim"), "err: {err}");
        assert!(!home.join("shims").join("node.exe").exists());
    }

    #[test]
    fn test_executables_in_scans_root_and_bin() {
        let dir = tempfile::tempdir().unwrap();
        let version_dir = dir.path().join("node").join("22.19.0");
        fs::create_dir_all(version_dir.join("bin")).unwrap();
        fs::write(version_dir.join("node.exe"), b"x").unwrap();
        fs::write(version_dir.join("bin/npm.cmd"), b"x").unwrap();
        // Non-executable entries are ignored
        fs::write(version_dir.join("README.md"), b"x").unwrap();
        fs::write(version_dir.join("node.dll"), b"x").unwrap();

        let mut names = executables_in(&version_dir);
        names.sort();
        if cfg!(windows) {
            assert_eq!(names, vec!["node.exe".to_string(), "npm.cmd".to_string()]);
        } else {
            // Unix: bare, dot-free names only
            assert!(names.is_empty() || names == vec!["npm".to_string(), "node".to_string()]);
        }
    }
}
