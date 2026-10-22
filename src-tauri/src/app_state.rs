//! Tauri-managed state and the `load_state` snapshot sent to the frontend.
use crate::{autostart, store::Store};
use quicker_domain::Config;
use quicker_platform::ExecutionResult;
use serde::Serialize;
use std::sync::atomic::Ordering;

/// The store lock is `parking_lot` so a panic while it is held cannot poison
/// it for every later command (B-01).
pub type Managed = parking_lot::Mutex<Store>;

#[derive(Clone, Serialize)]
pub struct AppState {
    draft: Config,
    active: Config,
    revision: u64,
    dirty: bool,
    paused: bool,
    engine_status: String,
    last_result: Option<ExecutionResult>,
    capabilities: Vec<quicker_platform::Capability>,
    autostart: bool,
    engine_detail: serde_json::Value,
    platform: String,
}
impl Store {
    pub fn state(&self) -> AppState {
        AppState {
            draft: self.draft.clone(),
            active: self.active.clone(),
            revision: self.active.revision,
            dirty: self.draft != self.active,
            paused: self.engine.paused.load(Ordering::SeqCst),
            engine_status: if self.engine.busy.load(Ordering::SeqCst) {
                "running"
            } else if self.engine.paused.load(Ordering::SeqCst) {
                "paused"
            } else {
                "listening"
            }
            .into(),
            last_result: self.engine.last.lock().unwrap().clone(),
            capabilities: quicker_platform::capabilities(),
            autostart: autostart::is_enabled(),
            engine_detail: quicker_platform::input_status(),
            platform: if cfg!(target_os = "macos") {
                "macos"
            } else {
                "windows"
            }
            .into(),
        }
    }
}
