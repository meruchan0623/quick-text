//! Tauri-managed state and the `load_state` snapshot sent to the frontend.
use crate::{autostart, error::SafeModeReason, events, startup::StartupProblem, store::Store};
use quicker_domain::Config;
use quicker_platform::{
    health::{EngineState, HealthSnapshot},
    Capability, ExecutionResult,
};
use serde::Serialize;
use std::path::PathBuf;

/// The store lock is `parking_lot` so a panic while it is held cannot poison
/// it for every later command (B-01).
pub type Managed = parking_lot::Mutex<Store>;

/// The resolved data directory, for the diagnostics commands that read or
/// open it without the store lock.
pub struct DataDir(pub PathBuf);

/// The one capability table (contract §5): `load_state.capabilities`,
/// `get_capabilities` and the diagnostics bundle's `health.json` all come
/// from here, lowered by the components of `health` (the runtime's current
/// snapshot, as the supervisor and the executor probe report them).
pub fn capabilities(health: &HealthSnapshot) -> Vec<Capability> {
    quicker_platform::capabilities_for(&health.components)
}

/// `std::env::consts::OS`: `windows`, `macos`, or the name of a platform
/// with no input adapter (e.g. `linux`).
pub const PLATFORM: &str = std::env::consts::OS;

#[derive(Clone, Serialize)]
pub struct AppState {
    active: Config,
    revision: u64,
    paused: bool,
    engine_status: String,
    last_result: Option<ExecutionResult>,
    capabilities: Vec<Capability>,
    autostart: bool,
    /// `prefs.json` `app.auto_listen_on_start` (B-10); false when prefs
    /// cannot be read. Written by `set_auto_listen`.
    auto_listen_on_start: bool,
    engine_detail: serde_json::Value,
    platform: String,
    startup_problems: Vec<StartupProblem>,
    safe_mode: Option<SafeModeReason>,
    health: HealthSnapshot,
    engine_state: EngineState,
}
impl Store {
    pub fn state(&self) -> AppState {
        // Derived from the runtime, the pause authority: paused is "not
        // listening", whatever holds it (a reason or the engine not up).
        // Live values, not the EventBus's last emitted ones: `load_state`
        // has no `seq` (`get_runtime_snapshot` has).
        let engine = self.runtime.engine_state();
        let health = self.runtime.health();
        AppState {
            active: self.active.clone(),
            revision: self.active.revision,
            paused: !engine.listening,
            // The same mapping as the legacy `engine-status` event.
            engine_status: events::legacy_status(&engine).into(),
            last_result: self.engine.last.lock().unwrap().clone(),
            capabilities: capabilities(&health),
            autostart: autostart::is_enabled(),
            auto_listen_on_start: crate::store::prefs::auto_listen_on_start(self.dir()),
            engine_detail: quicker_platform::input_status(),
            platform: PLATFORM.into(),
            startup_problems: self.startup_problems().to_vec(),
            safe_mode: self.safe_mode(),
            health,
            engine_state: engine,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quicker_platform::health::{Component, ComponentHealth, HealthState, ReasonCode};

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("quicker-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `load_state` reports the runtime's live Health and State, and its
    /// capabilities are lowered by that same Health.
    #[test]
    fn load_state_carries_health_engine_state_and_lowered_capabilities() {
        let dir = temp_dir();
        let store = Store::load(dir.clone());
        let health = HealthSnapshot {
            state: HealthState::Degraded,
            components: vec![ComponentHealth {
                component: Component::Clipboard,
                state: HealthState::Degraded,
                reason: Some(ReasonCode::ClipboardBusy),
                since_ms: 7,
            }],
        };
        store.runtime.publish_health(health.clone());
        let state = serde_json::to_value(store.state()).unwrap();
        assert_eq!(state["health"], serde_json::to_value(&health).unwrap());
        assert_eq!(
            state["engine_state"],
            serde_json::to_value(store.runtime.engine_state()).unwrap()
        );
        assert!(state["engine_state"].get("seq").is_none());
        assert_eq!(
            state["capabilities"],
            serde_json::to_value(quicker_platform::capabilities_for(&health.components)).unwrap()
        );
        assert_eq!(state["platform"], std::env::consts::OS);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `load_state.auto_listen_on_start` follows prefs.json (contract §4).
    #[test]
    fn load_state_reports_auto_listen_on_start_from_prefs() {
        let dir = temp_dir();
        let store = Store::load(dir.clone());
        assert_eq!(
            serde_json::to_value(store.state()).unwrap()["auto_listen_on_start"],
            true
        );
        crate::store::prefs::update_prefs(&dir, |prefs| {
            prefs.app.auto_listen_on_start = false;
            true
        })
        .unwrap();
        assert_eq!(
            serde_json::to_value(store.state()).unwrap()["auto_listen_on_start"],
            false
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
