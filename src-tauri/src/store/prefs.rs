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
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::{
    fs, io,
    ops::RangeInclusive,
    path::{Path, PathBuf},
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
            Some(value) => RestorePolicy::deserialize(&value).unwrap_or_else(|_| {
                let problem = match value {
                    Value::String(_) => "unknown_value",
                    _ => "wrong_type",
                };
                invalid("app.clipboard.restore_policy", &value, problem);
                RestorePolicy::default()
            }),
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
pub fn apply_clipboard_prefs(dir: &Path) {
    let clipboard = match load_prefs(dir) {
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
    };
    set_lease_options(clipboard.lease_options());
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
pub fn load_prefs(dir: &Path) -> Result<Prefs, CommandError> {
    let path = dir.join(FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Prefs::default()),
        Err(e) => return Err(CommandError::io(IoOp::Read, Some(&path), &e)),
    };
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    match serde_json::from_slice(bytes) {
        Ok(prefs) => Ok(prefs),
        Err(e) => {
            quarantine(&path, &e)
                .map_err(|err| CommandError::io(IoOp::Rename, Some(&path), &err))?;
            Ok(Prefs::default())
        }
    }
}

/// Writes `prefs.json` atomically (temp file + replace), unknown fields
/// included.
pub fn save_prefs(dir: &Path, prefs: &Prefs) -> Result<(), CommandError> {
    fs::create_dir_all(dir).map_err(|e| CommandError::io(IoOp::CreateDir, Some(dir), &e))?;
    atomic_json(&dir.join(FILE), prefs)
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
}
