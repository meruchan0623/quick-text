//! Log setup (§7.1 「可观测性」, B-09).
//!
//! # Output
//!
//! - `<data_dir>/logs/quicker-text.YYYY-MM-DD.log`, one file per UTC day,
//!   the newest [`MAX_LOG_FILES`] kept. Lines are written by a background
//!   thread (`tracing_appender::non_blocking`), so a log call never waits for
//!   the disk; under extreme load lines are dropped rather than stalling the
//!   caller (hook and executor threads log too).
//! - stderr as well in debug builds, or whenever the log directory is
//!   unusable.
//!
//! # Levels
//!
//! The default is `info`. Our crates (`quicker_text`, `quicker_platform`,
//! `quicker_domain`) follow the runtime level (the reload handle in
//! [`Built::filter`]); everything else (tauri, wry, ...) stays capped at
//! `info`, so raising the level never logs IPC payloads.
//!
//! # Event naming
//!
//! Every event carries the structured field `event = "<component>.<event>"`,
//! both parts lower snake_case:
//!
//! ```ignore
//! tracing::warn!(event = "store.quarantine", file = %name, "quarantined corrupt file");
//! ```
//!
//! The target stays the default module path; there are no custom targets.
//! The message is free text for people; tools filter on `event`. Events in
//! use:
//!
//! | event | level | meaning |
//! |---|---|---|
//! | `app.start` | info | process started; version and build profile |
//! | `app.exit` | info | event loop finished; the log is flushed next |
//! | `command.internal` | error | `CommandError::Internal`; `trace_id` and the original error |
//! | `command.io` | warn | `CommandError::Io`; operation, file name, OS error |
//! | `engine.unavailable` | warn | the platform refused to start or resume |
//! | `log.file_unavailable` | error | log files could not be opened; stderr only |
//! | `store.quarantine` | warn | a damaged file was renamed aside |
//! | `store.quarantine_failed` | error | the rename failed; the file stays in place |
//!
//! # Privacy
//!
//! Never log phrase bodies, rule match text, clipboard or selection content,
//! or the detection buffer. Log ids, counts, lengths, codes and file names
//! (not full paths) instead. Parser errors (serde_json) can quote their
//! input: log `classify()`, line and column rather than their `Display`.
use std::{
    io,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, PoisonError},
};
use tracing::{level_filters::LevelFilter, Subscriber};
use tracing_appender::{
    non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard},
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    filter::Targets, fmt, layer::SubscriberExt, registry::Registry, reload, util::SubscriberInitExt,
};

/// Subdirectory of the data directory that holds the log files.
pub const LOG_DIR: &str = "logs";
pub const FILE_PREFIX: &str = "quicker-text";
pub const FILE_SUFFIX: &str = "log";
pub const MAX_LOG_FILES: usize = 7;
pub const DEFAULT_LEVEL: LevelFilter = LevelFilter::INFO;
/// Targets that follow the runtime level; everything else is capped at
/// [`DEFAULT_LEVEL`].
const OWN_TARGETS: [&str; 3] = ["quicker_text", "quicker_platform", "quicker_domain"];

pub type FilterHandle = reload::Handle<Targets, Registry>;

static FILTER: OnceLock<FilterHandle> = OnceLock::new();
/// Keeps the writer thread alive; dropped by [`flush`] on exit.
static GUARD: Mutex<Option<WorkerGuard>> = Mutex::new(None);

pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(LOG_DIR)
}

/// The filter for `level`: our crates at `level`, the rest at most `info`.
pub fn targets(level: LevelFilter) -> Targets {
    OWN_TARGETS.iter().fold(
        Targets::new().with_default(level.min(DEFAULT_LEVEL)),
        |targets, target| targets.with_target(*target, level),
    )
}

/// A subscriber that is not installed yet, with its level handle and writer
/// guard. Tests install it with `tracing::subscriber::with_default`.
pub struct Built<S> {
    pub subscriber: S,
    pub filter: FilterHandle,
    /// Dropping it flushes buffered lines and stops the writer thread.
    pub guard: Option<WorkerGuard>,
    pub file_error: Option<io::Error>,
}

/// Builds the subscriber: rolling files in `log_dir` (if given), stderr if
/// `stderr` is set or the files cannot be opened.
pub fn build(
    log_dir: Option<&Path>,
    level: LevelFilter,
    stderr: bool,
) -> Built<impl Subscriber + Send + Sync + 'static> {
    let (filter, handle) = reload::Layer::new(targets(level));
    let (writer, guard, file_error) = match log_dir.map(file_writer).transpose() {
        Ok(Some((writer, guard))) => (Some(writer), Some(guard), None),
        Ok(None) => (None, None, None),
        Err(e) => (None, None, Some(e)),
    };
    let file = writer.map(|writer| {
        fmt::layer()
            .with_ansi(false)
            .with_thread_names(true)
            .with_writer(writer)
    });
    let stderr = (stderr || file_error.is_some())
        .then(|| fmt::layer().with_thread_names(true).with_writer(io::stderr));
    Built {
        subscriber: tracing_subscriber::registry()
            .with(filter)
            .with(file)
            .with(stderr),
        filter: handle,
        guard,
        file_error,
    }
}

