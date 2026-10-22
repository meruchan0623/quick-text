//! Diagnostics commands (§7.4 「诊断」, docs/ipc-contract-0.4.md §5).
//!
//! Every command uses `rename_all = "snake_case"`, so the frontend passes
//! arguments with the contract's names (`ttl_hours`, not Tauri's default
//! camelCase `ttlHours`).
use crate::{
    app_state::{DataDir, Managed},
    commands::import_export::{dialog, Filter},
    diagnostics::{
        bundle::{
            default_file_name, write_bundle_file, HealthReport, LastResult, Redaction, Snapshot,
            SystemInfo,
        },
        level::{LevelState, LogLevel, LogLevelControl},
        logging::log_dir,
        redact::{fingerprint, truncate_utf8, user_homes},
    },
    error::{CommandError, IoOp},
};
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::{Instant, SystemTime},
};
use tauri::State;

/// Save dialog filter for `export_diagnostics`.
const BUNDLE_FILTERS: &[Filter] = &[("诊断包", &["zip"])];
/// How much of a `log_frontend_error` stack is read (contract: 8 KiB). What
/// is logged is shorter: only parts of the frames in it.
pub const FRONTEND_ERROR_MAX_BYTES: usize = 8 * 1024;
/// Metric names `report_metric` accepts (contract §5, 0.4).
pub const METRICS: [&str; 2] = ["ui_first_paint_ms", "ui_event_lag_ms"];

/// `ttl_hours: null` restores the default level (`info`).
#[tauri::command(rename_all = "snake_case")]
pub fn set_log_level(
    control: State<LogLevelControl>,
    level: LogLevel,
    ttl_hours: Option<f64>,
) -> Result<LevelState, CommandError> {
    control.set(level, ttl_hours)
}

/// Writes the diagnostics bundle (see [`crate::diagnostics::bundle`]) where
/// the user chooses; `Ok(None)` when the dialog is cancelled.
#[tauri::command(rename_all = "snake_case")]
pub fn export_diagnostics(
    store: State<Managed>,
    control: State<LogLevelControl>,
    data_dir: State<DataDir>,
    redaction: Redaction,
) -> Result<Option<String>, CommandError> {
    // The dialog is modal: choose the target before taking any lock.
    let Some(target) = dialog(BUNDLE_FILTERS)
        .set_file_name(default_file_name(SystemTime::now()))
        .save_file()
    else {
        return Ok(None);
    };
    let started = Instant::now();
    let snapshot = Snapshot {
        now: SystemTime::now(),
        health: health_report(&store, &control),
        system: SystemInfo::current(),
        homes: user_homes(),
    };
    let entries = write_bundle_file(&target, &data_dir.0, redaction, &snapshot)
        .map_err(|e| CommandError::io(IoOp::Write, Some(&target), &e))?;
    tracing::info!(
        event = "diagnostics.export",
        redaction = redaction.as_str(),
        entries = entries.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "diagnostics bundle written"
    );
    Ok(Some(target.to_string_lossy().into_owned()))
}

/// The runtime half of the bundle. The store lock is held only to clone
/// the engine handle.
fn health_report(store: &Managed, control: &LogLevelControl) -> HealthReport {
    let capabilities = quicker_platform::capabilities();
    let (engine, revision) = {
        let store = store.lock();
        (store.engine.clone(), store.active.revision)
    };
    let paused = engine.paused.load(Ordering::SeqCst);
    let busy = engine.busy.load(Ordering::SeqCst);
    let last_result = engine
        .last
        .lock()
        .ok()
        .and_then(|last| last.as_ref().map(LastResult::from));
    HealthReport {
        health: None,
        engine_status: if busy {
            "running"
        } else if paused {
            "paused"
        } else {
            "listening"
        },
        paused,
        busy,
        revision,
        last_result,
        capabilities,
        log_level: control.current(),
    }
}

#[tauri::command(rename_all = "snake_case")]
pub fn open_log_dir(data_dir: State<DataDir>) -> Result<(), CommandError> {
    open_dir(&log_dir(&data_dir.0), reveal)
}

#[tauri::command(rename_all = "snake_case")]
pub fn open_data_dir(data_dir: State<DataDir>) -> Result<(), CommandError> {
    open_dir(&data_dir.0, reveal)
}

/// Creates `dir` if it is missing and hands it to `open`; either failing is
/// `Io { op: open }`.
fn open_dir(dir: &Path, open: impl FnOnce(&Path) -> io::Result<()>) -> Result<(), CommandError> {
    fs::create_dir_all(dir)
        .and_then(|()| open(dir))
        .map_err(|e| CommandError::io(IoOp::Open, Some(dir), &e))
}

/// Shows `dir` in the system file manager. Only a failed launch is an
/// error: `explorer.exe` exits with 1 even when it opened the folder.
fn reveal(dir: &Path) -> io::Result<()> {
    // Explorer needs backslashes; collecting the components normalizes them.
    let dir: PathBuf = dir.components().collect();
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(program).arg(&dir).spawn()?;
    // Reap the child so it does not linger as a zombie on Unix.
    let _ = std::thread::Builder::new()
        .name("quicker-open-dir".into())
        .spawn(move || child.wait());
    Ok(())
}

/// Logs a frontend error as `frontend.error` (warn): its structure only,
/// see [`FrontendError`].
#[tauri::command(rename_all = "snake_case")]
pub fn log_frontend_error(
    assets: State<BundledAssets>,
    message: String,
    stack: Option<String>,
    source: Option<String>,
) {
    record_frontend_error(&assets, &message, stack.as_deref(), source.as_deref());
}

/// The Vite chunks embedded in this binary, by path
/// (`assets/index-D3f9Kq_x.js`), each with its [`Extent`]: the trusted list
/// a script location in a frontend error must be on, at a line and column
/// inside the script, to be logged. Empty (so every location is dropped)
/// unless built from the app's assets; in dev builds the dev server serves
/// the frontend and nothing is embedded.
#[derive(Debug, Default)]
pub struct BundledAssets(HashMap<String, Extent>);

