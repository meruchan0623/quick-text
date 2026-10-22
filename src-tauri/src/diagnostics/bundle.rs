//! Diagnostics bundle (§7.1 「可观测性」, S-08): one zip for the developer
//! that never carries phrase content.
//!
//! | Entry | Summary | Full |
//! |---|---|---|
//! | `manifest.json`: redaction, time, entry list, skipped files | yes | yes |
//! | `system.json`: version, build, OS | yes | yes |
//! | `health.json`: health and capability snapshot, log level | yes | yes |
//! | `config-summary.json`: counts and reason codes only | yes | yes |
//! | `logs/quicker-text.YYYY-MM-DD.log`: the last 7 days | yes | yes |
//! | `crashes/crash-*.txt`: the newest crash records | yes | yes |
//! | `active.json`: the active configuration, phrases included | no | yes |
//!
//! Clipboard, selection and detection buffer are never read. In every entry
//! the user name in profile paths is replaced by `%USERNAME%`.
use super::{
    level::LevelState,
    logging::{log_dir, FILE_PREFIX, FILE_SUFFIX, MAX_LOG_FILES},
    redact::redact_user_paths,
};
use crate::store::prefs::{civil_from_days, utc_stamp};
use quicker_domain::{compile_config, Config, Severity};
use quicker_platform::{health::HealthSnapshot, Capability, ExecutionResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    time::SystemTime,
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

/// Only the newest log bytes of a day are kept beyond this size.
pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
/// Only the newest crash records are included.
pub const MAX_CRASH_RECORDS: usize = 50;
/// Global terminator names reported by name; anything else counts as `other`.
const KNOWN_TERMINATORS: [&str; 4] = ["space", "tab", "enter", "semicolon"];

/// `export_diagnostics(redaction)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Redaction {
    /// Logs, crash records, snapshots and the configuration summary.
    #[default]
    Summary,
    /// Summary plus `active.json` with phrase content; only when the user
    /// ticks 「包含短语内容」 in the export dialog.
    Full,
}
impl Redaction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Full => "full",
        }
    }
}

/// Runtime state the bundle cannot read from the data directory. Built by
/// the command under a brief store lock; tests build it by hand.
pub struct Snapshot {
    pub now: SystemTime,
    pub health: HealthReport,
    pub system: SystemInfo,
    /// Home directories whose user name is replaced, see
    /// [`super::redact::user_homes`].
    pub homes: Vec<String>,
}

/// `health.json`.
#[derive(Serialize)]
pub struct HealthReport {
    /// TODO(B-03): the supervisor's snapshot once `get_health` exists (P1a).
    pub health: Option<HealthSnapshot>,
    pub engine_status: &'static str,
    pub paused: bool,
    pub busy: bool,
    pub revision: u64,
    pub last_result: Option<LastResult>,
    pub capabilities: Vec<Capability>,
    pub log_level: LevelState,
}

/// The last execution without its rule id and message.
#[derive(Serialize)]
pub struct LastResult {
    pub status: String,
    pub completed_steps: usize,
    pub side_effects: bool,
}
impl From<&ExecutionResult> for LastResult {
    fn from(result: &ExecutionResult) -> Self {
        Self {
            status: code_or_other(&result.status),
            completed_steps: result.completed_steps,
            side_effects: result.side_effects,
        }
    }
}

/// `system.json`.
#[derive(Serialize)]
pub struct SystemInfo {
    pub app_version: &'static str,
    pub build: &'static str,
    pub features: Vec<&'static str>,
    pub os: &'static str,
    pub arch: &'static str,
    pub os_version: Option<String>,
}
impl SystemInfo {
    pub fn current() -> Self {
        let features = [
            ("mock", cfg!(feature = "mock")),
            ("fault-injection", cfg!(feature = "fault-injection")),
        ]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect();
        Self {
            app_version: env!("CARGO_PKG_VERSION"),
            build: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            features,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            os_version: os_version(),
        }
    }
}

/// `quicker-text-diagnostics-<UTC yyyymmdd-hhmmss>.zip`.
pub fn default_file_name(now: SystemTime) -> String {
    format!("quicker-text-diagnostics-{}.zip", utc_stamp(now))
}