fn file_writer(dir: &Path) -> io::Result<(NonBlocking, WorkerGuard)> {
    std::fs::create_dir_all(dir)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(FILE_PREFIX)
        .filename_suffix(FILE_SUFFIX)
        .max_log_files(MAX_LOG_FILES)
        .build(dir)
        .map_err(io::Error::other)?;
    Ok(NonBlockingBuilder::default()
        .thread_name("quicker-log")
        .finish(appender))
}

/// Installs the global subscriber at [`DEFAULT_LEVEL`]. Call once, before
/// the store loads and before any command can run. If the log directory is
/// unusable the app still starts, logging to stderr only.
pub fn init(data_dir: &Path) {
    let Built {
        subscriber,
        filter,
        guard,
        file_error,
    } = build(
        Some(&log_dir(data_dir)),
        DEFAULT_LEVEL,
        cfg!(debug_assertions),
    );
    if subscriber.try_init().is_err() {
        // Already installed; keep the first one.
        return;
    }
    let _ = FILTER.set(filter);
    *GUARD.lock().unwrap_or_else(PoisonError::into_inner) = guard;
    if let Some(e) = file_error {
        tracing::error!(
            event = "log.file_unavailable",
            error = %e,
            "log files unavailable; logging to stderr only"
        );
    }
}

/// Writes out buffered lines and stops the writer thread. Call on exit;
/// later events no longer reach the file.
pub fn flush() {
    drop(GUARD.lock().unwrap_or_else(PoisonError::into_inner).take());
}

#[cfg(test)]
pub mod test_support {
    use super::*;

    /// A file-backed subscriber for a test that installs it with
    /// `tracing::subscriber::with_default`.
    ///
    /// tracing-core caches each callsite's interest process-wide. While only
    /// one dispatcher exists, a callsite first hit on another test thread is
    /// judged by that thread's default (none) and cached as "never", so events
    /// from shared code (e.g. `CommandError::internal`) would be missing.
    /// Keeping a second dispatcher alive makes tracing-core consult every
    /// live dispatcher instead.
    pub fn scoped(log_dir: &Path, level: LevelFilter) -> Built<impl Subscriber + Send + Sync> {
        static KEEP_ALIVE: OnceLock<tracing::Dispatch> = OnceLock::new();
        KEEP_ALIVE
            .get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
        build(Some(log_dir), level, false)
    }

    /// Everything written to the log files in `log_dir`, oldest file first.
    pub fn read_logs(log_dir: &Path) -> String {
        let mut files: Vec<_> = std::fs::read_dir(log_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        files
            .iter()
            .map(|file| std::fs::read_to_string(file).unwrap())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{self, read_logs};
    use super::*;
    use crate::{error::CommandError, store::Store};
    use quicker_domain::Config;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("quicker-log-test-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn our_crates_follow_the_level_and_the_rest_stays_at_info() {
        use tracing::Level;
        let info = targets(DEFAULT_LEVEL);
        assert!(info.would_enable("quicker_text::store", &Level::INFO));
        assert!(!info.would_enable("quicker_text::store", &Level::DEBUG));
        assert!(!info.would_enable("tauri::ipc", &Level::DEBUG));
        let trace = targets(LevelFilter::TRACE);
        for target in OWN_TARGETS {
            assert!(trace.would_enable(target, &Level::TRACE));
        }
        assert!(trace.would_enable("tauri::ipc", &Level::INFO));
        assert!(!trace.would_enable("tauri::ipc", &Level::DEBUG));
        let warn = targets(LevelFilter::WARN);
        assert!(!warn.would_enable("quicker_platform", &Level::INFO));
        assert!(!warn.would_enable("wry", &Level::INFO));
    }

    #[test]
    fn files_are_named_by_date_and_the_level_reloads() {
        let dir = temp_dir();
        let built = test_support::scoped(&dir, DEFAULT_LEVEL);
        assert!(built.file_error.is_none());
        tracing::subscriber::with_default(built.subscriber, || {
            tracing::debug!(event = "test.hidden", "before");
            built.filter.reload(targets(LevelFilter::DEBUG)).unwrap();
            tracing::debug!(event = "test.shown", "after");
        });
        drop(built.guard);
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1);
        let date = names[0]
            .strip_prefix("quicker-text.")
            .and_then(|rest| rest.strip_suffix(".log"))
            .unwrap();
        assert_eq!(date.len(), 10, "YYYY-MM-DD, got {date}");
        let log = read_logs(&dir);
        assert!(!log.contains("test.hidden"));
        assert!(log.contains("event=\"test.shown\""));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Codex review: the trace id returned to the frontend must find the
    /// original error in the log file.
    #[test]
    fn a_failed_command_logs_its_trace_id_with_the_original_error() {
        let dir = temp_dir();
        let built = test_support::scoped(&log_dir(&dir), DEFAULT_LEVEL);
        let err = tracing::subscriber::with_default(built.subscriber, || {
            let mut store = Store::load(dir.clone()).unwrap();
            store.active.revision = u64::MAX;
            store.publish(Config::default()).unwrap_err()
        });
        drop(built.guard);
        let CommandError::Internal { trace_id } = err else {
            panic!("revision overflow must be an internal error, got {err:?}");
        };
        let log = read_logs(&log_dir(&dir));
        let line = log
            .lines()
            .find(|line| line.contains(&trace_id))
            .unwrap_or_else(|| panic!("trace id {trace_id} missing from log:\n{log}"));
        assert!(line.contains("Revision counter exhausted"), "{line}");
        assert!(line.contains("event=\"command.internal\""), "{line}");
        assert!(line.contains("ERROR"), "{line}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
