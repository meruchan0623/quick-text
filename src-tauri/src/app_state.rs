//! Tauri-managed state and the `load_state` snapshot sent to the frontend.
use crate::{autostart, error::SafeModeReason, events, startup::StartupProblem, store::Store};
use quicker_domain::Config;
use quicker_platform::ExecutionResult;
use serde::Serialize;
use std::path::PathBuf;

/// The store lock is `parking_lot` so a panic while it is held cannot poison
/// it for every later command (B-01).
pub type Managed = parking_lot::Mutex<Store>;

/// The resolved data directory, for the diagnostics commands that read or
/// open it without the store lock.
pub struct DataDir(pub PathBuf);

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
    startup_problems: Vec<StartupProblem>,
    safe_mode: Option<SafeModeReason>,
}
impl Store {
    pub fn state(&self) -> AppState {
        // Derived from the runtime, the pause authority: paused is "not
        // listening", whatever holds it (a reason or the engine not up).
        let engine = self.runtime.engine_state();
        AppState {
            draft: self.draft.clone(),
            active: self.active.clone(),
            revision: self.active.revision,
            dirty: self.draft != self.active,
            paused: !engine.listening,
            // The same mapping as the legacy `engine-status` event.
            engine_status: events::legacy_status(&engine).into(),
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
            startup_problems: self.startup_problems().to_vec(),
            safe_mode: self.safe_mode(),
        }
    }
}
