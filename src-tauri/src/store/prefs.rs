//! User preferences in `prefs.json` (§9 「prefs.json」): `app` is defined by
//! the backend, `ui` by the frontend and stored verbatim. Not part of the
//! revision history or exports. A damaged file is quarantined and replaced by
//! defaults, so it never blocks rule loading. If it cannot be moved aside,
//! loading fails instead, so no caller can overwrite the only copy.
// Not wired to commands yet (phase 2: auto-listen, log level, hotkeys).
#![allow(dead_code)]

use super::atomic::atomic_json;
use crate::error::{file_label, CommandError, IoOp};
use quicker_platform::{
    clipboard_options::set_lease_options,
    traits::{LeaseOptions, RestorePolicy, DEFAULT_RESTORE_DELAY},
};
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::{
    fs, io,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const FILE: &str = "prefs.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub app: AppPrefs,
    /// Owned by the frontend; the backend only reads and writes it.
    pub ui: Value,
    /// Fields this version does not know (e.g. written by a newer version),
    /// kept and written back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            app: AppPrefs::default(),
            ui: Value::Object(Map::new()),
            extra: Map::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppPrefs {
    /// Defaults to false; plan appendix C.1 #11 is still undecided.
    pub auto_listen_on_start: bool,
    /// Temporary log level, in effect until `until_ms` (§7 logging).
    pub log_level: Option<LogLevelOverride>,
    /// How a paste restores the clipboard (B-08).
    pub clipboard: ClipboardPrefs,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogLevelOverride {
    pub level: String,
    /// Unix time in milliseconds.
    pub until_ms: i64,
    /// Fields this version does not know, kept and written back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Bounds of `app.clipboard.restore_delay_ms` (plan §6 settings table).
pub const RESTORE_DELAY_MS: RangeInclusive<u64> = 100..=2000;
const DEFAULT_RESTORE_DELAY_MS: u64 = DEFAULT_RESTORE_DELAY.as_millis() as u64;

/// `app.clipboard` (B-08), the lease options of every paste; set at startup
/// by [`apply_clipboard_prefs`]. No command or settings page edits it in 0.4.
///
/// Read field by field: a missing, mistyped, unknown or out-of-range value
/// takes its default and is logged (`store.prefs_invalid`), and never makes
/// the whole file count as damaged. A replaced value is written back as its
/// default the next time prefs are saved.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClipboardPrefs {
    /// `"full"`, `"best_effort"` (default) or `"text_only"`.
    pub restore_policy: RestorePolicy,
    /// Wait between the paste chord and the restore, within
    /// [`RESTORE_DELAY_MS`]; 300 by default.
    pub restore_delay_ms: u64,
    /// Fields this version does not know, kept and written back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for ClipboardPrefs {
    fn default() -> Self {
        Self {
            restore_policy: RestorePolicy::default(),
            restore_delay_ms: DEFAULT_RESTORE_DELAY_MS,
            extra: Map::new(),
        }
    }
}

impl<'de> Deserialize<'de> for ClipboardPrefs {
    /// Fails only where the JSON itself is invalid, which damages the file
    /// anyway; every value is checked by [`ClipboardPrefs::from_value`].
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::from_value)
    }
}

impl ClipboardPrefs {
    /// Runs inside serde, so inside every load of prefs.json
    /// ([`load_unlocked`]) with [`PREFS_FILE`] already held by whoever
    /// loads. It needs no lock of its own and takes none: it only checks the
    /// value it is given and logs (`store.prefs_invalid`), and never reads,
    /// moves or writes the file. A replaced value reaches the file only
    /// through a later save, which takes the lock.
    fn from_value(value: Value) -> Self {
        let mut fields = match value {
            Value::Object(fields) => fields,
            other => {
                invalid("app.clipboard", &other, "not_an_object");
                return Self::default();
            }
        };
        let restore_policy = match fields.remove("restore_policy") {
            None => RestorePolicy::default(),
            // Only a string names a policy: deserializing any `Value` would
            // also take an externally tagged object such as
            // `{"text_only": null}` (Codex B3).
            Some(value @ Value::String(_)) => {
                RestorePolicy::deserialize(&value).unwrap_or_else(|_| {
                    invalid("app.clipboard.restore_policy", &value, "unknown_value");
                    RestorePolicy::default()
                })
            }
            Some(other) => {
                invalid("app.clipboard.restore_policy", &other, "wrong_type");
                RestorePolicy::default()
            }
        };
        let restore_delay_ms = match fields.remove("restore_delay_ms") {
            None => DEFAULT_RESTORE_DELAY_MS,
            Some(Value::Number(ms)) => match ms.as_u64() {
                Some(ms) if RESTORE_DELAY_MS.contains(&ms) => ms,
                _ => {
                    tracing::warn!(
                        event = "store.prefs_invalid",
                        file = FILE,
                        field = "app.clipboard.restore_delay_ms",
                        problem = "out_of_range",
                        value = %ms,
                        min = *RESTORE_DELAY_MS.start(),
                        max = *RESTORE_DELAY_MS.end(),
                        "invalid preference replaced by its default"
                    );
                    DEFAULT_RESTORE_DELAY_MS
                }
            },
            Some(other) => {
                invalid("app.clipboard.restore_delay_ms", &other, "wrong_type");
                DEFAULT_RESTORE_DELAY_MS
            }
        };
        Self {
            restore_policy,
            restore_delay_ms,
            extra: fields,
        }
    }

