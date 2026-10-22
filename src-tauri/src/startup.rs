//! Startup problems (docs/ipc-contract-0.4.md §3 `startup-problem`) and the
//! startup/exit wiring around the store: autostart repair, the
//! SafeMode(Locked) retry and the run marker lifecycle (B-04).
use crate::{
    app_state::Managed,
    autostart::{self, LoginItem},
    error::SafeModeReason,
    store::{self, RecoverySource, Store, StoreEnv},
};
use quicker_platform::{runtime, supervisor_wiring, ExecutionResult, Shared};
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

/// Starts the engine under supervision (B-03): attaches it to the runtime
/// (its pause flag becomes the runtime's manual pause), starts the platform
/// engine, and installs the supervisor on this platform's adapters, which
/// publishes Health. Until then Health stays Starting and nothing can
/// resume. The supervisor stops with the engine: at session end (Windows)
/// and on `RunEvent::Exit` ([`on_run_event`]).
///
/// A failed start keeps the legacy `last` message; Health then ends in
/// Failed (`engine_down`) after the startup attempts instead of staying
/// Starting.
pub fn start_engine(engine: Arc<Shared>) {
    let runtime = runtime::current();
    runtime.attach(&engine);
    // Registered before the session window exists; runs on it at
    // WM_ENDSESSION, so it only sets the shutdown flag.
    #[cfg(windows)]
    quicker_platform::on_session_end(Box::new(|| {
        supervisor_wiring::begin_shutdown(&runtime::current());
    }));
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
        ];
        for (kind, expected) in cases {
            let value = serde_json::to_value(StartupProblem::from(kind)).unwrap();
            assert_eq!(value, expected);
        }
    }
}
