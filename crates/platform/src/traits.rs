//! Platform-agnostic ports (§7.1, B-48). 0.4 keeps them in the platform
//! crate; 0.5 moves them to `crates/engine/src/ports.rs` (B-49).
use crate::health::EngineEvent;
use crate::Foreground;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
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

/// Read-only clipboard access (B-08): everything a lease's backup needs, and
/// nothing that writes, so the backup can run on a worker thread that the
/// lease may abandon after a timeout. Methods documented as "locked" are only
/// valid inside [`ClipboardRead::locked`], on the thread that called it.
pub trait ClipboardRead {
    /// Current clipboard owner identity.
    fn owner(&self) -> u64;
    /// Current clipboard sequence number.
    fn sequence(&self) -> u32;
    /// Opens the clipboard, runs `f`, closes it (also when `f` unwinds).
    /// `None` when the clipboard is busy. A [`ClipboardBackend`] opens it as
    /// its `identity`.
    fn locked<R>(&self, f: impl FnOnce() -> R) -> Option<R>;
    /// Locked: formats currently on the clipboard, in enumeration order.
    fn formats(&self) -> Vec<u32>;
    /// Locked: reads `format` for a backup. Handle formats the adapter can
    /// serialize report their bytes (`CF_ENHMETAFILE`: the metafile bits).
    /// May block while the owner renders a delay-rendered format.
    fn read_format(&self, format: u32, limit: usize) -> FormatRead;
    /// Monotonic time, used to measure how long the clipboard stays open.
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Clipboard port for one lease (no-regression #1/#2): enough to back up all
/// formats, write a pre-staged payload, and re-verify sequence, owner and
/// content before restoring. Not `Send`: the Windows owner window is
/// thread-affine. Reads of another owner's data go through
/// [`ClipboardBackend::reader`] on a worker thread.
pub trait ClipboardBackend: ClipboardRead {
    /// Payload allocated before any destructive step, handed over by `set`.
    type Staged;
    /// Read-only access for the backup worker thread.
    type Reader: ClipboardRead + Send + 'static;
    /// Owner identity the clipboard reports after this backend writes.
    fn identity(&self) -> u64;
    /// A reader for the backup worker thread. It opens the clipboard on that
    /// thread, so the worker opens, reads and closes it there.
    fn reader(&self) -> Self::Reader;
    /// Locked: bytes of `format`; `Ok(None)` when absent, empty or larger
    /// than `limit`; `Err` when present but unreadable.
    fn read(&self, format: u32, limit: usize) -> Result<Option<Vec<u8>>, String> {
        match self.read_format(format, limit) {
            FormatRead::Data(bytes) => Ok(Some(bytes)),
            FormatRead::Missing | FormatRead::TooLarge => Ok(None),
            FormatRead::Unreadable => Err("Clipboard data lock failed".into()),
        }
    }
    /// Locked: empties the clipboard and takes ownership; `false` on failure.
    fn clear(&self) -> bool;
    /// Allocates `bytes` for a later `set`; also returns the bytes exactly as
    /// the clipboard will report them (allocation may round up).
    fn stage(&self, bytes: &[u8]) -> Result<(Self::Staged, Vec<u8>), String>;
    /// Locked: hands `staged` to the clipboard (consumed even on failure).
    fn set(&self, format: u32, staged: Self::Staged) -> bool;
    /// Waits `duration` between attempts. Never called inside
    /// [`ClipboardRead::locked`]: the clipboard stays closed while waiting.
    fn pause(&self, duration: Duration) {
        std::thread::sleep(duration)
    }
    /// Waits at most `timeout` for the backup worker's answer on `rx`.
    fn wait_for<T>(&self, rx: &Receiver<T>, timeout: Duration) -> Result<T, RecvTimeoutError> {
        rx.recv_timeout(timeout)
    }
    /// Publishes the clipboard component's health (B-08). Called outside
    /// [`ClipboardRead::locked`]; the default discards it.
    fn report_health(
        &self,
        _state: crate::health::HealthState,
        _reason: Option<crate::health::ReasonCode>,
    ) {
    }
    /// Like [`ClipboardBackend::stage`], allocating the handle type `format`
    /// needs (an `HENHMETAFILE` for `CF_ENHMETAFILE`).
    fn stage_format(&self, format: u32, bytes: &[u8]) -> Result<(Self::Staged, Vec<u8>), String> {
        let _ = format;
        self.stage(bytes)
    }
    /// Not locked (it may wait): whether clipboard owner `owner` stopped
    /// responding, so reading a delay-rendered format would block on it.
    fn owner_hung(&self, _owner: u64) -> bool {
        false
    }
    /// Not locked: the registered format id for `name`; 0 when it cannot be
    /// registered (the default).
    fn register_format(&self, _name: &str) -> u32 {
        0
    }
}

/// One format read for a clipboard backup (B-08).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatRead {
    Data(Vec<u8>),
    /// Enumerated but no data (rendering failed or empty).
    Missing,
    /// Larger than the limit; not copied.
    TooLarge,
    /// Present but its data cannot be read.
    Unreadable,
}

/// What a clipboard lease does with formats it cannot back up (B-08). Under
/// every policy a format that fails to read for any other reason, a hung
/// owner, or a backup that cannot finish within the lock budget refuses the
/// action before the trigger is erased.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorePolicy {
    /// Refuse the action (trigger preserved) unless every format that is
    /// not re-synthesized by the system can be backed up.
    Full,
    /// Skip and record only what is known not to be restorable: formats
    /// whose data is a handle only the owner can copy, and formats over the
    /// size limit. Formats the system re-synthesizes (CF_BITMAP,
    /// CF_METAFILEPICT) are dropped only when their source is backed up.
    /// Refuses when that would leave a non-empty clipboard empty after the
    /// restore.
    #[default]
    BestEffort,
    /// Back up only text formats (CF_UNICODETEXT, CF_TEXT, CF_OEMTEXT,
    /// CF_LOCALE); other formats are dropped without being recorded, even
    /// when nothing else is left. A text format over the size limit is
    /// recorded.
    TextOnly,
}

/// Default wait between the paste chord and restoring the clipboard
/// (espanso v2.4.1 uses the same; 0.3 waited a fixed 150 ms).
pub const DEFAULT_RESTORE_DELAY: Duration = Duration::from_millis(300);

/// Clipboard lease configuration. Leases use the app's options
/// (`crate::clipboard_options`, set at startup from prefs); `Default` until
/// then: BestEffort and [`DEFAULT_RESTORE_DELAY`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseOptions {
    pub policy: RestorePolicy,
    /// Wait after the paste chord before restoring the previous clipboard.
    pub restore_delay: Duration,
}

impl Default for LeaseOptions {
    fn default() -> Self {
        Self {
            policy: RestorePolicy::default(),
            restore_delay: DEFAULT_RESTORE_DELAY,
        }
    }
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