#[derive(Serialize)]
struct Manifest<'a> {
    format: u32,
    redaction: Redaction,
    generated_at_utc: String,
    entries: &'a [String],
    skipped: &'a [Skipped],
}

/// A file that was left out, and why (never its content).
#[derive(Serialize)]
struct Skipped {
    file: String,
    reason: String,
}

/// Writes the bundle for `data_dir` into `writer` and returns the writer and
/// the entry names. Only the zip writer itself failing is an error; files
/// that cannot be read are listed in the manifest instead.
pub fn build_bundle<W: Write + Seek>(
    data_dir: &Path,
    redaction: Redaction,
    writer: W,
    snapshot: &Snapshot,
) -> io::Result<(W, Vec<String>)> {
    let mut bundle = Bundle {
        zip: ZipWriter::new(writer),
        homes: &snapshot.homes,
        entries: Vec::new(),
        skipped: Vec::new(),
    };
    bundle.add_json("system.json", &snapshot.system)?;
    bundle.add_json("health.json", &snapshot.health)?;
    bundle.add_json("config-summary.json", &summarize(data_dir))?;
    for (name, path) in recent_logs(&log_dir(data_dir), snapshot.now) {
        match read_tail(&path, MAX_LOG_BYTES) {
            Ok(text) => bundle.add_text(&format!("logs/{name}"), &text)?,
            Err(e) => bundle.skip(name, &e),
        }
    }
    let (crashes, older) = crash_records(data_dir);
    for (name, path) in crashes {
        match fs::read(&path) {
            Ok(bytes) => {
                bundle.add_text(&format!("crashes/{name}"), &String::from_utf8_lossy(&bytes))?
            }
            Err(e) => bundle.skip(name, &e),
        }
    }
    if older > 0 {
        bundle.skipped.push(Skipped {
            file: "crash-*.txt".into(),
            reason: format!("{older} older crash records omitted"),
        });
    }
    if redaction == Redaction::Full {
        match fs::read(data_dir.join("active.json")) {
            Ok(bytes) => bundle.add_text("active.json", &String::from_utf8_lossy(&bytes))?,
            Err(e) => bundle.skip("active.json".into(), &e),
        }
    }
    let mut entries = bundle.entries.clone();
    entries.push("manifest.json".into());
    let skipped = std::mem::take(&mut bundle.skipped);
    let manifest = Manifest {
        format: 1,
        redaction,
        generated_at_utc: utc_stamp(snapshot.now),
        entries: &entries,
        skipped: &skipped,
    };
    bundle.add_json("manifest.json", &manifest)?;
    let writer = bundle.zip.finish().map_err(io::Error::other)?;
    Ok((writer, bundle.entries))
}

/// [`build_bundle`] into a new file at `target`, removed again if building
/// fails.
pub fn write_bundle_file(
    target: &Path,
    data_dir: &Path,
    redaction: Redaction,
    snapshot: &Snapshot,
) -> io::Result<Vec<String>> {
    let file = fs::File::create(target)?;
    let written = build_bundle(data_dir, redaction, io::BufWriter::new(file), snapshot).and_then(
        |(writer, entries)| {
            writer
                .into_inner()
                .map_err(io::IntoInnerError::into_error)?
                .sync_all()?;
            Ok(entries)
        },
    );
    if written.is_err() {
        let _ = fs::remove_file(target);
    }
    written
}

struct Bundle<'a, W: Write + Seek> {
    zip: ZipWriter<W>,
    homes: &'a [String],
    entries: Vec<String>,
    skipped: Vec<Skipped>,
}
impl<W: Write + Seek> Bundle<'_, W> {
    fn add_json(&mut self, name: &str, value: &impl Serialize) -> io::Result<()> {
        let text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
        self.add_text(name, &text)
    }

    /// Every entry goes through here, so every entry is path-redacted.
    fn add_text(&mut self, name: &str, text: &str) -> io::Result<()> {
        // Logs compress well even at level 1, which keeps a week of verbose
        // logs well inside the 5 s export budget (S-08).
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(1));
        self.zip
            .start_file(name, options)
            .map_err(io::Error::other)?;
        self.zip
            .write_all(redact_user_paths(text, self.homes).as_bytes())?;
        self.entries.push(name.into());
        Ok(())
    }

    fn skip(&mut self, file: String, err: &io::Error) {
        self.skipped.push(Skipped {
            file,
            reason: format!("{:?}", err.kind()),
        });
    }
}

