//! Startup problems (docs/ipc-contract-0.4.md §3 `startup-problem`) and the
//! startup/exit wiring around the store: autostart repair, the
//! SafeMode(Locked) retry, the run marker lifecycle (B-04), the restored
//! log level and the startup self-test with auto-listen (S-09, B-10).
use crate::{
    app_state::Managed,
    autostart::{self, LoginItem},
    diagnostics::level::{LevelState, LogLevel, LogLevelControl},
    error::{CommandError, SafeModeReason},
    store::{self, prefs, RecoverySource, Store, StoreEnv},
    tray,
};
use quicker_platform::{
    health::{PauseReasons, ReasonCode},
    runtime::{self, Runtime},
    self_test::{self, CheckName, SelfTestReport},
    supervisor_wiring, ExecutionResult, Shared,
};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::{Arc, PoisonError},
    time::Duration,
};
use tauri::{AppHandle, Manager, RunEvent};

/// One problem found while starting: `{ kind, severity, args }`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StartupProblem {
    #[serde(flatten)]
    pub kind: ProblemKind,
    pub severity: ProblemSeverity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemSeverity {
    Info,
    Warning,
    Error,
}

/// The 0.4 kinds owned by the store and autostart; file names never include
/// a directory.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", content = "args", rename_all = "snake_case")]
pub enum ProblemKind {
    SafeMode {
        reason: SafeModeReason,
    },
    Recovered {
        from: RecoverySource,
        revision: Option<u64>,
    },
    ConfigQuarantined {
        file: String,
        quarantined_as: String,
    },
    RulesSkipped {
        count: usize,
        ids: Vec<String>,
    },
    TerminatorUnsupported {
        keys: Vec<String>,
    },
    AutostartRepaired {},
    StoreMetaReset {},
    /// The startup self-test checks that failed (B-10); the engine stays
    /// paused until the user resumes it.
    SelfTestFailed {
        checks: Vec<CheckName>,
    },
    /// A temporary log level restored from prefs at startup, in effect
    /// until `until_ms` (Unix milliseconds).
    LogLevelElevated {
        level: LogLevel,
        until_ms: i64,
    },
    /// The backup before the first 0.4 write (`backup`, the directory name
    /// `backup-pre-0.4`) could not be made: saving is refused and retried
    /// with each write, the active configuration keeps working (R6).
    BackupFailed {
        backup: String,
    },
}

impl From<ProblemKind> for StartupProblem {
    fn from(kind: ProblemKind) -> Self {
        let severity = match kind {
            ProblemKind::SafeMode { .. } => ProblemSeverity::Error,
            ProblemKind::AutostartRepaired {} | ProblemKind::LogLevelElevated { .. } => {
                ProblemSeverity::Info
            }
            _ => ProblemSeverity::Warning,
        };
        Self { kind, severity }
    }
}

/// Runs once the store is managed: keeps the run marker reachable for
/// session end, reports a restored temporary log level, schedules the
/// autostart repair, and starts the SafeMode(Locked) retry.
pub fn after_load(app: &AppHandle) {
    let state = app.state::<Managed>();
    let mut store = state.lock();
    store::register_session_meta(&store);
    report_restored_level(&mut store, app.state::<LogLevelControl>().restored());
    // A development build would point the user's login item at itself.
    if !cfg!(debug_assertions) {
        if let Ok(exe) = std::env::current_exe() {
            let legacy_exe = exe.clone();
            store.when_writable(
                "autostart_migrate",
                Box::new(move |_| match autostart::migrate_legacy(&legacy_exe) {
                    Ok(true) => tracing::info!(
                        event = "autostart.legacy_migrated",
                        "legacy autostart entry carried over"
                    ),
                    Ok(false) => {}
                    Err(e) => tracing::warn!(
                        event = "autostart.legacy_migrate_failed",
                        error = %e,
                        "legacy autostart entry could not be carried over"
                    ),
                }),
            );
            defer_autostart_repair(&mut store, autostart::SystemLoginItem, exe);
        }
    }
    if store.safe_mode() == Some(SafeModeReason::Locked) {
        spawn_locked_retry(app.clone(), store.dir().to_path_buf());
    }
}

