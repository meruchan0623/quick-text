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
//! `quicker_domain`) follow the runtime level ([`apply_level`], driven by
//! [`super::level`] and the command `set_log_level`); everything else (tauri, wry, ...) stays capped at
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
//! use, every name logged under `crates/*/src` and `src-tauri/src` (the
//! test `every_event_in_the_code_is_in_the_table_and_back` keeps the table
//! and the code in step, both ways). Names under `test.` are left out: only
//! tests, fixtures and examples log them, and those do not ship.
//!
//! | event | level | meaning |
//! |---|---|---|
//! | `app.start` | info | process started; version and build profile |
//! | `app.exit` | info | event loop finished; the log is flushed next |
//! | `app.panic` | error | a panic; thread, crate-relative location, payload class, length and fingerprint (never the payload), crash file name |
//! | `autostart.repair` | info | the autostart entry was rewritten to point at this executable |
//! | `autostart.repair_failed` | warn | the autostart entry could not be checked or repaired at startup; error |
//! | `clipboard.options_set` | info | the clipboard lease options were set at startup (`app.clipboard` prefs); restore policy and `restore_delay_ms` |
//! | `clipboard.busy` | warn | the clipboard could not be opened; `attempts` |
//! | `clipboard.exclusion_unregistered` | warn | a clipboard-history exclusion format could not be registered and is left out (the paste does not depend on it); format name |
//! | `clipboard.lease_refused` | warn | the clipboard backup was refused, so no lease was taken; reason code |
//! | `clipboard.owner_hung` | warn | the clipboard owner does not respond; the lease degrades instead of reading its formats |
//! | `clipboard.read_timeout` | warn | a read on the clipboard backup worker did not finish in time; `wait_ms` |
//! | `clipboard.skipped` | info | formats were left out of the clipboard backup; the skipped formats and the restore policy |
//! | `clipboard.slow` | warn | the clipboard was held open past its budget; `held_ms`, `budget_ms` |
//! | `clipboard.unreadable` | warn | an enumerated clipboard format gave no data or unreadable memory; the backup is refused; format id |
//! | `clipboard.worker_failed` | warn | the clipboard backup worker thread did not start; error |
//! | `command.internal` | error | `CommandError::Internal`; `trace_id` and the original error |
//! | `command.io` | warn | `CommandError::Io`; operation, file name, OS error |
//! | `diagnostics.export` | info | a bundle was written; redaction, entry count, `elapsed_ms` |
//! | `engine.unavailable` | warn | the platform refused to start or resume |
//! | `engine.manual_pause` | info | `set_paused` changed the manual pause; `paused` |
//! | `engine.shutdown` | info | the engine began shutting down (logged once) |
//! | `engine.start_failed` | error | `start_engine` failed at startup; the platform's error |
//! | `events.emit_failed` | warn | an event could not be sent to the frontend; event name and error |
//! | `events.observer_panic` | warn | an EventBus observer (the tray) panicked; which callback |
//! | `events.start_failed` | error | the EventBus thread did not start; no events will be sent |
//! | `executor.clipboard_probe_cached` | debug | the clipboard health could not be read without waiting; the status last read is reported |
//! | `executor.budget_skipped` | debug | a stall-check round was skipped: a lock it needs is held elsewhere; which lock (`held`) |
//! | `executor.clipboard_busy` | warn | a paste was refused: the clipboard is held by a stalled task |
//! | `executor.fenced` | warn | a stalled task was fenced: its thread may send nothing more and its key-ups are owed; the new `epoch`, `in_flight` |
//! | `executor.manual_send_refused` | warn | manual input was not sent; refusal reason |
//! | `executor.panic` | error | an executor task panicked and the worker keeps serving; stage, payload kind (`str` / `string` / `other`, never the payload), `epoch` |
//! | `executor.probe_cached` | debug | the executor health could not be read without waiting; the status last read is reported |
//! | `executor.rebuild_refused` | warn | an executor rebuild was refused: the send lock stayed held; `epoch` |
//! | `executor.rebuilt` | info | the executor was rebuilt; `epoch`, whether the live worker was `reused` |
//! | `executor.release_deferred` | warn | owed key-ups were not paid yet: the send lock stayed held; `epoch` |
//! | `executor.release_failed` | warn | a key-up was refused while releasing held keys; the key class only (modifier name, `mouse` or `other`), never the key code |
//! | `executor.release_owed` | warn | key-ups were refused: they stay owed and input stays refused; `released`, `refused`, `epoch` |
//! | `executor.release_paid` | info | owed key-ups were paid; `released`, `epoch` |
//! | `executor.retired` | info | a fenced worker stopped serving; its `epoch` |
//! | `executor.send_refused` | warn | nothing was sent: the send lock is held by a stuck send; `epoch` |
//! | `executor.stalled` | warn | a task ran past its stall budget and is retired; `stalled_ms`, `budget_ms` |
//! | `executor.start_failed` | error | the executor did not start; error |
//! | `executor.unwound` | error | a task unwound without settling; its held keys were released; `epoch`, `released` |
//! | `export.refused_data_dir` | warn | an export target inside the data directory was refused; file name and the check that caught it (`stage`) |
//! | `export.temp_cleanup_failed` | warn | the export temp file could not be removed; file name, error |
//! | `fault.config_corrupt` | warn | fault injection damaged active.json, or could not write it (error) |
//! | `fault.injected` | warn | an injected fault was armed or fired; fault `point`, and `param` when armed by `debug_inject_fault` |
//! | `frontend.error` | warn | `log_frontend_error`; error class (a built-in name or `other`), message length and fingerprint, source and stack frames as `<chunk hash>.js:line:col` only for Vite chunks embedded in the binary (the list `AssetResolver::iter` gives; empty in dev builds) at a line and column inside the chunk, printed from the parsed numbers, whether the source was dropped, count of dropped stack lines; never any caller text |
//! | `history.cleared` | info | `clear_history` emptied the execution history |
//! | `hook.ping_timeout` | warn | the hook thread did not answer the self-test heartbeat in time |
//! | `hook.install_timeout` | warn | the hook thread did not answer an install request in time |
//! | `hook.panic` | error | hook callbacks panicked since the last report; `count` |
//! | `hook.thread_failed` | error | the hook thread could not take an install request; no hook is installed |
//! | `input.check` | info / warn | an input source check; `source` (`keyboard_hook` / `raw_input`) and `result` (`lost`, `window_missing`, `unregistered` at warn; `registered` at info) |
//! | `input.consumer_panic` | error | the input consumer panicked; its state is reset and it keeps running |
//! | `input.queue_full` | warn | input records were dropped because a queue was full; `source` (`hook` / `rawinput`), `count` |
//! | `input.reattach` | warn | an input component was reattached; component, the reason code it was down for, `ok` |
//! | `input.refused` | info / warn | a candidate was not expanded: a reattach or session signal crossed it (`signal`, info), or the hook is missing (`hook_missing`, warn) |
//! | `keystate.heal` | warn | a modifier the key mirror held down was found up and cleared; modifier name |
//! | `keystate.resync` | info | the key mirror was resynced from the physical key state; `pass` |
//! | `keystate.resync_deferred` | info | a resync was put off: an injection overlapped the sample; `pass` |
//! | `log.file_unavailable` | error | log files could not be opened; stderr only |
//! | `log.level_set` | info | `set_log_level`; level and `until_ms` |
//! | `log.level_restored` | info | a saved temporary level was re-applied at startup |
//! | `log.level_expired` | info | a temporary level ended; back to info |
//! | `log.level_restore_failed` | warn | prefs unreadable at startup; default level |
//! | `log.reload_failed` | error | the filter could not be swapped |
//! | `log.expiry_thread_failed` | error | the expiry thread did not start |
//! | `prefs.auto_listen` | info | `set_auto_listen`; `enabled` and whether it `changed` |
//! | `rawinput.rebuild_skipped` | info | a RawInput detector rebuild was skipped: the engine is shutting down |
//! | `rawinput.closed` | info | the RawInput window closed; whether the close was `expected` |
//! | `rawinput.dropped_busy` | debug | a RawInput-only match was dropped because the executor was busy; it goes to the execution history |
//! | `rawinput.panic` | error | RawInput callbacks panicked since the last report; `count` |
//! | `rawinput.rebuild_failed` | warn | the RawInput detector could not be rebuilt; error |
//! | `rawinput.rebuild_stuck` | warn | the old RawInput window did not close in time; the rebuild gave up |
//! | `recording.event_stale` | debug | a recorded input from an earlier recording was dropped; `session`, `current` |
//! | `recording.stop_stale` | debug | a stop request for an earlier recording was dropped; `session`, `current` |
//! | `runtime.failed_dropped_on_shutdown` | info | a Failed health was not published: the engine is shutting down (logged once); `source` |
//! | `runtime.event_dropped` | warn | the event thread could not start; the event was dropped |
//! | `runtime.events_dropped` | warn | the execution queue was full; execution events were dropped; `dropped` |
//! | `runtime.failed_latched` | error | the engine is latched Failed until repair; `reason` |
//! | `runtime.health_rejected` | debug | a health other than Failed was not published while the Failed latch holds; its `state` |
//! | `runtime.listener_panic` | warn | an event listener panicked; the other listeners still run |
//! | `self_test.check` | info | one self-test check; check name, `passed`, reason code, `elapsed_ms` |
//! | `self_test.finished` | info | a self-test run ended; overall `passed` and the names of the failed checks |
//! | `self_test.startup` | info | the startup self-test ended; health, `auto_listen`, outcome label |
//! | `self_test.startup_thread_failed` | error | the startup self-test thread did not start |
//! | `session.end` | info | the session is ending; `source` and the number of `callbacks` run |
//! | `session.end_callback_panic` | error | a session-end callback panicked; its `index` |
//! | `session.registered` | info | session notifications were registered; `attempts` |
//! | `session.signal` | info | a session signal arrived (lock, unlock, ...); signal name |
//! | `session.start_failed` | warn | session monitoring did not start; `stage` (`thread`, `window`, `desktop_switch`, or `wts` with `attempts`) |
//! | `store.quarantine` | warn | a damaged file was renamed aside |
//! | `store.quarantine_failed` | error | the rename failed; the file stays in place |
//! | `store.prefs_invalid` | warn | a prefs field had the wrong type or was out of range and its default is used; file name, field path, problem, the JSON type found (or an out-of-range number with `min` / `max`), never a string value |
//! | `store.prefs_unavailable` | warn | prefs.json could not be read at startup; clipboard restore uses the defaults; file name and error |
//! | `store.corrupt` | warn | a configuration file does not parse; file name, error code, error |
//! | `store.crash_loop` | warn | consecutive crashes at startup; `crash_count` |
//! | `store.create_dir_failed` | warn | the data directory could not be created; error |
//! | `store.diagnostic` | warn | a diagnostic on the stored configuration; code and the path inside the configuration (never the message, which may echo values) |
//! | `store.last_good` | info | a last-known-good `revision` was recorded |
//! | `store.locked_recovered` | info | the data directory is readable again after SafeMode(Locked); load outcome |
//! | `store.locked_retry` | error | the SafeMode(Locked) retry thread did not start; error |
//! | `store.meta_readonly` | warn | store-meta.json is left untouched; the run marker and crash count are kept in memory |
//! | `store.meta_reset` | warn | store-meta.json did not parse; it was quarantined and rebuilt |
//! | `store.meta_write_failed` | warn | store-meta.json was not written; error |
//! | `store.metadata_snapshot_failed` | warn | the active metadata was not snapshotted; error |
//! | `store.newer_schema` | warn | a file written by a newer release; opened read-only; file name, schema `version` |
//! | `store.previous_unreadable` | warn | the previous configuration is unavailable for rollback; file name, error code |
//! | `store.read_failed` | warn | a file stayed unreadable after retries; file name, error |
//! | `store.recovered` | warn | active.json was unusable and the configuration was recovered from another file; file name, source, revision |
//! | `store.rules_skipped` | warn | unreadable rules stay off at runtime; `count` |
//! | `store.run_marker_clear_failed` | warn | the run marker could not be cleared; error |
//! | `store.run_marker_cleared` | info | the run marker was cleared |
//! | `store.safe_mode` | warn | safe mode began and the engine is paused; reason |
//! | `store.safe_mode_exit` | info | safe mode ended |
//! | `store.safe_mode_import` | info | an import was published in safe mode, which ends it |
//! | `store.writable_hook_deferred` | info | a writable hook was deferred until safe mode ends; hook name |
//! | `supervisor.installed` | info | the production supervisor and its watchdog are running |
//! | `supervisor.watchdog_spawn_failed` | error | the watchdog thread did not start; the supervisor runs without it |
//! | `supervisor.driver_spawn_failed` | error | the supervisor driver thread did not start; Health is Failed until the watchdog starts it |
//! | `supervisor.unsupervised` | error | no supervisor driver ran for `checks` watchdog checks; Health is published as Failed |
//! | `supervisor.engine_unavailable` | error | the engine did not start; its executor refuses every action (`engine_down`) and Health ends in Failed |
//! | `supervisor.rebuild_skipped` | info | an engine rebuild was skipped: the engine is shutting down |
//! | `supervisor.rebuild_discarded` | info | a rebuild returned after shutdown began, so its result is not counted; `failed` |
//! | `supervisor.reattach_skipped` | info | a component reattach was skipped: the engine is shutting down; component |
//! | `supervisor.reattach_discarded` | info | a reattach returned after shutdown began, so its result is not counted; component, `failed` |
//! | `supervisor.watchdog_stopped` | info | the watchdog stopped: the engine is shutting down |
//! | `supervisor.attempt_discarded` | info / warn | a recovery attempt's result was not counted: the engine is shutting down (info); the attempt was superseded or hung, the circuit opened while it ran, or it returned after `ATTEMPT_TIMEOUT` (warn); `attempt` and the flags that apply |
//! | `supervisor.attempt_failed` | warn | a recovery attempt failed; error, `failures` so far, and `retry_in_ms` when another attempt is scheduled |
//! | `supervisor.attempt_hung` | error | a recovery attempt is still running after `ATTEMPT_TIMEOUT`; `attempt`, error, `failures` |
//! | `supervisor.attempt_superseded` | info | a repair waits for the running attempt to return; `attempt` |
//! | `supervisor.circuit_open` | error | the engine failed and waits for repair; `cause` |
//! | `supervisor.driver_hung` | error | the supervisor driver and its replacement are both stuck; the engine is failed |
//! | `supervisor.panic` | error | a supervisor tick panicked; the next tick continues |
//! | `supervisor.recovered` | info | recovery ended; the settled `state` |
//! | `supervisor.recovering` | warn | a required component is down and recovery begins; the components down and the incident count |
//! | `supervisor.repair` | info | a repair was requested |
//! | `supervisor.respawn` | warn | the watchdog replaces the supervisor driver; whether the old one `finished`, `stale` |
//! | `supervisor.respawn_failed` | error | the replacement supervisor driver did not start; error |
//! | `supervisor.tick_skipped` | warn | a supervisor tick was skipped: the state machine is busy (once per busy stretch) |
//! | `tray.resume_refused` | info | the tray's resume was refused; the command error (the user is notified) |
//! | `tray.notify_failed` | warn | a desktop notification was not shown; error |
//! | `tray.notify_start_failed` | warn | the run-notification thread did not start; run notifications are off |
//! | `ui.metric` | info / debug | `report_metric`; whitelisted name and value (first paint at info, event lag at debug) |
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

