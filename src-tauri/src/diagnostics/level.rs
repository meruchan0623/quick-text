//! Temporary log level (§7.1 「日志级别」, command `set_log_level`).
//!
//! A level other than `info` always has an expiry. The expiry is stored in
//! `prefs.json` (`app.log_level`), re-applied at startup while still in the
//! future and cleared once it has passed. A background thread sleeps until
//! the expiry (no polling) and then reverts to `info`.
//!
//! Prefs follow the store rule (Codex review): they are only written after
//! a successful `load_prefs`, so a file that could not be read or moved
//! aside is never replaced. Every write goes through `update_prefs`, so it
//! cannot lose a concurrent `set_auto_listen` (lock order: the level state,
//! then the prefs write lock).
use crate::{
    error::CommandError,
    store::prefs::{update_prefs, LogLevelOverride},
};
use parking_lot::{Condvar, Mutex};
use quicker_domain::{Diagnostic, SourceSpan};
use quicker_platform::traits::Clock;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc, thread::JoinHandle, time::Duration};
use tracing::level_filters::LevelFilter;

/// What the frontend sends when the user picks 「调试」 (§6 settings table:
/// debug reverts to info after 24 hours). `ttl_hours: null` means "back to
/// the default level" (docs/ipc-contract-0.4.md §5).
#[allow(dead_code)] // Documents the frontend's default; the backend has no implicit TTL.
pub const DEFAULT_TTL_HOURS: f64 = 24.0;

/// Upper bound for one wait of the expiry thread. Time spent in sleep or
/// hibernation may not count towards an OS wait timeout, so the thread
/// re-reads the wall clock at least this often while an override is active.
const MAX_WAIT: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn filter(self) -> LevelFilter {
        match self {
            Self::Error => LevelFilter::ERROR,
            Self::Warn => LevelFilter::WARN,
            Self::Info => LevelFilter::INFO,
            Self::Debug => LevelFilter::DEBUG,
            Self::Trace => LevelFilter::TRACE,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Error,
            Self::Warn,
            Self::Info,
            Self::Debug,
            Self::Trace,
        ]
        .into_iter()
        .find(|level| level.as_str() == text)
    }
}

/// The effective level; also the return value of `set_log_level`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LevelState {
    pub level: LogLevel,
    /// Unix milliseconds; `None` for the default level.
    pub until_ms: Option<i64>,
}

impl Default for LevelState {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            until_ms: None,
        }
    }
}

/// The state `set_log_level(level, ttl_hours)` asks for at `now_ms`.
/// `info` or a null TTL is the default level; any other level expires after
/// `ttl_hours`, which must be a positive finite number.
pub fn resolve(
    level: LogLevel,
    ttl_hours: Option<f64>,
    now_ms: i64,
) -> Result<LevelState, CommandError> {
    let hours = match (level, ttl_hours) {
        (LogLevel::Info, _) | (_, None) => return Ok(LevelState::default()),
        (_, Some(hours)) if hours.is_finite() && hours > 0.0 => hours,
        _ => {
            return Err(CommandError::validation(vec![Diagnostic::error(
                "log_ttl_invalid",
                "ttl_hours must be a positive number",
                SourceSpan::default(),
            )
            .at("ttl_hours")]))
        }
    };
    // Float to int casts saturate; at least 1 ms so the override is real.
    let ttl_ms = ((hours * 3_600_000.0) as i64).max(1);
    Ok(LevelState {
        level,
        until_ms: Some(now_ms.saturating_add(ttl_ms)),
    })
}

/// What to do at startup with the override saved in prefs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restore {
    /// Nothing usable saved (none, `info`, or a level this version does not
    /// know, which is left in prefs untouched): default level.
    Default,
    /// Still in effect: apply it.
    Keep(LevelState),
    /// Expired: default level, and clear `app.log_level`.
    Expired,
}

pub fn restore_decision(saved: Option<&LogLevelOverride>, now_ms: i64) -> Restore {
    let Some(saved) = saved else {
        return Restore::Default;
    };
    match LogLevel::parse(&saved.level) {
        None => Restore::Default,
        Some(_) if saved.until_ms <= now_ms => Restore::Expired,
        Some(LogLevel::Info) => Restore::Default,
        Some(level) => Restore::Keep(LevelState {
            level,
            until_ms: Some(saved.until_ms),
        }),
    }
}

