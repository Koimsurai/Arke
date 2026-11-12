//! "Open folders in Arke" at the shell level: registers Arke as the folder handler for the current
//! Windows account, and undoes it exactly.
//!
//! The per-user way the Files app uses (`HKCU` only, nothing outside `Software\Classes`):
//! - `Folder\shell\open\command` and `Folder\shell\explore\command` run `"<arke.exe>" "%1"`, with
//!   an empty `DelegateExecute` so the shell runs the command line instead of its stock handler.
//!   Every way the shell opens a folder, including `SHOpenFolderAndSelectItems` ("Show in folder" of
//!   browsers and editors), goes through these verbs, so no File Explorer window is created.
//! - Win+E: `CLSID\{52205fd8-...}\shell\opennewwindow\command` with an empty `DelegateExecute`
//!   runs `"<arke.exe>" --home`.
//!
//! `Folder` also covers Control Panel and other virtual folders, so `arke.exe` hands everything it
//! cannot show to `explorer.exe` (see `takeover::route_argument`).
//!
//! Everything goes through the [`Registry`] trait (the app implements it with the Win32 registry
//! API; tests use [`MemoryRegistry`]) and the [`BackupStore`] trait (the previous values are saved
//! to `%APPDATA%\Arke\folder-handler-backup.json` before the first write). Turning it off puts
//! every value back exactly as it was and deletes only the keys Arke created. All operations are
//! idempotent and safe to repeat after a partial run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Where every key Arke touches lives, relative to `HKEY_CURRENT_USER`.
pub const CLASSES: &str = r"Software\Classes";
/// File name of the backup, inside Arke's settings folder.
pub const BACKUP_FILE: &str = "folder-handler-backup.json";
/// `REG_SZ`.
pub const REG_SZ: u32 = 1;

/// The Win+E handler's class id.
const WIN_E_CLSID: &str = "{52205fd8-5dfb-447d-801a-d0b52f2e83e1}";
/// Verbs of the `Folder` class that open a folder.
const FOLDER_VERBS: [&str; 2] = ["open", "explore"];

/// Why registering or restoring failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A registry call failed.
    #[error("registry: {0}")]
    Registry(String),
    /// The backup could not be read or written.
    #[error("backup: {0}")]
    Backup(String),
    /// A key outside `HKCU\Software\Classes` was about to be touched.
    #[error("refusing to touch a key outside Software\\Classes: {0}")]
    Outside(String),
}

/// A registry value as stored: its type and raw bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegValue {
    /// `REG_*` type.
    pub kind: u32,
    /// Raw data.
    #[serde(with = "hex")]
    pub data: Vec<u8>,
}

impl RegValue {
    /// A `REG_SZ` value.
    #[must_use]
    pub fn sz(text: &str) -> Self {
        let data = text
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        Self { kind: REG_SZ, data }
    }

    /// The text of a `REG_SZ` value (without the terminator).
    #[must_use]
    pub fn as_sz(&self) -> Option<String> {
        if self.kind != REG_SZ || !self.data.len().is_multiple_of(2) {
            return None;
        }
        let units: Vec<u16> = self
            .data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        String::from_utf16(&units[..end]).ok()
    }
}

/// Raw data as a hex string in the JSON backup.
mod hex {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::fmt::Write as _;

    pub(super) fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut text = String::with_capacity(data.len() * 2);
        for b in data {
            let _ = write!(text, "{b:02x}");
        }
        s.serialize_str(&text)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        if !text.len().is_multiple_of(2) || !text.is_ascii() {
            return Err(serde::de::Error::custom("bad hex"));
        }
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect()
    }
}