impl BundledAssets {
    /// The app's embedded chunks. `AssetResolver::iter` gives the exact keys
    /// (`AssetResolver::get` falls back to `index.html` for an unknown path,
    /// so it cannot tell what exists) but compressed bytes; `get` of a key
    /// `iter` gave returns that asset decompressed. Only chunks are read:
    /// in a release build that is the app's JavaScript, read once.
    pub fn embedded<R: tauri::Runtime>(resolver: &tauri::AssetResolver<R>) -> Self {
        let chunks: Vec<String> = resolver
            .iter()
            .map(|(key, _)| key.into_owned())
            .filter(|key| chunk_path(key).is_some())
            .collect();
        Self::new(chunks.into_iter().filter_map(|key| {
            let asset = resolver.get(key.clone())?;
            Some((key, asset.bytes))
        }))
    }

    /// From `(key, contents)` pairs, keys as Tauri lists them
    /// (`/assets/index-D3f9Kq_x.js`). Assets other than chunks are left out.
    pub fn new<K: AsRef<str>, B: AsRef<[u8]>>(assets: impl IntoIterator<Item = (K, B)>) -> Self {
        let measured = |(key, contents): (K, B)| {
            let path = chunk_path(key.as_ref())?.to_string();
            Some((path, Extent::measure(contents.as_ref())))
        };
        Self(assets.into_iter().filter_map(measured).collect())
    }

    fn extent(&self, path: &str) -> Option<&Extent> {
        self.0.get(path)
    }
}

/// `assets/index-D3f9Kq_x.js` of an asset key `/assets/index-D3f9Kq_x.js`
/// when it names a Vite chunk (see [`vite_chunk_hash`]).
fn chunk_path(key: &str) -> Option<&str> {
    let path = key.strip_prefix('/').unwrap_or(key);
    vite_chunk_hash(path.strip_prefix("assets/")?).map(|_| path)
}

/// Above this many lines, an [`Extent`] keeps only the line count and the
/// longest line (a minified Vite chunk has a few dozen lines).
const MAX_LINE_TABLE: usize = 4096;

/// The lines of a script, as JavaScript engines number them: a line ends
/// at LF, CR, CRLF, U+2028 or U+2029. Lengths are in bytes, which is never
/// less than the UTF-16 code units engines count columns in, so the bound
/// is loose but never drops a real location.
#[derive(Debug, PartialEq, Eq)]
enum Extent {
    /// The length of each line.
    Lines(Box<[u32]>),
    /// More than [`MAX_LINE_TABLE`] lines: how many, and the longest.
    Long { lines: u32, longest: u32 },
}

impl Extent {
    fn measure(script: &[u8]) -> Self {
        let mut table = Vec::new();
        let (mut lines, mut longest) = (0u32, 0u32);
        for length in line_lengths(script) {
            let length = u32::try_from(length).unwrap_or(u32::MAX);
            lines = lines.saturating_add(1);
            longest = longest.max(length);
            if table.len() <= MAX_LINE_TABLE {
                table.push(length);
            }
        }
        if table.len() <= MAX_LINE_TABLE {
            Self::Lines(table.into_boxed_slice())
        } else {
            Self::Long { lines, longest }
        }
    }

    /// Whether `line` and `column`, both counted from one as V8 and WebKit
    /// print them, fall inside the script; a column may be one past the end
    /// of its line, where an engine reports an unexpected end.
    fn admits(&self, line: u32, column: u32) -> bool {
        let length = match self {
            Self::Lines(lengths) => {
                let index = (line as usize).checked_sub(1);
                match index.and_then(|index| lengths.get(index)) {
                    Some(&length) => length,
                    None => return false,
                }
            }
            Self::Long { lines, longest } if (1..=*lines).contains(&line) => *longest,
            Self::Long { .. } => return false,
        };
        (1..=length.saturating_add(1)).contains(&column)
    }
}

/// The byte length of each line of `script`, see [`Extent`].
fn line_lengths(script: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut rest = Some(script);
    std::iter::from_fn(move || {
        let text = rest?;
        for (at, &byte) in text.iter().enumerate() {
            let terminator = match byte {
                b'\n' => 1,
                b'\r' if text.get(at + 1) == Some(&b'\n') => 2,
                b'\r' => 1,
                0xE2 if matches!(text.get(at + 1..at + 3), Some([0x80, 0xA8 | 0xA9])) => 3,
                _ => continue,
            };
            rest = Some(&text[at + terminator..]);
            return Some(at);
        }
        rest = None;
        Some(text.len())
    })
}

/// [`log_frontend_error`] with the trusted asset list passed in.
///
/// Threat model: the input comes from the app's own frontend. A
/// compromised frontend can read every phrase through `load_state` or
/// `export_config` anyway, so a malicious caller is out of scope; what is
/// guarded against is accidental leakage, an error message, `eval`ed code
/// or a function name carrying phrase text into the log. So only fixed
/// labels, lengths, fingerprints and script locations are kept, and a
/// location only when it names a chunk embedded in the binary at a line and
/// column inside that chunk, printed from the parsed numbers. Accepted
/// residual risk: a number that falls inside a chunk's real extent is kept
/// as a line or column.
pub fn record_frontend_error(
    assets: &BundledAssets,
    message: &str,
    stack: Option<&str>,
    source: Option<&str>,
) {
    let report = FrontendError::fit(assets, message, stack, source);
    tracing::warn!(
        event = "frontend.error",
        class = report.class.as_str(),
        message_len = report.message_len,
        message_hash = %report.message_hash,
        source = report.source.as_deref(),
        source_dropped = report.source_dropped,
        stack = ?report.frames,
        dropped_lines = report.dropped_lines,
        truncated = report.truncated,
        "frontend error"
    );
}