type Apply = Arc<dyn Fn(LevelFilter) + Send + Sync>;

struct Shared {
    dir: PathBuf,
    clock: Arc<dyn Clock>,
    apply: Apply,
    state: Mutex<Inner>,
    wake: Condvar,
}

struct Inner {
    current: LevelState,
    shutdown: bool,
}

/// Owns the effective level and the expiry thread. Managed Tauri state;
/// dropping it stops the thread.
pub struct LogLevelControl {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// The override restored from prefs at start, if any.
    restored: Option<LevelState>,
}

impl LogLevelControl {
    /// Restores the saved override for the data directory `dir` and starts
    /// the expiry thread. `apply` changes the live filter.
    pub fn start(dir: PathBuf, clock: Arc<dyn Clock>, apply: Apply) -> Self {
        let shared = Arc::new(Shared {
            dir,
            clock,
            apply,
            state: Mutex::new(Inner {
                current: LevelState::default(),
                shutdown: false,
            }),
            wake: Condvar::new(),
        });
        let restored = shared.restore();
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("quicker-log-level".into())
            .spawn(move || worker.run())
            .map_err(|e| {
                tracing::error!(
                    event = "log.expiry_thread_failed",
                    error = %e,
                    "log level expiry thread did not start"
                )
            })
            .ok();
        Self {
            shared,
            thread,
            restored,
        }
    }

    pub fn current(&self) -> LevelState {
        self.shared.state.lock().current
    }

    /// The temporary level restored from prefs at start (still in effect
    /// then), for the `log_level_elevated` startup problem.
    pub fn restored(&self) -> Option<LevelState> {
        self.restored
    }

    /// `set_log_level`: persist first, then apply. Nothing changes if prefs
    /// cannot be loaded or written.
    pub fn set(&self, level: LogLevel, ttl_hours: Option<f64>) -> Result<LevelState, CommandError> {
        self.shared.set(level, ttl_hours)
    }
}

