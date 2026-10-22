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
pub fn log_frontend_error(message: String, stack: Option<String>, source: Option<String>) {
    let report = FrontendError::fit(&message, stack.as_deref(), source.as_deref());
    tracing::warn!(
        event = "frontend.error",
        class = report.class.as_deref(),
        message_len = report.message_len,
        message_hash = %report.message_hash,
        source = report.source.as_deref(),
        stack = ?report.frames,
        dropped_lines = report.dropped_lines,
        truncated = report.truncated,
        "frontend error"
    );
}

/// What `log_frontend_error` keeps of a report. The text of a frontend error
/// can carry phrase content (a message built from a rule, a query string in
/// a frame's URL), so none of it is logged verbatim (S-08): only the error
/// class, the message's length and fingerprint, and the app's own frames.
#[derive(Debug, PartialEq, Eq)]
struct FrontendError {
    /// The error class the message starts with (`TypeError`).
    class: Option<String>,
    /// Byte length of the whole message.
    message_len: usize,
    /// [`fingerprint`] of the whole message, to group repeated reports.
    message_hash: String,
    /// `basename:line:col` when `source` is a location in the app's assets.
    source: Option<String>,
    /// Stack frames in the app's assets as `function@basename:line:col`;
    /// the function is left out when its name is not an identifier.
    frames: Vec<String>,
    /// Stack lines left out: not a frame, or a frame outside the app's
    /// assets (the first line of a V8 stack repeats the message).
    dropped_lines: usize,
    /// The stack was longer than [`FRONTEND_ERROR_MAX_BYTES`]; only the
    /// complete lines of its head were read.
    truncated: bool,
}
impl FrontendError {
    fn fit(message: &str, stack: Option<&str>, source: Option<&str>) -> Self {
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
            match stack_frame(line) {
                Some(frame) => frames.push(frame),
                None => dropped_lines += 1,
            }
        }
        Self {
            class: error_class(message).map(str::to_string),
            message_len: message.len(),
            message_hash: fingerprint(message),
            source: source.and_then(asset_location),
            frames,
            dropped_lines,
            truncated,
        }
    }
}

/// The class of `TypeError: ...` (also `Uncaught TypeError: ...`, as
/// `window.onerror` reports it): an identifier ending in `Error` or
/// `Exception` (`DOMException`), so that the start of a free-form message
/// (`hello: ...`) does not pass as a class.
fn error_class(message: &str) -> Option<&str> {
    let message = message.trim_start();
    let message = message.strip_prefix("Uncaught ").unwrap_or(message);
    let head = message.split_once(':').map_or(message, |(head, _)| head);
    (is_identifier(head) && (head.ends_with("Error") || head.ends_with("Exception")))
        .then_some(head)
}

/// `^[A-Za-z_$][A-Za-z0-9_$]{0,63}$`.
fn is_identifier(text: &str) -> bool {
    let word = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$');
    text.len() <= 64
        && text.bytes().next().is_some_and(|b| !b.is_ascii_digit())
        && text.as_bytes().iter().all(word)
}

/// One stack line as `function@basename:line:col` when it is a frame in the
/// app's assets: V8 (`at fn (url:1:2)`, `at url:1:2`) or SpiderMonkey and
/// JavaScriptCore (`fn@url:1:2`). V8 qualifies methods (`Object.render`),
/// so a function name may be identifiers joined by dots.
fn stack_frame(line: &str) -> Option<String> {
    let (name, location) = match line.strip_prefix("at ") {
        Some(rest) => rest
            .strip_suffix(')')
            .and_then(|inner| inner.split_once(" ("))
            .unwrap_or(("", rest)),
        None => line.split_once('@')?,
    };
    let location = asset_location(location)?;
    let name = name.trim();
    let name = name.strip_prefix("async ").unwrap_or(name);
    let name = name.strip_prefix("new ").unwrap_or(name);
    let named = name.len() <= 64 && name.split('.').all(is_identifier);
    Some(format!("{}@{location}", if named { name } else { "" }))
}