/// `(name, path)` of the log files dated within the last [`MAX_LOG_FILES`]
/// days (UTC, as the appender names them), oldest first.
fn recent_logs(dir: &Path, now: SystemTime) -> Vec<(String, std::path::PathBuf)> {
    let today = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400) as i64;
    let (y, m, d) = civil_from_days(today - (MAX_LOG_FILES as i64 - 1));
    let oldest = format!("{y:04}-{m:02}-{d:02}");
    let mut logs: Vec<_> = list_dir(dir)
        .filter(|(name, _)| log_date(name).is_some_and(|date| date >= oldest.as_str()))
        .collect();
    logs.sort();
    logs
}

/// The `YYYY-MM-DD` of `quicker-text.YYYY-MM-DD.log`.
fn log_date(name: &str) -> Option<&str> {
    let date = name
        .strip_prefix(FILE_PREFIX)?
        .strip_prefix('.')?
        .strip_suffix(FILE_SUFFIX)?
        .strip_suffix('.')?;
    let shape_ok = date.len() == 10
        && date.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    shape_ok.then_some(date)
}

/// The newest [`MAX_CRASH_RECORDS`] crash records, oldest first, and how many
/// older ones were left out.
fn crash_records(data_dir: &Path) -> (Vec<(String, std::path::PathBuf)>, usize) {
    let mut records: Vec<_> = list_dir(data_dir)
        .filter(|(name, _)| name.starts_with("crash-") && name.ends_with(".txt"))
        .collect();
    records.sort();
    let older = records.len().saturating_sub(MAX_CRASH_RECORDS);
    (records.split_off(older), older)
}

fn list_dir(dir: &Path) -> impl Iterator<Item = (String, std::path::PathBuf)> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some((name, entry.path()))
        })
}

/// The file as text, or only its newest `max_bytes` from a line start.
fn read_tail(path: &Path, max_bytes: u64) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut bytes = Vec::new();
    if len <= max_bytes {
        file.read_to_end(&mut bytes)?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    file.seek(SeekFrom::Start(len - max_bytes))?;
    file.take(max_bytes).read_to_end(&mut bytes)?;
    let start = bytes
        .iter()
        .position(|&b| b == b'\n')
        .map_or(0, |at| at + 1);
    Ok(format!(
        "[earlier lines omitted: the file has {len} bytes]\n{}",
        String::from_utf8_lossy(&bytes[start..])
    ))
}

/// `config-summary.json`: counts and reason codes, never ids, names,
/// triggers or content.
#[derive(Serialize)]
pub struct ConfigSummary {
    pub active: ConfigFile,
    pub draft: ConfigFile,
    pub prefs: FileState,
    pub revisions_on_disk: usize,
    pub quarantined_files: usize,
}

#[derive(Serialize)]
pub struct ConfigFile {
    pub file: FileState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<ConfigCounts>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FileState {
    Ok,
    Missing,
    Unreadable {
        kind: String,
        os_code: Option<i32>,
    },
    /// Position only: serde's message quotes the offending value.
    Unparsable {
        category: &'static str,
        line: usize,
        column: usize,
    },
}

#[derive(Default, Serialize)]
pub struct ConfigCounts {
    pub schema_version: u32,
    pub revision: u64,
    pub enabled: bool,
    pub rules: usize,
    pub rules_enabled: usize,
    pub groups: usize,
    pub action_types: BTreeMap<String, usize>,
    pub match_kinds: BTreeMap<String, usize>,
    pub trigger_modes: BTreeMap<String, usize>,
    pub parameter_sources: BTreeMap<String, usize>,
    pub notifications_enabled: usize,
    pub terminator_overrides: usize,
    pub ime_erasure_overrides: usize,
    pub global_terminators: BTreeMap<String, usize>,
    pub blacklist_entries: usize,
    pub diagnostics: Vec<DiagnosticCount>,
}

#[derive(Serialize)]
pub struct DiagnosticCount {
    pub code: String,
    pub severity: Severity,
    pub count: usize,
}

pub fn summarize(data_dir: &Path) -> ConfigSummary {
    let names: Vec<String> = list_dir(data_dir).map(|(name, _)| name).collect();
    let revisions_on_disk = list_dir(&data_dir.join("revisions"))
        .filter(|(name, _)| {
            name.strip_suffix(".json")
                .is_some_and(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()))
        })
        .count();
    ConfigSummary {
        active: config_file(&data_dir.join("active.json")),
        draft: config_file(&data_dir.join("draft.json")),
        prefs: read_json::<serde_json::Value>(&data_dir.join("prefs.json")).0,
        revisions_on_disk,
        quarantined_files: names
            .iter()
            .filter(|name| name.contains(".corrupt-") && name.ends_with(".json"))
            .count(),
    }
}