/// What `log_frontend_error` keeps of a report. Every string in a frontend
/// error is supplied by the caller and can carry phrase content (a message
/// built from a rule, a function or file name, a query string), so none of
/// it is logged (S-08): only a fixed class label, the message's length and
/// fingerprint, and the content hashes of scripts embedded in the binary
/// with positions inside them (see [`record_frontend_error`]).
#[derive(Debug, PartialEq, Eq)]
struct FrontendError {
    class: ErrorClass,
    /// Byte length of the whole message.
    message_len: usize,
    /// [`fingerprint`] of the whole message, to group repeated reports.
    message_hash: String,
    /// See [`asset_location`].
    source: Option<String>,
    /// A source was given but is not in the app's bundled scripts.
    source_dropped: bool,
    /// The frames in the app's bundled scripts, see [`asset_location`].
    frames: Vec<String>,
    /// Stack lines left out: not a frame, or a frame outside the app's
    /// bundled scripts (the first line of a V8 stack repeats the message).
    dropped_lines: usize,
    /// The stack was longer than [`FRONTEND_ERROR_MAX_BYTES`]; only the
    /// complete lines of its head were read.
    truncated: bool,
}
impl FrontendError {
    fn fit(
        assets: &BundledAssets,
        message: &str,
        stack: Option<&str>,
        source: Option<&str>,
    ) -> Self {
        let stack = stack.unwrap_or("");
        let mut head = truncate_utf8(stack, FRONTEND_ERROR_MAX_BYTES);
        let truncated = head.len() < stack.len();
        if truncated {
            // The cut line is incomplete: its line and column may be wrong.
            head = head.rsplit_once('\n').map_or("", |(lines, _)| lines);
        }
        let mut frames = Vec::new();
        let mut dropped_lines = 0;
        for line in head.lines().map(str::trim).filter(|line| !line.is_empty()) {
            match stack_frame(assets, line) {
                Some(frame) => frames.push(frame),
                None => dropped_lines += 1,
            }
        }
        let kept_source = source.and_then(|source| asset_location(assets, source));
        Self {
            class: ErrorClass::of(message),
            message_len: message.len(),
            message_hash: fingerprint(message),
            source_dropped: source.is_some() && kept_source.is_none(),
            source: kept_source,
            frames,
            dropped_lines,
            truncated,
        }
    }
}

/// The error classes `frontend.error` names: JavaScript's built-in errors
/// and the contract's `CommandError`. Anything else (an app-defined class,
/// a phrase shaped like `SecretError`, free text) is `other`. What is logged
/// is always [`ErrorClass::as_str`], never the caller's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorClass {
    Error,
    TypeError,
    RangeError,
    ReferenceError,
    SyntaxError,
    EvalError,
    UriError,
    AggregateError,
    DomException,
    CommandError,
    Other,
}

impl ErrorClass {
    const NAMED: [Self; 10] = [
        Self::Error,
        Self::TypeError,
        Self::RangeError,
        Self::ReferenceError,
        Self::SyntaxError,
        Self::EvalError,
        Self::UriError,
        Self::AggregateError,
        Self::DomException,
        Self::CommandError,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::TypeError => "TypeError",
            Self::RangeError => "RangeError",
            Self::ReferenceError => "ReferenceError",
            Self::SyntaxError => "SyntaxError",
            Self::EvalError => "EvalError",
            Self::UriError => "URIError",
            Self::AggregateError => "AggregateError",
            Self::DomException => "DOMException",
            Self::CommandError => "CommandError",
            Self::Other => "other",
        }
    }

    /// The class `TypeError: ...` starts with, also behind `Uncaught ` or
    /// `Uncaught (in promise) ` as the webview reports uncaught errors.
    fn of(message: &str) -> Self {
        let message = message.trim_start();
        let message = ["Uncaught (in promise) ", "Uncaught "]
            .into_iter()
            .find_map(|prefix| message.strip_prefix(prefix))
            .unwrap_or(message);
        let head = message.split_once(':').map_or(message, |(head, _)| head);
        Self::NAMED
            .into_iter()
            .find(|class| class.as_str() == head)
            .unwrap_or(Self::Other)
    }
}

/// The location of one stack line, see [`asset_location`]: V8
/// (`at fn (url:1:2)`, `at url:1:2`) or SpiderMonkey and JavaScriptCore
/// (`fn@url:1:2`). Function names are never kept: nothing tells a real
/// function name from caller-supplied text.
fn stack_frame(assets: &BundledAssets, line: &str) -> Option<String> {
    let location = match line.strip_prefix("at ") {
        Some(rest) => rest
            .strip_suffix(')')
            .and_then(|inner| inner.split_once(" ("))
            .map_or(rest, |(_, location)| location),
        None => line.rsplit_once('@')?.1,
    };
    asset_location(assets, location)
}

/// Origins the app's own pages are served from.
const APP_ORIGINS: [&str; 3] = [
    "tauri://localhost/",
    "http://tauri.localhost/",
    "https://tauri.localhost/",
];

/// `D3f9Kq_x.js:line:col` of `url:line:col` when `url` is one of the app's
/// bundled scripts: served from an [`APP_ORIGINS`] origin, a Vite chunk
/// (see [`vite_chunk_hash`]) directly under `/assets/`, and embedded in this
/// binary ([`BundledAssets`]), with `line:col` inside that chunk
/// ([`Extent::admits`]). Membership is what makes the hash safe to log: a
/// caller can shape any text like a chunk name (`index-SECRETAB.js`), but
/// only a real build output is on the list. The line and column are parsed
/// ([`coordinate`]) and printed from the numbers, never copied. Only the
/// content hash of the name is kept, which identifies the chunk (and its
/// source map). Anything else (another host, the dev server, a relative
/// path, `blob:`, `data:`, `eval`, `<anonymous>`, a query string or
/// fragment, a position outside the chunk) is `None`.
fn asset_location(assets: &BundledAssets, location: &str) -> Option<String> {
    let (rest, column) = location.trim().rsplit_once(':')?;
    let (url, line) = rest.rsplit_once(':')?;
    let (line, column) = (coordinate(line)?, coordinate(column)?);
    let path = APP_ORIGINS
        .into_iter()
        .find_map(|origin| url.strip_prefix(origin))?;
    let hash = vite_chunk_hash(path.strip_prefix("assets/")?)?;
    assets
        .extent(path)?
        .admits(line, column)
        .then(|| format!("{hash}:{line}:{column}"))
}

/// A line or column number as engines print one: 1 to `u32::MAX` in
/// decimal digits, no sign, no leading zero.
fn coordinate(text: &str) -> Option<u32> {
    let canonical = matches!(text.as_bytes().first(), Some(b'1'..=b'9'))
        && text.bytes().all(|b| b.is_ascii_digit());
    if !canonical {
        return None;
    }
    // Fails past `u32::MAX`.
    text.parse().ok()
}

