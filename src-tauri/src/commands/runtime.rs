//! Engine runtime: state snapshot, pause, manual execution and recording.
use crate::{
    app_state::{AppState, Managed},
    error::CommandError,
    startup::StartupProblem,
};
use quicker_domain::*;
use quicker_platform::{
    health::{HealthState, PauseReasons, ReasonCode},
    runtime::Runtime,
    ExecutionResult, ManualFailure, Shared,
};
use std::{
    fmt::Display,
    sync::{atomic::Ordering, PoisonError},
    time::Duration,
};
use tauri::{Manager, State};

#[tauri::command]
pub fn load_state(store: State<Managed>) -> Result<AppState, CommandError> {
    Ok(store.lock().state())
}
/// Every problem found at startup (the `startup-problem` events in full).
#[tauri::command]
pub fn startup_problems(store: State<Managed>) -> Vec<StartupProblem> {
    store.lock().startup_problems().to_vec()
}
/// Sets or clears the manual pause reason only (contract §4). Other reasons
/// (safe mode, recording, self-test, the engine not up) keep the engine
/// paused, so `set_paused(false)` may return a paused state. A resume is
/// refused with `safe_mode` in safe mode and `engine_unavailable` while the
/// supervisor's health does not accept it; a resume during a recording
/// clears MANUAL and stays paused until the recording stops.
#[tauri::command]
pub fn set_paused(paused: bool, store: State<Managed>) -> Result<AppState, CommandError> {
    let s = store.lock();
    s.set_manual_pause(paused, quicker_platform::engine_ready)?;
    Ok(s.state())
}
#[tauri::command]
pub fn cancel_execution(store: State<Managed>) -> bool {
    let s = store.lock();
    s.engine.cancel.store(true, Ordering::SeqCst);
    true
}
#[tauri::command]
pub async fn execute_action(
    action: Action,
    app: tauri::AppHandle,
) -> Result<ExecutionResult, CommandError> {
    let (engine, runtime, busy, compiled, action, speed) = {
        let state = app.state::<Managed>();
        let s = state.lock();
        // Safe mode: the data is for viewing; nothing runs (B-04).
        s.ensure_writable()?;
        // `busy` is the executor's: claimed and given back under its
        // lifecycle lock, never over a task's (dropping the claim frees it).
        let Some(busy) = quicker_platform::ManualBusy::claim(&s.engine) else {
            return Err(CommandError::Busy);
        };
        s.engine.cancel.store(false, Ordering::SeqCst);
        // The active rules as compiled at load or publish (B-43).
        let compiled = s.engine.compiled.lock().unwrap().clone();
        (
            s.engine.clone(),
            s.runtime.clone(),
            busy,
            compiled,
            action.clone(),
            match &action {
                Action::Replay { speed, .. } => *speed,
                _ => 1.0,
            },
        )
    };
    // The runtime does not watch `busy`, so the claim and its release are
    // announced here (`engine-state`). The run's history entry is the
    // executor's: written when the run settles, or by the fence when the
    // run stalls (F5-1).
    runtime.refresh_state();
    let window = app.get_webview_window("main");
    if let Some(w) = &window {
        if let Err(e) = w.hide() {
            drop(busy);
            runtime.refresh_state();
            return Err(CommandError::internal(e));
        }
    }
    // The claim stays here and the blocking thread acts with tickets that
    // free nothing (F5-1 C): a thread that never comes back cannot hold or
    // free `busy`; dropping the claim below is the only release.
    let ticket = busy.ticket();
    let job = tauri::async_runtime::spawn_blocking(move || {
        // Each checkpoint restarts the claim's stall budget, and stops the
        // run, sending nothing, once the claim was fenced (F5-1 B).
        let checkpoint = || ticket.progress().map_err(|_| RunError::Fenced);
        std::thread::sleep(Duration::from_secs(3));
        checkpoint()?;
        let target = quicker_platform::foreground();
        let generation = engine.generation.load(Ordering::SeqCst);
        let mut context = VariableContext {
            clipboard: arboard::Clipboard::new()
                .ok()
                .and_then(|mut cb| cb.get_text().ok()),
            ..Default::default()
        };
        checkpoint()?;
        let needs_selection = serde_json::to_string(&action)
            .unwrap_or_default()
            .contains("{selection}")
            || matches!(action, Action::Reusable { .. })
                && serde_json::to_string(&compiled.config)
                    .unwrap_or_default()
                    .contains("{selection}");
        if needs_selection {
            context.selection = quicker_platform::capture_selection().ok();
        }
        checkpoint()?;
        let plan = compiled
            .expand_reusable(&action, &context)
            .map_err(RunError::Invalid)?;
        ticket
            .execute(plan, target, generation, 0, None, speed)
            .map_err(RunError::Failed)
    });
    let joined = job.await;
    drop(busy);
    runtime.refresh_state();
    if let Some(w) = window {
        let _ = w.show();
    }
    conclude(Some(joined), runtime.health().state)
}