/// The per-user registry, keys relative to `HKEY_CURRENT_USER`. The value name `""` is a key's
/// default value.
pub trait Registry: Send + Sync {
    /// The value, or `None` when the key or the value does not exist.
    ///
    /// # Errors
    /// The registry call failed.
    fn read(&self, key: &str, value: &str) -> Result<Option<RegValue>, Error>;
    /// Whether the key exists.
    ///
    /// # Errors
    /// The registry call failed.
    fn key_exists(&self, key: &str) -> Result<bool, Error>;
    /// Writes the value, creating the key (and its parents) when needed.
    ///
    /// # Errors
    /// The registry call failed.
    fn write(&self, key: &str, value: &str, data: &RegValue) -> Result<(), Error>;
    /// Deletes the value; not an error when it is already gone.
    ///
    /// # Errors
    /// The registry call failed.
    fn delete_value(&self, key: &str, value: &str) -> Result<(), Error>;
    /// Deletes the key when it has no values and no subkeys. `Ok(true)` when it is gone
    /// afterwards, `Ok(false)` when something is still in it.
    ///
    /// # Errors
    /// The registry call failed.
    fn delete_key_if_empty(&self, key: &str) -> Result<bool, Error>;
}

/// Where the backup of the previous values is kept.
pub trait BackupStore: Send + Sync {
    /// The saved backup, `None` when there is none (or it was unreadable and set aside).
    ///
    /// # Errors
    /// The store failed.
    fn load(&self) -> Result<Option<Backup>, Error>;
    /// Saves the backup, replacing the old one.
    ///
    /// # Errors
    /// The store failed.
    fn save(&self, backup: &Backup) -> Result<(), Error>;
    /// Deletes the backup.
    ///
    /// # Errors
    /// The store failed.
    fn remove(&self) -> Result<(), Error>;
}

/// How to recognise a value Arke wrote (as opposed to one somebody else set).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pattern {
    /// Exactly this text.
    Literal(String),
    /// `"<some arke.exe>"` followed by this suffix.
    Command(String),
}

impl Pattern {
    /// Whether `value` is something Arke wrote.
    #[must_use]
    pub fn matches(&self, value: &RegValue) -> bool {
        let Some(text) = value.as_sz() else {
            return false;
        };
        match self {
            Self::Literal(expected) => text == *expected,
            Self::Command(suffix) => command_exe(&text, suffix)
                .and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().to_lowercase()))
                .is_some_and(|name| name == "arke.exe"),
        }
    }
}

/// The exe path inside `"<exe>"<suffix>`.
fn command_exe(text: &str, suffix: &str) -> Option<PathBuf> {
    let rest = text.strip_prefix('"')?;
    let (exe, tail) = rest.split_once('"')?;
    (tail == suffix).then(|| PathBuf::from(exe))
}

/// The exe a registered command points at, if `text` is a command Arke wrote.
#[must_use]
pub fn registered_exe(text: &str) -> Option<PathBuf> {
    [r#" "%1""#, " --home"]
        .iter()
        .find_map(|suffix| command_exe(text, suffix))
}

/// One value Arke manages.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Spec {
    key: String,
    value: &'static str,
    data: String,
    pattern: Pattern,
}

fn classes_key(rest: &str) -> String {
    format!(r"{CLASSES}\{rest}")
}

