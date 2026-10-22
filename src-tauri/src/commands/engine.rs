//! Engine health, capabilities and execution history (contract §5, 0.4).
//!
//! Every command takes the store lock only to clone the runtime handle (and
//! read the revision and safe mode); nothing here blocks on the engine
//! (`self_test` waits for its checks on a blocking worker).
use crate::{
    app_state::{capabilities, Managed},
    error::{CommandError, SafeModeReason},
    events::{BusView, EventBus},
};
use quicker_platform::{
    health::{EngineState, HealthSnapshot, HistoryEntry},
    history::MissExplanation,
    runtime::Runtime,
    self_test::SelfTestReport,
    Capability,
};
use serde::Serialize;
use std::sync::Arc;
use tauri::{Manager, State};

/// `get_runtime_snapshot`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RuntimeSnapshot {
    /// The global event `seq` that `health` and `state` were last sent with
    /// (0 before the first event).
    pub seq: u64,
    pub health: HealthSnapshot,
    pub state: EngineState,
    pub revision: u64,
    pub history_seq: u64,
    pub safe_mode: Option<SafeModeReason>,
}

/// Builds the snapshot from the EventBus's view and, read after it, the
/// rest. The order is what makes the snapshot a safe baseline: `seq`,
/// `health` and `state` are one consistent copy of the emitted stream, and
/// anything read later (the revision, safe mode, `history_seq`) is at least
/// as new as it. A change the view does not contain is sent afterwards with
/// a larger `seq`; in particular every history entry above `history_seq` was
/// recorded after the view was taken, so its `execution-result` (or the
/// `suppressed` count of one) has a `seq` above the snapshot's.
fn runtime_snapshot(view: BusView, store: &Managed) -> RuntimeSnapshot {
    let (runtime, revision, safe_mode) = {
        let store = store.lock();
        (
            store.runtime.clone(),
            store.active.revision,
            store.safe_mode(),
        )
    };
    RuntimeSnapshot {
        seq: view.seq,
        health: view.health,
        state: view.state,
        revision,
        history_seq: runtime.history_seq(),
        safe_mode,
    }
}

fn runtime(store: &Managed) -> Arc<Runtime> {
    store.lock().runtime.clone()
}

/// The baseline for event `seq` bookkeeping (contract §3): Health and State
/// as last emitted, not the live values (those still being merged arrive as
/// events with a larger `seq`).
#[tauri::command(rename_all = "snake_case")]
pub fn get_runtime_snapshot(
    store: State<Managed>,
    bus: State<EventBus>,
) -> Result<RuntimeSnapshot, CommandError> {
    Ok(runtime_snapshot(bus.view(), &store))
}

/// The runtime's live Health.
#[tauri::command(rename_all = "snake_case")]
pub fn get_health(store: State<Managed>) -> Result<HealthSnapshot, CommandError> {
    Ok(runtime(&store).health())
}

/// Asks the supervisor for a forced rebuild and returns the current Health
/// at once; Recovering and the outcome follow as `engine-health` events.
/// Allowed in safe mode: a rebuild resumes nothing (SAFE_MODE and MANUAL
/// still hold the engine paused).
#[tauri::command(rename_all = "snake_case")]
pub fn repair_engine() -> Result<HealthSnapshot, CommandError> {
    Ok(quicker_platform::supervisor::repair())
}

/// The capability table lowered by the live Health (the same source as
/// `load_state.capabilities` and the diagnostics bundle).
#[tauri::command(rename_all = "snake_case")]
pub fn get_capabilities(store: State<Managed>) -> Result<Vec<Capability>, CommandError> {
    Ok(capabilities(&runtime(&store).health()))
}

/// History entries with `seq > since_seq`, oldest first (all when `null`).
#[tauri::command(rename_all = "snake_case")]
pub fn list_history(
    store: State<Managed>,
    since_seq: Option<u64>,
) -> Result<Vec<HistoryEntry>, CommandError> {
    Ok(runtime(&store).list_history(since_seq))
}

/// Empties the in-memory execution history. Allowed in safe mode: it
/// writes no data or settings. History `seq` keeps counting.
#[tauri::command(rename_all = "snake_case")]
pub fn clear_history(store: State<Managed>) -> Result<(), CommandError> {
    runtime(&store).clear_history();
    tracing::info!(event = "history.cleared", "execution history cleared");
    Ok(())
}

