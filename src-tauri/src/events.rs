//! Backend → frontend events. The EventBus with the global `seq` counter
//! (B-44, docs/ipc-contract-0.4.md §3) lands here; for now only the legacy
//! `engine-status` event exists.
use crate::app_state::AppState;
use tauri::{AppHandle, Emitter, Runtime};

/// Legacy `engine-status` event (kept as an alias through 0.5; new code
/// subscribes to `engine-state`).
pub fn emit_engine_status<R: Runtime>(app: &AppHandle<R>, state: AppState) {
    let _ = app.emit("engine-status", state);
}