/// Every value Arke writes for the exe at `exe`.
fn specs(exe: &Path) -> Vec<Spec> {
    let exe = exe.display();
    let mut out = Vec::new();
    for verb in FOLDER_VERBS {
        let key = classes_key(&format!(r"Folder\shell\{verb}\command"));
        out.push(Spec {
            key: key.clone(),
            value: "",
            data: format!(r#""{exe}" "%1""#),
            pattern: Pattern::Command(r#" "%1""#.into()),
        });
        // Empty on purpose: the shell then runs the default value instead of the stock handler.
        out.push(Spec {
            key,
            value: "DelegateExecute",
            data: String::new(),
            pattern: Pattern::Literal(String::new()),
        });
    }
    let win_e = classes_key(&format!(r"CLSID\{WIN_E_CLSID}\shell\opennewwindow\command"));
    out.push(Spec {
        key: win_e.clone(),
        value: "",
        data: format!(r#""{exe}" --home"#),
        pattern: Pattern::Command(" --home".into()),
    });
    // Empty on purpose: the shell then runs the default value instead of the stock handler.
    out.push(Spec {
        key: win_e,
        value: "DelegateExecute",
        data: String::new(),
        pattern: Pattern::Literal(String::new()),
    });
    out
}

/// `key` and every key above it that lies below `Software\Classes`, top first.
fn chain(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(next) = key[at..].find('\\') {
        at += next;
        if at > CLASSES.len() {
            out.push(key[..at].to_owned());
        }
        at += 1;
    }
    out.push(key.to_owned());
    out
}

fn check_allowed(key: &str) -> Result<(), Error> {
    let prefix = format!(r"{CLASSES}\");
    if key.len() > prefix.len()
        && key.is_char_boundary(prefix.len())
        && key[..prefix.len()].eq_ignore_ascii_case(&prefix)
    {
        Ok(())
    } else {
        Err(Error::Outside(key.to_owned()))
    }
}

/// What was there before Arke wrote a value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupEntry {
    /// Key, relative to `HKEY_CURRENT_USER`.
    pub key: String,
    /// Value name (`""` is the default value).
    pub value: String,
    /// How to recognise what Arke wrote.
    pub ours: Pattern,
    /// The value that was there; `None` when there was none.
    pub prior: Option<RegValue>,
}

/// Everything needed to undo the registration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backup {
    /// Format version.
    pub version: u32,
    /// Values Arke manages and what they were.
    pub entries: Vec<BackupEntry>,
    /// Keys that did not exist before and were created by Arke.
    pub created_keys: Vec<String>,
}

/// What [`apply`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Values written.
    pub written: usize,
    /// Of those, commands that pointed to another (moved or deleted) Arke exe.
    pub repaired: usize,
}

/// What [`restore`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// Values put back or removed.
    pub values: usize,
    /// Values left alone because somebody else changed them after Arke.
    pub left_alone: usize,
    /// There was a backup (otherwise Arke's own values were removed without one).
    pub had_backup: bool,
}

/// Registers Arke (at `exe`) as the folder handler. Safe to call again: only what differs is
/// written, which also repairs a moved exe.
///
/// # Errors
/// A registry or backup call failed. The backup is saved before the first write, so [`restore`]
/// can always undo a partial run.
pub fn apply(reg: &dyn Registry, store: &dyn BackupStore, exe: &Path) -> Result<Applied, Error> {
    let specs = specs(exe);
    for spec in &specs {
        check_allowed(&spec.key)?;
    }
    let mut backup = store.load()?.unwrap_or_else(|| Backup {
        version: 1,
        ..Backup::default()
    });
    let mut dirty = false;
    for spec in &specs {
        let current = reg.read(&spec.key, spec.value)?;
        // What to put back later: whatever is there that Arke didn't write.
        let foreign = current.filter(|v| !spec.pattern.matches(v));
        let known = backup
            .entries
            .iter_mut()
            .find(|e| e.key == spec.key && e.value == spec.value);
        if let Some(entry) = known {
            if foreign.is_some() && entry.prior != foreign {
                // Changed by someone else since: that is the original now.
                entry.prior = foreign;
                dirty = true;
            }
            if entry.ours != spec.pattern {
                entry.ours = spec.pattern.clone();
                dirty = true;
            }
        } else {
            backup.entries.push(BackupEntry {
                key: spec.key.clone(),
                value: spec.value.to_owned(),
                ours: spec.pattern.clone(),
                prior: foreign,
            });
            dirty = true;
        }
        for key in chain(&spec.key) {
            if !backup.created_keys.contains(&key) && !reg.key_exists(&key)? {
                backup.created_keys.push(key);
                dirty = true;
            }
        }
    }
    if dirty {
        store.save(&backup)?;
    }
    let mut applied = Applied::default();
    for spec in &specs {
        let desired = RegValue::sz(&spec.data);
        let current = reg.read(&spec.key, spec.value)?;
        if current.as_ref() == Some(&desired) {
            continue;
        }
        if current
            .as_ref()
            .and_then(RegValue::as_sz)
            .and_then(|t| registered_exe(&t))
            .is_some()
        {
            applied.repaired += 1;
        }
        reg.write(&spec.key, spec.value, &desired)?;
        applied.written += 1;
    }
    Ok(applied)
}

