//! Platform-agnostic ports (§7.1, B-48). 0.4 keeps them in the platform
//! crate; 0.5 moves them to `crates/engine/src/ports.rs` (B-49).
use crate::health::EngineEvent;
use crate::Foreground;
use std::time::{Duration, Instant};

/// Engine → shell event channel. Implementations must not block: the
/// supervisor and executor call this from their own threads.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: EngineEvent);
}

/// Discards every event (default before the shell attaches its EventBus).
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: EngineEvent) {}
}

/// Monotonic time source, so backoff, timeouts and log-level TTLs can be
/// driven by a fake clock in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    /// Milliseconds since the Unix epoch (for timestamps shown to users).
    fn wall_ms(&self) -> i64;
    fn sleep(&self, duration: Duration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn wall_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration)
    }
}

/// Output port (S-23). Each method submits synthesized input and returns the
/// number of events the OS accepted, so callers can fail-stop when it differs
/// from the request (no-regression #6). Key codes are Windows virtual-key
/// codes and mouse arguments follow Win32 `MOUSEINPUT`; 0.5 (B-49) maps them
/// per platform.
pub trait Injector: Send + Sync {
    /// Types `text` as Unicode: requests two events per UTF-16 unit.
    fn send_text(&self, text: &str) -> u32;
    /// Taps each key in order: requests two events per key.
    fn send_keys(&self, keys: &[u16]) -> u32;
    /// Presses `keys` as one chord, releasing in reverse: two events per key.
    fn send_chord(&self, keys: &[u16]) -> u32;
    /// One key transition: requests one event.
    fn send_key(&self, key: u16, up: bool) -> u32;
    /// One mouse event: requests one event.
    fn send_mouse(&self, flags: u32, data: u32, x: i32, y: i32) -> u32;
}

/// Clipboard port for one lease (no-regression #1/#2): enough to back up all
/// formats, write a pre-staged payload, and re-verify sequence, owner and
/// content before restoring. Methods documented as "locked" are only valid
/// inside [`ClipboardBackend::locked`]. Not `Send`: the Windows owner window
/// is thread-affine.
pub trait ClipboardBackend {
    /// Payload allocated before any destructive step, handed over by `set`.
    type Staged;
    /// Owner identity the clipboard reports after this backend writes.
    fn identity(&self) -> u64;
    /// Current clipboard owner identity.
    fn owner(&self) -> u64;
    /// Current clipboard sequence number.
    fn sequence(&self) -> u32;
    /// Opens the clipboard as `identity`, runs `f`, closes it. `None` when
    /// the clipboard is busy.
    fn locked<R>(&self, f: impl FnOnce() -> R) -> Option<R>;
    /// Locked: formats currently on the clipboard, in enumeration order.
    fn formats(&self) -> Vec<u32>;
    /// Locked: bytes of `format`; `Ok(None)` when absent, empty or larger
    /// than `limit`; `Err` when present but unreadable.
    fn read(&self, format: u32, limit: usize) -> Result<Option<Vec<u8>>, String>;
    /// Locked: empties the clipboard and takes ownership; `false` on failure.
    fn clear(&self) -> bool;
    /// Allocates `bytes` for a later `set`; also returns the bytes exactly as
    /// the clipboard will report them (allocation may round up).
    fn stage(&self, bytes: &[u8]) -> Result<(Self::Staged, Vec<u8>), String>;
    /// Locked: hands `staged` to the clipboard (consumed even on failure).
    fn set(&self, format: u32, staged: Self::Staged) -> bool;
}

/// Input context a match is bound to (no-regression #3). Together with the
/// configuration revision it forms the transaction stamp that is captured
/// with each typed key and re-checked before every destructive step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputContext {
    /// Focused control handle.
    pub focus: u64,
    /// Advances whenever focus moves.
    pub focus_epoch: u64,
    /// Advances whenever the caret moves.
    pub caret_epoch: u64,
    /// Keyboard layout of the focused thread.
    pub layout: u64,
}

/// Foreground/target port (S-23).
pub trait ForegroundProbe: Send + Sync {
    /// Snapshot of the focused window and its process.
    fn foreground(&self) -> Foreground;
    /// Veto reason when the focused control must not receive input
    /// (password, read-only or disabled; no-regression #5).
    fn protection(&self) -> Option<String>;
    /// Current input context for the stamp guard; `None` when it cannot be
    /// read, which the guard treats as a change (fail closed).
    fn input_context(&self) -> Option<InputContext>;
    /// Whether two snapshots name the same target window and process.
    fn same_window(&self, a: &Foreground, b: &Foreground) -> bool {
        a.id == b.id && a.process.eq_ignore_ascii_case(&b.process)
    }
}