/// The latest history entry that did not succeed (0.4: history only).
#[tauri::command(rename_all = "snake_case")]
pub fn explain_last_miss(store: State<Managed>) -> Result<Option<MissExplanation>, CommandError> {
    Ok(runtime(&store).explain_last_miss())
}

/// Runs the four platform checks (S-09, contract §5 `self_test`) on a
/// blocking worker, never under the store lock. Detection pauses through
/// `SelfTest` while it runs; it resumes nothing and writes nothing, so it
/// is allowed in safe mode. Takes at most one check timeout (2.5 s), after
/// a run already in progress.
#[tauri::command(rename_all = "snake_case")]
pub async fn self_test(app: tauri::AppHandle) -> Result<SelfTestReport, CommandError> {
    let runtime = runtime(&app.state::<Managed>());
    tauri::async_runtime::spawn_blocking(move || {
        quicker_platform::self_test::run_platform(&runtime)
    })
    .await
    .map_err(CommandError::internal)
}

/// Arms a one-shot fault (B-52). Exists only with the `fault-injection`
/// feature; release builds have neither the command nor the symbol.
#[cfg(feature = "fault-injection")]
#[tauri::command(rename_all = "snake_case")]
pub fn debug_inject_fault(
    point: quicker_platform::fault::FaultPoint,
    param: u64,
) -> Result<(), CommandError> {
    tracing::warn!(event = "fault.injected", ?point, param, "fault armed");
    quicker_platform::fault::debug_inject_fault(point, param);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        events::{Observer, Sink},
        store::Store,
    };
    use quicker_platform::{
        health::{HealthState, Outcome, PauseReasons, ReasonCode, TriggerKind},
        history::Record,
    };
    use serde_json::Value;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    struct Channel(mpsc::Sender<(&'static str, Value)>);
    impl Sink for Channel {
        fn emit(&self, name: &'static str, payload: Value) {
            let _ = self.0.send((name, payload));
        }
    }
    struct Quiet;
    impl Observer for Quiet {
        fn status(&mut self, _: &HealthSnapshot, _: &EngineState) {}
        fn executed(&mut self, _: &HistoryEntry) {}
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("quicker-engine-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Waits for the next event that carries a `seq`.
    fn next_seq(events: &mpsc::Receiver<(&'static str, Value)>) -> (&'static str, Value) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (name, payload) = events.recv_timeout(left).expect("event");
            if payload.get("seq").is_some() {
                return (name, payload);
            }
        }
    }

    /// The snapshot is the emitted stream at one point plus what was read
    /// after it: its `seq` is the last one sent, its Health and State are
    /// the ones sent with it, and `history_seq` covers every entry whose
    /// `execution-result` is at or below that `seq`.
    #[test]
    fn the_snapshot_is_a_baseline_for_the_event_stream() {
        let dir = temp_dir();
        let store = Store::load(dir.clone());
        let runtime = store.runtime.clone();
        let managed = Managed::new(store);
        let (tx, events) = mpsc::channel();
        let bus = EventBus::start(&runtime, Channel(tx), Box::new(Quiet));

        let before = runtime_snapshot(bus.view(), &managed);
        assert_eq!(before.seq, 0);
        assert_eq!(before.health, runtime.health());
        assert_eq!(before.history_seq, 0);
        assert_eq!(before.revision, managed.lock().active.revision);
        assert_eq!(before.safe_mode, None);

        let healthy = HealthSnapshot {
            state: HealthState::Healthy,
            components: vec![],
        };
        runtime.publish_health(healthy.clone());
        assert_eq!(next_seq(&events).0, "engine-health");
        runtime.set_pause(PauseReasons::MANUAL, false);
        let (name, state) = next_seq(&events);
        assert_eq!(name, "engine-state");
        assert_eq!(state["listening"], true);
        let entry = runtime.record(Record::new(
            "rule",
            TriggerKind::Immediate,
            Outcome::Failed,
            ReasonCode::FocusChanged,
        ));
        let (name, execution) = next_seq(&events);
        assert_eq!(name, "execution-result");

        let after = runtime_snapshot(bus.view(), &managed);
        assert_eq!(after.seq, execution["seq"].as_u64().unwrap());
        assert_eq!(after.health, healthy);
        let mut sent = state;
        sent.as_object_mut().unwrap().remove("seq");
        assert_eq!(serde_json::to_value(&after.state).unwrap(), sent);
        assert!(after.state.listening);
        assert!(after.history_seq >= entry.seq);
        drop(bus);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
