//! Native input runs on one hook path. Hooks never perform file I/O or execute actions.
use quicker_domain::*;
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

fn acceptance_input_policy(debug: bool, environment: Option<&str>, explicit_flag: bool) -> bool {
    environment == Some("1") && (debug || explicit_flag)
}

/// Explicit startup opt-in for Windows automation acceptance with external SendInput.
/// Cached once so data isolation and the keyboard hook use the same startup mode.
pub fn acceptance_test_input_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        cfg!(windows)
            && acceptance_input_policy(
                cfg!(debug_assertions),
                std::env::var("QUICKER_TEXT_TEST_INPUT").ok().as_deref(),
                std::env::args_os().any(|arg| arg == "--acceptance-test-input"),
            )
    })
}

#[cfg(test)]
mod acceptance_mode_tests {
    use super::acceptance_input_policy;

    #[test]
    fn release_requires_both_startup_gates() {
        for environment in [None, Some("0"), Some("true"), Some(""), Some("1 ")] {
            assert!(!acceptance_input_policy(false, environment, false));
            assert!(!acceptance_input_policy(false, environment, true));
        }
        assert!(!acceptance_input_policy(false, Some("1"), false));
        assert!(acceptance_input_policy(false, Some("1"), true));
    }

    #[test]
    fn debug_preserves_environment_opt_in() {
        assert!(!acceptance_input_policy(true, None, false));
        assert!(!acceptance_input_policy(true, None, true));
        assert!(acceptance_input_policy(true, Some("1"), false));
        assert!(acceptance_input_policy(true, Some("1"), true));
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Foreground {
    pub platform: String,
    pub bundle_id: String,
    pub id: u64,
    pub process: String,
    pub title: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExecutionResult {
    pub id: String,
    pub status: String,
    pub message: String,
    pub completed_steps: usize,
    pub side_effects: bool,
}
pub struct Shared {
    pub config: Mutex<Config>,
    pub compiled: Mutex<CompiledConfig>,
    pub paused: AtomicBool,
    pub generation: AtomicU64,
    pub cancel: AtomicBool,
    pub busy: AtomicBool,
    pub last: Mutex<Option<ExecutionResult>>,
    pub recording: Mutex<Option<Vec<Step>>>,
    pub shutdown: AtomicBool,
}
impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            config: Mutex::new(Config::default()),
            compiled: Mutex::new(compile_config(&Config::default()).unwrap()),
            paused: AtomicBool::new(true),
            generation: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            last: Mutex::new(None),
            recording: Mutex::new(None),
            shutdown: AtomicBool::new(false),
        })
    }
}
pub mod fault;
pub mod health;
#[cfg(any(test, feature = "mock"))]
pub mod mock;
pub mod traits;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported;
#[cfg(not(any(windows, target_os = "macos")))]
pub use unsupported::*;
/// The Windows executor's public API where there is no such executor.
#[cfg(not(windows))]
mod no_executor {
    use super::*;
    /// A manual run's claim on [`Shared::busy`] (`src-tauri`'s
    /// `execute_action`). There is no executor lifecycle lock here: the
    /// claim is a bare compare-and-set and dropping it a compare-and-clear,
    /// the atomics the manual run used before the Windows executor owned
    /// `busy`.
    #[must_use = "dropping the claim frees `busy` at once"]
    pub struct ManualBusy {
        shared: Arc<Shared>,
    }
    impl ManualBusy {
        /// Claims `busy` for a manual run; `None` while another run holds it.
        pub fn claim(shared: &Arc<Shared>) -> Option<Self> {
            shared
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .ok()?;
            Some(Self {
                shared: Arc::clone(shared),
            })
        }
    }
    impl Drop for ManualBusy {
        fn drop(&mut self) {
            let _ =
                self.shared
                    .busy
                    .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst);
        }
    }
    /// The executor as the supervisor drives it, for `Supervisor::new`:
    /// always `None`, there is no executor service on this platform.
    pub fn executor_control() -> Option<Arc<dyn supervisor::ExecutorControl>> {
        None
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn there_is_no_executor_and_manual_runs_claim_busy_directly() {
            assert!(executor_control().is_none());
            let shared = Shared::new();
            let claim = ManualBusy::claim(&shared).expect("busy is free");
            assert!(shared.busy.load(Ordering::SeqCst));
            assert!(ManualBusy::claim(&shared).is_none(), "one claim at a time");
            drop(claim);
            assert!(!shared.busy.load(Ordering::SeqCst));
        }
    }
}
#[cfg(not(windows))]
pub use no_executor::*;

mod capabilities;
pub use capabilities::{
    capabilities, capabilities_for, Capability, CapabilityReason, CapabilityStatus, PlatformReason,
};
/// Starts a recording session. Pausing detection for its duration is the
/// caller's job, through the runtime's `PauseReasons::RECORDING` (the shell
/// sets it before this call and clears it once no recording is active), so
/// `Shared::paused` stays a value derived only by the runtime.
pub fn start_recording(shared: &Arc<Shared>) -> Result<(), String> {
    // Recording needs a running engine (B-03: `accepts_resume` replaces
    // `native_ready`).
    #[cfg(any(windows, target_os = "macos"))]
    if !runtime::accepts_resume() {
        return Err("The input engine is not running; recording is unavailable".into());
    }
    if shared.busy.load(Ordering::SeqCst) {
        return Err("Wait for the running action to stop before recording".into());
    }
    let mut r = shared.recording.lock().map_err(|_| "recording lock")?;
    if r.is_some() {
        return Err("Recording already active".into());
    }
    shared.cancel.store(false, Ordering::SeqCst);
    shared.generation.fetch_add(1, Ordering::SeqCst);
    // A new recording session, under the recording lock (M2).
    #[cfg(windows)]
    windows::recording_started();
    *r = Some(vec![]);
    Ok(())
}
pub fn stop_recording(shared: &Arc<Shared>) -> Result<ActionPlan, String> {
    let steps = shared
        .recording
        .lock()
        .map_err(|_| "recording lock")?
        .take()
        .ok_or("Recording is not active")?;
    Ok(ActionPlan {
        steps,
        dialect: Dialect::Replay,
        ..Default::default()
    })
}
pub mod history;
pub mod runtime;
pub mod supervisor;
pub mod supervisor_wiring;