/// Puts everything back as it was before [`apply`] and removes the backup. Values somebody else
/// changed after Arke are left alone. Without a backup, only Arke's own values are removed.
///
/// # Errors
/// A registry or backup call failed; the backup is kept so it can be retried.
pub fn restore(reg: &dyn Registry, store: &dyn BackupStore) -> Result<Restored, Error> {
    let mut done = Restored::default();
    let backup = store.load()?;
    done.had_backup = backup.is_some();
    let (entries, created) = if let Some(b) = backup {
        (b.entries, b.created_keys)
    } else {
        // No record of the originals: assume there were none.
        let entries: Vec<BackupEntry> = specs(Path::new("arke.exe"))
            .into_iter()
            .map(|s| BackupEntry {
                key: s.key,
                value: s.value.to_owned(),
                ours: s.pattern,
                prior: None,
            })
            .collect();
        let created = entries.iter().flat_map(|e| chain(&e.key)).collect();
        (entries, created)
    };
    for entry in &entries {
        check_allowed(&entry.key)?;
        let current = reg.read(&entry.key, &entry.value)?;
        if !current.as_ref().is_none_or(|v| entry.ours.matches(v)) {
            done.left_alone += 1;
            continue;
        }
        match (&entry.prior, current) {
            (Some(prior), current) if current.as_ref() != Some(prior) => {
                reg.write(&entry.key, &entry.value, prior)?;
                done.values += 1;
            }
            (None, Some(_)) => {
                reg.delete_value(&entry.key, &entry.value)?;
                done.values += 1;
            }
            _ => {}
        }
    }
    // Deepest first; a key that still holds anything stays.
    let mut keys: Vec<String> = created;
    keys.sort_by_key(|k| std::cmp::Reverse(k.matches('\\').count()));
    for key in &keys {
        check_allowed(key)?;
        reg.delete_key_if_empty(key)?;
    }
    store.remove()?;
    Ok(done)
}

/// A [`Registry`] in memory, for tests.
#[derive(Default)]
pub struct MemoryRegistry {
    /// Key (lower case) to its values (names lower case).
    state: Mutex<BTreeMap<String, BTreeMap<String, RegValue>>>,
    /// Number of `write` calls.
    writes: Mutex<usize>,
}

impl MemoryRegistry {
    /// Number of writes so far.
    #[must_use]
    pub fn writes(&self) -> usize {
        *self.writes.lock()
    }

    /// Sets a string value directly (test setup; creates parent keys).
    pub fn put(&self, key: &str, value: &str, text: &str) {
        self.insert(key, value, RegValue::sz(text));
    }

    /// Sets a raw value directly (test setup; creates parent keys).
    pub fn put_raw(&self, key: &str, value: &str, data: RegValue) {
        self.insert(key, value, data);
    }

    /// Creates an empty key and its parents.
    pub fn make_key(&self, key: &str) {
        let mut state = self.state.lock();
        for k in chain(key) {
            state.entry(k.to_lowercase()).or_default();
        }
    }

    /// The text of a string value.
    #[must_use]
    pub fn text(&self, key: &str, value: &str) -> Option<String> {
        self.read(key, value).ok().flatten().and_then(|v| v.as_sz())
    }

