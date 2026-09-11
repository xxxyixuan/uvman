//! User PATH storage abstraction behind `shims enable` / `disable` and the
//! doctor shims check.
//!
//! The user-level PATH lives in `HKCU\Environment\Path` on Windows; its value
//! may be `REG_EXPAND_SZ` carrying `%VAR%` fragments that must round-trip
//! unexpanded (type fidelity). The registry backend is Windows-only; the
//! enable/disable *edit logic* is platform-neutral and tested against an
//! in-memory store so behavior is verified on every CI (plan 0.3.0 task 4:
//! `PathStore` trait with registry + memory/file implementations).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::error::UError;

/// Registry value type: `REG_EXPAND_SZ` values keep `%VAR%` tokens that must
/// survive writes untouched (`REG_SZ` values are plain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathValueKind {
    /// REG_EXPAND_SZ: raw value may contain `%VAR%` fragments
    Expandable,
    /// REG_SZ: plain string
    Plain,
}

/// A stored user PATH value: the raw (unexpanded) text plus its kind, so a
/// rewrite is byte/type-faithful for the parts uvman does not own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPath {
    pub raw: String,
    pub kind: PathValueKind,
}

/// Storage backend for the user-level PATH value.
///
/// `read` returning `Ok(None)` means "no PATH value is set but the store is
/// manageable" (fresh Windows profile), while `Err` means the store itself is
/// unavailable on this platform (Unix: PATH is shell-owned, not managed here).
pub trait PathStore {
    fn read(&self) -> Result<Option<StoredPath>, UError>;
    fn write(&self, value: &StoredPath) -> Result<(), UError>;
    /// Notify running processes that the environment changed (Windows
    /// `WM_SETTINGCHANGE`); no-op elsewhere.
    fn broadcast(&self);
}

/// Platform PATH separator
fn path_sep() -> &'static str {
    if cfg!(windows) { ";" } else { ":" }
}

