//! Engine runtime: state snapshot, pause, manual execution and recording.
use crate::{
    app_state::{AppState, Managed},
    error::CommandError,
    startup::StartupProblem,
};
use quicker_domain::*;
use quicker_platform::ExecutionResult;
use std::sync::atomic::Ordering;
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
#[tauri::command]
pub fn set_paused(paused: bool, store: State<Managed>) -> Result<AppState, CommandError> {
    let s = store.lock();
    if !paused {
        // Safe mode keeps the engine paused (B-04).
        s.ensure_writable()?;
        if s.engine.recording.lock().unwrap().is_some() {
            // Recording owns the input until it stops.
            return Err(CommandError::Busy);
        }
        quicker_platform::native_ready().map_err(CommandError::engine_unavailable)?;
    }
    if !paused && !quicker_platform::engine_ready() {
        return Err(CommandError::engine_unavailable(
            "This platform input adapter is not ready; engine remains paused",
        ));
    }
    s.engine.paused.store(paused, Ordering::SeqCst);
    s.engine.generation.fetch_add(1, Ordering::SeqCst);
    if paused {
        s.engine.cancel.store(true, Ordering::SeqCst)
    }
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
    let (engine, compiled, action, speed) = {
        let state = app.state::<Managed>();
        let s = state.lock();
        // Safe mode: the data is for viewing; nothing runs (B-04).
        s.ensure_writable()?;
        if s.engine.busy.swap(true, Ordering::SeqCst) {
            return Err(CommandError::Busy);
        }
        s.engine.cancel.store(false, Ordering::SeqCst);
        // The active rules as compiled at load or publish (B-43).
        let compiled = s.engine.compiled.lock().unwrap().clone();
        (
            s.engine.clone(),
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
            engine.busy.store(false, Ordering::SeqCst);
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
    engine.busy.store(false, Ordering::SeqCst);
    if let Some(w) = window {
        let _ = w.show();
    }
    result.map_err(CommandError::internal)?
}
#[tauri::command]
pub fn start_recording(store: State<Managed>) -> Result<(), CommandError> {
    if store.lock().engine.busy.load(Ordering::SeqCst) {
        return Err(CommandError::Busy);
    }
    quicker_platform::start_recording(&store.lock().engine).map_err(CommandError::internal)
}
#[tauri::command]
pub fn stop_recording(store: State<Managed>) -> Result<ActionPlan, CommandError> {
    let engine = store.lock().engine.clone();
    // Windows stops and takes the recording in one critical section, so no
    // hook event lands between the two (M2).
    #[cfg(windows)]
    return quicker_platform::finish_and_take(&engine).map_err(CommandError::internal);
    #[cfg(not(windows))]
    quicker_platform::stop_recording(&engine).map_err(CommandError::internal)
}