    /// The platform's lease options. A delay outside [`RESTORE_DELAY_MS`]
    /// (only possible when set in code, never when read) takes the default.
    pub fn lease_options(&self) -> LeaseOptions {
        let ms = if RESTORE_DELAY_MS.contains(&self.restore_delay_ms) {
            self.restore_delay_ms
        } else {
            DEFAULT_RESTORE_DELAY_MS
        };
        LeaseOptions {
            policy: self.restore_policy,
            restore_delay: Duration::from_millis(ms),
        }
    }
}

/// Logs a replaced value by its JSON type only: a string value is never
/// echoed.
fn invalid(field: &'static str, found: &Value, problem: &'static str) {
    let found = match found {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    tracing::warn!(
        event = "store.prefs_invalid",
        file = FILE,
        field,
        problem,
        found,
        "invalid preference replaced by its default"
    );
}

/// Startup, before the engine starts (one call in `main.rs` setup): hands
/// `app.clipboard` to the platform as the options of every clipboard lease
/// (B-08). Prefs that cannot be read leave the defaults, logged.
///
/// The read is a full load, which may quarantine a damaged file, so it holds
/// [`PREFS_FILE`] like every load (review A6-3). The options are handed to
/// the platform only after that lock is released: `set_lease_options` takes
/// the platform's own lock, and nothing is taken while [`PREFS_FILE`] is
/// held.
pub fn apply_clipboard_prefs(dir: &Path) {
    set_lease_options(clipboard_prefs_then(dir, || {}).lease_options());
}

/// `app.clipboard` as [`apply_clipboard_prefs`] reads it, through
/// [`load_prefs_then`] (so under [`PREFS_FILE`], with `after_read` run in
/// the gap between reading the bytes and acting on them: tests). The
/// fallback is logged after the lock is released.
fn clipboard_prefs_then(dir: &Path, after_read: impl FnOnce()) -> ClipboardPrefs {
    match load_prefs_then(dir, after_read) {
        Ok(prefs) => prefs.app.clipboard,
        Err(e) => {
            tracing::warn!(
                event = "store.prefs_unavailable",
                file = FILE,
                error = %e,
                "prefs unavailable; clipboard restore uses the defaults"
            );
            ClipboardPrefs::default()
        }
    }
}

/// Reads `prefs.json` leniently. A missing file gives defaults; a file that
/// is not valid prefs JSON is renamed to `prefs.corrupt-<time>.json` and
/// defaults are returned. Errors, so callers never overwrite preferences
/// they could not see or could not preserve:
/// - the file exists but cannot be read (locked, access denied): `Io { op: read }`;
/// - the file is damaged and the rename fails: `Io { op: rename }`, and the
///   original stays where it is.
///
/// Callers that write prefs must load them first and give up on error.
///
/// Holds [`PREFS_FILE`] for the whole load, since a load may quarantine.
pub fn load_prefs(dir: &Path) -> Result<Prefs, CommandError> {
    load_prefs_then(dir, || {})
}

/// [`load_prefs`], running `after_read` (under the lock) between reading the
/// bytes and acting on them: the gap a concurrent writer used to fall into
/// (tests).
fn load_prefs_then(dir: &Path, after_read: impl FnOnce()) -> Result<Prefs, CommandError> {
    let _file = lock_prefs_file();
    load_unlocked(dir, after_read)
}

/// The load itself; [`PREFS_FILE`] is held by the caller.
fn load_unlocked(dir: &Path, after_read: impl FnOnce()) -> Result<Prefs, CommandError> {
    let path = dir.join(FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Prefs::default()),
        Err(e) => return Err(CommandError::io(IoOp::Read, Some(&path), &e)),
    };
    after_read();
    match parse_prefs(&bytes) {
        Ok(prefs) => Ok(prefs),
        Err(e) => {
            quarantine(&path, &e)
                .map_err(|err| CommandError::io(IoOp::Rename, Some(&path), &err))?;
            Ok(Prefs::default())
        }
    }
}