/// Split a raw PATH value into items (empty trailing segments dropped)
pub fn split_items(raw: &str) -> Vec<String> {
    raw.split(path_sep()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Join items back into a raw PATH value
pub fn join_items(items: &[String]) -> String {
    items.join(path_sep())
}

/// Expand `%VAR%` environment references (Windows style), best-effort: known
/// variables are substituted for comparison purposes; unknown/malformed tokens
/// stay as-is.
fn expanded(text: &str) -> String {
    let mut out = text.to_string();
    loop {
        let Some(start) = out.find('%') else { break };
        let Some(end) = out[start + 1..].find('%') else { break };
        let end = start + 1 + end;
        let name = &out[start + 1..end];
        if name.is_empty() {
            break;
        }
        match std::env::var(name) {
            Ok(value) => out.replace_range(start..=end, &value),
            Err(_) => break,
        }
    }
    out
}

/// Case-fold a path item for ownership comparison (Windows paths are
/// case-insensitive)
fn fold(item: &str) -> String {
    if cfg!(windows) { item.to_lowercase() } else { item.to_string() }
}

/// Backup file for the user PATH value (before any enable/disable rewrite),
/// kept under `<home>/backup/`
const BACKUP_FILE: &str = "user-path.bak";

#[derive(Debug, Serialize, Deserialize)]
struct PathBackup {
    kind: String,
    raw: String,
}

/// Write the current user PATH value aside before a rewrite (rollback source).
pub fn backup_current(backup_dir: &Path, value: &StoredPath) -> Result<(), UError> {
    fs_ensure_dir(backup_dir)?;
    let kind = match value.kind {
        PathValueKind::Expandable => "expand-sz",
        PathValueKind::Plain => "sz",
    };
    let json = serde_json::to_vec_pretty(&PathBackup { kind: kind.into(), raw: value.raw.clone() })
        .map_err(|source| UError::JsonError { source })?;
    let path = backup_dir.join(BACKUP_FILE);
    std::fs::write(&path, json).map_err(|source| UError::FileError { path, source })
}

fn fs_ensure_dir(dir: &Path) -> Result<(), UError> {
    std::fs::create_dir_all(dir).map_err(|source| UError::FileError { path: dir.to_path_buf(), source })
}

/// Outcome of an idempotent enable/disable run
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    /// The change was applied (PATH was rewritten)
    Applied,
    /// Nothing to do: the target state already holds (idempotent no-op)
    Unchanged,
}

/// Report of one PATH edit, for the CLI message
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnableReport {
    Enabled,
    AlreadyEnabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisableReport {
    Disabled,
    AlreadyDisabled,
}

/// Platform-neutral PATH editing: prepend the shims dir on enable, strip
/// uvman-owned entries on disable. Owns the backup step; the store does the
/// raw read/write round-trip.
pub struct PathEditor<'a> {
    store: &'a dyn PathStore,
    shims_dir: &'a Path,
    backup_dir: &'a Path,
}

impl<'a> PathEditor<'a> {
    pub fn new(store: &'a dyn PathStore, shims_dir: &'a Path, backup_dir: &'a Path) -> Self {
        Self { store, shims_dir, backup_dir }
    }

    /// Whether the shims dir currently appears in the user PATH (expanded
    /// comparison). Err = store unavailable (Unix).
    pub fn in_path(&self) -> Result<bool, UError> {
        Ok(self.items()?.iter().any(|item| self.owns_item(item)))
    }

    /// The user PATH as item list; Err on store-unavailable platforms
    pub fn items(&self) -> Result<Vec<String>, UError> {
        Ok(self.store.read()?.map(|v| split_items(&v.raw)).unwrap_or_default())
    }

    /// Prepend the shims dir to the user PATH (idempotent: already present is
    /// a no-op). Backs up the pre-edit value first; the new value keeps the
    /// original registry kind and unexpanded `%VAR%` fragments.
    pub fn enable(&self) -> Result<EnableReport, UError> {
        let Some(value) = self.store.read()? else {
            // No PATH value yet: start fresh with just the shims dir
            let items = vec![self.prepend_raw()];
            return self.write_new(&items, PathValueKind::Expandable).map(|_| EnableReport::Enabled);
        };
        let items = split_items(&value.raw);
        if items.iter().any(|item| self.owns_item(item)) {
            return Ok(EnableReport::AlreadyEnabled);
        }
        let mut next = vec![self.prepend_raw()];
        next.extend(items);
        backup_current(self.backup_dir, &value)?;
        self.write_new(&next, value.kind).map(|_| EnableReport::Enabled)
    }

    /// Remove uvman-owned entries (expanded value starts with the shims
    /// prefix) from the user PATH (idempotent no-op when nothing matches).
    pub fn disable(&self) -> Result<DisableReport, UError> {
        let Some(value) = self.store.read()? else {
            return Ok(DisableReport::AlreadyDisabled);
        };
        let items = split_items(&value.raw);
        let kept: Vec<String> = items.iter().filter(|item| !self.owns_item(item)).cloned().collect();
        if kept.len() == items.len() {
            return Ok(DisableReport::AlreadyDisabled);
        }
        backup_current(self.backup_dir, &value)?;
        self.write_new(&kept, value.kind).map(|_| DisableReport::Disabled)
    }

    /// The raw text to prepend (the exact, attribute-preserving value; no
    /// expansion happens in storage)
    fn prepend_raw(&self) -> String {
        self.shims_dir.to_string_lossy().into_owned()
    }

    /// Whether a (possibly unexpanded) PATH item belongs to uvman: it equals
    /// the shims dir, or starts with it followed by a separator (a user
    /// tweaked a sub-path inside shims/). Comparison on expanded, case-folded
    /// forms.
    fn owns_item(&self, item: &str) -> bool {
        let item = fold(&expanded(item));
        let owned = fold(&expanded(&self.prepend_raw()));
        item == owned || item.starts_with(&format!("{owned}{}", std::path::MAIN_SEPARATOR))
    }

    fn write_new(&self, items: &[String], kind: PathValueKind) -> Result<(), UError> {
        let value = StoredPath { raw: join_items(items), kind };
        self.store.write(&value)?;
        self.store.broadcast();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Platform backends
// ---------------------------------------------------------------------------

/// Non-Windows default: the user PATH is shell-owned; nothing to manage.
#[cfg(not(windows))]
pub struct NoopPathStore;

#[cfg(not(windows))]
impl PathStore for NoopPathStore {
    fn read(&self) -> Result<Option<StoredPath>, UError> {
        Err(UError::SimpleError(
            "the user PATH is managed by the shell on this platform; \
             use `uvman activate` instead".into(),
        ))
    }
    fn write(&self, _value: &StoredPath) -> Result<(), UError> {
        Err(UError::SimpleError(
            "the user PATH is managed by the shell on this platform".into(),
        ))
    }
    fn broadcast(&self) {}
}

/// Construct the production store: registry-backed on Windows, a no-op
/// placeholder elsewhere.
pub fn native_store() -> Box<dyn PathStore> {
    #[cfg(windows)]
    {
        Box::new(crate::core::pathstore::registry::RegistryPathStore)
    }
    #[cfg(not(windows))]
    {
        Box::new(NoopPathStore)
    }
}

/// Windows registry backend (`HKCU\Environment\Path`).
#[cfg(windows)]
pub mod registry {
    use super::*;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_SZ};
    use winreg::{RegKey, RegValue};

    const ENV_KEY: &str = "Environment";
    const PATH_VALUE: &str = "Path";

    pub struct RegistryPathStore;

    impl PathStore for RegistryPathStore {
        fn read(&self) -> Result<Option<StoredPath>, UError> {
            let key = open_key(KEY_READ).map_err(registry_error)?;
            match key.get_raw_value(PATH_VALUE) {
                Ok(value) => {
                    let kind = if value.vtype == REG_EXPAND_SZ {
                        PathValueKind::Expandable
                    } else {
                        PathValueKind::Plain
                    };
                    let raw = utf16_from_bytes(&value.bytes);
                    Ok(Some(StoredPath { raw, kind }))
                },
                // "value not set" is a valid empty state, not an error
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(source) => Err(UError::FileError { path: Path::new(ENV_KEY).into(), source }),
            }
        }

        fn write(&self, value: &StoredPath) -> Result<(), UError> {
            let key = open_key(KEY_WRITE).map_err(registry_error)?;
            let vtype = match value.kind {
                PathValueKind::Expandable => REG_EXPAND_SZ,
                PathValueKind::Plain => REG_SZ,
            };
            key.set_raw_value(PATH_VALUE, &RegValue { vtype, bytes: encode_utf16(&value.raw) })
                .map_err(|source| registry_error(source))?;
            Ok(())
        }

        fn broadcast(&self) {
            // Post WM_SETTINGCHANGE asking running apps to re-read the
            // environment (Explorer forwards it to child processes on next
            // launch; send_timeout so a hung window can't stall the command)
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE, HWND_BROADCAST,
            };
            let wide: Vec<u16> = "Environment".encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                let _ = SendMessageTimeoutW(
                    HWND_BROADCAST,
                    WM_SETTINGCHANGE,
                    0,
                    wide.as_ptr() as isize,
                    SMTO_ABORTIFHUNG,
                    1000,
                    std::ptr::null_mut(),
                );
            }
        }
    }

    fn open_key(access: u32) -> Result<RegKey, std::io::Error> {
        RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(ENV_KEY, access)
    }

    fn registry_error(source: std::io::Error) -> UError {
        UError::FileError { path: Path::new(ENV_KEY).into(), source }
    }

    /// Decode a UTF-16LE registry buffer (REG_EXPAND_SZ / REG_SZ) to a String;
    /// NUL terminator and any trailing garbage after it are dropped.
    fn utf16_from_bytes(bytes: &[u8]) -> String {
        let wide: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        String::from_utf16_lossy(&wide)
    }

    /// Encode a String as NUL-terminated UTF-16LE (registry string layout)
    fn encode_utf16(text: &str) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(text.len() * 2 + 2);
        for unit in text.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;

    /// In-memory store for behavior tests on every platform (shared with
    /// other modules' tests, e.g. the doctor shims check)
    #[derive(Default)]
    pub struct MemoryStore(pub RefCell<Option<StoredPath>>);

    impl PathStore for MemoryStore {
        fn read(&self) -> Result<Option<StoredPath>, UError> {
            Ok(self.0.borrow().clone())
        }
        fn write(&self, value: &StoredPath) -> Result<(), UError> {
            *self.0.borrow_mut() = Some(value.clone());
            Ok(())
        }
        fn broadcast(&self) {}
    }

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn test_split_and_join_roundtrip() {
        let raw = if cfg!(windows) {
            r"C:\Windows;C:\Users\me\bin;C:\Program Files;"
        } else {
            "/usr/bin:/bin:/usr/local/bin"
        };
        let items = split_items(raw);
        assert!(items.len() >= 3);
        assert_eq!(join_items(&items), raw.trim_end_matches(';'));
    }

    #[test]
    fn test_expanded_substitutes_known_vars() {
        // %PATH% etc. may be absent in test envs; use a var we control
        unsafe { std::env::set_var("UVMAN_TEST_EXPAND", "E:/w") };
        assert_eq!(expanded("%UVMAN_TEST_EXPAND%/x"), "E:/w/x");
        unsafe { std::env::remove_var("UVMAN_TEST_EXPAND") };
    }

    #[test]
    fn test_enable_prepends_and_is_idempotent() {
        let dir = home();
        let store = MemoryStore(RefCell::new(Some(StoredPath {
            raw: "%USERPROFILE%\\bin".to_string(),
            kind: PathValueKind::Expandable,
        })));
        let shims = dir.path().join("shims");
        let backup = dir.path().join("backup");
        let editor = PathEditor::new(&store, &shims, &backup);

        assert_eq!(editor.enable().unwrap(), EnableReport::Enabled);
        let stored = store.0.borrow().clone().unwrap();
        assert!(stored.raw.starts_with(shims.to_str().unwrap()), "shims dir first: {}", stored.raw);
        // The pre-existing item survived after the shims dir
        assert!(stored.raw.contains("%USERPROFILE%\\bin"));
        // The pre-edit value was backed up
        assert!(backup.join(BACKUP_FILE).is_file());

        // Second enable is a no-op (idempotent)
        assert_eq!(editor.enable().unwrap(), EnableReport::AlreadyEnabled);
    }

    #[test]
    fn test_enable_with_no_existing_path_starts_fresh() {
        let dir = home();
        let store = MemoryStore::default();
        let shims = dir.path().join("shims");
        let backup = dir.path().join("backup");
        let editor = PathEditor::new(&store, &shims, &backup);

        assert_eq!(editor.enable().unwrap(), EnableReport::Enabled);
        let stored = store.0.borrow().clone().unwrap();
        assert_eq!(split_items(&stored.raw), vec![shims.to_string_lossy().into_owned()]);
        // No prior value existed, so nothing was backed up
        assert!(!backup.join(BACKUP_FILE).exists());
    }

    #[test]
    fn test_enable_keeps_existing_items_and_kind() {
        let dir = home();
        let existing = StoredPath {
            raw: "%USERPROFILE%\\bin;".to_string(),
            kind: PathValueKind::Expandable,
        };
        let store = MemoryStore(RefCell::new(Some(existing)));
        let shims = dir.path().join("shims");
        let backup = dir.path().join("backup");
        let editor = PathEditor::new(&store, &shims, &backup);

        editor.enable().unwrap();
        let saved = store.0.borrow().clone().unwrap();
        assert_eq!(saved.kind, PathValueKind::Expandable, "type fidelity");
        // The unexpanded %USERPROFILE% token round-trips untouched
        assert!(saved.raw.contains("%USERPROFILE%\\bin"));
        assert!(saved.raw.starts_with(shims.to_str().unwrap()));
    }

    #[cfg(windows)]
    #[test]
    fn test_enable_detects_existing_expanded_entry() {
        let dir = home();
        // User already put %UVMAN_HOME% (which resolves to a dir ending in
        // shims) on PATH: enable must no-op instead of duplicating
        unsafe { std::env::set_var("UVMAN_HOME_FOR_TEST", dir.path().to_str().unwrap()) };
        let existing = StoredPath {
            raw: format!("%UVMAN_HOME_FOR_TEST%\\shims;"),
            kind: PathValueKind::Expandable,
        };
        let store = MemoryStore(RefCell::new(Some(existing)));
        let shims = dir.path().join("shims");
        let backup = dir.path().join("backup");
        let editor = PathEditor::new(&store, &shims, &backup);
        assert_eq!(editor.enable().unwrap(), EnableReport::AlreadyEnabled);
        unsafe { std::env::remove_var("UVMAN_HOME_FOR_TEST") };
    }

    #[test]
    fn test_disable_removes_only_owned_items() {
        let dir = home();
        let shims_display = dir.path().join("shims").to_string_lossy().into_owned();
        let foreign = if cfg!(windows) { r"C:\Windows\System32".to_string() } else { "/usr/bin".to_string() };
        let store = MemoryStore(RefCell::new(Some(StoredPath {
            raw: format!("{shims_display};{foreign}"),
            kind: PathValueKind::Plain,
        })));
        let shims = dir.path().join("shims");
        let backup = dir.path().join("backup");
        let editor = PathEditor::new(&store, &shims, &backup);

        assert_eq!(editor.disable().unwrap(), DisableReport::Disabled);
        let kept = store.0.borrow().clone().unwrap();
        assert_eq!(kept.raw, foreign, "foreign entries untouched");
        // Disabling again is a no-op
        assert_eq!(editor.disable().unwrap(), DisableReport::AlreadyDisabled);
    }

    #[test]
    fn test_in_path_after_enable() {
        let dir = home();
        let store = MemoryStore::default();
        let shims = dir.path().join("shims");
        let backup = dir.path().join("backup");
        let editor = PathEditor::new(&store, &shims, &backup);
        assert!(!editor.in_path().unwrap());
        editor.enable().unwrap();
        assert!(editor.in_path().unwrap());
    }
}