/// `basename:line:col` of `url:line:col` when `url` is one of the app's
/// bundled scripts, served from `tauri://localhost`, `http(s)://tauri.localhost`
/// or the dev server `http://localhost:<port>`, or given as a relative path.
/// The query string, fragment and directories are left out.
fn asset_location(location: &str) -> Option<String> {
    let (rest, col) = location.trim().rsplit_once(':')?;
    let (url, line) = rest.rsplit_once(':')?;
    if !is_number(line) || !is_number(col) {
        return None;
    }
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let path = [
        "tauri://localhost/",
        "http://tauri.localhost/",
        "https://tauri.localhost/",
    ]
    .into_iter()
    .find_map(|origin| path.strip_prefix(origin))
    .or_else(|| {
        let (port, path) = path.strip_prefix("http://localhost:")?.split_once('/')?;
        is_number(port).then_some(path)
    })
    .or_else(|| (!path.contains(':') && !path.starts_with("//")).then_some(path))?;
    let name = path.rsplit('/').next().unwrap_or_default();
    is_script_name(name).then(|| format!("{name}:{line}:{col}"))
}

/// A bundled script's file name: `index-4f2a.js`, `App.tsx`.
fn is_script_name(name: &str) -> bool {
    const EXTENSIONS: [&str; 8] = ["js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "html"];
    let charset = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.');
    name.len() <= 128
        && name.bytes().all(charset)
        && name
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && EXTENSIONS.contains(&ext))
}