/// Why the blocking job of a manual run ended without a result.
enum RunError {
    /// The action does not expand (an invalid or unavailable rule).
    Invalid(Vec<Diagnostic>),
    /// A progress checkpoint found the claim fenced: nothing was sent.
    Fenced,
    /// The run failed: the reason code of its history entry, never the
    /// run's text, which may echo the rule's content (F5-1 H).
    Failed(ManualFailure),
}

/// The command's answer for a manual run (F5-1 E, H), given the job's join
/// (`None`: not joined) and the supervisor's health. Fenced or
/// refused by the send exclusion: `EngineUnavailable`. A failed run:
/// `Internal`, logged by its reason code and trace id only. Nothing the
/// run's text says reaches the log or the frontend.
fn conclude<E: Display>(
    joined: Option<Result<Result<ExecutionResult, RunError>, E>>,
    health: HealthState,
) -> Result<ExecutionResult, CommandError> {
    match joined {
        Some(Ok(Ok(result))) => Ok(result),
        None | Some(Ok(Err(RunError::Fenced))) => Err(stalled(health, ReasonCode::ExecutorTimeout)),
        Some(Ok(Err(RunError::Invalid(diagnostics)))) => Err(CommandError::validation(diagnostics)),
        Some(Ok(Err(RunError::Failed(failure))))
            if failure.reason == ReasonCode::ExecutorTimeout =>
        {
            Err(stalled(health, failure.reason))
        }
        Some(Ok(Err(RunError::Failed(failure)))) => Err(run_failed(failure)),
        // 集成时改用 CommandError::task_failed（F2-1）
        Some(Err(e)) => Err(CommandError::internal(e)),
    }
}

/// The run was fenced (it stalled past its budget) or refused by the send
/// exclusion: the executor is down until the supervisor rebuilds it. The
/// health is where the supervisor is heading: its state when that already
/// refuses a resume, `Recovering` while it still reads Healthy or Degraded
/// (its next tick publishes the executor down).
fn stalled(health: HealthState, reason: ReasonCode) -> CommandError {
    let health = if health.accepts_resume() {
        HealthState::Recovering
    } else {
        health
    };
    tracing::warn!(
        event = "command.execute_failed",
        category = "stalled",
        reason = ?reason,
        ?health,
        "manual run stopped: the executor is down"
    );
    CommandError::EngineUnavailable { health }
}

/// A manual run that failed otherwise: `Internal` with a fresh trace id,
/// logged with the run's reason code (H: never its text).
fn run_failed(failure: ManualFailure) -> CommandError {
    let trace_id = uuid::Uuid::new_v4().to_string();
    tracing::error!(
        event = "command.execute_failed",
        category = "run_failed",
        reason = ?failure.reason,
        trace_id = %trace_id,
        "manual run failed"
    );
    CommandError::Internal { trace_id }
}
/// Opens a recording session. Detection pauses through the `Recording`
/// reason before the session opens and for as long as it stays open.
#[tauri::command]
pub fn start_recording(store: State<Managed>) -> Result<(), CommandError> {
    let (engine, runtime) = {
        let s = store.lock();
        if s.engine.busy.load(Ordering::SeqCst) {
            return Err(CommandError::Busy);
        }
        (s.engine.clone(), s.runtime.clone())
    };
    runtime.set_pause(PauseReasons::RECORDING, true);
    let started = quicker_platform::start_recording(&engine).map_err(CommandError::internal);
    sync_recording_reason(&runtime, &engine);
    started
}
/// Closes the recording session and returns what it captured. The
/// `Recording` reason clears with it, so the engine goes back to what the
/// other reasons say (a manual pause from before the recording still holds).
#[tauri::command]
pub fn stop_recording(store: State<Managed>) -> Result<ActionPlan, CommandError> {
    let (engine, runtime) = {
        let s = store.lock();
        (s.engine.clone(), s.runtime.clone())
    };
    // Windows stops and takes the recording in one critical section, so no
    // hook event lands between the two (M2).
    #[cfg(windows)]
    let plan = quicker_platform::finish_and_take(&engine);
    #[cfg(not(windows))]
    let plan = quicker_platform::stop_recording(&engine);
    sync_recording_reason(&runtime, &engine);
    plan.map_err(CommandError::internal)
}
/// `Recording` is held exactly while a recording session is open. Read
/// after every start or stop, whatever its outcome, so a failed start does
/// not leave the reason behind and a failed stop does not drop it early.
/// The recording lock is released before the runtime is touched.
fn sync_recording_reason(runtime: &Runtime, engine: &Shared) {
    let active = engine
        .recording
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some();
    runtime.set_pause(PauseReasons::RECORDING, active);
}

#[cfg(test)]
mod tests {
    use super::*;
    use quicker_platform::traits::SystemClock;
    use std::sync::Arc;

