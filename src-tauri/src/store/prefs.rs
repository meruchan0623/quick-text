//! User preferences in `prefs.json` (§9 「prefs.json」): `app` is defined by
//! the backend, `ui` by the frontend and stored verbatim. Not part of the
//! revision history or exports. A damaged file is quarantined and replaced by
//! defaults, so it never blocks rule loading.
// Not wired to commands yet (phase 2: auto-listen, log level, hotkeys).
#![allow(dead_code)]

use super::atomic::atomic_json;
use crate::error::{file_label, CommandError, IoOp};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
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
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogLevelOverride {
    pub level: String,
    /// Unix time in milliseconds.
    pub until_ms: i64,
}

/// Reads `prefs.json` leniently. A missing file gives defaults; a file that
/// is not valid prefs JSON is renamed to `prefs.corrupt-<time>.json` and
/// defaults are returned. Only a file that exists but cannot be read (locked,
/// access denied) is an error, so callers never overwrite preferences they
/// could not see.
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
            quarantine(&path, &e);
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

/// Moves a damaged file aside. If that fails the defaults are still used and
/// the next save replaces the damaged file.
fn quarantine(path: &Path, reason: &dyn std::fmt::Display) {
    let dir = path.parent().unwrap_or(Path::new("."));
    let target = quarantine_path(dir, SystemTime::now());
    match fs::rename(path, &target) {
        Ok(()) => tracing::warn!(
            file = FILE,
            to = ?file_label(&target),
            reason = %reason,
            "store.quarantine"
        ),
        Err(e) => tracing::error!(
            file = FILE,
            reason = %reason,
            error = %e,
            "store.quarantine failed"
        ),
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

fn utc_stamp(now: SystemTime) -> String {
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
fn civil_from_days(days: i64) -> (i64, u32, u32) {
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
}