/// `D3f9Kq_x.js` of a Vite chunk file name `index-D3f9Kq_x.js`: a non-empty
/// name, `-`, an 8-character base64url hash, then `.js` or `.mjs`.
fn vite_chunk_hash(file: &str) -> Option<&str> {
    let (stem, extension) = file.rsplit_once('.')?;
    if !matches!(extension, "js" | "mjs") {
        return None;
    }
    let split = stem.len().checked_sub(8)?;
    let (name, hash) = (stem.get(..split)?.strip_suffix('-')?, stem.get(split..)?);
    let base64url = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_');
    let named = !name.is_empty() && name.bytes().all(|b| base64url(b) || b == b'.');
    (named && hash.bytes().all(base64url)).then(|| &file[split..])
}

/// Logs a whitelisted UI metric as `ui.metric`; other names and values that
/// are not finite and non-negative are ignored, as the contract says.
#[tauri::command(rename_all = "snake_case")]
pub fn report_metric(name: String, value: f64) {
    // TODO(P1a/P5): also feed the get_health p50/p95 histograms (§7.1).
    match accepted_metric(&name, value) {
        // Once per start: worth keeping at the default level.
        Some(name @ "ui_first_paint_ms") => {
            tracing::info!(event = "ui.metric", name, value, "ui metric")
        }
        Some(name) => tracing::debug!(event = "ui.metric", name, value, "ui metric"),
        None => {}
    }
}