/// Reports a temporary log level restored from prefs (still in effect) as
/// `log_level_elevated`, so the user sees that logging is more verbose than
/// usual and until when.
pub fn report_restored_level(store: &mut Store, restored: Option<LevelState>) {
    if let Some(LevelState {
        level,
        until_ms: Some(until_ms),
    }) = restored
    {
        store.add_problem(ProblemKind::LogLevelElevated { level, until_ms });
    }
}

/// Repairs a stale login item (B-04) once, and only while the store is
/// writable: never during safe mode, where it waits until safe mode ends
/// (see [`Store::when_writable`]). The repair itself only writes when the
/// entry points somewhere else.
pub fn defer_autostart_repair(
    store: &mut Store,
    item: impl LoginItem + Send + 'static,
    exe: PathBuf,
) {
    store.when_writable(
        "autostart_repair",
        Box::new(move |store| match autostart::repair(&item, &exe) {
            Ok(true) => store.add_problem(ProblemKind::AutostartRepaired {}),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                event = "autostart.repair_failed",
                error = %e,
                "autostart entry could not be checked or repaired"
            ),
        }),
    );
}

/// Starts the engine under supervision (B-03): starts the platform engine
/// and installs the supervisor on this platform's adapters, which publishes
/// Health. `runtime` is the one the store attached `engine` to at load
/// (`Store::runtime`, the pause authority). Until the first Healthy publish
/// the engine is held paused and nothing can resume. The supervisor stops
/// with the engine: at session end (Windows) and on `RunEvent::Exit`
/// ([`on_run_event`]).
///
/// A failed start keeps the legacy `last` message; Health then ends in
/// Failed (`engine_down`) after the startup attempts instead of staying
/// Starting.
pub fn start_engine(engine: Arc<Shared>, runtime: Arc<Runtime>) {
    // Registered before the session window exists; runs on it at
    // WM_ENDSESSION (see [`end_session`]).
    #[cfg(windows)]
    {
        let runtime = Arc::clone(&runtime);
        quicker_platform::on_session_end(Box::new(move || end_session(&runtime)));
    }
    let started = quicker_platform::start_engine(engine.clone());
    if let Err(message) = &started {
        tracing::error!(event = "engine.start_failed", error = %message);
        *engine.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(ExecutionResult {
            id: "engine".into(),
            status: "blocked".into(),
            message: message.clone(),
            completed_steps: 0,
            side_effects: false,
        });
    }
    let (executor, inputs) = supervisor_wiring::platform_adapters(&started);
    let clock = Arc::clone(runtime.clock());
    supervisor_wiring::install_supervisor(clock, runtime, executor, inputs);
}

/// The session-end callback (R5): runs on the session window thread while
/// it handles `WM_ENDSESSION`, real or replayed by the `session_end` fault,
/// and Windows may end the process as soon as it returns. The engine stops,
/// then the run marker is cleared, one small atomic write and no waiting.
/// [`on_run_event`] clears it too, but only if tao turns the session end
/// into `RunEvent::Exit` before the process dies (待实测), and the fault
/// never reaches tao; a marker left behind would count the shutdown as a
/// crash, and two of them in a row would end in SafeMode(CrashLoop).
/// Clearing twice is a no-op.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn end_session(runtime: &Runtime) {
    supervisor_wiring::begin_shutdown(runtime);
    store::clear_run_marker_on_session_end();
}

/// What startup did once its self-test finished (B-10).
#[derive(Debug)]
pub enum StartupListen {
    /// Every check passed and auto-listen is on: listening.
    Listening,
    /// Every check passed, but auto-listen is off: the engine stays paused.
    AutoListenOff,
    /// Safe mode never starts listening on its own.
    SafeMode,
    /// The user paused or resumed while startup waited: left as they set it.
    UserDecided,
    /// These checks failed: `self_test_failed` was reported and the pause
    /// state was left alone. `resumed`: the user resumed while startup
    /// waited and that resume still stands (MANUAL clear, review A6-2, R3).
    /// It records the user's action, not the engine's state: another reason
    /// (e.g. RECORDING, ENGINE_DOWN) may still keep the engine from
    /// listening. Otherwise MANUAL holds and the engine stays paused.
    Failed {
        checks: Vec<CheckName>,
        resumed: bool,
    },
    /// The resume was refused (the engine is no longer up).
    Refused(CommandError),
}

