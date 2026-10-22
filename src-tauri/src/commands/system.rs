//! Process lifetime and OS-level probes (cursor, selection).
use crate::{app_state::Managed, error::CommandError};
use std::sync::atomic::Ordering;
use tauri::Manager;

#[tauri::command]
pub fn get_cursor() -> serde_json::Value {
    quicker_platform::cursor()
}
#[tauri::command]
pub async fn capture_selection(app: tauri::AppHandle) -> Result<serde_json::Value, CommandError> {
    let w = app.get_webview_window("main");
    if let Some(w) = &w {
        w.hide().map_err(CommandError::internal)?
    }
    let result = tauri::async_runtime::spawn_blocking(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        quicker_platform::capture_selection()
    })
    .await;
    if let Some(w) = w {
        let _ = w.show();
    }
    let text = result
        .map_err(CommandError::internal)?
        .map_err(CommandError::internal)?;
    Ok(
        serde_json::json!({"text":text,"status":"success","message":"Selection read through accessibility; clipboard unchanged"}),
    )
}
#[tauri::command]
pub fn exit_app(app: tauri::AppHandle) {
    let state = app.state::<Managed>();
    let s = state.lock();
    s.engine.cancel.store(true, Ordering::SeqCst);
    s.engine.paused.store(true, Ordering::SeqCst);
    app.exit(0)
}
