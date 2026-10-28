//! Windows backend. Split by responsibility (B-48); the public API is
//! re-exported from here so `quicker_platform::*` is unchanged.
use super::*;
use std::{
    collections::BTreeSet,
    mem::{size_of, zeroed},
    process::Command,
    sync::{mpsc, OnceLock},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{DataExchange::*, Memory::*, Threading::*},
    UI::{
        Input::{Ime::*, KeyboardAndMouse::*},
        Shell::*,
        WindowsAndMessaging::*,
    },
};
mod actions;
mod clipboard;
mod executor;
mod foreground;
mod hook;
mod ime;
mod inject;
mod input;
mod keystate;
mod liveness;
mod observer;
#[cfg(test)]
mod pipeline_tests;
#[cfg(test)]
mod pipeline_tests_input;
mod protection;
mod rawinput;
mod recording;
mod selection;
mod session;
#[cfg(test)]
mod trace_capture;
pub use actions::{execute, preflight};
pub use clipboard::clipboard_health;
pub(crate) use clipboard::read_clipboard_text;
pub use foreground::{cursor, foreground};
pub use input::{engine_ready, input_status, native_ready};
pub use input::{input_health, reattach};
pub use recording::finish_and_take;
pub use recording::finish_recording;
pub(crate) use recording::recording_started;
pub use selection::capture_selection;
pub use session::on_session_end;
// S-19: `start_executor` wires the busy-drop sink to the executor's history.
pub(crate) use rawinput::{set_dropped_busy_sink, DroppedBusy};
const MARK: usize = 0x51545854;
static LAST_GATE: OnceLock<Mutex<String>> = OnceLock::new();
fn gate(reason: &str) {
    if let Some(g) = LAST_GATE.get() {
        if let Ok(mut state) = g.try_lock() {
            *state = reason.into();
        }
    }
}
static ENGINE: OnceLock<Arc<Shared>> = OnceLock::new();
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub fn start_engine(shared: Arc<Shared>) -> Result<(), String> {
    LAST_GATE.set(Mutex::new("后台默认暂停".into())).ok();
    ENGINE
        .set(shared.clone())
        .map_err(|_| "Engine already started")?;
    keystate::seed_physical();
    executor::start_executor(shared.clone());
    input::start_input_sources(shared)
}
pub use executor::{executor_control, ManualBusy};
#[cfg(test)]
mod pipeline_tests_executor;
pub use input::try_input_health;
// S-09/B-10: the self-test's Windows probes (runner in `crate::self_test`).
mod self_test_probes;
pub(crate) use self_test_probes::WinSelfTest;
// F5-1: what the blocking thread of a manual run acts with, and its failure.
pub use executor::{ManualFailure, ManualTicket};