/// Parses `prefs.json` as every load does: a leading UTF-8 BOM (Notepad
/// writes one) is skipped. The diagnostics bundle parses with this too, so
/// a file the app reads never shows there as unparsable.
pub(crate) fn parse_prefs<T: DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
    serde_json::from_slice(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes))
}

/// Writes `prefs.json` atomically (temp file + replace), unknown fields
/// included. Holds [`PREFS_FILE`].
pub fn save_prefs(dir: &Path, prefs: &Prefs) -> Result<(), CommandError> {
    let _file = lock_prefs_file();
    save_unlocked(dir, prefs)
}

/// The save itself; [`PREFS_FILE`] is held by the caller.
fn save_unlocked(dir: &Path, prefs: &Prefs) -> Result<(), CommandError> {
    fs::create_dir_all(dir).map_err(|e| CommandError::io(IoOp::CreateDir, Some(dir), &e))?;
    atomic_json(&dir.join(FILE), prefs)
}

/// Serialises, within the process, every access to `prefs.json` that may
/// move or replace it: each load (a damaged file is quarantined, i.e.
/// renamed), each save and each read-modify-write ([`update_prefs`], held
/// across the load and the save). So two writers (`set_auto_listen`, the
/// log level) never lose each other's change, and a reader that saw a
/// damaged file can never rename the file a writer has just quarantined
/// and replaced (review A6-3).
///
/// Lock order: taken after any lock of the caller's (the store lock in
/// `load_state`, the log level state), never before; nothing else is taken
/// while it is held, including in an [`update_prefs`] closure.
static PREFS_FILE: Mutex<()> = Mutex::new(());

fn lock_prefs_file() -> MutexGuard<'static, ()> {
    PREFS_FILE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Loads `prefs.json`, applies `change` and saves the result if `change`
/// returned true; returns that value. Nothing is written when the load
/// fails (the store rule, see [`load_prefs`]). [`PREFS_FILE`] is held
/// throughout; `change` must not take any lock.
pub fn update_prefs(
    dir: &Path,
    change: impl FnOnce(&mut Prefs) -> bool,
) -> Result<bool, CommandError> {
    let _file = lock_prefs_file();
    let mut prefs = load_unlocked(dir, || {})?;
    let changed = change(&mut prefs);
    if changed {
        save_unlocked(dir, &prefs)?;
    }
    Ok(changed)
}

/// `app.auto_listen_on_start`; false when prefs cannot be read.
pub fn auto_listen_on_start(dir: &Path) -> bool {
    load_prefs(dir).is_ok_and(|prefs| prefs.app.auto_listen_on_start)
}

/// Moves a damaged file aside, keeping its bytes. Only the error category
/// and position are logged: serde messages can quote file content.
fn quarantine(path: &Path, reason: &serde_json::Error) -> io::Result<PathBuf> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let target = quarantine_path(dir, SystemTime::now());
    match fs::rename(path, &target) {
        Ok(()) => {
            tracing::warn!(
                event = "store.quarantine",
                file = FILE,
                to = ?file_label(&target),
                reason = ?reason.classify(),
                line = reason.line(),
                column = reason.column(),
                "quarantined corrupt file"
            );
            Ok(target)
        }
        Err(e) => {
            tracing::error!(
                event = "store.quarantine_failed",
                file = FILE,
                reason = ?reason.classify(),
                error = %e,
                "could not move corrupt file aside; leaving it in place"
            );
            Err(e)
        }
    }
}

/// `prefs.corrupt-<UTC yyyymmdd-hhmmss>.json`, with `-<n>` appended when a
/// file from the same second already exists.
fn quarantine_path(dir: &Path, now: SystemTime) -> PathBuf {
    let stamp = utc_stamp(now);
    let mut target = dir.join(format!("prefs.corrupt-{stamp}.json"));
    let mut n = 1;
    while target.exists() {
        target = dir.join(format!("prefs.corrupt-{stamp}-{n}.json"));
        n += 1;
    }
    target
}

