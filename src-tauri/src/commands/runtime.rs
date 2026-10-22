//! Engine runtime: state snapshot, pause, manual execution and recording.
use crate::{
    app_state::{AppState, Managed},
    error::CommandError,
    startup::StartupProblem,
};
use quicker_domain::*;
use quicker_platform::{health::PauseReasons, runtime::Runtime, ExecutionResult, Shared};
use std::sync::{atomic::Ordering, PoisonError};
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
    let (engine, busy, compiled, action, speed) = {
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
            busy,
            compiled,
            action.clone(),
            match &action {
                Action::Replay { speed, .. } => *speed,
                _ => 1.0,
            },
        )
    };
    let window = app.get_webview_window("main");
    if let Some(w) = &window {
        if let Err(e) = w.hide() {
            drop(busy);
            return Err(CommandError::internal(e));
        }
    }
    let e = engine.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let target = quicker_platform::foreground();
        let generation = e.generation.load(Ordering::SeqCst);
        let mut context = VariableContext {
            clipboard: arboard::Clipboard::new()
                .ok()
                .and_then(|mut cb| cb.get_text().ok()),
            ..Default::default()
        };
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
        let plan = compiled
            .expand_reusable(&action, &context)
            .map_err(CommandError::validation)?;
        quicker_platform::execute(e, plan, target, generation, 0, None, speed)
            .map_err(CommandError::internal)
    })
    .await;
    drop(busy);
    if let Some(w) = window {
        let _ = w.show();
    }
    result.map_err(CommandError::internal)?
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
}
