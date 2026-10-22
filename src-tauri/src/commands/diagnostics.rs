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
        redact::{redact_user_paths, truncate_utf8, user_homes},
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
use tracing::field;

/// Save dialog filter for `export_diagnostics`.
const BUNDLE_FILTERS: &[Filter] = &[("诊断包", &["zip"])];
/// Byte budget of one `log_frontend_error` report (contract: 8 KiB), shared
/// by message, source and stack in that order.
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

/// Logs a frontend error as `frontend.error` (warn), cut to
/// [`FRONTEND_ERROR_MAX_BYTES`].
#[tauri::command(rename_all = "snake_case")]
pub fn log_frontend_error(message: String, stack: Option<String>, source: Option<String>) {
    let report = FrontendError::fit(&message, stack.as_deref(), source.as_deref(), &user_homes());
    // Debug formatting keeps multi-line stacks on one log line.
    tracing::warn!(
        event = "frontend.error",
        message = ?report.message,
        source = report.source.as_deref().map(field::debug),
        stack = report.stack.as_deref().map(field::debug),
        truncated = report.truncated,
        "frontend error"
    );
}

#[derive(Debug, PartialEq, Eq)]
struct FrontendError {
    message: String,
    source: Option<String>,
    stack: Option<String>,
    truncated: bool,
}
impl FrontendError {
    /// User names in paths replaced, then cut on char boundaries so that
    /// message, source and stack together fit the byte budget.
    fn fit(message: &str, stack: Option<&str>, source: Option<&str>, homes: &[String]) -> Self {
        let mut budget = FRONTEND_ERROR_MAX_BYTES;
        let mut truncated = false;
        let mut take = |raw: &str| {
            // Cut before redacting so a huge report costs no more than 8 KiB
            // of work, and again after, as a replacement can be longer.
            let head = truncate_utf8(raw, FRONTEND_ERROR_MAX_BYTES);
            let text = redact_user_paths(head, homes);
            let kept = truncate_utf8(&text, budget).to_string();
            truncated |= head.len() < raw.len() || kept.len() < text.len();
            budget -= kept.len();
            kept
        };
        let message = take(message);
        let source = source.map(&mut take);
        let stack = stack.map(&mut take);
        Self {
            message,
            source,
            stack,
            truncated,
        }
    }
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

    #[test]
    fn frontend_errors_share_one_budget_and_cut_on_char_boundaries() {
        let short = FrontendError::fit("boom", Some("at a.js:1"), Some("window.onerror"), &[]);
        assert_eq!(
            short,
            FrontendError {
                message: "boom".into(),
                source: Some("window.onerror".into()),
                stack: Some("at a.js:1".into()),
                truncated: false,
            }
        );

        let long = "中".repeat(5000); // 15 000 bytes
        let cut = FrontendError::fit("message", Some(&long), Some(&long), &[]);
        assert!(cut.truncated);
        let total = cut.message.len()
            + cut.source.as_ref().map_or(0, String::len)
            + cut.stack.as_ref().map_or(0, String::len);
        assert!(total <= FRONTEND_ERROR_MAX_BYTES, "{total}");
        assert!(total > FRONTEND_ERROR_MAX_BYTES - 3, "{total}");
        assert_eq!(cut.message, "message");
        assert_eq!(cut.stack.as_deref(), Some(""));

        let paths = FrontendError::fit(r"failed at C:\Users\alice\x", None, None, &[]);
        assert_eq!(paths.message, r"failed at C:\Users\%USERNAME%\x");
        assert_eq!(paths.source, None);
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
