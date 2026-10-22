//! Engine runtime: state snapshot, pause, manual execution and recording.
use crate::{
    app_state::{AppState, Managed},
    error::diagnostics,
};
use quicker_domain::*;
use quicker_platform::ExecutionResult;
use std::sync::atomic::Ordering;
use tauri::{Manager, State};

#[tauri::command]
pub fn load_state(store: State<Managed>) -> Result<AppState, String> {
    Ok(store.lock().map_err(|_| "Store lock")?.state())
}
#[tauri::command]
pub fn set_paused(paused: bool, store: State<Managed>) -> Result<AppState, String> {
    let s = store.lock().map_err(|_| "Store lock")?;
    if !paused {
        if s.engine.recording.lock().unwrap().is_some() {
            return Err("Stop recording before resuming automatic matching".into());
        }
        quicker_platform::native_ready()?;
    }
    if !paused && !quicker_platform::engine_ready() {
        return Err("This platform input adapter is not ready; engine remains paused".into());
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
    let s = store.lock().unwrap();
    s.engine.cancel.store(true, Ordering::SeqCst);
    true
}
#[tauri::command]
pub async fn execute_action(
    action: Action,
    app: tauri::AppHandle,
) -> Result<ExecutionResult, String> {
    let (engine, config, action, speed) = {
        let state = app.state::<Managed>();
        let s = state.lock().unwrap();
        if s.engine.busy.swap(true, Ordering::SeqCst) {
            return Err("An action is already running".into());
        }
        s.engine.cancel.store(false, Ordering::SeqCst);
        (
            s.engine.clone(),
            s.active.clone(),
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
            return Err(e.to_string());
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
                && serde_json::to_string(&config)
                    .unwrap_or_default()
                    .contains("{selection}");
        if needs_selection {
            context.selection = quicker_platform::capture_selection().ok();
        }
        let plan = expand_reusable(&action, &config, &context).map_err(diagnostics)?;
        quicker_platform::execute(e, plan, target, generation, 0, None, speed)
    })
    .await;
    engine.busy.store(false, Ordering::SeqCst);
    if let Some(w) = window {
        let _ = w.show();
    }
    result.map_err(|e| e.to_string())?
}
#[tauri::command]
pub fn start_recording(store: State<Managed>) -> Result<(), String> {
    if store.lock().unwrap().engine.busy.load(Ordering::SeqCst) {
        return Err("Stop the current action before starting recording".into());
    }
    quicker_platform::start_recording(&store.lock().unwrap().engine)
}
#[tauri::command]
pub fn stop_recording(store: State<Managed>) -> Result<ActionPlan, String> {
    #[cfg(windows)]
    quicker_platform::finish_recording(&store.lock().unwrap().engine);
    quicker_platform::stop_recording(&store.lock().unwrap().engine)
}