    /// Every key and value as `(key, name, text)`, sorted, for comparing whole states. A key shows
    /// up as `(key, "<key>", "")`.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, String, String)> {
        let state = self.state.lock();
        let mut out = Vec::new();
        for (key, values) in state.iter() {
            out.push((key.clone(), "<key>".into(), String::new()));
            for (name, v) in values {
                out.push((
                    key.clone(),
                    name.clone(),
                    v.as_sz().unwrap_or_else(|| format!("{:?}", v.data)),
                ));
            }
        }
        drop(state);
        out
    }

    fn insert(&self, key: &str, value: &str, data: RegValue) {
        let mut state = self.state.lock();
        for k in chain(key) {
            state.entry(k.to_lowercase()).or_default();
        }
        state
            .entry(key.to_lowercase())
            .or_default()
            .insert(value.to_lowercase(), data);
    }
}

impl Registry for MemoryRegistry {
    fn read(&self, key: &str, value: &str) -> Result<Option<RegValue>, Error> {
        Ok(self
            .state
            .lock()
            .get(&key.to_lowercase())
            .and_then(|v| v.get(&value.to_lowercase()))
            .cloned())
    }

    fn key_exists(&self, key: &str) -> Result<bool, Error> {
        Ok(self.state.lock().contains_key(&key.to_lowercase()))
    }

    fn write(&self, key: &str, value: &str, data: &RegValue) -> Result<(), Error> {
        *self.writes.lock() += 1;
        self.insert(key, value, data.clone());
        Ok(())
    }

    fn delete_value(&self, key: &str, value: &str) -> Result<(), Error> {
        if let Some(values) = self.state.lock().get_mut(&key.to_lowercase()) {
            values.remove(&value.to_lowercase());
        }
        Ok(())
    }

    fn delete_key_if_empty(&self, key: &str) -> Result<bool, Error> {
        let mut state = self.state.lock();
        let lower = key.to_lowercase();
        let Some(values) = state.get(&lower) else {
            return Ok(true);
        };
        let child = format!("{lower}\\");
        if !values.is_empty() || state.keys().any(|k| k.starts_with(&child)) {
            return Ok(false);
        }
        state.remove(&lower);
        drop(state);
        Ok(true)
    }
}

/// A [`BackupStore`] in memory, for tests.
#[derive(Default)]
pub struct MemoryBackup(pub Mutex<Option<Backup>>);

impl BackupStore for MemoryBackup {
    fn load(&self) -> Result<Option<Backup>, Error> {
        Ok(self.0.lock().clone())
    }

    fn save(&self, backup: &Backup) -> Result<(), Error> {
        *self.0.lock() = Some(backup.clone());
        Ok(())
    }

    fn remove(&self) -> Result<(), Error> {
        *self.0.lock() = None;
        Ok(())
    }
}

/// The backup as a JSON file.
pub struct FileBackup(pub PathBuf);