pub(crate) fn utc_stamp(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day), after
/// Howard Hinnant's `civil_from_days`.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("quicker-prefs-test-{}", uuid::Uuid::new_v4()))
    }

    fn quarantined(dir: &Path) -> Vec<PathBuf> {
        let mut files: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with("prefs.corrupt-") && name.ends_with(".json")
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn missing_file_gives_defaults_without_writing() {
        let dir = temp_dir();
        let prefs = load_prefs(&dir).unwrap();
        assert_eq!(prefs, Prefs::default());
        assert!(!prefs.app.auto_listen_on_start);
        assert_eq!(prefs.ui, json!({}));
        assert!(!dir.exists());
    }

    #[test]
    fn prefs_roundtrip() {
        let dir = temp_dir();
        let prefs = Prefs {
            app: AppPrefs {
                auto_listen_on_start: true,
                log_level: Some(LogLevelOverride {
                    level: "debug".into(),
                    until_ms: 1_792_664_493_000,
                    extra: Map::new(),
                }),
                ..AppPrefs::default()
            },
            ui: json!({"theme": "dark", "sidebar": {"width": 280}}),
            ..Prefs::default()
        };
        save_prefs(&dir, &prefs).unwrap();
        assert_eq!(load_prefs(&dir).unwrap(), prefs);
        assert!(!dir.join("prefs.json.tmp").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unknown_fields_are_preserved_and_written_back() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let original = json!({
            "version": 1,
            "app": {
                "auto_listen_on_start": false,
                "log_level": null,
                "clipboard": {"restore_policy": "best_effort", "restore_delay_ms": 300},
                "hotkey": "Ctrl+Alt+Q",
                "stats": {"enabled": true}
            },
            "ui": {"theme": "light", "columns": [1, 2]},
            "future_section": {"nested": [true, null, 1.5]}
        });
        fs::write(dir.join(FILE), serde_json::to_vec(&original).unwrap()).unwrap();
        let mut prefs = load_prefs(&dir).unwrap();
        assert_eq!(prefs.extra["version"], 1);
        assert_eq!(prefs.app.extra["hotkey"], "Ctrl+Alt+Q");
        assert_eq!(prefs.ui["columns"], json!([1, 2]));
        save_prefs(&dir, &prefs).unwrap();
        let written: Value = serde_json::from_slice(&fs::read(dir.join(FILE)).unwrap()).unwrap();
        assert_eq!(written, original);
        prefs.app.auto_listen_on_start = true;
        save_prefs(&dir, &prefs).unwrap();
        let written: Value = serde_json::from_slice(&fs::read(dir.join(FILE)).unwrap()).unwrap();
        assert_eq!(written["app"]["auto_listen_on_start"], true);
        assert_eq!(written["app"]["stats"], original["app"]["stats"]);
        assert_eq!(written["future_section"], original["future_section"]);
        assert!(quarantined(&dir).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unknown_log_level_fields_survive_unrelated_edits() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let original = json!({
            "app": {
                "auto_listen_on_start": false,
                "log_level": {
                    "level": "debug",
                    "until_ms": 1_792_664_493_000_i64,
                    "set_by": "settings",
                    "scope": {"targets": ["engine", "store"], "sample": {"rate": 0.5}}
                }
            },
            "ui": {}
        });
        fs::write(dir.join(FILE), serde_json::to_vec(&original).unwrap()).unwrap();
        let mut prefs = load_prefs(&dir).unwrap();
        let level = prefs.app.log_level.as_ref().unwrap();
        assert_eq!(level.level, "debug");
        assert_eq!(level.extra["scope"]["sample"]["rate"], 0.5);
        prefs.app.auto_listen_on_start = true;
        save_prefs(&dir, &prefs).unwrap();
        let written: Value = serde_json::from_slice(&fs::read(dir.join(FILE)).unwrap()).unwrap();
        assert_eq!(written["app"]["auto_listen_on_start"], true);
        assert_eq!(written["app"]["log_level"], original["app"]["log_level"]);
        assert!(quarantined(&dir).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_files_fill_in_defaults_and_bom_is_accepted() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE), b"\xEF\xBB\xBF{\"app\":{}}").unwrap();
        assert_eq!(load_prefs(&dir).unwrap(), Prefs::default());
        assert!(quarantined(&dir).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn damaged_files_are_quarantined_and_defaults_used() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let samples: [&[u8]; 4] = [
            b"{\"app\": {\"auto_listen_on_start\": tr",
            b"{\"ui\": \"\xFF\xFE\"}",
            b"{\"app\": {\"auto_listen_on_start\": \"yes\"}}",
            b"[]",
        ];
        for (i, sample) in samples.iter().enumerate() {
            fs::write(dir.join(FILE), sample).unwrap();
            assert_eq!(load_prefs(&dir).unwrap(), Prefs::default());
            assert!(!dir.join(FILE).exists());
            let files = quarantined(&dir);
            assert_eq!(files.len(), i + 1, "each damaged file keeps its own copy");
            assert!(files.iter().any(|f| fs::read(f).unwrap() == *sample));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_file_is_an_error_and_is_not_quarantined() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join(FILE)).unwrap();
        let Err(CommandError::Io { op, path, .. }) = load_prefs(&dir) else {
            panic!("an unreadable prefs.json must be an Io error");
        };
        assert_eq!(op, IoOp::Read);
        assert_eq!(path.as_deref(), Some(FILE));
        assert!(dir.join(FILE).is_dir());
        assert!(quarantined(&dir).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    /// Codex review: a damaged file that cannot be moved aside must not be
    /// replaced by defaults on the next save.
    #[cfg(windows)]
    #[test]
    fn failed_quarantine_is_an_error_and_keeps_the_original() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let damaged = b"{\"app\": {\"auto_listen_on_start\": tr";
        fs::write(dir.join(FILE), damaged).unwrap();
        // Another process holding the file without delete sharing: reads
        // work, the rename does not.
        let holder = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(dir.join(FILE))
            .unwrap();
        let Err(CommandError::Io { op, path, .. }) = load_prefs(&dir) else {
            panic!("a failed quarantine must be an Io error");
        };
        assert_eq!(op, IoOp::Rename);
        assert_eq!(path.as_deref(), Some(FILE));
        drop(holder);
        assert_eq!(fs::read(dir.join(FILE)).unwrap(), damaged);
        assert!(quarantined(&dir).is_empty());
        // Once the file can be moved, the normal path applies again.
        assert_eq!(load_prefs(&dir).unwrap(), Prefs::default());
        let files = quarantined(&dir);
        assert_eq!(files.len(), 1);
        assert_eq!(fs::read(&files[0]).unwrap(), damaged);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quarantine_names_use_utc_time() {
        let at = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(utc_stamp(at(0)), "19700101-000000");
        assert_eq!(utc_stamp(at(951_868_799)), "20000229-235959");
        assert_eq!(utc_stamp(at(1_792_664_493)), "20261022-102133");
        assert_eq!(utc_stamp(at(4_107_542_400)), "21000301-000000");
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let first = quarantine_path(&dir, at(1_792_664_493));
        assert_eq!(
            file_label(&first).unwrap(),
            "prefs.corrupt-20261022-102133.json"
        );
        fs::write(&first, b"").unwrap();
        let second = quarantine_path(&dir, at(1_792_664_493));
        assert_eq!(
            file_label(&second).unwrap(),
            "prefs.corrupt-20261022-102133-1.json"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    fn with_clipboard(dir: &Path, clipboard: Value) {
        let file = json!({
            "app": {"auto_listen_on_start": true, "clipboard": clipboard},
            "ui": {"theme": "dark"}
        });
        fs::write(dir.join(FILE), serde_json::to_vec(&file).unwrap()).unwrap();
    }

    #[test]
    fn clipboard_prefs_default_to_best_effort_after_300_ms() {
        let clipboard = Prefs::default().app.clipboard;
        assert_eq!(clipboard.restore_policy, RestorePolicy::BestEffort);
        assert_eq!(clipboard.restore_delay_ms, 300);
        assert_eq!(clipboard.lease_options(), LeaseOptions::default());
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(FILE),
            br#"{"app": {"auto_listen_on_start": true}}"#,
        )
        .unwrap();
        assert_eq!(load_prefs(&dir).unwrap().app.clipboard, clipboard);
        with_clipboard(&dir, json!({}));
        assert_eq!(load_prefs(&dir).unwrap().app.clipboard, clipboard);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clipboard_prefs_are_read_and_written_back() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        for (name, policy, ms) in [
            ("full", RestorePolicy::Full, 100),
            ("best_effort", RestorePolicy::BestEffort, 750),
            ("text_only", RestorePolicy::TextOnly, 2000),
        ] {
            let section = json!({
                "restore_policy": name,
                "restore_delay_ms": ms,
                "office_formats": {"keep": [true, null]}
            });
            with_clipboard(&dir, section.clone());
            let prefs = load_prefs(&dir).unwrap();
            let clipboard = &prefs.app.clipboard;
            assert_eq!(
                (clipboard.restore_policy, clipboard.restore_delay_ms),
                (policy, ms)
            );
            assert_eq!(
                clipboard.lease_options(),
                LeaseOptions {
                    policy,
                    restore_delay: Duration::from_millis(ms),
                }
            );
            save_prefs(&dir, &prefs).unwrap();
            let written: Value =
                serde_json::from_slice(&fs::read(dir.join(FILE)).unwrap()).unwrap();
            assert_eq!(written["app"]["clipboard"], section, "{name}");
        }
        assert!(quarantined(&dir).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    /// A bad clipboard value takes its default and is logged, by field and
    /// JSON type only; the rest of the file still loads and nothing is
    /// quarantined.
    #[test]
    fn invalid_clipboard_values_take_their_defaults_and_are_logged() {
        use crate::diagnostics::logging::{
            test_support::{read_logs, scoped},
            DEFAULT_LEVEL,
        };
        const SECRET: &str = "QT-SENTINEL-not-a-policy";
        let (policy, delay) = (
            "app.clipboard.restore_policy",
            "app.clipboard.restore_delay_ms",
        );
        let cases = [
            (
                json!({"restore_policy": SECRET, "restore_delay_ms": 500}),
                (RestorePolicy::BestEffort, 500),
                (policy, "unknown_value"),
            ),
            (
                json!({"restore_policy": "Full", "restore_delay_ms": 500}),
                (RestorePolicy::BestEffort, 500),
                (policy, "unknown_value"),
            ),
            (
                json!({"restore_policy": 2, "restore_delay_ms": 500}),
                (RestorePolicy::BestEffort, 500),
                (policy, "wrong_type"),
            ),
            // Externally tagged objects name a policy to serde, not to prefs
            // (Codex B3).
            (
                json!({"restore_policy": {"text_only": null}, "restore_delay_ms": 500}),
                (RestorePolicy::BestEffort, 500),
                (policy, "wrong_type"),
            ),
            (
                json!({"restore_policy": {"full": SECRET}, "restore_delay_ms": 500}),
                (RestorePolicy::BestEffort, 500),
                (policy, "wrong_type"),
            ),
            (
                json!({"restore_policy": ["text_only"], "restore_delay_ms": 500}),
                (RestorePolicy::BestEffort, 500),
                (policy, "wrong_type"),
            ),
            (
                json!({"restore_policy": "text_only", "restore_delay_ms": 99}),
                (RestorePolicy::TextOnly, 300),
                (delay, "out_of_range"),
            ),
            (
                json!({"restore_policy": "full", "restore_delay_ms": 2001}),
                (RestorePolicy::Full, 300),
                (delay, "out_of_range"),
            ),
            (
                json!({"restore_delay_ms": -1}),
                (RestorePolicy::BestEffort, 300),
                (delay, "out_of_range"),
            ),
            (
                json!({"restore_delay_ms": 300.5}),
                (RestorePolicy::BestEffort, 300),
                (delay, "out_of_range"),
            ),
            (
                json!({"restore_policy": "text_only", "restore_delay_ms": "300"}),
                (RestorePolicy::TextOnly, 300),
                (delay, "wrong_type"),
            ),
            (
                json!({"restore_delay_ms": null}),
                (RestorePolicy::BestEffort, 300),
                (delay, "wrong_type"),
            ),
            (
                json!(SECRET),
                (RestorePolicy::BestEffort, 300),
                ("app.clipboard", "not_an_object"),
            ),
            (
                json!(["full", 500]),
                (RestorePolicy::BestEffort, 300),
                ("app.clipboard", "not_an_object"),
            ),
            (
                Value::Null,
                (RestorePolicy::BestEffort, 300),
                ("app.clipboard", "not_an_object"),
            ),
        ];
        for (section, (expected_policy, expected_ms), (field, problem)) in cases {
            let dir = temp_dir();
            fs::create_dir_all(&dir).unwrap();
            with_clipboard(&dir, section.clone());
            let logs = dir.join("logs");
            let built = scoped(&logs, DEFAULT_LEVEL);
            let prefs = tracing::subscriber::with_default(built.subscriber, || load_prefs(&dir))
                .unwrap_or_else(|e| panic!("{section}: {e}"));
            drop(built.guard);
            let clipboard = &prefs.app.clipboard;
            assert_eq!(
                (clipboard.restore_policy, clipboard.restore_delay_ms),
                (expected_policy, expected_ms),
                "{section}"
            );
            assert!(prefs.app.auto_listen_on_start, "{section}");
            assert_eq!(prefs.ui, json!({"theme": "dark"}), "{section}");
            assert!(dir.join(FILE).exists(), "{section}");
            assert!(quarantined(&dir).is_empty(), "{section}");
            let log = read_logs(&logs);
            let line = log
                .lines()
                .find(|line| line.contains("event=\"store.prefs_invalid\""))
                .unwrap_or_else(|| panic!("{section}: no log line in\n{log}"));
            assert!(line.contains(&format!("field=\"{field}\"")), "{line}");
            assert!(line.contains(&format!("problem=\"{problem}\"")), "{line}");
            assert!(!log.contains(SECRET), "{line}");
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// The startup call sets the platform's lease options (process-wide; no
    /// other test in this crate opens a lease or reads them).
    #[test]
    fn startup_hands_the_clipboard_prefs_to_the_platform() {
        use quicker_platform::clipboard_options::lease_options;
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        with_clipboard(
            &dir,
            json!({"restore_policy": "text_only", "restore_delay_ms": 1500}),
        );
        apply_clipboard_prefs(&dir);
        let text_only = LeaseOptions {
            policy: RestorePolicy::TextOnly,
            restore_delay: Duration::from_millis(1500),
        };
        assert_eq!(lease_options(), text_only);
        // Unreadable prefs: the defaults, and the file is left alone.
        fs::remove_file(dir.join(FILE)).unwrap();
        fs::create_dir_all(dir.join(FILE)).unwrap();
        apply_clipboard_prefs(&dir);
        assert_eq!(lease_options(), LeaseOptions::default());
        assert!(dir.join(FILE).is_dir());
        fs::remove_dir_all(dir.join(FILE)).unwrap();
        with_clipboard(&dir, json!({"restore_policy": "full"}));
        apply_clipboard_prefs(&dir);
        assert_eq!(
            lease_options(),
            LeaseOptions {
                policy: RestorePolicy::Full,
                ..LeaseOptions::default()
            }
        );
        // No file: the defaults.
        fs::remove_file(dir.join(FILE)).unwrap();
        apply_clipboard_prefs(&dir);
        assert_eq!(lease_options(), LeaseOptions::default());
        fs::remove_dir_all(dir).unwrap();
    }

    /// `update_prefs` keeps everything it does not change, writes only when
    /// asked to, and never writes over prefs it could not read.
    #[test]
    fn update_prefs_changes_one_field_and_keeps_the_rest() {
        let dir = temp_dir();
        assert!(!auto_listen_on_start(&dir), "missing file: off");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(FILE),
            br#"{"app":{"hotkey":"Ctrl+Q"},"ui":{"theme":"dark"},"future":1}"#,
        )
        .unwrap();
        let changed = update_prefs(&dir, |prefs| {
            prefs.app.auto_listen_on_start = true;
            true
        })
        .unwrap();
        assert!(changed);
        assert!(auto_listen_on_start(&dir));
        let written: Value = serde_json::from_slice(&fs::read(dir.join(FILE)).unwrap()).unwrap();
        assert_eq!(written["app"]["auto_listen_on_start"], true);
        assert_eq!(written["app"]["hotkey"], "Ctrl+Q");
        assert_eq!(written["ui"], json!({"theme": "dark"}));
        assert_eq!(written["future"], 1);
        // No change asked for: the file is not touched.
        fs::remove_file(dir.join(FILE)).unwrap();
        assert!(!update_prefs(&dir, |_| false).unwrap());
        assert!(!dir.join(FILE).exists());
        // Unreadable: an error, nothing written, and auto-listen reads as off.
        fs::create_dir_all(dir.join(FILE)).unwrap();
        assert!(matches!(
            update_prefs(&dir, |prefs| {
                prefs.app.auto_listen_on_start = true;
                true
            }),
            Err(CommandError::Io { op: IoOp::Read, .. })
        ));
        assert!(dir.join(FILE).is_dir());
        assert!(!auto_listen_on_start(&dir));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Review A6-3: a load has just read a damaged prefs.json when a writer
    /// arrives (barrier). The writer waits for the load (one lock), so the
    /// load quarantines the damaged file, not the one the writer would have
    /// put in its place: the writer's change ends up in prefs.json and the
    /// only quarantined copy is the damaged bytes. Without the shared lock
    /// the writer quarantines and replaces the file in the gap, and the
    /// load then moves the writer's file aside.
    #[test]
    fn a_writer_never_lands_between_a_damaged_read_and_its_quarantine() {
        use std::sync::{mpsc, Arc, Barrier};
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let damaged = b"{\"app\": {\"auto_listen_on_start\": tr";
        fs::write(dir.join(FILE), damaged).unwrap();
        let read = Arc::new(Barrier::new(2));
        let (done_tx, done_rx) = mpsc::channel();
        let writer = {
            let dir = dir.clone();
            let read = Arc::clone(&read);
            std::thread::spawn(move || {
                read.wait();
                let result = update_prefs(&dir, |prefs| {
                    prefs.app.auto_listen_on_start = true;
                    true
                });
                let _ = done_tx.send(());
                result
            })
        };
        let mut writer_ran_in_the_gap = false;
        let loaded = load_prefs_then(&dir, || {
            read.wait();
            // Every chance for the writer to run now; it can only finish
            // here if it does not wait for this load.
            writer_ran_in_the_gap = done_rx.recv_timeout(Duration::from_millis(300)).is_ok();
        });
        assert_eq!(loaded.unwrap(), Prefs::default());
        assert!(writer.join().unwrap().unwrap());
        assert!(!writer_ran_in_the_gap);
        assert!(auto_listen_on_start(&dir), "the writer's change is kept");
        let files = quarantined(&dir);
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(fs::read(&files[0]).unwrap(), damaged);
        fs::remove_dir_all(dir).unwrap();
    }

    /// The same barrier for the P5-B reader, the startup read of
    /// `app.clipboard` ([`apply_clipboard_prefs`], through
    /// `clipboard_prefs_then` so the process-wide lease options stay
    /// untouched): it has just read a damaged prefs.json when a writer
    /// arrives. The writer waits for the read, so the read takes the
    /// defaults and quarantines only the damaged bytes, and the writer's
    /// change (clipboard policy included) ends up in prefs.json. Without the
    /// shared lock the writer would quarantine and replace the file in the
    /// gap, and the read would then move the writer's file aside.
    #[test]
    fn a_writer_never_lands_between_a_damaged_clipboard_read_and_its_quarantine() {
        use std::sync::{mpsc, Arc, Barrier};
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let damaged =
            b"{\"app\": {\"clipboard\": {\"restore_policy\": \"text_only\", \"restore_del";
        fs::write(dir.join(FILE), damaged).unwrap();
        let read = Arc::new(Barrier::new(2));
        let (done_tx, done_rx) = mpsc::channel();
        let writer = {
            let dir = dir.clone();
            let read = Arc::clone(&read);
            std::thread::spawn(move || {
                read.wait();
                let result = update_prefs(&dir, |prefs| {
                    prefs.app.clipboard.restore_policy = RestorePolicy::Full;
                    prefs.app.auto_listen_on_start = true;
                    true
                });
                let _ = done_tx.send(());
                result
            })
        };
        let mut writer_ran_in_the_gap = false;
        let clipboard = clipboard_prefs_then(&dir, || {
            read.wait();
            // Every chance for the writer to run now; it can only finish
            // here if it does not wait for this read.
            writer_ran_in_the_gap = done_rx.recv_timeout(Duration::from_millis(300)).is_ok();
        });
        assert_eq!(clipboard, ClipboardPrefs::default());
        assert!(writer.join().unwrap().unwrap());
        assert!(!writer_ran_in_the_gap);
        let kept = load_prefs(&dir).unwrap();
        assert_eq!(
            kept.app.clipboard.restore_policy,
            RestorePolicy::Full,
            "the writer's change is kept"
        );
        assert!(kept.app.auto_listen_on_start);
        let files = quarantined(&dir);
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(fs::read(&files[0]).unwrap(), damaged);
        fs::remove_dir_all(dir).unwrap();
    }
}