    #[test]
    fn the_recording_reason_follows_the_open_session() {
        let runtime = Runtime::new(Arc::new(SystemClock));
        let engine = Shared::new();
        runtime.attach(&engine);
        runtime.set_pause(PauseReasons::MANUAL, false);
        // A start that failed leaves no reason behind.
        runtime.set_pause(PauseReasons::RECORDING, true);
        sync_recording_reason(&runtime, &engine);
        assert!(!runtime.pause_reasons().contains(PauseReasons::RECORDING));
        // An open session holds it, and the engine stays paused even with
        // MANUAL clear.
        *engine.recording.lock().unwrap() = Some(vec![]);
        sync_recording_reason(&runtime, &engine);
        assert_eq!(runtime.pause_reasons(), PauseReasons::RECORDING);
        assert!(engine.paused.load(Ordering::SeqCst));
        // Closing it clears only RECORDING.
        runtime.set_pause(PauseReasons::MANUAL, true);
        engine.recording.lock().unwrap().take();
        sync_recording_reason(&runtime, &engine);
        assert_eq!(runtime.pause_reasons(), PauseReasons::MANUAL);
    }

    fn succeeded() -> ExecutionResult {
        ExecutionResult {
            id: "run".into(),
            status: "completed".into(),
            message: String::new(),
            completed_steps: 1,
            side_effects: true,
        }
    }

    /// F5-1 E: a run that finished is returned as is. A checkpoint that
    /// found the claim fenced, and a run refused as fenced or by the send
    /// exclusion, are `engine_unavailable` with the health the supervisor is
    /// heading to; a run that was never joined is the same.
    #[test]
    fn a_run_concludes_by_its_reason_code() {
        let result = conclude::<tauri::Error>(Some(Ok(Ok(succeeded()))), HealthState::Healthy)
            .expect("the run succeeded");
        assert_eq!(result.id, "run");
        for refused in [
            Err(RunError::Fenced),
            Err(RunError::Failed(ManualFailure {
                reason: ReasonCode::ExecutorTimeout,
            })),
        ] {
            assert!(matches!(
                conclude::<tauri::Error>(Some(Ok(refused)), HealthState::Degraded),
                Err(CommandError::EngineUnavailable {
                    health: HealthState::Recovering
                })
            ));
        }
        assert!(matches!(
            conclude::<tauri::Error>(None, HealthState::Failed),
            Err(CommandError::EngineUnavailable {
                health: HealthState::Failed
            })
        ));
    }

    /// F5-1 H: a manual run whose rule carries a private value (an unknown
    /// common function, missing image and file paths, an unknown key name)
    /// fails preflight in the platform, which hands back only the reason
    /// code (`ManualFailure` has no text; the platform's
    /// `a_manual_runs_failure_never_echoes_the_rules_values` checks the
    /// run's own text and history). The mapping logs the category, the
    /// reason and the trace id, and the frontend gets the code and the
    /// trace id: the value is in neither.
    #[test]
    fn a_failed_run_reaches_the_log_and_the_frontend_by_its_reason_code_only() {
        use crate::diagnostics::logging::{test_support, DEFAULT_LEVEL};
        const SENTINEL: &str = "SENTINEL-private-7d41";
        let missing = std::env::temp_dir()
            .join(format!("qt-missing-{SENTINEL}"))
            .to_string_lossy()
            .into_owned();
        let plan = ActionPlan {
            steps: [
                Operation::Common {
                    function: SENTINEL.into(),
                },
                Operation::PasteImage {
                    path: format!("{missing}.png"),
                },
                Operation::PasteFiles {
                    paths: vec![format!("{missing}.txt")],
                },
                Operation::KeyPress {
                    key: Key::named(SENTINEL),
                },
            ]
            .into_iter()
            .map(|operation| Step::new(operation, SourceSpan::default()))
            .collect(),
            ..Default::default()
        };
        // Stands in for `ManualTicket::execute` (no real input in tests):
        // what the platform returns for this plan.
        let execute = |plan: ActionPlan| -> Result<ExecutionResult, ManualFailure> {
            assert!(serde_json::to_string(&plan).unwrap().contains(SENTINEL));
            Err(ManualFailure {
                reason: ReasonCode::PreflightFailed,
            })
        };
        let dir = std::env::temp_dir().join(format!("quicker-exec-log-{}", uuid::Uuid::new_v4()));
        let built = test_support::scoped(&dir, DEFAULT_LEVEL);
        let error = tracing::subscriber::with_default(built.subscriber, || {
            let joined = Some(Ok::<_, tauri::Error>(
                execute(plan).map_err(RunError::Failed),
            ));
            conclude(joined, HealthState::Healthy).expect_err("the run failed")
        });
        drop(built.guard);
        let CommandError::Internal { trace_id } = &error else {
            panic!("a failed run is an internal error, got {error:?}");
        };
        let shown = serde_json::to_string(&error).unwrap();
        assert!(!shown.contains(SENTINEL), "{shown}");
        assert_eq!(
            shown,
            format!(r#"{{"code":"internal","detail":{{"trace_id":"{trace_id}"}}}}"#)
        );
        let log = test_support::read_logs(&dir);
        assert!(!log.contains(SENTINEL), "{log}");
        let line = log
            .lines()
            .find(|line| line.contains(trace_id.as_str()))
            .unwrap_or_else(|| panic!("trace id {trace_id} missing from log:\n{log}"));
        assert!(line.contains("event=\"command.execute_failed\""), "{line}");
        assert!(line.contains("category=\"run_failed\""), "{line}");
        assert!(line.contains("reason=PreflightFailed"), "{line}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