impl BackupStore for FileBackup {
    fn load(&self) -> Result<Option<Backup>, Error> {
        let bytes = match std::fs::read(&self.0) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::Backup(err.to_string())),
        };
        if let Ok(backup) = serde_json::from_slice::<Backup>(&bytes) {
            return Ok(Some(backup));
        }
        // Unreadable: keep it for the user and carry on as if there were none.
        let aside = self.0.with_extension("json.bad");
        log::warn!(
            "unreadable folder handler backup, moved to {}",
            aside.display()
        );
        let _ = std::fs::rename(&self.0, aside);
        Ok(None)
    }

    fn save(&self, backup: &Backup) -> Result<(), Error> {
        let io = |e: std::io::Error| Error::Backup(e.to_string());
        if let Some(dir) = self.0.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let json = serde_json::to_vec_pretty(backup).map_err(|e| Error::Backup(e.to_string()))?;
        let tmp = self.0.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(io)?;
        std::fs::rename(&tmp, &self.0).map_err(io)
    }

    fn remove(&self) -> Result<(), Error> {
        match std::fs::remove_file(&self.0) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                Err(Error::Backup(err.to_string()))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Apps\Arke\arke.exe";
    const OPEN_CMD: &str = r"Software\Classes\Folder\shell\open\command";
    const EXPLORE_CMD: &str = r"Software\Classes\Folder\shell\explore\command";
    const WIN_E_CMD: &str = r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command";
    const STOCK_DELEGATE: &str = "{11dbb47c-a525-400b-9e80-a54615a090c0}";

    fn exe() -> &'static Path {
        Path::new(EXE)
    }

    fn cmd(exe: &str) -> String {
        format!(r#""{exe}" "%1""#)
    }

    #[test]
    fn writes_the_folder_verbs_and_win_e_and_nothing_else() {
        let reg = MemoryRegistry::default();
        let backup = MemoryBackup::default();
        let applied = apply(&reg, &backup, exe()).unwrap();
        assert_eq!(
            applied,
            Applied {
                written: 6,
                repaired: 0
            }
        );
        for key in [OPEN_CMD, EXPLORE_CMD] {
            assert_eq!(reg.text(key, "").unwrap(), cmd(EXE));
            assert_eq!(reg.text(key, "DelegateExecute").as_deref(), Some(""));
        }
        assert_eq!(
            reg.text(WIN_E_CMD, "").as_deref(),
            Some(r#""C:\Apps\Arke\arke.exe" --home"#)
        );
        assert_eq!(reg.text(WIN_E_CMD, "DelegateExecute").as_deref(), Some(""));
        for (key, _, _) in reg.snapshot() {
            assert!(
                key.starts_with(r"software\classes\folder")
                    || key.starts_with(r"software\classes\clsid"),
                "{key} is outside the Folder verbs and the Win+E class"
            );
            assert!(
                !key.contains(r"\directory") && !key.contains(r"\drive"),
                "Directory and Drive stay untouched"
            );
        }
    }

    #[test]
    fn restore_after_a_clean_apply_leaves_no_trace() {
        let reg = MemoryRegistry::default();
        let backup = MemoryBackup::default();
        apply(&reg, &backup, exe()).unwrap();
        assert!(backup.load().unwrap().is_some());
        let done = restore(&reg, &backup).unwrap();
        assert!(done.had_backup);
        assert_eq!(done.left_alone, 0);
        assert!(reg.snapshot().is_empty(), "{:?}", reg.snapshot());
        assert!(backup.load().unwrap().is_none(), "backup removed");
    }

    #[test]
    fn restore_puts_back_what_another_file_manager_had_set() {
        let reg = MemoryRegistry::default();
        let backup = MemoryBackup::default();
        // Another file manager registered the same verbs (with a non-empty delegate on one).
        reg.put(OPEN_CMD, "", &cmd(r"D:\Other\fm.exe"));
        reg.put(OPEN_CMD, "DelegateExecute", STOCK_DELEGATE);
        reg.put(EXPLORE_CMD, "", &cmd(r"D:\Other\fm.exe"));
        reg.put(WIN_E_CMD, "", r#""D:\Other\fm.exe""#);
        reg.make_key(r"Software\Classes\Folder");
        let before = reg.snapshot();
        apply(&reg, &backup, exe()).unwrap();
        assert_ne!(reg.snapshot(), before);
        restore(&reg, &backup).unwrap();
        assert_eq!(reg.snapshot(), before, "exactly the state before");
    }

    #[test]
    fn restore_keeps_non_string_values_exactly() {
        let dword = RegValue {
            kind: 4,
            data: vec![1, 0, 0, 0],
        };
        let reg = MemoryRegistry::default();
        let backup = MemoryBackup::default();
        reg.put_raw(OPEN_CMD, "DelegateExecute", dword.clone());
        apply(&reg, &backup, exe()).unwrap();
        restore(&reg, &backup).unwrap();
        assert_eq!(reg.read(OPEN_CMD, "DelegateExecute").unwrap(), Some(dword));
    }

    #[test]
    fn applying_twice_writes_nothing_the_second_time() {
        let reg = MemoryRegistry::default();
        let backup = MemoryBackup::default();
        reg.put(OPEN_CMD, "DelegateExecute", STOCK_DELEGATE);
        apply(&reg, &backup, exe()).unwrap();
        let first = backup.load().unwrap();
        let writes = reg.writes();
        let again = apply(&reg, &backup, exe()).unwrap();
        assert_eq!(again, Applied::default());
        assert_eq!(reg.writes(), writes, "no registry writes");
        assert_eq!(backup.load().unwrap(), first, "the originals are kept");
        // The original survives any number of applies.
        restore(&reg, &backup).unwrap();
        assert_eq!(
            reg.text(OPEN_CMD, "DelegateExecute").as_deref(),
            Some(STOCK_DELEGATE)
        );
    }

    #[test]
    fn a_moved_exe_is_repaired_at_the_next_apply() {
        let reg = MemoryRegistry::default();
        let backup = MemoryBackup::default();
        apply(&reg, &backup, exe()).unwrap();
        let moved = Path::new(r"E:\Portable\arke.exe");
        let applied = apply(&reg, &backup, moved).unwrap();
        assert_eq!(
            applied,
            Applied {
                written: 3,
                repaired: 3
            }
        );
        assert_eq!(
            reg.text(EXPLORE_CMD, "").unwrap(),
            cmd(r"E:\Portable\arke.exe")
        );
        assert_eq!(
            reg.text(WIN_E_CMD, "").as_deref(),
            Some(r#""E:\Portable\arke.exe" --home"#)
        );
        // The repair did not turn Arke's old command into the "original".
        restore(&reg, &backup).unwrap();
        assert!(reg.snapshot().is_empty(), "{:?}", reg.snapshot());
    }

    #[test]
    fn a_partial_state_is_completed_and_still_restores() {
        let reg = MemoryRegistry::default();
        let backup = MemoryBackup::default();
        reg.put(EXPLORE_CMD, "", &cmd(r"D:\Other\fm.exe"));
        let before = reg.snapshot();
        apply(&reg, &backup, exe()).unwrap();
        // Some of it went missing again (an interrupted run, a cleanup tool).
        reg.delete_value(WIN_E_CMD, "DelegateExecute").unwrap();
        reg.delete_value(OPEN_CMD, "").unwrap();
        let fixed = apply(&reg, &backup, exe()).unwrap();
        assert_eq!(fixed.written, 2);
        assert_eq!(reg.text(WIN_E_CMD, "DelegateExecute").as_deref(), Some(""));
        assert_eq!(reg.text(OPEN_CMD, "").unwrap(), cmd(EXE));
        restore(&reg, &backup).unwrap();
        assert_eq!(reg.snapshot(), before);
    }

    #[test]
    fn a_backup_with_no_writes_after_it_restores_cleanly() {
        let reg = MemoryRegistry::default();
        let backup = MemoryBackup::default();
        reg.put(OPEN_CMD, "DelegateExecute", STOCK_DELEGATE);
        let before = reg.snapshot();
        apply(&reg, &backup, exe()).unwrap();
        // As if the process died right after saving the backup: undo every write.
        for spec in specs(exe()) {
            if spec.key == OPEN_CMD && spec.value == "DelegateExecute" {
                reg.put(OPEN_CMD, "DelegateExecute", STOCK_DELEGATE);
            } else {
                reg.delete_value(&spec.key, spec.value).unwrap();
            }
        }
        restore(&reg, &backup).unwrap();
        assert_eq!(reg.snapshot(), before);
    }

    #[test]
    fn something_changed_by_another_program_is_not_clobbered_on_restore() {
        let reg = MemoryRegistry::default();
        let backup = MemoryBackup::default();
        apply(&reg, &backup, exe()).unwrap();
        reg.put(EXPLORE_CMD, "", &cmd(r"D:\Newer\fm.exe"));
        let done = restore(&reg, &backup).unwrap();
        assert_eq!(done.left_alone, 1);
        assert_eq!(reg.text(EXPLORE_CMD, "").unwrap(), cmd(r"D:\Newer\fm.exe"));
        assert!(reg.text(OPEN_CMD, "").is_none(), "the rest is still undone");
    }

    #[test]
    fn a_foreign_change_after_apply_becomes_the_new_original() {
        let reg = MemoryRegistry::default();
        let backup = MemoryBackup::default();
        apply(&reg, &backup, exe()).unwrap();
        reg.put(OPEN_CMD, "", &cmd(r"D:\Newer\fm.exe"));
        apply(&reg, &backup, exe()).unwrap();
        restore(&reg, &backup).unwrap();
        assert_eq!(reg.text(OPEN_CMD, "").unwrap(), cmd(r"D:\Newer\fm.exe"));
    }

    #[test]
    fn restore_without_a_backup_removes_only_arkes_own_values() {
        let reg = MemoryRegistry::default();
        let backup = MemoryBackup::default();
        apply(&reg, &backup, exe()).unwrap();
        *backup.0.lock() = None;
        // Someone else's verb in the same class is untouched.
        reg.put(r"Software\Classes\Folder\shell\other\command", "", "x");
        let done = restore(&reg, &backup).unwrap();
        assert!(!done.had_backup);
        let keys: Vec<String> = reg.snapshot().into_iter().map(|(k, _, _)| k).collect();
        assert!(
            keys.iter()
                .all(|k| k.starts_with(r"software\classes\folder"))
        );
        assert!(keys.contains(&r"software\classes\folder\shell\other\command".into()));
        assert!(reg.text(OPEN_CMD, "DelegateExecute").is_none());
        assert!(reg.text(OPEN_CMD, "").is_none());
        // Nothing to do at all is fine too.
        restore(&MemoryRegistry::default(), &MemoryBackup::default()).unwrap();
    }

    #[test]
    fn patterns_recognise_only_arke_commands() {
        let cmd_pattern = Pattern::Command(r#" "%1""#.into());
        assert!(cmd_pattern.matches(&RegValue::sz(&cmd(r"C:\x y\Arke.EXE"))));
        assert!(!cmd_pattern.matches(&RegValue::sz(&cmd(r"C:\x\other.exe"))));
        assert!(!cmd_pattern.matches(&RegValue::sz(r#""C:\x\arke.exe" --home"#)));
        assert!(!cmd_pattern.matches(&RegValue::sz(r"C:\x\arke.exe %1")));
        assert_eq!(
            registered_exe(r#""C:\a\arke.exe" --home"#),
            Some(PathBuf::from(r"C:\a\arke.exe"))
        );
    }

    #[test]
    fn keys_outside_classes_are_refused() {
        assert!(check_allowed(r"Software\Classes\Folder\shell").is_ok());
        assert!(check_allowed(r"Software\Microsoft\Windows\Run").is_err());
        assert!(check_allowed(r"Software\Classes").is_err());
        assert!(check_allowed(r"Software\ClassesEvil\x").is_err());
        assert!(check_allowed("Software\\Classe\u{e9}\\x").is_err());
    }

    #[test]
    fn the_backup_file_round_trips_and_a_broken_one_is_set_aside() {
        let dir = std::env::temp_dir().join(format!("arke-fh-test-{}", std::process::id()));
        let file = FileBackup(dir.join(BACKUP_FILE));
        assert!(file.load().unwrap().is_none());
        let reg = MemoryRegistry::default();
        reg.put(OPEN_CMD, "DelegateExecute", STOCK_DELEGATE);
        apply(&reg, &file, exe()).unwrap();
        let saved = file.load().unwrap().unwrap();
        assert_eq!(saved.version, 1);
        assert!(saved.entries.iter().any(|e| e.key == OPEN_CMD
            && e.value == "DelegateExecute"
            && e.prior == Some(RegValue::sz(STOCK_DELEGATE))));
        restore(&reg, &file).unwrap();
        assert!(!dir.join(BACKUP_FILE).exists());
        std::fs::write(dir.join(BACKUP_FILE), b"{ not json").unwrap();
        assert!(file.load().unwrap().is_none());
        assert!(dir.join("folder-handler-backup.json.bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