impl StartupListen {
    fn label(&self) -> &'static str {
        match self {
            Self::Listening => "listening",
            Self::AutoListenOff => "auto_listen_off",
            Self::SafeMode => "safe_mode",
            Self::UserDecided => "user_decided",
            Self::Failed { resumed: false, .. } => "failed",
            Self::Failed { resumed: true, .. } => "failed_user_resumed",
            Self::Refused(_) => "refused",
        }
    }
}

/// The checks that keep startup from listening: every failed check except
/// those this platform cannot run (`unsupported`, e.g. every check on
/// macOS), which the `self_test` command still reports as failed.
pub fn startup_failures(report: &SelfTestReport) -> Vec<CheckName> {
    report
        .checks
        .iter()
        .filter(|check| !check.passed && check.reason != Some(ReasonCode::Unsupported))
        .map(|check| check.name)
        .collect()
}

/// Decides, with the store locked, what startup does after its self-test;
/// everything it reads (safe mode, the user's pause changes and reasons) is
/// read under that one lock, so the user's commands and the Locked retry
/// fall either wholly before or wholly after the decision.
///
/// A failure is always reported as `self_test_failed` (also in safe mode
/// and with auto-listen off) and never changes the pause state: the engine
/// stays paused, unless the user resumed since `manual_seen` (read before
/// the wait) and that resume still stands, which is kept and only told
/// apart (`Failed { resumed: true }`, review A6-2). Otherwise the engine
/// starts listening only when auto-listen is on, the store is not in safe
/// mode, the user has not paused or resumed since `manual_seen` and the
/// resume itself is accepted (`ready` as for `set_paused`).
///
/// SafeMode(Locked) retry (review A6-1): one that succeeded before this
/// call left a store out of safe mode, and the rules above apply as usual.
/// One that succeeds after it (outcome [`StartupListen::SafeMode`]) never
/// listens on its own: leaving safe mode clears only SAFE_MODE, and MANUAL
/// stays until the user resumes.
pub fn finish_self_test(
    store: &mut Store,
    report: &SelfTestReport,
    auto_listen: bool,
    manual_seen: u64,
    ready: impl FnOnce() -> bool,
) -> StartupListen {
    let failed = startup_failures(report);
    if !failed.is_empty() {
        store.add_problem(ProblemKind::SelfTestFailed {
            checks: failed.clone(),
        });
        let resumed = store.manual_changes() != manual_seen
            && !store.runtime.pause_reasons().contains(PauseReasons::MANUAL);
        return StartupListen::Failed {
            checks: failed,
            resumed,
        };
    }
    if store.safe_mode().is_some() {
        return StartupListen::SafeMode;
    }
    if !auto_listen {
        return StartupListen::AutoListenOff;
    }
    if store.manual_changes() != manual_seen {
        return StartupListen::UserDecided;
    }
    match store.set_manual_pause(false, ready) {
        Ok(()) => StartupListen::Listening,
        Err(error) => StartupListen::Refused(error),
    }
}

/// Starts the startup self-test (B-10) on its own thread, after
/// [`start_engine`]: waits up to [`self_test::STARTUP_HEALTHY_WAIT`] for
/// Healthy, runs the checks with no lock held, then [`finish_self_test`].
/// A failure, or a resume that was refused, is also shown as a system
/// notification. 待实测: the timing of a cold start at login.
pub fn spawn_self_test(app: AppHandle) {
    let spawned = std::thread::Builder::new()
        .name("qt-self-test-startup".into())
        .spawn(move || startup_self_test(&app));
    if let Err(e) = spawned {
        tracing::error!(
            event = "self_test.startup_thread_failed",
            error = %e,
            "startup self-test did not start"
        );
    }
}

fn startup_self_test(app: &AppHandle) {
    let state = app.state::<Managed>();
    let (runtime, dir, manual_seen) = {
        let store = state.lock();
        (
            store.runtime.clone(),
            store.dir().to_path_buf(),
            store.manual_changes(),
        )
    };
    let health = self_test::wait_for_healthy(&runtime, self_test::STARTUP_HEALTHY_WAIT);
    if runtime.shutting_down() {
        return;
    }
    let report = self_test::run_platform(&runtime);
    let auto_listen = prefs::auto_listen_on_start(&dir);
    let outcome = {
        let mut store = state.lock();
        finish_self_test(
            &mut store,
            &report,
            auto_listen,
            manual_seen,
            quicker_platform::engine_ready,
        )
    };
    tracing::info!(
        event = "self_test.startup",
        health = ?health,
        auto_listen,
        outcome = outcome.label(),
        "startup self-test done"
    );
    if let Some((title, body)) = outcome_notice(&outcome) {
        tray::notify(app, title, &body);
    }
}