fn is_number(text: &str) -> bool {
    (1..=10).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit())
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

    /// Only the head of a long stack is read, cut on a line (and so on a
    /// char boundary); the message is measured and fingerprinted whole.
    #[test]
    fn frontend_errors_share_one_budget_and_cut_on_char_boundaries() {
        let frame = "at render (http://tauri.localhost/assets/index-4f2a.js:12:345)\n";
        let mut stack = frame.repeat(FRONTEND_ERROR_MAX_BYTES / frame.len() + 10);
        stack.push_str(&"中".repeat(5000));
        let long = "中".repeat(5000); // 15 000 bytes
        let cut = FrontendError::fit(&long, Some(&stack), Some(&long));
        assert!(cut.truncated);
        assert_eq!(cut.frames.len(), FRONTEND_ERROR_MAX_BYTES / frame.len());
        assert!(cut
            .frames
            .iter()
            .all(|f| f == "render@index-4f2a.js:12:345"));
        assert_eq!(cut.dropped_lines, 0);
        assert_eq!(cut.message_len, 15_000);
        assert_eq!(cut.message_hash, fingerprint(&long));
        assert_eq!((cut.class, cut.source), (None, None));
        let logged: usize = cut.frames.iter().map(String::len).sum();
        assert!(logged <= FRONTEND_ERROR_MAX_BYTES, "{logged}");

        let cjk = FrontendError::fit("boom", Some(&long), None);
        assert!(cjk.truncated);
        assert_eq!((cjk.frames.len(), cjk.dropped_lines), (0, 0));
    }

    #[test]
    fn frontend_errors_keep_only_the_class_and_the_apps_frames() {
        let stack = "TypeError: Cannot read properties of undefined (reading 'body')\n\
            \x20   at RuleEditor (http://tauri.localhost/assets/index-4f2a.js:12:345)\n\
            \x20   at async Object.save (tauri://localhost/assets/index-4f2a.js:3:4)\n\
            \x20   at new Store (http://localhost:1420/src/store.ts?t=1729591293:7:8)\n\
            \x20   at http://tauri.localhost/assets/vendor.js:1:2\n\
            \x20   at Array.map (<anonymous>)\n\
            \x20   at Object.<anonymous> (./src/main.tsx:9:10)\n\
            \x20   at fn [as alias] (https://tauri.localhost/a/b/c.mjs:11:12)\n\
            \x20   at eval (eval at run (http://tauri.localhost/x.js:1:2), <anonymous>:1:1)\n\
            \x20   at load (https://example.com/evil.js:5:6)\n\
            \x20   at load (http://tauri.localhost/assets/index.css:5:6)\n\
            onClick@tauri://localhost/assets/index-4f2a.js:20:30\n\
            global code@tauri://localhost/assets/index-4f2a.js:21:31\n\
            @http://tauri.localhost/assets/index-4f2a.js:22:32#frag\n\
            \n";
        let report = FrontendError::fit(
            "Uncaught TypeError: Cannot read properties of undefined (reading 'body')",
            Some(stack),
            Some("http://tauri.localhost/assets/index-4f2a.js?v=3:40:5"),
        );
        assert_eq!(report.class.as_deref(), Some("TypeError"));
        assert_eq!(report.source.as_deref(), Some("index-4f2a.js:40:5"));
        assert_eq!(
            report.frames,
            [
                "RuleEditor@index-4f2a.js:12:345",
                "Object.save@index-4f2a.js:3:4",
                "Store@store.ts:7:8",
                "@vendor.js:1:2",
                "@main.tsx:9:10",
                "@c.mjs:11:12",
                "onClick@index-4f2a.js:20:30",
                "@index-4f2a.js:21:31",
            ]
        );
        // The message line, `<anonymous>`, eval, another host, a style sheet
        // and the line whose column carries a fragment.
        assert_eq!(report.dropped_lines, 6);
        assert!(!report.truncated);

        for (message, class) in [
            ("RangeError: Invalid array length", Some("RangeError")),
            (
                "DOMException: The operation was aborted.",
                Some("DOMException"),
            ),
            ("IpcError", Some("IpcError")),
            ("hello: world", None),
            ("Script error.", None),
            ("Type Error: x", None),
            ("", None),
        ] {
            let report = FrontendError::fit(message, None, None);
            assert_eq!(report.class.as_deref(), class, "{message}");
            assert_eq!(report.message_len, message.len());
            assert_eq!((report.frames.len(), report.dropped_lines), (0, 0));
        }
        for source in [
            "window.onerror",
            "http://tauri.localhost/assets/index.js",
            "https://example.com/a.js:1:2",
            "file:///C:/Users/alice/a.js:1:2",
            "http://localhost:1420/src/main.tsx:x:1",
        ] {
            assert_eq!(FrontendError::fit("e", None, Some(source)).source, None);
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
                     \x20   at {S} (http://tauri.localhost/assets/index-4f2a.js:1:2)\n\
                     \x20   at render (http://localhost:1420/src/App.tsx?t={S}:3:4)\n\
                     \x20   at load (http://tauri.localhost/{S}/chunk.js:5:6)\n\
                     \x20   at load (http://tauri.localhost/assets/a.js#{S}:7:8)\n\
                     {S}@tauri://localhost/assets/index-4f2a.js:9:10\n\
                     \x20   at {S}"
                )),
                Some(format!(
                    "http://tauri.localhost/assets/index-4f2a.js?q={S}:11:12"
                )),
            ),
            (format!("{S} could not be typed"), None, Some(S.to_string())),
            (format!("{IDENT}: {S}"), Some(format!("{IDENT}@{S}")), None),
            (format!("Uncaught Error: {IDENT}"), None, Some(IDENT.into())),
            (format!("{S}:{S}:{S}"), Some(format!("{S}:1:2")), None),
        ];

        let dir = std::env::temp_dir().join(format!("quicker-fe-test-{}", uuid::Uuid::new_v4()));
        let built = scoped(&log_dir(&dir), LevelFilter::TRACE);
        tracing::subscriber::with_default(built.subscriber, || {
            for (message, stack, source) in reports {
                log_frontend_error(message, stack, source);
            }
        });
        drop(built.guard);

        let log = read_logs(&log_dir(&dir));
        assert_eq!(log.matches("frontend.error").count(), 5, "{log}");
        for expected in [
            r#"class="TypeError""#,
            "index-4f2a.js:11:12",
            "@index-4f2a.js:1:2",
            "render@App.tsx:3:4",
            "load@chunk.js:5:6",
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