impl Drop for LogLevelControl {
    fn drop(&mut self) {
        self.shared.state.lock().shutdown = true;
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Shared {
    /// Applies the override saved in prefs while it is still in effect and
    /// returns it (the caller reports `log_level_elevated`); clears an
    /// expired one.
    fn restore(&self) -> Option<LevelState> {
        let now = self.clock.wall_ms();
        let mut decision = Restore::Default;
        let result = update_prefs(&self.dir, |prefs| {
            decision = restore_decision(prefs.app.log_level.as_ref(), now);
            let expired = decision == Restore::Expired;
            if expired {
                prefs.app.log_level = None;
            }
            expired
        });
        match (decision, result) {
            (Restore::Keep(state), _) => {
                (self.apply)(state.level.filter());
                self.state.lock().current = state;
                tracing::info!(
                    event = "log.level_restored",
                    level = state.level.as_str(),
                    until_ms = state.until_ms,
                    "temporary log level restored"
                );
                Some(state)
            }
            (Restore::Expired, saved) => {
                tracing::info!(
                    event = "log.level_expired",
                    persisted = saved.is_ok(),
                    "temporary log level expired while the app was not running"
                );
                None
            }
            (Restore::Default, Err(e)) => {
                tracing::warn!(
                    event = "log.level_restore_failed",
                    error = %e,
                    "prefs unavailable; using the default log level"
                );
                None
            }
            (Restore::Default, Ok(_)) => None,
        }
    }

    fn set(&self, level: LogLevel, ttl_hours: Option<f64>) -> Result<LevelState, CommandError> {
        let target = resolve(level, ttl_hours, self.clock.wall_ms())?;
        let mut state = self.state.lock();
        update_prefs(&self.dir, |prefs| {
            let extra = prefs
                .app
                .log_level
                .take()
                .map(|saved| saved.extra)
                .unwrap_or_default();
            prefs.app.log_level = target.until_ms.map(|until_ms| LogLevelOverride {
                level: target.level.as_str().into(),
                until_ms,
                extra,
            });
            true
        })?;
        (self.apply)(target.level.filter());
        state.current = target;
        self.wake.notify_all();
        tracing::info!(
            event = "log.level_set",
            level = target.level.as_str(),
            until_ms = target.until_ms,
            "log level changed"
        );
        Ok(target)
    }

    /// Reverts to `info` if the override has expired. The live level always
    /// reverts; prefs are cleared only if they can be loaded.
    fn expire_if_due(&self) -> bool {
        let mut state = self.state.lock();
        let now = self.clock.wall_ms();
        match state.current.until_ms {
            Some(until) if until <= now => {}
            _ => return false,
        }
        (self.apply)(LogLevel::Info.filter());
        state.current = LevelState::default();
        let persisted = update_prefs(&self.dir, |prefs| {
            let due = prefs
                .app
                .log_level
                .as_ref()
                .is_some_and(|saved| saved.until_ms <= now);
            if due {
                prefs.app.log_level = None;
            }
            due
        });
        tracing::info!(
            event = "log.level_expired",
            persisted = persisted.is_ok(),
            "temporary log level expired; back to info"
        );
        true
    }

    /// Expiry thread: sleeps until the current override ends (or a change
    /// wakes it), never on a fixed polling interval.
    fn run(&self) {
        loop {
            let mut state = self.state.lock();
            if state.shutdown {
                return;
            }
            let Some(until) = state.current.until_ms else {
                self.wake.wait(&mut state);
                continue;
            };
            let now = self.clock.wall_ms();
            if until > now {
                let remaining = Duration::from_millis(until.abs_diff(now)).min(MAX_WAIT);
                self.wake.wait_for(&mut state, remaining);
                continue;
            }
            drop(state);
            self.expire_if_due();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::{
        fs,
        sync::atomic::{AtomicI64, Ordering},
        time::Instant,
    };

    const HOUR: i64 = 3_600_000;
    const T0: i64 = 1_792_664_493_000;

    struct FakeClock(AtomicI64);
    impl FakeClock {
        fn advance(&self, ms: i64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            Instant::now()
        }
        fn wall_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
        fn sleep(&self, duration: Duration) {
            self.advance(duration.as_millis() as i64);
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("quicker-level-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A controller without the expiry thread, driven by a fake clock.
    fn manual(
        dir: &std::path::Path,
        clock: Arc<FakeClock>,
    ) -> (Arc<Shared>, Arc<Mutex<Vec<LevelFilter>>>) {
        let applied = Arc::new(Mutex::new(Vec::new()));
        let log = applied.clone();
        let shared = Arc::new(Shared {
            dir: dir.to_path_buf(),
            clock,
            apply: Arc::new(move |level| log.lock().push(level)),
            state: Mutex::new(Inner {
                current: LevelState::default(),
                shutdown: false,
            }),
            wake: Condvar::new(),
        });
        shared.restore();
        (shared, applied)
    }

    fn prefs_json(dir: &std::path::Path) -> Value {
        serde_json::from_slice(&fs::read(dir.join("prefs.json")).unwrap()).unwrap()
    }

    #[test]
    fn resolve_follows_the_contract() {
        assert_eq!(
            resolve(LogLevel::Debug, None, T0).unwrap(),
            LevelState::default()
        );
        assert_eq!(
            resolve(LogLevel::Info, Some(5.0), T0).unwrap(),
            LevelState::default()
        );
        assert_eq!(
            resolve(LogLevel::Info, Some(-1.0), T0).unwrap(),
            LevelState::default()
        );
        assert_eq!(
            resolve(LogLevel::Debug, Some(DEFAULT_TTL_HOURS), T0).unwrap(),
            LevelState {
                level: LogLevel::Debug,
                until_ms: Some(T0 + 24 * HOUR)
            }
        );
        assert_eq!(
            resolve(LogLevel::Warn, Some(0.5), T0).unwrap().until_ms,
            Some(T0 + HOUR / 2)
        );
        assert_eq!(
            resolve(LogLevel::Trace, Some(1e300), T0).unwrap().until_ms,
            Some(i64::MAX)
        );
        for bad in [0.0, -2.0, f64::NAN, f64::INFINITY] {
            let Err(CommandError::Validation { diagnostics }) =
                resolve(LogLevel::Trace, Some(bad), T0)
            else {
                panic!("ttl {bad} must be rejected");
            };
            assert_eq!(diagnostics[0].code, "log_ttl_invalid");
            assert_eq!(diagnostics[0].path, "ttl_hours");
        }
        assert_eq!(
            serde_json::to_value(resolve(LogLevel::Debug, Some(1.0), T0).unwrap()).unwrap(),
            json!({"level": "debug", "until_ms": T0 + HOUR})
        );
        assert_eq!(
            serde_json::to_value(LevelState::default()).unwrap(),
            json!({"level": "info", "until_ms": null})
        );
    }

    #[test]
    fn restore_decision_is_a_pure_function_of_time() {
        let saved = |level: &str, until_ms| LogLevelOverride {
            level: level.into(),
            until_ms,
            extra: Default::default(),
        };
        assert_eq!(restore_decision(None, T0), Restore::Default);
        assert_eq!(
            restore_decision(Some(&saved("debug", T0 + 1)), T0),
            Restore::Keep(LevelState {
                level: LogLevel::Debug,
                until_ms: Some(T0 + 1)
            })
        );
        assert_eq!(
            restore_decision(Some(&saved("debug", T0)), T0),
            Restore::Expired
        );
        assert_eq!(
            restore_decision(Some(&saved("trace", T0 - HOUR)), T0),
            Restore::Expired
        );
        assert_eq!(
            restore_decision(Some(&saved("info", T0 + HOUR)), T0),
            Restore::Default
        );
        assert_eq!(
            restore_decision(Some(&saved("verbose", T0 + HOUR)), T0),
            Restore::Default
        );
    }

    #[test]
    fn debug_reverts_to_info_after_its_ttl_with_a_fake_clock() {
        let dir = temp_dir();
        fs::write(
            dir.join("prefs.json"),
            br#"{"app":{"hotkey":"Ctrl+Q","log_level":{"level":"warn","until_ms":1,"set_by":"ui"}},"ui":{"theme":"dark"}}"#,
        )
        .unwrap();
        let clock = Arc::new(FakeClock(AtomicI64::new(T0)));
        let (control, applied) = manual(&dir, clock.clone());
        // The stale override from the file expired at startup and was cleared.
        assert_eq!(prefs_json(&dir)["app"]["log_level"], Value::Null);
        assert!(applied.lock().is_empty());
        let state = control.set(LogLevel::Debug, Some(24.0)).unwrap();
        assert_eq!(state.until_ms, Some(T0 + 24 * HOUR));
        let written = prefs_json(&dir);
        assert_eq!(
            written["app"]["log_level"],
            json!({"level": "debug", "until_ms": T0 + 24 * HOUR})
        );
        assert_eq!(written["app"]["hotkey"], "Ctrl+Q");
        assert_eq!(written["ui"]["theme"], "dark");
        clock.advance(24 * HOUR - 1);
        assert!(!control.expire_if_due());
        assert_eq!(control.state.lock().current.level, LogLevel::Debug);
        clock.advance(1);
        assert!(control.expire_if_due());
        assert!(!control.expire_if_due());
        assert_eq!(control.state.lock().current, LevelState::default());
        assert_eq!(*applied.lock(), [LevelFilter::DEBUG, LevelFilter::INFO]);
        let written = prefs_json(&dir);
        assert_eq!(written["app"]["log_level"], Value::Null);
        assert_eq!(written["app"]["hotkey"], "Ctrl+Q");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_live_override_survives_restart_and_keeps_its_unknown_fields() {
        let dir = temp_dir();
        let clock = Arc::new(FakeClock(AtomicI64::new(T0)));
        let (control, _) = manual(&dir, clock.clone());
        control.set(LogLevel::Trace, Some(2.0)).unwrap();
        let mut prefs: Value = prefs_json(&dir);
        prefs["app"]["log_level"]["set_by"] = json!({"source": "health_page"});
        fs::write(dir.join("prefs.json"), serde_json::to_vec(&prefs).unwrap()).unwrap();
        drop(control);
        clock.advance(HOUR);
        let (control, applied) = manual(&dir, clock.clone());
        assert_eq!(*applied.lock(), [LevelFilter::TRACE]);
        assert_eq!(
            control.state.lock().current,
            LevelState {
                level: LogLevel::Trace,
                until_ms: Some(T0 + 2 * HOUR)
            }
        );
        // Changing the level again keeps the unknown nested field.
        control.set(LogLevel::Debug, Some(1.0)).unwrap();
        assert_eq!(
            prefs_json(&dir)["app"]["log_level"]["set_by"],
            json!({"source": "health_page"})
        );
        // Back to the default via a null TTL.
        assert_eq!(
            control.set(LogLevel::Debug, None).unwrap(),
            LevelState::default()
        );
        assert_eq!(prefs_json(&dir)["app"]["log_level"], Value::Null);
        assert_eq!(*applied.lock().last().unwrap(), LevelFilter::INFO);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Codex review: an unreadable prefs.json is never written, neither by
    /// `set_log_level` nor at startup.
    #[test]
    fn unreadable_prefs_block_writes_and_changes() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("prefs.json")).unwrap();
        let clock = Arc::new(FakeClock(AtomicI64::new(T0)));
        let (control, applied) = manual(&dir, clock);
        assert!(matches!(
            control.set(LogLevel::Debug, Some(1.0)),
            Err(CommandError::Io { .. })
        ));
        assert!(applied.lock().is_empty());
        assert_eq!(control.state.lock().current, LevelState::default());
        assert!(dir.join("prefs.json").is_dir());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expiry_still_reverts_the_live_level_when_prefs_cannot_be_written() {
        let dir = temp_dir();
        let clock = Arc::new(FakeClock(AtomicI64::new(T0)));
        let (control, applied) = manual(&dir, clock.clone());
        control.set(LogLevel::Debug, Some(1.0)).unwrap();
        fs::remove_file(dir.join("prefs.json")).unwrap();
        fs::create_dir_all(dir.join("prefs.json")).unwrap();
        clock.advance(HOUR);
        assert!(control.expire_if_due());
        assert_eq!(*applied.lock(), [LevelFilter::DEBUG, LevelFilter::INFO]);
        assert!(dir.join("prefs.json").is_dir());
        fs::remove_dir_all(dir).unwrap();
    }

    /// What startup reports as `log_level_elevated`: only an override that
    /// is still in effect.
    #[test]
    fn restore_returns_only_an_override_still_in_effect() {
        let dir = temp_dir();
        let clock = Arc::new(FakeClock(AtomicI64::new(T0)));
        let (control, _) = manual(&dir, clock.clone());
        assert_eq!(control.restore(), None);
        control.set(LogLevel::Debug, Some(1.0)).unwrap();
        assert_eq!(
            control.restore(),
            Some(LevelState {
                level: LogLevel::Debug,
                until_ms: Some(T0 + HOUR)
            })
        );
        clock.advance(HOUR);
        assert_eq!(control.restore(), None, "expired");
        assert_eq!(prefs_json(&dir)["app"]["log_level"], Value::Null);
        fs::remove_dir_all(dir).unwrap();

        // `LogLevelControl` keeps what it restored at start.
        let dir = temp_dir();
        fs::write(
            dir.join("prefs.json"),
            br#"{"app":{"log_level":{"level":"trace","until_ms":4102444800000}}}"#,
        )
        .unwrap();
        let control = LogLevelControl::start(
            dir.clone(),
            Arc::new(quicker_platform::traits::SystemClock),
            Arc::new(|_| {}),
        );
        assert_eq!(
            control.restored(),
            Some(LevelState {
                level: LogLevel::Trace,
                until_ms: Some(4_102_444_800_000)
            })
        );
        drop(control);
        fs::remove_dir_all(dir).unwrap();
    }

    /// The expiry thread wakes on its own at the deadline (real clock).
    #[test]
    fn the_expiry_thread_reverts_at_the_deadline() {
        let dir = temp_dir();
        let applied = Arc::new(Mutex::new(Vec::new()));
        let log = applied.clone();
        let control = LogLevelControl::start(
            dir.clone(),
            Arc::new(quicker_platform::traits::SystemClock),
            Arc::new(move |level| log.lock().push(level)),
        );
        // 0.00003 h = 108 ms.
        control.set(LogLevel::Debug, Some(0.00003)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while control.current().level != LogLevel::Info {
            assert!(Instant::now() < deadline, "override never expired");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*applied.lock(), [LevelFilter::DEBUG, LevelFilter::INFO]);
        assert_eq!(prefs_json(&dir)["app"]["log_level"], Value::Null);
        drop(control);
        fs::remove_dir_all(dir).unwrap();
    }
}