/// The system notification for a startup outcome: a failed check (with
/// the way back, or the user's own resume) or a refused resume.
fn outcome_notice(outcome: &StartupListen) -> Option<(&'static str, String)> {
    match outcome {
        StartupListen::Failed { checks, resumed } => {
            Some(("启动自检未通过", failed_notice(checks, *resumed)))
        }
        StartupListen::Refused(error) => {
            Some(("无法自动开始监听", tray::resume_refused(error).to_owned()))
        }
        _ => None,
    }
}

fn check_label(check: CheckName) -> &'static str {
    match check {
        CheckName::RawInput => "原始键盘输入",
        CheckName::HookHeartbeat => "键盘鼠标监听",
        CheckName::Foreground => "前台窗口识别",
        CheckName::Clipboard => "剪贴板",
    }
}

/// The notification body for a failed startup self-test. `resumed`: the
/// user resumed during startup and it was kept (review A6-2), so it must
/// not claim the engine stayed paused; it states what the user did, not
/// that the engine listens, which another pause reason may prevent
/// (review R3).
fn failed_notice(checks: &[CheckName], resumed: bool) -> String {
    let names: Vec<_> = checks.iter().map(|check| check_label(*check)).collect();
    let names = names.join("、");
    if resumed {
        format!("未通过：{names}。你已在启动期间手动恢复监听，监听可能无法正常工作；可在「运行状态与恢复」中查看详情。")
    } else {
        format!("未通过：{names}。输入监听保持暂停，可从托盘菜单选择「恢复监听」，或打开管理界面查看引擎状态。")
    }
}

/// Clears the run marker when the event loop ends. tao turns
/// `WM_ENDSESSION` (session really ending) into `LoopDestroyed`, which Tauri
/// delivers as `RunEvent::Exit` before the process exits, so shutdown and
/// Restart Manager are covered as well as `exit_app` and the tray.
///
/// The engine shuts down first: the supervisor stops and sees the engine's
/// teardown as nothing (no rebuild, no Failed while exiting).
pub fn on_run_event(_app: &AppHandle, event: RunEvent) {
    if let RunEvent::Exit = event {
        supervisor_wiring::begin_shutdown(&runtime::current());
        store::clear_run_marker_on_session_end();
    }
}