/// Changes the level of our crates at runtime. Does nothing before [`init`].
pub fn apply_level(level: LevelFilter) {
    if let Some(handle) = FILTER.get() {
        if let Err(e) = handle.reload(targets(level)) {
            tracing::error!(event = "log.reload_failed", error = %e, "log level not changed");
        }
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
            let mut store = Store::load(dir.clone());
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

/// The event table in the module docs against the code: every
/// `event = "…"` under `crates/*/src` and `src-tauri/src` needs a row, and
/// every row must be logged somewhere. Names under `test.` are left out.
#[cfg(test)]
mod event_table {
    use std::{
        collections::BTreeMap,
        ops::Range,
        path::{Path, PathBuf},
    };

    /// Event fields whose name is chosen at runtime, which the scan cannot
    /// read: (file relative to the workspace root, the field as written with
    /// its whitespace collapsed, every name it can take). The names must be
    /// in the table like any other. A runtime field missing here fails the
    /// test, and so does an entry whose field is gone. There are none today.
    const RUNTIME_EVENT_NAMES: &[(&str, &str, &[&str])] = &[];

    /// Logging macros by name, with or without a path: `tracing::warn!`,
    /// `warn!`, `event!`, ...
    const LOG_MACROS: [&[u8]; 6] = [b"trace", b"debug", b"info", b"warn", b"error", b"event"];

    #[derive(Debug, PartialEq)]
    enum EventField {
        Literal(String),
        /// The field as written, whitespace collapsed.
        Runtime(String),
        Missing,
    }

    /// (line, name) of each literal, and (line, event field) of each call.
    struct Scan {
        literals: Vec<(usize, String)>,
        calls: Vec<(usize, EventField)>,
    }

    fn is_ident(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || byte == b'_'
    }

    fn find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
        haystack
            .get(from..)?
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|at| from + at)
    }

    fn skip_ws(src: &[u8], mut at: usize) -> usize {
        while src.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        at
    }

    /// Spaces over `range`, keeping line breaks so offsets and lines hold.
    fn blank(out: &mut [u8], range: Range<usize>) {
        for byte in &mut out[range] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }

    /// The `#` count of a raw string starting at `at` (`r"`, `r#"`, `br"`),
    /// or `None`: not a raw string (a raw identifier `r#name`, or an `r`
    /// that ends an identifier).
    fn raw_hashes(src: &[u8], at: usize) -> Option<usize> {
        let starts = |i: Option<usize>| i.is_none_or(|i| !is_ident(src[i]));
        let before = at.checked_sub(1);
        let prefixed =
            before.is_some_and(|i| matches!(src[i], b'b' | b'c')) && starts(at.checked_sub(2));
        if src[at] != b'r' || !(starts(before) || prefixed) {
            return None;
        }
        let hashes = src[at + 1..]
            .iter()
            .take_while(|&&byte| byte == b'#')
            .count();
        (src.get(at + 1 + hashes) == Some(&b'"')).then_some(hashes)
    }

    fn char_width(lead: u8) -> usize {
        match lead {
            0xF0.. => 4,
            0xE0.. => 3,
            0xC0.. => 2,
            _ => 1,
        }
    }

    /// The source with comments blanked (`code`), and with comments and the
    /// contents of string and char literals blanked (`bare`). Both keep every
    /// offset, so a match found in `bare` reads its literal from `code`.
    fn lex(src: &str) -> (Vec<u8>, Vec<u8>) {
        let src = src.as_bytes();
        let (mut code, mut bare) = (src.to_vec(), src.to_vec());
        let at = |i: usize| src.get(i).copied().unwrap_or(0);
        let mut i = 0;
        while i < src.len() {
            if src[i] == b'/' && at(i + 1) == b'/' {
                let end = find(src, i, b"\n").unwrap_or(src.len());
                blank(&mut code, i..end);
                blank(&mut bare, i..end);
                i = end;
            } else if src[i] == b'/' && at(i + 1) == b'*' {
                let (mut depth, mut end) = (1, i + 2);
                while end < src.len() && depth > 0 {
                    match (src[end], at(end + 1)) {
                        (b'/', b'*') => (depth, end) = (depth + 1, end + 2),
                        (b'*', b'/') => (depth, end) = (depth - 1, end + 2),
                        _ => end += 1,
                    }
                }
                let end = end.min(src.len());
                blank(&mut code, i..end);
                blank(&mut bare, i..end);
                i = end;
            } else if let Some(hashes) = raw_hashes(src, i) {
                let open = i + 1 + hashes;
                let close: Vec<u8> = std::iter::once(b'"')
                    .chain(std::iter::repeat_n(b'#', hashes))
                    .collect();
                let end = find(src, open + 1, &close).unwrap_or(src.len());
                blank(&mut bare, open + 1..end);
                i = end + close.len();
            } else if src[i] == b'"' {
                let mut end = i + 1;
                while end < src.len() && src[end] != b'"' {
                    end += if src[end] == b'\\' { 2 } else { 1 };
                }
                let end = end.min(src.len());
                blank(&mut bare, i + 1..end);
                i = end + 1;
            } else if src[i] == b'\'' {
                // A char literal, or a lifetime or label (left alone).
                let close = if at(i + 1) == b'\\' {
                    find(src, i + 3, b"'")
                } else {
                    let close = i + 1 + char_width(at(i + 1));
                    (at(close) == b'\'').then_some(close)
                };
                match close {
                    Some(close) => {
                        blank(&mut bare, i + 1..close);
                        i = close + 1;
                    }
                    None => i += 1,
                }
            } else {
                i += 1;
            }
        }
        (code, bare)
    }

    fn ends_with_word(text: &[u8], word: &[u8]) -> bool {
        text.ends_with(word)
            && (text.len() == word.len() || !is_ident(text[text.len() - word.len() - 1]))
    }

    /// Every `event = "…"` in code, spaces or none: the field, not a
    /// `let event = …` binding, a `.event` field or text inside a literal.
    fn literal_events(code: &[u8], bare: &[u8]) -> Vec<(usize, String)> {
        let mut found = Vec::new();
        let mut from = 0;
        while let Some(at) = find(bare, from, b"event") {
            from = at + 1;
            if at
                .checked_sub(1)
                .is_some_and(|i| is_ident(bare[i]) || bare[i] == b'.')
                || bare.get(at + 5).is_some_and(|&byte| is_ident(byte))
            {
                continue;
            }
            let eq = skip_ws(bare, at + 5);
            if bare.get(eq) != Some(&b'=') || bare.get(eq + 1) == Some(&b'=') {
                continue;
            }
            let open = skip_ws(bare, eq + 1);
            let head = bare[..at].trim_ascii_end();
            if bare.get(open) != Some(&b'"')
                || ends_with_word(head, b"let")
                || ends_with_word(head, b"mut")
            {
                continue;
            }
            let end = find(code, open + 1, b"\"").unwrap_or(code.len());
            found.push((
                at,
                String::from_utf8_lossy(&code[open + 1..end]).into_owned(),
            ));
        }
        found
    }

    /// The arguments of the macro call whose opening bracket is at `open`,
    /// split at its top-level commas.
    fn split_args(bare: &[u8], open: usize) -> Vec<Range<usize>> {
        let (mut args, mut depth, mut start) = (Vec::new(), 0usize, open + 1);
        for (i, &byte) in bare.iter().enumerate().skip(open) {
            match byte {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        args.push(start..i);
                        return args;
                    }
                }
                b',' if depth == 1 => {
                    args.push(start..i);
                    start = i + 1;
                }
                _ => {}
            }
        }
        args.push(start..bare.len());
        args
    }

    /// The event field in one macro argument, or `None`: another argument.
    fn event_field(arg: &[u8]) -> Option<EventField> {
        let arg = String::from_utf8_lossy(arg);
        let arg = arg.trim();
        if matches!(arg, "event" | "%event" | "?event") {
            return Some(EventField::Runtime(arg.to_owned()));
        }
        let value = arg.strip_prefix("event")?.trim_start().strip_prefix('=')?;
        if value.starts_with('=') {
            return None;
        }
        let name = value
            .trim()
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'));
        Some(match name {
            Some(name) if !name.contains(['"', '\\']) => EventField::Literal(name.to_owned()),
            _ => EventField::Runtime(arg.split_whitespace().collect::<Vec<_>>().join(" ")),
        })
    }

    /// Every logging macro call and its event field.
    fn macro_events(code: &[u8], bare: &[u8]) -> Vec<(usize, EventField)> {
        let mut calls = Vec::new();
        for name in LOG_MACROS {
            let mut from = 0;
            while let Some(at) = find(bare, from, name) {
                from = at + 1;
                let bang = at + name.len();
                if at.checked_sub(1).is_some_and(|i| is_ident(bare[i]))
                    || bare.get(bang) != Some(&b'!')
                {
                    continue;
                }
                let open = skip_ws(bare, bang + 1);
                if !matches!(bare.get(open), Some(b'(' | b'[' | b'{')) {
                    continue;
                }
                let field = split_args(bare, open)
                    .into_iter()
                    .find_map(|range| event_field(&code[range]))
                    .unwrap_or(EventField::Missing);
                calls.push((at, field));
            }
        }
        calls.sort_by_key(|(at, _)| *at);
        calls
    }

    fn scan(src: &str) -> Scan {
        let (code, bare) = lex(src);
        let line = |at: usize| {
            src.as_bytes()[..at]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count()
                + 1
        };
        Scan {
            literals: literal_events(&code, &bare)
                .into_iter()
                .map(|(at, name)| (line(at), name))
                .collect(),
            calls: macro_events(&code, &bare)
                .into_iter()
                .map(|(at, field)| (line(at), field))
                .collect(),
        }
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// The names in the event table, one per row, in table order.
    fn table_names() -> Vec<&'static str> {
        include_str!("logging.rs")
            .lines()
            .filter_map(|line| line.strip_prefix("//! | `"))
            .filter_map(|row| row.split('`').next())
            .collect()
    }

    /// `<component>.<event>`, both lower snake case.
    fn well_formed(name: &str) -> bool {
        let part = |part: &str| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        };
        name.split_once('.')
            .is_some_and(|(component, event)| part(component) && part(event))
    }

    #[test]
    fn every_event_in_the_code_is_in_the_table_and_back() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut files = Vec::new();
        rust_files(&root.join("src-tauri").join("src"), &mut files);
        let mut crates: Vec<PathBuf> = std::fs::read_dir(root.join("crates"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join("src"))
            .filter(|src| src.is_dir())
            .collect();
        crates.sort();
        for src in &crates {
            rust_files(src, &mut files);
        }

        let mut problems = Vec::new();
        // name -> where it is first logged
        let mut in_code: BTreeMap<String, String> = BTreeMap::new();
        let mut runtime_seen = vec![false; RUNTIME_EVENT_NAMES.len()];
        for file in &files {
            let rel = file
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let scan = scan(&std::fs::read_to_string(file).unwrap());
            for (line, name) in scan.literals {
                if name.starts_with("test.") {
                    continue;
                }
                if !well_formed(&name) {
                    problems.push(format!(
                        "malformed event name `{name}` at {rel}:{line}: want `<component>.<event>`, lower snake case"
                    ));
                }
                in_code
                    .entry(name)
                    .or_insert_with(|| format!("{rel}:{line}"));
            }
            for (line, field) in scan.calls {
                match field {
                    EventField::Literal(_) => {}
                    EventField::Runtime(written) => {
                        match RUNTIME_EVENT_NAMES
                            .iter()
                            .position(|(file, field, _)| *file == rel && *field == written)
                        {
                            Some(entry) => runtime_seen[entry] = true,
                            None => problems.push(format!(
                                "event name chosen at runtime at {rel}:{line} (`{written}`): list it in RUNTIME_EVENT_NAMES with every name it can take"
                            )),
                        }
                    }
                    EventField::Missing => {
                        problems.push(format!("log call without an `event` field at {rel}:{line}"))
                    }
                }
            }
        }
        for ((file, field, names), seen) in RUNTIME_EVENT_NAMES.iter().zip(&runtime_seen) {
            if !seen {
                problems.push(format!(
                    "stale RUNTIME_EVENT_NAMES entry: no `{field}` in {file}"
                ));
            }
            for name in *names {
                in_code
                    .entry((*name).to_owned())
                    .or_insert_with(|| format!("{file} (chosen at runtime)"));
            }
        }

        let mut rows: BTreeMap<&str, usize> = BTreeMap::new();
        for name in table_names() {
            *rows.entry(name).or_default() += 1;
        }
        for (name, count) in &rows {
            if *count > 1 {
                problems.push(format!("`{name}` has {count} rows in the event table"));
            }
        }
        for (name, site) in &in_code {
            if !rows.contains_key(name.as_str()) {
                problems.push(format!(
                    "missing from the event table in logging.rs: `{name}` (logged at {site})"
                ));
            }
        }
        for name in rows.keys() {
            if !in_code.contains_key(*name) {
                problems.push(format!(
                    "in the event table in logging.rs but logged nowhere: `{name}`"
                ));
            }
        }

        assert!(
            files.len() > 50 && in_code.len() > 100,
            "the scan read {} files and found {} names: wrong root?",
            files.len(),
            in_code.len()
        );
        assert!(
            problems.is_empty(),
            "{} problem(s):\n{}",
            problems.len(),
            problems.join("\n")
        );
    }

    #[test]
    fn the_event_scan_reads_literals_and_flags_names_chosen_at_runtime() {
        // `@` stands for `event`, so the scan of this file does not read the sample.
        let src = r##"
// tracing::warn!(@ = "in.line_comment");
/* outer /* nested tracing::warn!(@ = "in.block_comment") */ still a comment */
fn sample<'a>(name: &'a str) {
    let url = "https://example.com/*/@ = \"in.string\"";
    tracing::info!(@ = "after.url", url);
    let quote = '"';
    tracing::warn!(@ = "after.quote", ?quote);
    tracing::debug!(@ = "after.lifetime", name);
    let raw = r#"a "quoted" // not a comment"#;
    tracing::event!(Level::INFO, @ = "after.raw", "a, b {raw}");
    info!(
        @ =
            "multi.line",
        "split"
    );
    let @ = "a.binding";
    warn!(@ = %name, "chosen at runtime");
    warn!(@, "shorthand");
    error!(?@, "debug shorthand");
    info!("no field");
}
"##
        .replace('@', "event");
        let scan = scan(&src);
        let literals: Vec<(usize, &str)> = scan
            .literals
            .iter()
            .map(|(line, name)| (*line, name.as_str()))
            .collect();
        assert_eq!(
            literals,
            [
                (6, "after.url"),
                (8, "after.quote"),
                (9, "after.lifetime"),
                (11, "after.raw"),
                (13, "multi.line"),
            ]
        );
        let literal = |name: &str| EventField::Literal(name.to_owned());
        let runtime = |written: &str| EventField::Runtime(written.to_owned());
        assert_eq!(
            scan.calls,
            [
                (6, literal("after.url")),
                (8, literal("after.quote")),
                (9, literal("after.lifetime")),
                (11, literal("after.raw")),
                (12, literal("multi.line")),
                (18, runtime("event = %name")),
                (19, runtime("event")),
                (20, runtime("?event")),
                (21, EventField::Missing),
            ]
        );
    }
}