fn config_file(path: &Path) -> ConfigFile {
    let (file, config) = read_json::<Config>(path);
    ConfigFile {
        file,
        counts: config.as_ref().map(count),
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> (FileState, Option<T>) {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (FileState::Missing, None),
        Err(e) => {
            let state = FileState::Unreadable {
                kind: format!("{:?}", e.kind()),
                os_code: e.raw_os_error(),
            };
            return (state, None);
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(value) => (FileState::Ok, Some(value)),
        Err(e) => {
            let category = match e.classify() {
                serde_json::error::Category::Io => "io",
                serde_json::error::Category::Syntax => "syntax",
                serde_json::error::Category::Data => "data",
                serde_json::error::Category::Eof => "eof",
            };
            let state = FileState::Unparsable {
                category,
                line: e.line(),
                column: e.column(),
            };
            (state, None)
        }
    }
}

fn count(config: &Config) -> ConfigCounts {
    let mut counts = ConfigCounts {
        schema_version: config.version,
        revision: config.revision,
        enabled: config.enabled,
        rules: config.rules.len(),
        blacklist_entries: config.globals.blacklist.len(),
        ..ConfigCounts::default()
    };
    let mut groups = BTreeSet::new();
    for rule in &config.rules {
        counts.rules_enabled += usize::from(rule.enabled);
        if !rule.group.is_empty() {
            groups.insert(rule.group.as_str());
        }
        *counts.action_types.entry(tag(&rule.action)).or_default() += 1;
        *counts
            .match_kinds
            .entry(tag(&rule.match_spec.kind))
            .or_default() += 1;
        *counts
            .trigger_modes
            .entry(tag(&rule.trigger_mode))
            .or_default() += 1;
        *counts
            .parameter_sources
            .entry(tag(&rule.parameter))
            .or_default() += 1;
        counts.notifications_enabled += usize::from(rule.notification.enabled);
        counts.terminator_overrides += usize::from(rule.terminator.is_some());
        counts.ime_erasure_overrides += usize::from(rule.ime_erasure.is_some());
    }
    counts.groups = groups.len();
    for name in &config.globals.terminators {
        let name = if KNOWN_TERMINATORS.contains(&name.as_str()) {
            name.as_str()
        } else {
            "other"
        };
        *counts.global_terminators.entry(name.into()).or_default() += 1;
    }
    if let Err(diagnostics) = compile_config(config) {
        let mut by_code: BTreeMap<(String, bool), (Severity, usize)> = BTreeMap::new();
        for diagnostic in diagnostics {
            let key = (
                code_or_other(&diagnostic.code),
                diagnostic.severity == Severity::Error,
            );
            by_code.entry(key).or_insert((diagnostic.severity, 0)).1 += 1;
        }
        counts.diagnostics = by_code
            .into_iter()
            .map(|((code, _), (severity, count))| DiagnosticCount {
                code,
                severity,
                count,
            })
            .collect();
    }
    counts
}

/// The serde name of a unit variant or the `type` tag of a tagged enum.
fn tag(value: &impl Serialize) -> String {
    let name = match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        Ok(serde_json::Value::Object(fields)) => match fields.get("type") {
            Some(serde_json::Value::String(name)) => name.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    code_or_other(&name)
}

/// `value` if it looks like a code (`[a-z0-9_]`, at most 64 bytes), else
/// `other`, so a free-form string can never reach the bundle.
fn code_or_other(value: &str) -> String {
    let is_code = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if is_code { value } else { "other" }.into()
}

/// `major.minor.build` from `RtlGetVersion`, which unlike `GetVersionEx`
/// is not capped by the manifest.
#[cfg(windows)]
fn os_version() -> Option<String> {
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
    }
    let mut info = OsVersionInfoW {
        size: size_of::<OsVersionInfoW>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // SAFETY: `info` is a correctly sized OSVERSIONINFOW with `size` set, as
    // RtlGetVersion requires; it only writes into it.
    let status = unsafe { RtlGetVersion(&mut info) };
    (status == 0).then(|| format!("{}.{}.{}", info.major, info.minor, info.build))
}

#[cfg(not(windows))]
fn os_version() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::level::LogLevel;
    use quicker_domain::{Action, ContentSource, MatchKind, MatchSpec, OutputMethod, Rule};
    use std::{
        io::Cursor,
        path::PathBuf,
        time::{Duration, Instant, UNIX_EPOCH},
    };

    const SENTINEL: &str = "SENTINEL-bundle-4c1e";
    /// 2026-10-22 10:21:33 UTC.
    const NOW_SECS: u64 = 1_792_664_493;

    fn temp_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("quicker-bundle-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn snapshot(homes: &[&str]) -> Snapshot {
        Snapshot {
            now: UNIX_EPOCH + Duration::from_secs(NOW_SECS),
            health: HealthReport {
                health: None,
                engine_status: "listening",
                paused: false,
                busy: false,
                revision: 3,
                last_result: Some(LastResult::from(&ExecutionResult {
                    id: format!("rule-{SENTINEL}"),
                    status: "failed".into(),
                    message: format!("could not type {SENTINEL}"),
                    completed_steps: 2,
                    side_effects: true,
                })),
                capabilities: vec![Capability {
                    id: "keys".into(),
                    available: true,
                    reason: "ready".into(),
                }],
                log_level: LevelState {
                    level: LogLevel::Debug,
                    until_ms: Some(1_792_700_000_000),
                },
            },
            system: SystemInfo::current(),
            homes: homes.iter().map(|home| home.to_string()).collect(),
        }
    }

    fn sentinel_config() -> Config {
        Config {
            revision: 3,
            rules: vec![
                Rule {
                    id: format!("id-{SENTINEL}"),
                    enabled: false,
                    group: format!("group-{SENTINEL}"),
                    description: format!("description-{SENTINEL}"),
                    match_spec: MatchSpec {
                        kind: MatchKind::Literal,
                        value: format!("trigger-{SENTINEL}"),
                    },
                    action: Action::Text {
                        method: OutputMethod::Keys,
                        source: ContentSource::inline(format!("body-{SENTINEL}")),
                    },
                    ..Rule::default()
                },
                Rule {
                    id: "second".into(),
                    match_spec: MatchSpec {
                        kind: MatchKind::Regex,
                        value: "(unclosed".into(),
                    },
                    ..Rule::default()
                },
            ],
            ..Config::default()
        }
    }

    /// A data directory with a configuration, prefs, one revision, a
    /// quarantined file, logs from 9 days and two crash records.
    fn populated_dir() -> PathBuf {
        let dir = temp_dir();
        let config = serde_json::to_string_pretty(&sentinel_config()).unwrap();
        fs::write(dir.join("active.json"), &config).unwrap();
        fs::write(dir.join("draft.json"), "{ not json").unwrap();
        fs::write(dir.join("prefs.json"), r#"{"app":{}}"#).unwrap();
        fs::write(dir.join("prefs.corrupt-20261001-000000.json"), "x").unwrap();
        fs::create_dir_all(dir.join("revisions")).unwrap();
        fs::write(dir.join("revisions/2.json"), &config).unwrap();
        fs::write(dir.join("revisions/2.metadata.json"), "[]").unwrap();
        let logs = log_dir(&dir);
        fs::create_dir_all(&logs).unwrap();
        for day in 14..=22 {
            fs::write(
                logs.join(format!("quicker-text.2026-10-{day}.log")),
                format!("day {day} at C:\\Users\\alice\\AppData\\Roaming\n"),
            )
            .unwrap();
        }
        fs::write(logs.join("unrelated.txt"), SENTINEL).unwrap();
        fs::write(dir.join("crash-20261020-080000.txt"), "thread: main\n").unwrap();
        fs::write(dir.join("crash-20261021-080000.txt"), "thread: hook\n").unwrap();
        dir
    }

    fn unzip(bytes: Vec<u8>) -> BTreeMap<String, String> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        (0..archive.len())
            .map(|i| {
                let mut file = archive.by_index(i).unwrap();
                let mut text = String::new();
                file.read_to_string(&mut text).unwrap();
                (file.name().unwrap().into_owned(), text)
            })
            .collect()
    }

    fn build(dir: &Path, redaction: Redaction, homes: &[&str]) -> BTreeMap<String, String> {
        let (cursor, entries) =
            build_bundle(dir, redaction, Cursor::new(Vec::new()), &snapshot(homes)).unwrap();
        let files = unzip(cursor.into_inner());
        let listed: BTreeSet<_> = entries.into_iter().collect();
        assert_eq!(files.keys().cloned().collect::<BTreeSet<_>>(), listed);
        files
    }

    #[test]
    fn summary_has_the_listed_files_and_no_content() {
        let dir = populated_dir();
        let files = build(&dir, Redaction::Summary, &[]);
        let names: Vec<_> = files.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "config-summary.json",
                "crashes/crash-20261020-080000.txt",
                "crashes/crash-20261021-080000.txt",
                "health.json",
                "logs/quicker-text.2026-10-16.log",
                "logs/quicker-text.2026-10-17.log",
                "logs/quicker-text.2026-10-18.log",
                "logs/quicker-text.2026-10-19.log",
                "logs/quicker-text.2026-10-20.log",
                "logs/quicker-text.2026-10-21.log",
                "logs/quicker-text.2026-10-22.log",
                "manifest.json",
                "system.json",
            ]
        );
        for (name, text) in &files {
            assert!(
                !text.contains(SENTINEL),
                "{name} leaks the sentinel:\n{text}"
            );
        }
        let manifest: serde_json::Value = serde_json::from_str(&files["manifest.json"]).unwrap();
        assert_eq!(manifest["redaction"], "summary");
        assert_eq!(manifest["generated_at_utc"], "20261022-102133");
        assert_eq!(manifest["entries"].as_array().unwrap().len(), files.len());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_summary_counts_without_naming_anything() {
        let dir = populated_dir();
        let files = build(&dir, Redaction::Summary, &[]);
        let summary: serde_json::Value =
            serde_json::from_str(&files["config-summary.json"]).unwrap();
        let active = &summary["active"];
        assert_eq!(active["file"]["status"], "ok");
        let counts = &active["counts"];
        assert_eq!(counts["rules"], 2);
        assert_eq!(counts["rules_enabled"], 1);
        assert_eq!(counts["groups"], 1);
        assert_eq!(counts["action_types"]["text"], 1);
        assert_eq!(counts["action_types"]["none"], 1);
        assert_eq!(counts["match_kinds"]["regex"], 1);
        assert_eq!(counts["parameter_sources"]["default_match"], 2);
        assert_eq!(counts["global_terminators"]["semicolon"], 1);
        assert_eq!(counts["diagnostics"][0]["severity"], "error");
        assert!(counts["diagnostics"][0]["count"].as_u64().unwrap() >= 1);
        assert_eq!(summary["draft"]["file"]["status"], "unparsable");
        assert_eq!(summary["draft"]["file"]["line"], 1);
        assert!(summary["draft"].get("counts").is_none());
        assert_eq!(summary["prefs"]["status"], "ok");
        assert_eq!(summary["revisions_on_disk"], 1);
        assert_eq!(summary["quarantined_files"], 1);
        let health: serde_json::Value = serde_json::from_str(&files["health.json"]).unwrap();
        assert_eq!(health["last_result"]["status"], "failed");
        assert_eq!(health["log_level"]["level"], "debug");
        assert!(health["health"].is_null());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn full_adds_the_active_configuration() {
        let dir = populated_dir();
        let files = build(&dir, Redaction::Full, &[]);
        let active = &files["active.json"];
        assert!(active.contains(&format!("body-{SENTINEL}")));
        let manifest: serde_json::Value = serde_json::from_str(&files["manifest.json"]).unwrap();
        assert_eq!(manifest["redaction"], "full");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn user_names_in_paths_are_replaced_everywhere() {
        let dir = populated_dir();
        let mut config = sentinel_config();
        config.rules[0].action = Action::Open {
            target: r"D:\Profiles\alice\tool.exe".into(),
            args: vec![],
            working_directory: None,
        };
        fs::write(
            dir.join("active.json"),
            serde_json::to_string(&config).unwrap(),
        )
        .unwrap();
        let files = build(&dir, Redaction::Full, &[r"D:\Profiles\alice"]);
        assert!(files["logs/quicker-text.2026-10-22.log"].contains(r"C:\Users\%USERNAME%\AppData"));
        assert!(files["active.json"].contains(r"D:\\Profiles\\%USERNAME%\\tool.exe"));
        for (name, text) in &files {
            assert!(!text.contains("alice"), "{name} names the user:\n{text}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_bundle_file_is_a_complete_zip() {
        let dir = populated_dir();
        let target = dir.join("out.zip");
        let entries = write_bundle_file(&target, &dir, Redaction::Summary, &snapshot(&[])).unwrap();
        let files = unzip(fs::read(&target).unwrap());
        assert_eq!(files.len(), entries.len());
        // A target that cannot be created is an error and leaves nothing.
        let blocked = dir.join("missing-dir").join("out.zip");
        assert!(write_bundle_file(&blocked, &dir, Redaction::Summary, &snapshot(&[])).is_err());
        assert!(!blocked.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_files_and_an_empty_directory_still_make_a_bundle() {
        let dir = temp_dir();
        let files = build(&dir, Redaction::Full, &[]);
        assert!(!files.contains_key("active.json"));
        let summary: serde_json::Value =
            serde_json::from_str(&files["config-summary.json"]).unwrap();
        assert_eq!(summary["active"]["file"]["status"], "missing");
        let manifest: serde_json::Value = serde_json::from_str(&files["manifest.json"]).unwrap();
        assert_eq!(manifest["skipped"][0]["file"], "active.json");
        assert_eq!(manifest["skipped"][0]["reason"], "NotFound");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_the_newest_crash_records_and_log_tail_are_kept() {
        let dir = temp_dir();
        for n in 0..MAX_CRASH_RECORDS + 3 {
            fs::write(dir.join(format!("crash-20261022-{n:06}.txt")), "x").unwrap();
        }
        let files = build(&dir, Redaction::Summary, &[]);
        let crashes: Vec<_> = files
            .keys()
            .filter(|name| name.starts_with("crashes/"))
            .collect();
        assert_eq!(crashes.len(), MAX_CRASH_RECORDS);
        assert_eq!(crashes[0], "crashes/crash-20261022-000003.txt");
        assert!(files["manifest.json"].contains("3 older crash records omitted"));

        let log = dir.join("big.log");
        fs::write(&log, "first line\nsecond line\nthird line\n").unwrap();
        let tail = read_tail(&log, 15).unwrap();
        assert_eq!(
            tail,
            "[earlier lines omitted: the file has 34 bytes]\nthird line\n"
        );
        assert_eq!(
            read_tail(&log, 1000).unwrap(),
            "first line\nsecond line\nthird line\n"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn log_names_and_codes_are_validated() {
        assert_eq!(log_date("quicker-text.2026-10-22.log"), Some("2026-10-22"));
        for name in [
            "quicker-text.2026-10-22.log.bak",
            "quicker-text.2026-1-22.log",
            "other.2026-10-22.log",
            "quicker-text.log",
        ] {
            assert_eq!(log_date(name), None, "{name}");
        }
        assert_eq!(code_or_other("regex_invalid"), "regex_invalid");
        assert_eq!(code_or_other("Hello world"), "other");
        assert_eq!(code_or_other(""), "other");
        assert_eq!(
            default_file_name(UNIX_EPOCH + Duration::from_secs(NOW_SECS)),
            "quicker-text-diagnostics-20261022-102133.zip"
        );
    }

    /// S-08 privacy test, store half: phrase content goes through every store
    /// path that logs (save, publish, rollback, a rejected save, an internal
    /// error, prefs quarantine, a log level change) with the most verbose
    /// level on a real log file. Neither the log nor a Summary bundle of the
    /// same data directory may contain it. The "100 executions" half needs
    /// the executor (P1a).
    #[test]
    fn phrase_content_never_reaches_the_log_or_the_summary_bundle() {
        use crate::diagnostics::{
            level::LogLevelControl,
            logging::test_support::{read_logs, scoped},
        };
        use crate::store::{prefs::load_prefs, Store};
        use std::sync::Arc;
        use tracing::level_filters::LevelFilter;

        let dir = temp_dir();
        let built = scoped(&log_dir(&dir), LevelFilter::TRACE);
        tracing::subscriber::with_default(built.subscriber, || {
            tracing::info!(event = "test.start", "privacy test");
            let mut store = Store::load(dir.clone()).unwrap();
            let mut config = sentinel_config();
            config.rules[0].enabled = true;
            config.rules[0].notification.enabled = true;
            config.rules[0].notification.message = format!("notify-{SENTINEL}");
            config.rules[1].match_spec.value = format!("literal-{SENTINEL}");
            config.rules[1].match_spec.kind = MatchKind::Literal;
            store.save(config.clone()).unwrap();
            store.publish(config.clone()).unwrap();
            config.rules[0].description = format!("edited-{SENTINEL}");
            store.save(config.clone()).unwrap();
            store.publish(config.clone()).unwrap();
            store.rollback_previous().unwrap();
            let mut invalid = config.clone();
            invalid.rules[1].match_spec = MatchSpec {
                kind: MatchKind::Regex,
                value: format!("({SENTINEL}"),
            };
            assert!(store.save(invalid).is_err());
            store.active.revision = u64::MAX;
            assert!(store.publish(config).is_err());

            fs::write(dir.join("prefs.json"), format!("{{\"app\": {SENTINEL}")).unwrap();
            assert!(load_prefs(&dir).is_ok());
            let control = LogLevelControl::start(
                dir.clone(),
                Arc::new(quicker_platform::traits::SystemClock),
                Arc::new(|_| {}),
            );
            control.set(LogLevel::Debug, Some(1.0)).unwrap();
            drop(control);
        });
        drop(built.guard);

        let log = read_logs(&log_dir(&dir));
        for expected in [
            "test.start",
            "command.internal",
            "store.quarantine",
            "log.level_set",
        ] {
            assert!(
                log.contains(expected),
                "{expected} missing from the log:\n{log}"
            );
        }
        assert!(
            !log.contains(SENTINEL),
            "the log leaks the sentinel:\n{log}"
        );
        // The data directory does hold the sentinel (configuration,
        // revisions, the quarantined prefs), so the bundle has to filter.
        assert!(fs::read_to_string(dir.join("active.json"))
            .unwrap()
            .contains(SENTINEL));

        // The log file is dated by the real clock.
        let mut now = snapshot(&[]);
        now.now = SystemTime::now();
        let (cursor, _) =
            build_bundle(&dir, Redaction::Summary, Cursor::new(Vec::new()), &now).unwrap();
        let files = unzip(cursor.into_inner());
        assert!(files.keys().any(|name| name.starts_with("logs/")));
        for (name, text) in &files {
            assert!(
                !text.contains(SENTINEL),
                "{name} leaks the sentinel:\n{text}"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// S-08: export < 5 s. A week of 2 MiB logs stands in for verbose days.
    #[test]
    fn a_week_of_logs_is_bundled_well_inside_the_budget() {
        let dir = temp_dir();
        let logs = log_dir(&dir);
        fs::create_dir_all(&logs).unwrap();
        let line = "2026-10-22T10:21:33.000000Z  INFO quicker_text::store: event=\"store.publish\" revision=3 rules=120 elapsed_ms=4\n";
        let day = line.repeat(2 * 1024 * 1024 / line.len());
        for d in 16..=22 {
            fs::write(logs.join(format!("quicker-text.2026-10-{d}.log")), &day).unwrap();
        }
        fs::write(
            dir.join("active.json"),
            serde_json::to_string(&sentinel_config()).unwrap(),
        )
        .unwrap();
        let started = Instant::now();
        let (cursor, entries) = build_bundle(
            &dir,
            Redaction::Full,
            Cursor::new(Vec::new()),
            &snapshot(&[r"C:\Users\alice"]),
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(
            entries
                .iter()
                .filter(|name| name.starts_with("logs/"))
                .count(),
            7
        );
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
        eprintln!(
            "bundle of {} MiB logs: {elapsed:?}, {} KiB zip",
            7 * 2,
            cursor.into_inner().len() / 1024
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
