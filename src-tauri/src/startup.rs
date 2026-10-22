//! Startup problems (docs/ipc-contract-0.4.md §3 `startup-problem`) and the
//! startup/exit wiring around the store: autostart repair, the
//! SafeMode(Locked) retry, the run marker lifecycle (B-04) and the startup
//! self-test with auto-listen (S-09, B-10).
use crate::{
    app_state::Managed,
    autostart::{self, LoginItem},
    error::{CommandError, SafeModeReason},
    store::{self, prefs, RecoverySource, Store, StoreEnv},
    tray,
};
use quicker_platform::{
    health::ReasonCode,
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
}

impl From<ProblemKind> for StartupProblem {
    fn from(kind: ProblemKind) -> Self {
        let severity = match kind {
            ProblemKind::SafeMode { .. } => ProblemSeverity::Error,
            ProblemKind::AutostartRepaired {} => ProblemSeverity::Info,
            _ => ProblemSeverity::Warning,
        };
        Self { kind, severity }
    }
}

/// Runs once the store is managed: keeps the run marker reachable for
/// session end, schedules the autostart repair, and starts the
/// SafeMode(Locked) retry.
pub fn after_load(app: &AppHandle) {
    let state = app.state::<Managed>();
    let mut store = state.lock();
    store::register_session_meta(&store);
    // A development build would point the user's login item at itself.
    if !cfg!(debug_assertions) {
        if let Ok(exe) = std::env::current_exe() {
            defer_autostart_repair(&mut store, autostart::SystemLoginItem, exe);
        }
    }
    if store.safe_mode() == Some(SafeModeReason::Locked) {
        spawn_locked_retry(app.clone(), store.dir().to_path_buf());
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
    // WM_ENDSESSION, so it only sets the shutdown flag.
    #[cfg(windows)]
    {
        let runtime = Arc::clone(&runtime);
        quicker_platform::on_session_end(Box::new(move || {
            supervisor_wiring::begin_shutdown(&runtime);
        }));
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
    /// These checks failed: `self_test_failed` was reported and the engine
    /// stays paused.
    Failed(Vec<CheckName>),
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
            Self::Failed(_) => "failed",
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

/// Decides, with the store locked, what startup does after its self-test.
/// A failure is always reported as `self_test_failed` (also in safe mode
/// and with auto-listen off) and leaves the engine paused. Otherwise the
/// engine starts listening only when auto-listen is on, the store is not in
/// safe mode, the user has not paused or resumed since `manual_seen` (read
/// before the wait) and the resume itself is accepted (`ready` as for
/// `set_paused`).
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
        return StartupListen::Failed(failed);
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
    match &outcome {
        StartupListen::Failed(checks) => {
            tray::notify(app, "启动自检未通过", &failed_notice(checks));
        }
        StartupListen::Refused(error) => {
            tray::notify(app, "无法自动开始监听", tray::resume_refused(error));
        }
        _ => {}
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

/// The notification body for a failed startup self-test.
fn failed_notice(checks: &[CheckName]) -> String {
    let names: Vec<_> = checks.iter().map(|check| check_label(*check)).collect();
    format!(
        "未通过：{}。输入监听保持暂停，可从托盘菜单选择「恢复监听」，或打开管理界面查看引擎状态。",
        names.join("、")
    )
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

// TODO(P1a): move this loop into the supervisor once it exists.
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
            let StartupListen::Failed(checks) = outcome else {
                panic!("{outcome:?}");
            };
            assert_eq!(checks, [CheckName::HookHeartbeat]);
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
        assert!(matches!(outcome, StartupListen::Failed(_)), "{outcome:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_self_test_notice_names_the_checks_and_the_way_back() {
        let notice = failed_notice(&[CheckName::RawInput, CheckName::Clipboard]);
        assert!(
            notice.starts_with("未通过：原始键盘输入、剪贴板。"),
            "{notice}"
        );
        assert!(notice.contains("「恢复监听」"));
    }
}