fn accepted_metric(name: &str, value: f64) -> Option<&'static str> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    METRICS.into_iter().find(|known| *known == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("quicker-open-test-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn open_dir_creates_the_directory_and_reports_failures_as_open() {
        let dir = temp_dir().join("logs");
        let opened = RefCell::new(None);
        open_dir(&dir, |path| {
            *opened.borrow_mut() = Some(path.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert!(dir.is_dir());
        assert_eq!(opened.into_inner().as_deref(), Some(dir.as_path()));

        let err = open_dir(&dir, |_| Err(io::Error::from(io::ErrorKind::NotFound))).unwrap_err();
        assert!(
            matches!(err, CommandError::Io { op: IoOp::Open, .. }),
            "{err:?}"
        );

        // A file where the directory should be: creating it fails.
        let file = dir.join("not-a-dir");
        fs::write(&file, b"").unwrap();
        let err = open_dir(&file.join("x"), |_| unreachable!("not reached")).unwrap_err();
        let CommandError::Io { op, path, .. } = err else {
            panic!("expected Io, got {err:?}");
        };
        assert_eq!((op, path.as_deref()), (IoOp::Open, Some("x")));
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    /// What a build embeds, keyed as `AssetResolver::iter` lists it: the
    /// trusted list the tests run against. `index-D3f9Kq_x.js` has 50 lines
    /// of 400 bytes (columns 1 to 401), `vendor-a1B2-c_D.mjs` one line of 17
    /// bytes; the others are not chunks.
    fn bundled() -> BundledAssets {
        BundledAssets::new([
            ("/index.html", "<!doctype html>".to_string()),
            (
                "/assets/index-D3f9Kq_x.js",
                vec!["x".repeat(400); 50].join("\n"),
            ),
            ("/assets/index-D3f9Kq_x.css", "body{}".to_string()),
            (
                "/assets/vendor-a1B2-c_D.mjs",
                "export const a=1;".to_string(),
            ),
        ])
    }

    #[test]
    fn script_extents_follow_javascript_line_terminators() {
        let measured = Extent::measure("ab\ncd\r\nefg\rh\u{2028}ij\u{2029}中".as_bytes());
        assert_eq!(measured, Extent::Lines(Box::new([2, 2, 3, 1, 2, 3])));
        assert_eq!(Extent::measure(b""), Extent::Lines(Box::new([0])));
        assert_eq!(Extent::measure(b"a\n"), Extent::Lines(Box::new([1, 0])));
        for (line, column, admitted) in [
            (1, 1, true),
            (1, 3, true),
            (1, 4, false),
            (6, 4, true),
            (6, 5, false),
            (7, 1, false),
            (0, 1, false),
            (1, 0, false),
            (u32::MAX, 1, false),
        ] {
            assert_eq!(measured.admits(line, column), admitted, "{line}:{column}");
        }

        let long = format!("{}abcdef", "a\n".repeat(MAX_LINE_TABLE + 10));
        let lines = (MAX_LINE_TABLE + 11) as u32;
        let measured = Extent::measure(long.as_bytes());
        assert_eq!(measured, Extent::Long { lines, longest: 6 });
        assert!(measured.admits(lines, 7) && measured.admits(1, 7));
        assert!(!measured.admits(lines + 1, 1) && !measured.admits(1, 8));
        assert!(!measured.admits(0, 1) && !measured.admits(1, 0));
        let table = Extent::measure("a\n".repeat(MAX_LINE_TABLE - 1).as_bytes());
        assert!(matches!(table, Extent::Lines(lengths) if lengths.len() == MAX_LINE_TABLE));

        // Only chunks are kept, by path without the leading `/`.
        let assets = bundled();
        assert_eq!(assets.0.len(), 2);
        assert!(assets.extent("assets/vendor-a1B2-c_D.mjs").is_some());
        assert!(assets.extent("index.html").is_none());
        assert!(assets.extent("assets/index-D3f9Kq_x.css").is_none());

        for (text, parsed) in [
            ("1", Some(1)),
            ("401", Some(401)),
            ("4294967295", Some(u32::MAX)),
            ("4294967296", None),
            ("98765432109", None),
            ("0", None),
            ("012", None),
            ("00001234", None),
            ("+1", None),
            ("1e3", None),
            (" 1", None),
            ("", None),
        ] {
            assert_eq!(coordinate(text), parsed, "{text:?}");
        }
    }

    /// Only the head of a long stack is read, cut on a line (and so on a
    /// char boundary); the message is measured and fingerprinted whole.
    #[test]
    fn frontend_errors_share_one_budget_and_cut_on_char_boundaries() {
        let frame = "at render (http://tauri.localhost/assets/index-D3f9Kq_x.js:12:345)\n";
        let mut stack = frame.repeat(FRONTEND_ERROR_MAX_BYTES / frame.len() + 10);
        stack.push_str(&"中".repeat(5000));
        let long = "中".repeat(5000); // 15 000 bytes
        let cut = FrontendError::fit(&bundled(), &long, Some(&stack), Some(&long));
        assert!(cut.truncated);
        assert_eq!(cut.frames.len(), FRONTEND_ERROR_MAX_BYTES / frame.len());
        assert!(cut.frames.iter().all(|f| f == "D3f9Kq_x.js:12:345"));
        assert_eq!(cut.dropped_lines, 0);
        assert_eq!(cut.message_len, 15_000);
        assert_eq!(cut.message_hash, fingerprint(&long));
        assert_eq!((cut.class, cut.source), (ErrorClass::Other, None));
        assert!(cut.source_dropped);
        let logged: usize = cut.frames.iter().map(String::len).sum();
        assert!(logged <= FRONTEND_ERROR_MAX_BYTES, "{logged}");

        let cjk = FrontendError::fit(&bundled(), "boom", Some(&long), None);
        assert!(cjk.truncated);
        assert_eq!((cjk.frames.len(), cjk.dropped_lines), (0, 0));
        assert!(!cjk.source_dropped);
    }

    #[test]
    fn frontend_errors_keep_only_the_class_and_the_apps_frames() {
        let kept = "TypeError: Cannot read properties of undefined (reading 'body')\n\
            \x20   at RuleEditor (http://tauri.localhost/assets/index-D3f9Kq_x.js:12:345)\n\
            \x20   at async Object.save (tauri://localhost/assets/index-D3f9Kq_x.js:3:4)\n\
            \x20   at https://tauri.localhost/assets/vendor-a1B2-c_D.mjs:1:2\n\
            onClick@tauri://localhost/assets/index-D3f9Kq_x.js:20:30\n\
            global code@tauri://localhost/assets/index-D3f9Kq_x.js:21:31\n\
            \n";
        // Each line is dropped: the dev server, no hash, `<anonymous>`,
        // relative paths, outside `/assets/`, a subdirectory, eval,
        // `Function`, `blob:`, `data:`, another host, a style sheet, a
        // query string, a fragment, a 6-character hash, no name,
        // Vite-shaped names the build did not embed (any text, one character
        // off, a known hash under another name), and positions that are not
        // in the chunk or not written canonically (past the last line, past
        // the end of the line plus one, zero, a leading zero).
        let dropped = [
            "at new Store (http://localhost:1420/src/store.ts?t=1729591293:7:8)",
            "at http://tauri.localhost/assets/vendor.js:1:2",
            "at Array.map (<anonymous>)",
            "at Object.<anonymous> (./src/main.tsx:9:10)",
            "at load (/assets/index-D3f9Kq_x.js:9:10)",
            "at load (assets/index-D3f9Kq_x.js:9:10)",
            "at fn [as alias] (https://tauri.localhost/a/b/c-D3f9Kq_x.js:11:12)",
            "at load (http://tauri.localhost/assets/sub/index-D3f9Kq_x.js:1:1)",
            "at eval (eval at run (http://tauri.localhost/assets/index-D3f9Kq_x.js:1:2), <anonymous>:1:1)",
            "at new Function (<anonymous>)",
            "at blob:http://tauri.localhost/0b6c1f2e-1d2f-4c0e-9a52-0d1f2e3c4b5a:1:2",
            "at data:text/javascript,boom:1:2",
            "at load (https://example.com/assets/index-D3f9Kq_x.js:5:6)",
            "at load (http://tauri.localhost/assets/index-D3f9Kq_x.css:5:6)",
            "at load (http://tauri.localhost/assets/index-D3f9Kq_x.js?v=1:5:6)",
            "@http://tauri.localhost/assets/index-D3f9Kq_x.js:22:32#frag",
            "at load (http://tauri.localhost/assets/index-D3f9Kq.js:5:6)",
            "at load (http://tauri.localhost/assets/-D3f9Kq_x.js:5:6)",
            "at load (http://tauri.localhost/assets/D3f9Kq_x.js:5:6)",
            "at load (http://tauri.localhost/assets/index-SECRETAB.js:5:6)",
            "at load (http://tauri.localhost/assets/index-D3f9Kq_y.js:5:6)",
            "at load (http://tauri.localhost/assets/vendor-D3f9Kq_x.js:5:6)",
            "load@tauri://localhost/assets/index-a1B2-c_D.mjs:5:6",
            "at load (http://tauri.localhost/assets/index-D3f9Kq_x.js:51:1)",
            "at load (http://tauri.localhost/assets/index-D3f9Kq_x.js:50:402)",
            "load@tauri://localhost/assets/vendor-a1B2-c_D.mjs:1:19",
            "at load (http://tauri.localhost/assets/index-D3f9Kq_x.js:0:1)",
            "at load (http://tauri.localhost/assets/index-D3f9Kq_x.js:1:0)",
            "at load (http://tauri.localhost/assets/index-D3f9Kq_x.js:012:345)",
        ];
        let stack = format!("{kept}{}", dropped.join("\n"));
        let message = "Uncaught TypeError: Cannot read properties of undefined (reading 'body')";
        let source = "http://tauri.localhost/assets/index-D3f9Kq_x.js:40:5";
        let report = FrontendError::fit(&bundled(), message, Some(&stack), Some(source));
        assert_eq!(report.class, ErrorClass::TypeError);
        assert_eq!(report.source.as_deref(), Some("D3f9Kq_x.js:40:5"));
        assert!(!report.source_dropped);
        assert_eq!(
            report.frames,
            [
                "D3f9Kq_x.js:12:345",
                "D3f9Kq_x.js:3:4",
                "a1B2-c_D.mjs:1:2",
                "D3f9Kq_x.js:20:30",
                "D3f9Kq_x.js:21:31",
            ]
        );
        // And the message line.
        assert_eq!(report.dropped_lines, dropped.len() + 1);
        assert!(!report.truncated);

        // Without the app's asset list (no `AppHandle`, a dev build), no
        // location is trusted: everything but the class is dropped.
        let closed = FrontendError::fit(
            &BundledAssets::default(),
            message,
            Some(&stack),
            Some(source),
        );
        assert_eq!((closed.class, closed.source), (ErrorClass::TypeError, None));
        assert!(closed.source_dropped);
        assert!(closed.frames.is_empty(), "{:?}", closed.frames);
        assert_eq!(
            closed.dropped_lines,
            report.frames.len() + dropped.len() + 1
        );

        for (message, class) in [
            ("Error: boom", ErrorClass::Error),
            ("RangeError: Invalid array length", ErrorClass::RangeError),
            (
                "ReferenceError: x is not defined",
                ErrorClass::ReferenceError,
            ),
            ("SyntaxError: Unexpected token", ErrorClass::SyntaxError),
            ("EvalError", ErrorClass::EvalError),
            (
                "Uncaught (in promise) URIError: URI malformed",
                ErrorClass::UriError,
            ),
            (
                "Uncaught AggregateError: All promises were rejected",
                ErrorClass::AggregateError,
            ),
            (
                "DOMException: The operation was aborted.",
                ErrorClass::DomException,
            ),
            ("CommandError: io", ErrorClass::CommandError),
            ("IpcError", ErrorClass::Other),
            ("URIError2: x", ErrorClass::Other),
            ("typeerror: x", ErrorClass::Other),
            ("hello: world", ErrorClass::Other),
            ("Script error.", ErrorClass::Other),
            ("Type Error: x", ErrorClass::Other),
            ("", ErrorClass::Other),
        ] {
            let report = FrontendError::fit(&bundled(), message, None, None);
            assert_eq!(report.class, class, "{message}");
            assert_eq!(report.message_len, message.len());
            assert_eq!((report.frames.len(), report.dropped_lines), (0, 0));
        }
        for class in ErrorClass::NAMED {
            assert_eq!(ErrorClass::of(class.as_str()), class);
        }
        for source in [
            "window.onerror",
            "http://tauri.localhost/assets/index-D3f9Kq_x.js",
            "http://tauri.localhost/assets/index-D3f9Kq_x.js?v=3:40:5",
            "https://example.com/assets/index-D3f9Kq_x.js:1:2",
            "file:///C:/Users/alice/assets/index-D3f9Kq_x.js:1:2",
            "http://localhost:1420/src/main.tsx:1:1",
            "http://tauri.localhost/assets/index-D3f9Kq_x.js:x:1",
            "http://tauri.localhost/assets/index-SECRETAB.js:1:2",
            "http://tauri.localhost/index.html:1:2",
        ] {
            let report = FrontendError::fit(&bundled(), "e", None, Some(source));
            assert_eq!(
                (report.source, report.source_dropped),
                (None, true),
                "{source}"
            );
        }
    }

    /// S-08 privacy test, frontend half: phrase content in every field of a
    /// frontend error (after the class, a message without a class, a frame's
    /// function name, URL path and query string, the source) reaches neither
    /// the log nor a Summary bundle.
    #[test]
    fn frontend_error_text_never_reaches_the_log_or_the_summary_bundle() {
        use crate::diagnostics::{
            bundle::test_support::summary_entries,
            logging::test_support::{read_logs, scoped},
        };
        use tracing::level_filters::LevelFilter;

        const S: &str = "SENTINEL-frontend-91f3";
        // An identifier, so it would pass a bare identifier check.
        const IDENT: &str = "Sentinel91f3";
        let reports = [
            (
                format!("TypeError: cannot expand {S}"),
                Some(format!(
                    "TypeError: cannot expand {S}\n\
                     \x20   at {S} (http://tauri.localhost/assets/index-D3f9Kq_x.js:1:2)\n\
                     \x20   at render (http://localhost:1420/src/App.tsx?t={S}:3:4)\n\
                     \x20   at load (http://tauri.localhost/{S}/chunk-D3f9Kq_x.js:5:6)\n\
                     \x20   at load (http://tauri.localhost/assets/a-D3f9Kq_x.js#{S}:7:8)\n\
                     {S}@tauri://localhost/assets/index-D3f9Kq_x.js:9:10\n\
                     \x20   at {S}"
                )),
                Some(format!(
                    "http://tauri.localhost/assets/index-D3f9Kq_x.js?q={S}:11:12"
                )),
            ),
            (format!("{S} could not be typed"), None, Some(S.to_string())),
            (format!("{IDENT}: {S}"), Some(format!("{IDENT}@{S}")), None),
            (format!("Uncaught Error: {IDENT}"), None, Some(IDENT.into())),
            (format!("{S}:{S}:{S}"), Some(format!("{S}:1:2")), None),
        ];

        let dir = std::env::temp_dir().join(format!("quicker-fe-test-{}", uuid::Uuid::new_v4()));
        let built = scoped(&log_dir(&dir), LevelFilter::TRACE);
        let assets = bundled();
        tracing::subscriber::with_default(built.subscriber, || {
            for (message, stack, source) in &reports {
                record_frontend_error(&assets, message, stack.as_deref(), source.as_deref());
            }
        });
        drop(built.guard);

        let log = read_logs(&log_dir(&dir));
        assert_eq!(log.matches("frontend.error").count(), 5, "{log}");
        assert_eq!(log.matches(r#"class="other""#).count(), 3, "{log}");
        for expected in [
            r#"class="TypeError""#,
            r#"class="Error""#,
            r#"stack=["D3f9Kq_x.js:1:2", "D3f9Kq_x.js:9:10"]"#,
        ] {
            assert!(log.contains(expected), "{expected} missing:\n{log}");
        }
        for sentinel in [S, IDENT] {
            assert!(!log.contains(sentinel), "the log leaks {sentinel}:\n{log}");
        }

        let files = summary_entries(&dir);
        assert!(files.keys().any(|name| name.starts_with("logs/")));
        for (name, text) in &files {
            for sentinel in [S, IDENT] {
                assert!(!text.contains(sentinel), "{name} leaks {sentinel}:\n{text}");
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// Every file below `dir` (the log directory and the crash records) and
    /// its text.
    fn files_under(dir: &Path) -> Vec<(PathBuf, String)> {
        let mut files = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(files_under(&path));
            } else {
                let text = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
                files.push((path, text));
            }
        }
        files
    }

    /// S-08 privacy test for the shapes a syntax check lets through: phrases
    /// shaped like error classes (`SecretPhraseError`), like function names
    /// (`SecretPhrase`) and like script names (`SecretAbbreviation.js`, the
    /// Vite-shaped `Sentinel-abcdefgh.js`, which the build did not embed)
    /// reach no file in the data directory and no entry of a Summary bundle.
    #[test]
    fn phrase_shaped_classes_and_names_reach_no_log_crash_record_or_bundle_entry() {
        use crate::diagnostics::{
            bundle::test_support::summary_entries,
            logging::test_support::{read_logs, scoped},
        };
        use tracing::level_filters::LevelFilter;

        const SENTINELS: [&str; 4] = ["SecretPhrase", "SecretAbbreviation", "Sentinel", "abcdefgh"];
        let reports = [
            ("SecretPhraseError", None, None),
            ("SecretPhraseError: SentinelXyzException", None, None),
            (
                "SentinelXyzException",
                Some(
                    "SentinelXyzException\n    at SecretPhrase \
                     (http://tauri.localhost/assets/SecretAbbreviation.js:1:2)",
                ),
                Some("http://tauri.localhost/assets/SecretAbbreviation.js:1:2"),
            ),
            (
                "Uncaught TypeError: x",
                Some(
                    "    at SecretPhrase (http://tauri.localhost/assets/Sentinel-abcdefgh.js:3:4)\n\
                     \x20   at async SecretPhrase.SentinelXyz \
                     (http://tauri.localhost/assets/index-D3f9Kq_x.js:5:6)\n\
                     SecretPhrase@tauri://localhost/assets/Sentinel-abcdefgh.mjs:7:8\n\
                     \x20   at SecretAbbreviation.js:9:10\n\
                     \x20   at SecretPhrase (./assets/SecretAbbreviation-abcdefgh.js:9:10)",
                ),
                Some("tauri://localhost/assets/Sentinel-abcdefgh.js:11:12"),
            ),
        ];

        let dir = std::env::temp_dir().join(format!("quicker-fe-test-{}", uuid::Uuid::new_v4()));
        let built = scoped(&log_dir(&dir), LevelFilter::TRACE);
        let assets = bundled();
        tracing::subscriber::with_default(built.subscriber, || {
            for (message, stack, source) in reports {
                record_frontend_error(&assets, message, stack, source);
            }
        });
        drop(built.guard);

        let log = read_logs(&log_dir(&dir));
        assert_eq!(log.matches("frontend.error").count(), 4, "{log}");
        assert_eq!(log.matches(r#"class="other""#).count(), 3, "{log}");
        assert_eq!(log.matches(r#"class="TypeError""#).count(), 1, "{log}");
        assert_eq!(log.matches("source_dropped=true").count(), 2, "{log}");
        assert!(!log.contains(r#"source=""#), "{log}");
        for expected in [
            r#"stack=["D3f9Kq_x.js:5:6"]"#,
            "dropped_lines=2",
            "dropped_lines=4",
        ] {
            assert!(log.contains(expected), "{expected} missing:\n{log}");
        }

        let on_disk = files_under(&dir);
        assert!(!on_disk.is_empty());
        let bundle = summary_entries(&dir);
        assert!(bundle.keys().any(|name| name.starts_with("logs/")));
        let every_entry = on_disk
            .iter()
            .map(|(path, text)| (path.display().to_string(), text))
            .chain(
                bundle
                    .iter()
                    .map(|(name, text)| (format!("bundle:{name}"), text)),
            );
        for (name, text) in every_entry {
            for sentinel in SENTINELS {
                assert!(!text.contains(sentinel), "{name} leaks {sentinel}:\n{text}");
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// S-08 privacy test for the chunk hash: a caller can shape any short
    /// text like a Vite chunk (`index-SECRETAB.js`, an abbreviation padded to
    /// 8 characters), so a hash is logged only for a script the build
    /// embedded. Through `source` and stack frames alike, such a name reaches
    /// no file in the data directory and no entry of a Summary or Full bundle
    /// (but Full's `active.json`, which carries phrases by design); a chunk
    /// that is embedded keeps its hash.
    #[test]
    fn caller_chosen_chunk_hashes_reach_no_log_crash_record_or_bundle_entry() {
        use crate::diagnostics::{
            bundle::test_support::{entries, summary_entries},
            logging::test_support::{read_logs, scoped},
        };
        use quicker_domain::{Config, MatchKind, MatchSpec, Rule};
        use tracing::level_filters::LevelFilter;

        const SENTINELS: [&str; 3] = ["SECRETAB", "ab______", "abc-----"];
        let dir = std::env::temp_dir().join(format!("quicker-fe-test-{}", uuid::Uuid::new_v4()));
        let built = scoped(&log_dir(&dir), LevelFilter::TRACE);
        let assets = bundled();
        tracing::subscriber::with_default(built.subscriber, || {
            for hash in SENTINELS {
                let stack = format!(
                    "TypeError: x\n\
                     \x20   at render (http://tauri.localhost/assets/index-{hash}.js:1:2)\n\
                     load@tauri://localhost/assets/vendor-{hash}.mjs:3:4\n\
                     \x20   at https://tauri.localhost/assets/index-{hash}.js:5:6"
                );
                let source = format!("tauri://localhost/assets/index-{hash}.js:1:2");
                record_frontend_error(&assets, "TypeError: x", Some(&stack), Some(&source));
            }
            // The positive case: the same shapes, for a chunk the build embedded.
            record_frontend_error(
                &assets,
                "TypeError: x",
                Some("    at render (http://tauri.localhost/assets/index-D3f9Kq_x.js:3:4)"),
                Some("tauri://localhost/assets/index-D3f9Kq_x.js:1:2"),
            );
        });
        drop(built.guard);

        let log = read_logs(&log_dir(&dir));
        assert_eq!(log.matches("frontend.error").count(), 4, "{log}");
        assert_eq!(log.matches("source_dropped=true").count(), 3, "{log}");
        assert_eq!(log.matches("stack=[]").count(), 3, "{log}");
        assert_eq!(log.matches("dropped_lines=4").count(), 3, "{log}");
        let kept = r#"source="D3f9Kq_x.js:1:2" source_dropped=false stack=["D3f9Kq_x.js:3:4"] dropped_lines=0"#;
        assert!(log.contains(kept), "{kept} missing:\n{log}");

        // Before `active.json` exists, every file in the data directory is
        // one the sinks wrote.
        let on_disk = files_under(&dir);
        assert!(!on_disk.is_empty());
        for (path, text) in &on_disk {
            for sentinel in SENTINELS {
                let path = path.display();
                assert!(!text.contains(sentinel), "{path} leaks {sentinel}:\n{text}");
            }
        }

        // A configuration that carries the sentinels, so the bundles have
        // them to leak.
        let config = Config {
            rules: vec![Rule {
                id: "chunk-hash".into(),
                match_spec: MatchSpec {
                    kind: MatchKind::Literal,
                    value: SENTINELS.join(" "),
                },
                ..Rule::default()
            }],
            ..Config::default()
        };
        fs::write(
            dir.join("active.json"),
            serde_json::to_string(&config).unwrap(),
        )
        .unwrap();
        let summary = summary_entries(&dir);
        let full = entries(&dir, Redaction::Full);
        assert!(summary.keys().any(|name| name.starts_with("logs/")));
        for sentinel in SENTINELS {
            assert!(full["active.json"].contains(sentinel), "{sentinel}");
        }
        let every_entry = summary
            .iter()
            .map(|(name, text)| (format!("summary:{name}"), text))
            .chain(
                full.iter()
                    .filter(|(name, _)| *name != "active.json")
                    .map(|(name, text)| (format!("full:{name}"), text)),
            );
        for (name, text) in every_entry {
            for sentinel in SENTINELS {
                assert!(!text.contains(sentinel), "{name} leaks {sentinel}:\n{text}");
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// S-08 privacy test for line and column numbers, the caller text left
    /// in a location of a bundled chunk: digits that are not a position
    /// inside the chunk (a numeric phrase, a zero-padded one, past the last
    /// line or the end of a line, beyond `u32`) drop the whole frame through
    /// `source`, V8 frames and Firefox frames alike, and reach no file in the
    /// data directory and no entry of a Summary bundle. Positions inside the
    /// chunk are kept, printed from the parsed numbers.
    #[test]
    fn caller_chosen_coordinates_reach_no_log_or_bundle_entry() {
        use crate::diagnostics::{
            bundle::test_support::summary_entries,
            logging::test_support::{read_logs, scoped},
        };
        use tracing::level_filters::LevelFilter;

        // Searchable sentinels: digit runs that occur nowhere else in a log.
        const SENTINELS: [&str; 5] = [
            "1234567890",
            "00001234",
            "00000012",
            "00000345",
            "98765432109",
        ];
        // `index-D3f9Kq_x.js` has 50 lines of 400 bytes.
        let positions = [
            ("1234567890", "1"),
            ("1", "1234567890"),
            ("00001234", "1"),
            ("1", "00001234"),
            ("00000012", "00000345"),
            ("98765432109", "1"),
            ("1", "98765432109"),
            ("51", "1"),
            ("50", "402"),
            ("0", "1"),
            ("1", "0"),
        ];
        let dir = std::env::temp_dir().join(format!("quicker-fe-test-{}", uuid::Uuid::new_v4()));
        let built = scoped(&log_dir(&dir), LevelFilter::TRACE);
        let assets = bundled();
        tracing::subscriber::with_default(built.subscriber, || {
            for (line, column) in positions {
                let at = format!("assets/index-D3f9Kq_x.js:{line}:{column}");
                let stack = format!(
                    "TypeError: x\n\
                     \x20   at render (http://tauri.localhost/{at})\n\
                     \x20   at https://tauri.localhost/{at}\n\
                     render@tauri://localhost/{at}"
                );
                let source = format!("tauri://localhost/{at}");
                record_frontend_error(&assets, "TypeError: x", Some(&stack), Some(&source));
            }
            // In range, at the edges of the chunk and of a line.
            record_frontend_error(
                &assets,
                "TypeError: x",
                Some(
                    "    at render (http://tauri.localhost/assets/index-D3f9Kq_x.js:1:1)\n\
                     \x20   at https://tauri.localhost/assets/vendor-a1B2-c_D.mjs:1:18\n\
                     render@tauri://localhost/assets/index-D3f9Kq_x.js:12:345",
                ),
                Some("tauri://localhost/assets/index-D3f9Kq_x.js:50:401"),
            );
        });
        drop(built.guard);

        let log = read_logs(&log_dir(&dir));
        let dropped = positions.len();
        assert_eq!(log.matches("frontend.error").count(), dropped + 1, "{log}");
        assert_eq!(log.matches("source_dropped=true").count(), dropped, "{log}");
        assert_eq!(log.matches("stack=[]").count(), dropped, "{log}");
        assert_eq!(log.matches("dropped_lines=4").count(), dropped, "{log}");
        let kept = r#"source="D3f9Kq_x.js:50:401" source_dropped=false stack=["D3f9Kq_x.js:1:1", "a1B2-c_D.mjs:1:18", "D3f9Kq_x.js:12:345"] dropped_lines=0"#;
        assert!(log.contains(kept), "{kept} missing:\n{log}");
        // No other position of the chunk made it.
        assert_eq!(log.matches("D3f9Kq_x.js:").count(), 3, "{log}");

        let on_disk = files_under(&dir);
        assert!(!on_disk.is_empty());
        let bundle = summary_entries(&dir);
        assert!(bundle.keys().any(|name| name.starts_with("logs/")));
        let every_entry = on_disk
            .iter()
            .map(|(path, text)| (path.display().to_string(), text))
            .chain(
                bundle
                    .iter()
                    .map(|(name, text)| (format!("bundle:{name}"), text)),
            );
        for (name, text) in every_entry {
            for sentinel in SENTINELS {
                assert!(!text.contains(sentinel), "{name} leaks {sentinel}:\n{text}");
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_whitelisted_finite_metrics_are_accepted() {
        assert_eq!(
            accepted_metric("ui_first_paint_ms", 412.5),
            Some("ui_first_paint_ms")
        );
        assert_eq!(
            accepted_metric("ui_event_lag_ms", 0.0),
            Some("ui_event_lag_ms")
        );
        for (name, value) in [
            ("ui_first_paint_ms", f64::NAN),
            ("ui_first_paint_ms", f64::INFINITY),
            ("ui_event_lag_ms", -1.0),
            ("phrase_text", 1.0),
            ("", 1.0),
        ] {
            assert_eq!(accepted_metric(name, value), None, "{name} {value}");
        }
    }
}