const LOCKED_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Retries a SafeMode(Locked) store every [`LOCKED_RETRY_INTERVAL`] until
/// the data directory can be read, on its own thread. Deliberately not part
/// of the supervisor: that is the platform crate's engine supervisor, which
/// knows nothing of the store, its files or Tauri, while this retry needs
/// the data directory, the managed store and the `AppHandle`, runs only in
/// this one safe mode and ends after its first success. Moving it there
/// would couple the supervisor to the shell for no gain in recovery.
fn spawn_locked_retry(app: AppHandle, dir: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("store-locked-retry".into())
        .spawn(move || {
            let env = StoreEnv::system();
            loop {
                std::thread::sleep(LOCKED_RETRY_INTERVAL);
                if let Some(report) = store::retry_locked(&dir, &env) {
                    app.state::<Managed>().lock().apply_retry(report);
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!(event = "store.locked_retry", error = %e, "retry thread did not start");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quicker_platform::{
        health::{HealthSnapshot, HealthState, PauseReasons},
        self_test::SelfTestCheck,
    };
    use serde_json::json;

    /// Shapes from docs/ipc-contract-0.4.md §3.
    #[test]
    fn problems_serialize_as_kind_severity_args() {
        let cases = [
            (
                ProblemKind::SafeMode {
                    reason: SafeModeReason::CrashLoop,
                },
                json!({"kind": "safe_mode", "severity": "error", "args": {"reason": "crash_loop"}}),
            ),
            (
                ProblemKind::Recovered {
                    from: RecoverySource::LastGood,
                    revision: Some(3),
                },
                json!({"kind": "recovered", "severity": "warning", "args": {"from": "last_good", "revision": 3}}),
            ),
            (
                ProblemKind::Recovered {
                    from: RecoverySource::Previous,
                    revision: None,
                },
                json!({"kind": "recovered", "severity": "warning", "args": {"from": "previous", "revision": null}}),
            ),
            (
                ProblemKind::ConfigQuarantined {
                    file: "active.json".into(),
                    quarantined_as: "active.corrupt-20260101-000000.json".into(),
                },
                json!({"kind": "config_quarantined", "severity": "warning", "args": {"file": "active.json", "quarantined_as": "active.corrupt-20260101-000000.json"}}),
            ),
            (
                ProblemKind::RulesSkipped {
                    count: 1,
                    ids: vec!["a".into()],
                },
                json!({"kind": "rules_skipped", "severity": "warning", "args": {"count": 1, "ids": ["a"]}}),
            ),
            (
                ProblemKind::TerminatorUnsupported {
                    keys: vec!["tab".into()],
                },
                json!({"kind": "terminator_unsupported", "severity": "warning", "args": {"keys": ["tab"]}}),
            ),
            (
                ProblemKind::AutostartRepaired {},
                json!({"kind": "autostart_repaired", "severity": "info", "args": {}}),
            ),
            (
                ProblemKind::StoreMetaReset {},
                json!({"kind": "store_meta_reset", "severity": "warning", "args": {}}),
            ),
            (
                ProblemKind::SelfTestFailed {
                    checks: vec![CheckName::HookHeartbeat, CheckName::Clipboard],
                },
                json!({"kind": "self_test_failed", "severity": "warning", "args": {"checks": ["hook_heartbeat", "clipboard"]}}),
            ),
            (
                ProblemKind::LogLevelElevated {
                    level: LogLevel::Debug,
                    until_ms: 1_792_664_493_000,
                },
                json!({"kind": "log_level_elevated", "severity": "info", "args": {"level": "debug", "until_ms": 1_792_664_493_000_i64}}),
            ),
            (
                ProblemKind::BackupFailed {
                    backup: "backup-pre-0.4".into(),
                },
                json!({"kind": "backup_failed", "severity": "warning", "args": {"backup": "backup-pre-0.4"}}),
            ),
        ];
        for (kind, expected) in cases {
            let value = serde_json::to_value(StartupProblem::from(kind)).unwrap();
            assert_eq!(value, expected);
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("quicker-startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn report(results: [Result<(), ReasonCode>; 4]) -> SelfTestReport {
        let checks: Vec<_> = CheckName::ALL
            .into_iter()
            .zip(results)
            .map(|(name, result)| SelfTestCheck {
                name,
                passed: result.is_ok(),
                reason: result.err(),
                elapsed_ms: 1,
            })
            .collect();
        SelfTestReport {
            passed: checks.iter().all(|check| check.passed),
            checks,
        }
    }

    fn healthy_store(dir: &std::path::Path) -> Store {
        let store = Store::load(dir.to_path_buf());
        store.runtime.publish_health(HealthSnapshot {
            state: HealthState::Healthy,
            components: vec![],
        });
        store
    }

    fn manual_paused(store: &Store) -> bool {
        store.runtime.pause_reasons().contains(PauseReasons::MANUAL)
    }

    /// B-10: listen on start only after a passing self-test with
    /// auto-listen on; a failure is reported and keeps the engine paused.
    #[test]
    fn startup_listens_only_after_a_passing_self_test_with_auto_listen() {
        let dir = temp_dir();
        let passed = report([Ok(()); 4]);

        // One store per directory: loading one directory again and again
        // counts as a crash loop.
        let mut store = healthy_store(&dir.join("listening"));
        let outcome = finish_self_test(&mut store, &passed, true, 0, || true);
        assert!(matches!(outcome, StartupListen::Listening), "{outcome:?}");
        assert!(!manual_paused(&store));
        assert!(store.runtime.engine_state().listening);
        assert!(store.startup_problems().is_empty());

        let mut store = healthy_store(&dir.join("auto_listen_off"));
        let outcome = finish_self_test(&mut store, &passed, false, 0, || true);
        assert!(
            matches!(outcome, StartupListen::AutoListenOff),
            "{outcome:?}"
        );
        assert!(manual_paused(&store));

        // A failed check: reported (with auto-listen on or off), paused.
        let failed = report([
            Ok(()),
            Err(ReasonCode::HookMissing),
            Err(ReasonCode::Unsupported),
            Ok(()),
        ]);
        for auto_listen in [true, false] {
            let mut store = healthy_store(&dir.join(format!("failed-{auto_listen}")));
            let outcome = finish_self_test(&mut store, &failed, auto_listen, 0, || true);
            let StartupListen::Failed { checks, resumed } = outcome else {
                panic!("{outcome:?}");
            };
            assert_eq!(checks, [CheckName::HookHeartbeat]);
            assert!(!resumed);
            assert!(manual_paused(&store));
            assert_eq!(
                store.startup_problems().last().map(|p| &p.kind),
                Some(&ProblemKind::SelfTestFailed {
                    checks: vec![CheckName::HookHeartbeat]
                })
            );
        }

        // Checks this platform cannot run do not block listening.
        let mut store = healthy_store(&dir.join("unsupported"));
        let unsupported = report([Err(ReasonCode::Unsupported); 4]);
        let outcome = finish_self_test(&mut store, &unsupported, true, 0, || true);
        assert!(matches!(outcome, StartupListen::Listening), "{outcome:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The user's own pause or resume while startup waited wins; a resume
    /// the engine refuses is passed on; safe mode never listens on its own
    /// but still reports a failed check.
    #[test]
    fn startup_respects_the_user_safe_mode_and_a_refused_resume() {
        let dir = temp_dir();
        let passed = report([Ok(()); 4]);

        let mut store = healthy_store(&dir);
        let seen = store.manual_changes();
        store.set_manual_pause(false, || true).unwrap();
        store.set_manual_pause(true, || true).unwrap();
        let outcome = finish_self_test(&mut store, &passed, true, seen, || true);
        assert!(matches!(outcome, StartupListen::UserDecided), "{outcome:?}");
        assert!(manual_paused(&store));

        // Health not up (Starting): refused with that health.
        let mut store = Store::load(dir.clone());
        let outcome = finish_self_test(&mut store, &passed, true, 0, || true);
        assert!(
            matches!(
                outcome,
                StartupListen::Refused(CommandError::EngineUnavailable {
                    health: HealthState::Starting
                })
            ),
            "{outcome:?}"
        );
        assert!(manual_paused(&store));
        std::fs::remove_dir_all(&dir).unwrap();

        // SafeMode(NoValidConfig): an unusable active.json and nothing else.
        let dir = temp_dir();
        std::fs::write(dir.join("active.json"), b"{").unwrap();
        let mut store = healthy_store(&dir);
        assert_eq!(store.safe_mode(), Some(SafeModeReason::NoValidConfig));
        let outcome = finish_self_test(&mut store, &passed, true, 0, || true);
        assert!(matches!(outcome, StartupListen::SafeMode), "{outcome:?}");
        assert!(manual_paused(&store));
        let failed = report([Err(ReasonCode::RawInputMissing), Ok(()), Ok(()), Ok(())]);
        let outcome = finish_self_test(&mut store, &failed, true, 0, || true);
        assert!(
            matches!(outcome, StartupListen::Failed { resumed: false, .. }),
            "{outcome:?}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_self_test_notice_names_the_checks_and_the_way_back() {
        let notice = failed_notice(&[CheckName::RawInput, CheckName::Clipboard], false);
        assert!(
            notice.starts_with("未通过：原始键盘输入、剪贴板。"),
            "{notice}"
        );
        assert!(notice.contains("保持暂停"));
        assert!(notice.contains("「恢复监听」"));
    }

    /// Review A6-2: the user resumed while the checks ran and the clipboard
    /// check failed. The problem is reported, the user's resume stands, and
    /// the notification says so instead of "stays paused".
    #[test]
    fn a_resume_during_the_checks_is_kept_when_a_check_fails() {
        let dir = temp_dir();
        let mut store = healthy_store(&dir);
        let seen = store.manual_changes();
        // The user resumes from the tray while the checks run.
        store.set_manual_pause(false, || true).unwrap();
        let failed = report([Ok(()), Ok(()), Ok(()), Err(ReasonCode::ClipboardBusy)]);
        let outcome = finish_self_test(&mut store, &failed, true, seen, || true);
        let StartupListen::Failed { checks, resumed } = &outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(checks, &[CheckName::Clipboard]);
        assert!(resumed);
        assert_eq!(outcome.label(), "failed_user_resumed");
        assert!(!manual_paused(&store));
        assert!(store.runtime.engine_state().listening);
        assert_eq!(
            store.startup_problems().last().map(|p| &p.kind),
            Some(&ProblemKind::SelfTestFailed {
                checks: vec![CheckName::Clipboard]
            })
        );
        let (title, body) = outcome_notice(&outcome).unwrap();
        assert_eq!(title, "启动自检未通过");
        assert_eq!(
            body,
            "未通过：剪贴板。你已在启动期间手动恢复监听，监听可能无法正常工作；可在「运行状态与恢复」中查看详情。"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Review R3: the user resumed during startup while RECORDING holds the
    /// engine (a recording is open), then a check failed. The resume is
    /// kept (MANUAL not set again) but the engine does not listen, so the
    /// notice states the user's action and never claims it is listening.
    #[test]
    fn a_resume_kept_under_another_reason_is_not_reported_as_listening() {
        let dir = temp_dir();
        let mut store = healthy_store(&dir);
        store.runtime.set_pause(PauseReasons::RECORDING, true);
        let seen = store.manual_changes();
        store.set_manual_pause(false, || true).unwrap();
        let failed = report([Ok(()), Err(ReasonCode::HookMissing), Ok(()), Ok(())]);
        let outcome = finish_self_test(&mut store, &failed, true, seen, || true);

        assert!(
            matches!(outcome, StartupListen::Failed { resumed: true, .. }),
            "{outcome:?}"
        );
        assert_eq!(outcome.label(), "failed_user_resumed");
        assert!(!manual_paused(&store), "MANUAL not set again");
        assert_eq!(store.runtime.pause_reasons(), PauseReasons::RECORDING);
        assert!(!store.runtime.engine_state().listening);
        let (_, body) = outcome_notice(&outcome).unwrap();
        assert!(body.contains("你已在启动期间手动恢复监听"), "{body}");
        for claim in ["已开启", "已由你手动开启", "正在监听", "保持暂停"] {
            assert!(!body.contains(claim), "{claim}: {body}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Review A6-2: no action by the user and a failed check: paused, with
    /// the "stays paused" text. A resume followed by a pause counts as
    /// paused too.
    #[test]
    fn a_failed_check_without_a_user_resume_stays_paused() {
        let dir = temp_dir();
        let failed = report([Ok(()), Ok(()), Ok(()), Err(ReasonCode::ClipboardBusy)]);

        let mut store = healthy_store(&dir.join("untouched"));
        let seen = store.manual_changes();
        let outcome = finish_self_test(&mut store, &failed, true, seen, || true);
        assert!(
            matches!(outcome, StartupListen::Failed { resumed: false, .. }),
            "{outcome:?}"
        );
        assert_eq!(outcome.label(), "failed");
        assert!(manual_paused(&store));
        assert!(!store.runtime.engine_state().listening);
        let (_, body) = outcome_notice(&outcome).unwrap();
        assert!(body.contains("输入监听保持暂停"), "{body}");

        let mut store = healthy_store(&dir.join("resumed_then_paused"));
        let seen = store.manual_changes();
        store.set_manual_pause(false, || true).unwrap();
        store.set_manual_pause(true, || true).unwrap();
        let outcome = finish_self_test(&mut store, &failed, true, seen, || true);
        assert!(
            matches!(outcome, StartupListen::Failed { resumed: false, .. }),
            "{outcome:?}"
        );
        assert!(manual_paused(&store));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Only a level still in effect is reported, as `log_level_elevated`.
    #[test]
    fn a_restored_log_level_is_reported_once() {
        let dir = temp_dir();
        let mut store = Store::load(dir.clone());
        report_restored_level(&mut store, None);
        report_restored_level(&mut store, Some(LevelState::default()));
        assert!(store.startup_problems().is_empty());
        report_restored_level(
            &mut store,
            Some(LevelState {
                level: LogLevel::Trace,
                until_ms: Some(42),
            }),
        );
        assert_eq!(
            store
                .startup_problems()
                .iter()
                .map(|p| (&p.kind, p.severity))
                .collect::<Vec<_>>(),
            [(
                &ProblemKind::LogLevelElevated {
                    level: LogLevel::Trace,
                    until_ms: 42
                },
                ProblemSeverity::Info
            )]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
