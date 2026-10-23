//! Clipboard lease over the [`ClipboardBackend`] port: backup, verified
//! writes and verified restore, plus the Windows adapter.
use super::inject::mark_external_effect;
use super::*;
use crate::clipboard_options::lease_options;
use crate::fault::{self, FaultPoint};
use crate::health::{Component, ComponentHealth, HealthState, ReasonCode};
use crate::traits::{ClipboardBackend, ClipboardRead, FormatRead, LeaseOptions, RestorePolicy};
use std::cell::Cell;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{PoisonError, TryLockError};
use windows_sys::Win32::Graphics::Gdi::{
    DeleteEnhMetaFile, GetEnhMetaFileBits, SetEnhMetaFileBits,
};

/// Bounded `OpenClipboard` retry (B-08): clipboard managers, remote-desktop
/// clients and listeners open the clipboard briefly after every change.
pub(super) const OPEN_ATTEMPTS: usize = 10;
/// Pause between `OpenClipboard` attempts, always with the clipboard closed.
pub(super) const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(15);
/// Longest a lease holds the clipboard open: every other `OpenClipboard`
/// fails meanwhile. Enforced while reading another owner's data (the read
/// that crosses it stops the backup); reported after the fact for the
/// lease's own writes. Either way the component is degraded.
pub(super) const LOCK_HOLD_BUDGET: Duration = Duration::from_millis(250);
/// How long a lease waits for one backup worker section: the hold budget,
/// plus a margin for the read that crosses it and for scheduling. A section
/// still running then (a read blocked on an owner rendering a delay-rendered
/// format) is abandoned.
const READ_WAIT: Duration = LOCK_HOLD_BUDGET.saturating_add(Duration::from_millis(50));
/// Re-verification attempts while the clipboard sequence settles.
const VERIFY_ATTEMPTS: usize = 8;
/// Pause between ownership re-verification attempts (clipboard closed).
const VERIFY_RETRY_INTERVAL: Duration = Duration::from_millis(5);
/// Largest single format a lease backs up; larger ones follow the policy.
pub(super) const MAX_FORMAT_BYTES: usize = 32 * 1024 * 1024;
/// Owner changes between the hung-owner probe and the backup tolerated
/// before the lease gives up (each change is a new copy by the user).
const OWNER_PROBES: usize = 3;
/// How long the hung-owner probe waits for the owner's message loop.
/// 待实测: tune against slow-but-alive owners.
const OWNER_PROBE_TIMEOUT_MS: u32 = 200;
/// Registered formats every paste write adds as a DWORD 0 (B-08), so the
/// expansion stays out of clipboard monitors, Win+V history and cloud sync
/// ("Cloud Clipboard and Clipboard History Formats", learn.microsoft.com).
pub(super) const EXCLUSION_FORMATS: [&str; 3] = [
    "ExcludeClipboardContentFromMonitorProcessing",
    "CanIncludeInClipboardHistory",
    "CanUploadToCloudClipboard",
];

// Standard clipboard formats (windows-sys `System::Ole::CF_*`, as u32).
const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_OEMTEXT: u32 = 7;
const CF_DIB: u32 = 8;
const CF_PALETTE: u32 = 9;
const CF_UNICODETEXT: u32 = 13;
const CF_ENHMETAFILE: u32 = 14;
const CF_LOCALE: u32 = 16;
const CF_DIBV5: u32 = 17;

/// What a lease does with one format when backing up (B-08).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handling {
    /// Back up its bytes and restore them.
    Copy,
    /// Known not to be restorable (only its owner can copy the data):
    /// BestEffort skips it and records it in `skipped_formats`; Full refuses.
    Skip,
    /// Re-synthesized by the system from any of these formats once they are
    /// restored: dropped unrecorded when one of them is backed up, refused
    /// otherwise.
    Covered(&'static [u32]),
    /// Unknown handle type, so nothing says it can be dropped: refused.
    Unknown,
    /// Neither backed up nor recorded (TextOnly's non-text formats).
    Ignore,
}

/// Per-format backup decision. Handle types follow "Standard Clipboard
/// Formats" (learn.microsoft.com); values match windows-sys `CF_*`.
fn handling(format: u32, policy: RestorePolicy) -> Handling {
    if policy == RestorePolicy::TextOnly {
        return match format {
            CF_UNICODETEXT | CF_TEXT | CF_OEMTEXT | CF_LOCALE => Handling::Copy,
            _ => Handling::Ignore,
        };
    }
    match format {
        // HBITMAP: re-synthesized by the system from a restored
        // CF_DIB/CF_DIBV5 (it synthesizes those from it too, so they are
        // enumerated alongside it).
        CF_BITMAP => Handling::Covered(&[CF_DIB, CF_DIBV5]),
        // HGLOBAL to a METAFILEPICT holding an HMETAFILE that would need its
        // own copy; re-synthesized from a restored CF_ENHMETAFILE (and the
        // reverse). 待实测后定: copy the HMETAFILE so it survives alone.
        CF_METAFILEPICT => Handling::Covered(&[CF_ENHMETAFILE]),
        // HPALETTE, and owner-display formats whose data is drawn by the
        // owner (CF_OWNERDISPLAY, CF_DSP* = 0x80..=0x8F).
        CF_PALETTE | 0x80..=0x8F => Handling::Skip,
        // CF_PRIVATEFIRST..=CF_PRIVATELAST: handles of an owner-defined type
        // that the owner frees on WM_DESTROYCLIPBOARD; only it can copy them.
        0x200..=0x2FF => Handling::Skip,
        // CF_GDIOBJFIRST..=CF_GDIOBJLAST: application-defined GDI objects.
        0x300..=0x3FF => Handling::Skip,
        // HGLOBAL standard formats (CF_DIB, CF_DIBV5, CF_HDROP, text, ...)
        // and CF_ENHMETAFILE, which the adapter serializes to its bits.
        CF_TEXT | 4..=8 | 10..=17 => Handling::Copy,
        // Registered formats are HGLOBAL by convention.
        0xC000..=0xFFFF => Handling::Copy,
        // Outside every defined range.
        _ => Handling::Unknown,
    }
}

/// Formats the system lists by itself once a format is on the clipboard, so
/// a lease may find them after its own write without anyone else writing
/// (B-08). Source → formats, from the conversion table in "Synthesized
/// Clipboard Formats" (learn.microsoft.com, clipboard-formats), plus
/// CF_LOCALE for the text formats: "When you close the clipboard, if it
/// contains CF_TEXT data but no CF_LOCALE data, the system automatically sets
/// the CF_LOCALE format" (Standard Clipboard Formats), and CF_TEXT is
/// synthesized from either of the others (待实测: CF_LOCALE after writing
/// CF_UNICODETEXT alone). Formats a paste writes that nothing is synthesized
/// from: CF_HDROP and registered formats ("HTML Format", the markers).
///
/// Consulted only when the baseline is captured, right after the write:
/// conversion formats are listed as soon as their source is
/// ("EnumClipboardFormats" enumerates a format, "then enumerates the formats
/// to which it can be converted"), and only their data is rendered later, on
/// demand. Once the baseline exists, nothing may join it.
const SYNTHESIZED: [(u32, &[u32]); 8] = [
    (CF_BITMAP, &[CF_DIB, CF_DIBV5]),
    (CF_DIB, &[CF_BITMAP, CF_PALETTE, CF_DIBV5]),
    (CF_DIBV5, &[CF_BITMAP, CF_DIB, CF_PALETTE]),
    (CF_ENHMETAFILE, &[CF_METAFILEPICT]),
    (CF_METAFILEPICT, &[CF_ENHMETAFILE]),
    (CF_OEMTEXT, &[CF_TEXT, CF_UNICODETEXT, CF_LOCALE]),
    (CF_TEXT, &[CF_OEMTEXT, CF_UNICODETEXT, CF_LOCALE]),
    (CF_UNICODETEXT, &[CF_OEMTEXT, CF_TEXT, CF_LOCALE]),
];

/// Whether the system lists `format` by itself once `written` is on the
/// clipboard ([`SYNTHESIZED`]).
fn synthesized_from(format: u32, written: &[(u32, Vec<u8>)]) -> bool {
    SYNTHESIZED.iter().any(|(source, derived)| {
        derived.contains(&format) && written.iter().any(|(written, _)| written == source)
    })
}

/// Formats whose data is a GDI handle (HBITMAP, HPALETTE, HENHMETAFILE, and
/// the HMETAFILE inside a METAFILEPICT): a restore holds them to presence
/// only, not to bytes. Accepted residual risk: a malicious process of the
/// same user is outside the threat model, and normal programs never rewrite
/// a derived format in place without emptying the clipboard.
const HANDLE_FORMATS: [u32; 4] = [CF_BITMAP, CF_PALETTE, CF_ENHMETAFILE, CF_METAFILEPICT];

/// Whether the lease wrote `format` itself.
fn own(format: u32, written: &[(u32, Vec<u8>)]) -> bool {
    written.iter().any(|(written, _)| *written == format)
}

/// Baseline capture: every format on a clipboard the lease just wrote
/// `written` to is its own or one the system synthesizes from it.
fn formats_synthesized(formats: &[u32], written: &[(u32, Vec<u8>)]) -> bool {
    formats
        .iter()
        .all(|format| own(*format, written) || synthesized_from(*format, written))
}

/// After the capture: the format set is exactly `baseline`. Another
/// application can add a format without emptying the clipboard, which keeps
/// the owner and the lease's payload; any addition, even of a format the
/// system could synthesize, is an external change. `written`'s own formats
/// are left to the byte-for-byte check that follows (one missing is a
/// changed payload).
fn formats_match(formats: &[u32], baseline: &[u32], written: &[(u32, Vec<u8>)]) -> bool {
    formats.iter().all(|format| baseline.contains(format))
        && baseline
            .iter()
            .all(|format| own(*format, written) || formats.contains(format))
}

/// Process-wide clipboard component health, published by every lease.
static HEALTH: OnceLock<Mutex<ComponentHealth>> = OnceLock::new();
fn health_cell() -> &'static Mutex<ComponentHealth> {
    HEALTH.get_or_init(|| {
        Mutex::new(ComponentHealth {
            component: Component::Clipboard,
            state: HealthState::Healthy,
            reason: None,
            since_ms: chrono::Utc::now().timestamp_millis(),
        })
    })
}
fn record_health(state: HealthState, reason: Option<ReasonCode>) {
    let mut health = health_cell().lock().unwrap_or_else(PoisonError::into_inner);
    if health.state != state {
        health.since_ms = chrono::Utc::now().timestamp_millis();
    }
    health.state = state;
    health.reason = reason;
}
/// Clipboard component health for the supervisor (B-08): Degraded with
/// `ClipboardBusy` after a lease could not open the clipboard within the
/// bounded retry, held it open too long, gave up waiting for a backup read
/// or found the owner not responding; Healthy again after the next lease
/// whose critical sections all completed in time.
pub fn clipboard_health() -> ComponentHealth {
    health_cell()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}
/// [`clipboard_health`] without waiting, for the executor's `probe` (A2):
/// `None` while a lease is updating it, and before anything created it (no
/// lease has published a health yet; the executor then keeps its last
/// status, Ok at first).
pub(super) fn try_clipboard_health() -> Option<ComponentHealth> {
    try_health(&HEALTH)
}
/// Only non-blocking steps: `get`, never `get_or_init`, which waits while
/// another thread is initializing the cell; then `try_lock`.
fn try_health(cell: &OnceLock<Mutex<ComponentHealth>>) -> Option<ComponentHealth> {
    match cell.get()?.try_lock() {
        Ok(health) => Some(health.clone()),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner().clone()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// End of the `FaultPoint::ClipboardBusy` window (B-52).
static FAULT_BUSY_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
/// Arms the busy window for `armed` milliseconds when the fault fired and
/// reports whether `now` falls inside it.
fn fault_busy_window(armed: Option<u64>, until: &mut Option<Instant>, now: Instant) -> bool {
    if let Some(ms) = armed {
        *until = Some(now + Duration::from_millis(ms));
    }
    until.is_some_and(|until| now < until)
}

/// Why a lease step failed: a stable reason code for history (B-35) plus the
/// message the action result has always carried. Both reach the run's
/// history entry as they are (`RunReport`), never through the message text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LeaseError {
    pub(super) reason: ReasonCode,
    pub(super) message: String,
    /// A refused backup's losses (B-08), for `HistoryEntry::skipped_formats`:
    /// the formats it skipped up to the refusal plus the ones that refused
    /// it (Full's first loss, an unknown handle type, a synthesized format
    /// without its source; nothing more when no restorable format was
    /// left). Empty for every other failure.
    pub(super) formats: Vec<u32>,
}
impl LeaseError {
    fn new(reason: ReasonCode, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
            formats: vec![],
        }
    }
    fn refusing(self, formats: Vec<u32>) -> Self {
        Self { formats, ..self }
    }
}

/// Reports the component Degraded (`ClipboardBusy`) and keeps it Degraded
/// for the rest of the lease (`degraded`).
fn degrade<C: ClipboardBackend>(backend: &C, degraded: &Cell<bool>) {
    degraded.set(true);
    backend.report_health(HealthState::Degraded, Some(ReasonCode::ClipboardBusy));
}
/// Health after a critical section that held the clipboard open for `held`.
fn report_held<C: ClipboardBackend>(backend: &C, degraded: &Cell<bool>, held: Duration) {
    if held > LOCK_HOLD_BUDGET {
        tracing::warn!(
            event = "clipboard.slow",
            held_ms = held.as_millis() as u64,
            budget_ms = LOCK_HOLD_BUDGET.as_millis() as u64
        );
        degrade(backend, degraded);
    } else if !degraded.get() {
        backend.report_health(HealthState::Healthy, None);
    }
}
/// The clipboard stayed busy for every `OpenClipboard` attempt.
fn exhausted<C: ClipboardBackend>(backend: &C, degraded: &Cell<bool>) {
    tracing::warn!(event = "clipboard.busy", attempts = OPEN_ATTEMPTS as u64);
    degrade(backend, degraded);
}

/// Runs `f` with the clipboard open on the lease thread. Only for sections
/// over the lease's own data (its writes and their verification), which
/// never wait on another owner; reads of another owner's data go through
/// [`read_section`]. The critical section is only open → read/write →
/// close: callers never sleep, wait or inject input in `f`. A busy
/// clipboard is retried `OPEN_ATTEMPTS` times with the clipboard closed in
/// between. Staying busy, or holding it open longer than `LOCK_HOLD_BUDGET`
/// (measured after the fact), degrades the component.
fn critical<C: ClipboardBackend, R>(
    backend: &C,
    degraded: &Cell<bool>,
    f: impl FnOnce() -> R,
) -> Result<R, LeaseError> {
    let mut f = Some(f);
    for attempt in 0..OPEN_ATTEMPTS {
        if attempt > 0 {
            backend.pause(OPEN_RETRY_INTERVAL);
        }
        let opened = backend.locked(|| {
            let start = backend.now();
            let result = f.take().map(|f| f());
            (result, backend.now().saturating_duration_since(start))
        });
        if let Some((Some(result), held)) = opened {
            report_held(backend, degraded, held);
            return Ok(result);
        }
    }
    exhausted(backend, degraded);
    Err(busy())
}
/// [`critical`] found the clipboard busy on every attempt.
fn busy() -> LeaseError {
    LeaseError::new(ReasonCode::ClipboardBusy, "Clipboard is busy")
}

/// Clipboard time of one backup worker section.
struct Section<'a> {
    opened: Instant,
    abandoned: &'a AtomicBool,
}
impl Section<'_> {
    /// Checked after every read: stop once the lease gave up waiting, or the
    /// clipboard has been open longer than `LOCK_HOLD_BUDGET`.
    fn expired(&self, reader: &impl ClipboardRead) -> bool {
        self.abandoned.load(Ordering::SeqCst)
            || reader.now().saturating_duration_since(self.opened) > LOCK_HOLD_BUDGET
    }
}
/// A worker section stopped at a read that crossed the hold budget.
fn overrun() -> LeaseError {
    LeaseError::new(
        ReasonCode::ClipboardBusy,
        "Clipboard data took too long to read; trigger preserved",
    )
}
fn worker_failed() -> LeaseError {
    LeaseError::new(
        ReasonCode::Internal,
        "Clipboard backup failed; trigger preserved",
    )
}

type Job<R> = Box<dyn FnOnce(&R, &AtomicBool) + Send>;

/// A worker section's answer, sent once the clipboard is closed again.
enum Attempt<S, T> {
    /// The clipboard was busy; the state comes back for the next attempt.
    Busy(S),
    /// The section ran, holding the clipboard open for `held`.
    Done { value: T, held: Duration },
}

/// The thread a lease reads another owner's clipboard data on (B-08).
/// `GetClipboardData` on a delay-rendered format blocks until the owner
/// renders it, so the lease waits for each section at most `READ_WAIT`.
/// The thread holds only a [`ClipboardRead`]: abandoned, it can still finish
/// a stuck call and close the clipboard, but it has nothing to write with.
struct BackupWorker<R> {
    jobs: mpsc::Sender<Job<R>>,
    abandoned: Arc<AtomicBool>,
}
impl<R: ClipboardRead + Send + 'static> BackupWorker<R> {
    fn start(reader: R) -> Result<Self, LeaseError> {
        let (jobs, queue) = mpsc::channel::<Job<R>>();
        let abandoned = Arc::new(AtomicBool::new(false));
        let flag = abandoned.clone();
        std::thread::Builder::new()
            .name("clipboard-backup".into())
            .spawn(move || {
                // Ends once the lease drops its sender.
                for job in queue {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    job(&reader, &flag);
                }
            })
            .map_err(|error| {
                tracing::warn!(event = "clipboard.worker_failed", error = %error);
                worker_failed()
            })?;
        Ok(Self { jobs, abandoned })
    }
    /// Queues one attempt at running `body` on `state` with the clipboard
    /// open. The answer is sent after the worker closed the clipboard.
    fn attempt<S: Send + 'static, T: Send + 'static>(
        &self,
        state: S,
        body: fn(&R, S, &Section<'_>) -> T,
    ) -> Option<mpsc::Receiver<Attempt<S, T>>> {
        let (answer, receiver) = mpsc::sync_channel(1);
        let job: Job<R> = Box::new(move |reader, abandoned| {
            let mut state = Some(state);
            let opened = reader.locked(|| {
                let section = Section {
                    opened: reader.now(),
                    abandoned,
                };
                let value = state.take().map(|state| body(reader, state, &section));
                (
                    value,
                    reader.now().saturating_duration_since(section.opened),
                )
            });
            let attempt = match (opened, state) {
                (Some((Some(value), held)), _) => Attempt::Done { value, held },
                (_, Some(state)) => Attempt::Busy(state),
                _ => return,
            };
            // After a timeout nobody receives it.
            let _ = answer.send(attempt);
        });
        self.jobs.send(job).ok()?;
        Some(receiver)
    }
    /// The lease stopped waiting: the section in progress stops after its
    /// current read, and nothing else runs.
    fn abandon(&self) {
        self.abandoned.store(true, Ordering::SeqCst);
    }
}

/// Runs `body` on `state` on the backup worker with the clipboard open,
/// retrying a busy clipboard like [`critical`]. Each attempt is waited for at
/// most `READ_WAIT`; a section still running then is abandoned and the lease
/// refuses (`ClipboardBusy`, degraded) with the trigger preserved.
fn read_section<C, S, T>(
    backend: &C,
    worker: &BackupWorker<C::Reader>,
    degraded: &Cell<bool>,
    mut state: S,
    body: fn(&C::Reader, S, &Section<'_>) -> T,
) -> Result<T, LeaseError>
where
    C: ClipboardBackend,
    S: Send + 'static,
    T: Send + 'static,
{
    for attempt in 0..OPEN_ATTEMPTS {
        if attempt > 0 {
            backend.pause(OPEN_RETRY_INTERVAL);
        }
        let answer = worker.attempt(state, body).ok_or_else(worker_failed)?;
        match backend.wait_for(&answer, READ_WAIT) {
            Ok(Attempt::Busy(returned)) => state = returned,
            Ok(Attempt::Done { value, held }) => {
                report_held(backend, degraded, held);
                return Ok(value);
            }
            Err(RecvTimeoutError::Timeout) => {
                worker.abandon();
                tracing::warn!(
                    event = "clipboard.read_timeout",
                    wait_ms = READ_WAIT.as_millis() as u64
                );
                degrade(backend, degraded);
                return Err(LeaseError::new(
                    ReasonCode::ClipboardBusy,
                    "Clipboard data did not arrive in time; trigger preserved",
                ));
            }
            // The section panicked; the clipboard was closed on unwind.
            Err(RecvTimeoutError::Disconnected) => return Err(worker_failed()),
        }
    }
    exhausted(backend, degraded);
    Err(LeaseError::new(
        ReasonCode::ClipboardBusy,
        "Clipboard is busy; trigger preserved",
    ))
}
fn clipboard_operation(operation: &Operation) -> bool {
    matches!(
        operation,
        Operation::Text {
            method: OutputMethod::Paste,
            ..
        } | Operation::Html { .. }
            | Operation::PasteFiles { .. }
            | Operation::PasteImage { .. }
    )
}

// no-regression #2: a paste whose clipboard lease fails never downgrades to typed keys.
pub(super) fn acquire_clipboard<T, E>(
    plan: &ActionPlan,
    acquire: impl FnOnce() -> Result<T, E>,
) -> Result<Option<T>, E> {
    if plan
        .steps
        .iter()
        .any(|step| clipboard_operation(&step.operation))
    {
        // Output method is a user contract. Never turn a failed Paste into
        // typed keys (especially multiline text that could submit a chat).
        acquire().map(Some)
    } else {
        Ok(None)
    }
}
/// Index of the plan's first clipboard step when its paste can be written
/// before the trigger is erased (plan §5.4 principle 4, "resolve first, then
/// delete"): every earlier step only types text, so nothing before it can
/// touch the clipboard. After a step that may make the target copy or cut
/// (KeyPress, KeyDown, Hotkey, ...), the write stays at its step, as does
/// every later paste's: a failure there stops after erasure (no-regression
/// #6), an accepted residual risk.
pub(super) fn written_before_erase(plan: &ActionPlan) -> Option<usize> {
    let first = plan
        .steps
        .iter()
        .position(|step| clipboard_operation(&step.operation))?;
    plan.steps[..first]
        .iter()
        .all(|step| {
            matches!(
                step.operation,
                Operation::Text {
                    method: OutputMethod::Keys,
                    ..
                }
            )
        })
        .then_some(first)
}
fn html_clipboard(fragment: &str) -> String {
    let prefix = "<html><body><!--StartFragment-->";
    let suffix = "<!--EndFragment--></body></html>";
    let head="Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
    let start = head.len();
    let start_fragment = start + prefix.len();
    format!("Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n{}{}{}\0",start,start+prefix.len()+fragment.len()+suffix.len(),start_fragment,start_fragment+fragment.len(),prefix,fragment,suffix)
}
type ClipboardFormats = Vec<(u32, Vec<u8>)>;
pub(super) fn clipboard_formats(op: &Operation) -> Result<Option<ClipboardFormats>, String> {
    Ok(Some(match op {
        Operation::Text {
            text,
            method: OutputMethod::Paste,
        } => vec![(
            13,
            wide(text).iter().flat_map(|v| v.to_le_bytes()).collect(),
        )],
        Operation::Html { html, plain_text } => vec![
            (
                unsafe { RegisterClipboardFormatW(wide("HTML Format").as_ptr()) },
                html_clipboard(html).into_bytes(),
            ),
            (
                13,
                wide(plain_text)
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect(),
            ),
        ],
        Operation::PasteFiles { paths } => {
            let mut b = vec![0u8; 20];
            b[..4].copy_from_slice(&20u32.to_le_bytes());
            b[16..20].copy_from_slice(&1u32.to_le_bytes());
            for p in paths {
                for v in wide(p) {
                    b.extend(v.to_le_bytes())
                }
            }
            b.extend([0, 0]);
            vec![(15, b)]
        }
        Operation::PasteImage { path } => {
            // A fixed text (F5-1 H): never the decoder's details.
            let img = image::open(path)
                .map_err(|_| "Image could not be decoded".to_string())?
                .to_rgba8();
            let (w, h) = img.dimensions();
            let mut b = vec![0u8; 40];
            b[..4].copy_from_slice(&40u32.to_le_bytes());
            b[4..8].copy_from_slice(&(w as i32).to_le_bytes());
            b[8..12].copy_from_slice(&(-(h as i32)).to_le_bytes());
            b[12..14].copy_from_slice(&1u16.to_le_bytes());
            b[14..16].copy_from_slice(&32u16.to_le_bytes());
            for p in img.pixels() {
                b.extend([p[2], p[1], p[0], p[3]])
            }
            vec![(8, b)]
        }
        _ => return Ok(None),
    }))
}
/// Production [`ClipboardBackend`]: a message-only owner window per lease.
pub(super) struct WinClipboard {
    window: HWND,
}
impl WinClipboard {
    pub(super) fn new() -> Result<Self, String> {
        let window = unsafe {
            CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide("Quicktext Clipboard").as_ptr(),
                0,
                0,
                0,
                0,
                0,
                (-3isize) as HWND,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if window.is_null() {
            return Err("Clipboard owner window creation failed".into());
        }
        Ok(Self { window })
    }
}
impl Drop for WinClipboard {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.window);
        }
    }
}
/// Handle not yet handed to the clipboard (an HGLOBAL, or an HENHMETAFILE
/// for `CF_ENHMETAFILE`); freed on drop.
pub(super) struct WinStaged {
    handle: HANDLE,
    metafile: bool,
}
impl WinStaged {
    fn into_raw(mut self) -> (HANDLE, bool) {
        (
            std::mem::replace(&mut self.handle, std::ptr::null_mut()),
            self.metafile,
        )
    }
}
/// Frees a handle the clipboard did not take.
unsafe fn free_staged(handle: HANDLE, metafile: bool) {
    if metafile {
        DeleteEnhMetaFile(handle);
    } else {
        GlobalFree(handle);
    }
}
impl Drop for WinStaged {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { free_staged(self.handle, self.metafile) }
        }
    }
}
/// Serialized bits of the enhanced metafile `handle`.
unsafe fn metafile_bits(handle: HANDLE, limit: usize) -> FormatRead {
    let size = GetEnhMetaFileBits(handle, 0, std::ptr::null_mut());
    if size == 0 {
        return FormatRead::Unreadable;
    }
    if size as usize > limit {
        return FormatRead::TooLarge;
    }
    let mut bytes = vec![0u8; size as usize];
    if GetEnhMetaFileBits(handle, size, bytes.as_mut_ptr()) != size {
        return FormatRead::Unreadable;
    }
    FormatRead::Data(bytes)
}
/// Opens the clipboard associated with `window` (null: no owner window),
/// runs `f` and closes it again, also when `f` unwinds. Both happen on the
/// calling thread, as `CloseClipboard` requires. `None` when the clipboard
/// is busy, or inside the `FaultPoint::ClipboardBusy` window.
fn with_clipboard_open<R>(window: HWND, f: impl FnOnce() -> R) -> Option<R> {
    struct Close;
    impl Drop for Close {
        fn drop(&mut self) {
            unsafe {
                CloseClipboard();
            }
        }
    }
    let fault_busy = fault_busy_window(
        fault::take(FaultPoint::ClipboardBusy),
        &mut FAULT_BUSY_UNTIL
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
        Instant::now(),
    );
    if fault_busy || unsafe { OpenClipboard(window) } == 0 {
        return None;
    }
    let _close = Close;
    Some(f())
}
/// Read-only clipboard access for the backup worker thread: it opens the
/// clipboard with no owner window (`OpenClipboard(NULL)`) and has no write
/// methods.
pub(super) struct WinReader;
impl ClipboardRead for WinReader {
    fn owner(&self) -> u64 {
        unsafe { GetClipboardOwner() as u64 }
    }
    fn sequence(&self) -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }
    fn locked<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        with_clipboard_open(std::ptr::null_mut(), f)
    }
    fn formats(&self) -> Vec<u32> {
        let mut formats = Vec::new();
        let mut format = 0;
        loop {
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                break;
            }
            formats.push(format);
        }
        formats
    }
    fn read_format(&self, format: u32, limit: usize) -> FormatRead {
        unsafe {
            // Only formats `handling` copies get here: HGLOBAL ones and
            // CF_ENHMETAFILE. A delay-rendered format is rendered now: this
            // blocks until the owner handles WM_RENDERFORMAT (待实测: how
            // long Windows waits for an owner that never does).
            let handle = GetClipboardData(format);
            if handle.is_null() {
                return FormatRead::Missing;
            }
            if format == CF_ENHMETAFILE {
                return metafile_bits(handle, limit);
            }
            let size = GlobalSize(handle);
            if size == 0 {
                return FormatRead::Missing;
            }
            if size > limit {
                return FormatRead::TooLarge;
            }
            let pointer = GlobalLock(handle);
            if pointer.is_null() {
                return FormatRead::Unreadable;
            }
            let bytes = std::slice::from_raw_parts(pointer as *const u8, size).to_vec();
            GlobalUnlock(handle);
            FormatRead::Data(bytes)
        }
    }
}
impl ClipboardRead for WinClipboard {
    fn owner(&self) -> u64 {
        WinReader.owner()
    }
    fn sequence(&self) -> u32 {
        WinReader.sequence()
    }
    fn locked<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        with_clipboard_open(self.window, f)
    }
    fn formats(&self) -> Vec<u32> {
        WinReader.formats()
    }
    fn read_format(&self, format: u32, limit: usize) -> FormatRead {
        WinReader.read_format(format, limit)
    }
}
impl ClipboardBackend for WinClipboard {
    type Staged = WinStaged;
    type Reader = WinReader;
    fn identity(&self) -> u64 {
        self.window as u64
    }
    fn reader(&self) -> WinReader {
        WinReader
    }
    fn clear(&self) -> bool {
        unsafe { EmptyClipboard() != 0 }
    }
    fn stage_format(&self, format: u32, bytes: &[u8]) -> Result<(WinStaged, Vec<u8>), String> {
        if format != CF_ENHMETAFILE {
            return self.stage(bytes);
        }
        // A new HENHMETAFILE from the backed-up bits (a memory copy, like
        // CopyEnhMetaFile(h, NULL) of the original), reported as its bits.
        unsafe {
            let size = u32::try_from(bytes.len()).map_err(|_| "Clipboard metafile too large")?;
            let handle = SetEnhMetaFileBits(size, bytes.as_ptr());
            if handle.is_null() {
                return Err("Clipboard metafile allocation failed".into());
            }
            let staged = WinStaged {
                handle,
                metafile: true,
            };
            match metafile_bits(handle, usize::MAX) {
                FormatRead::Data(reported) => Ok((staged, reported)),
                _ => Err("Clipboard metafile allocation failed".into()),
            }
        }
    }
    fn owner_hung(&self, owner: u64) -> bool {
        let window = owner as HWND;
        if window.is_null() || window == self.window {
            return false;
        }
        unsafe {
            if IsWindow(window) == 0 {
                return false;
            }
            if IsHungAppWindow(window) != 0 {
                return true;
            }
            // 待实测: a slow-but-alive owner past the timeout counts as hung.
            let mut result = 0;
            SetLastError(0);
            SendMessageTimeoutW(
                window,
                WM_NULL,
                0,
                0,
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                OWNER_PROBE_TIMEOUT_MS,
                &mut result,
            ) == 0
                && GetLastError() == ERROR_TIMEOUT
        }
    }
    fn register_format(&self, name: &str) -> u32 {
        unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) }
    }
    fn stage(&self, bytes: &[u8]) -> Result<(WinStaged, Vec<u8>), String> {
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len());
            if handle.is_null() {
                return Err("Clipboard allocation failed".into());
            }
            let staged = WinStaged {
                handle,
                metafile: false,
            };
            let pointer = GlobalLock(handle);
            if pointer.is_null() {
                return Err("Clipboard allocation lock failed".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
            let reported =
                std::slice::from_raw_parts(pointer as *const u8, GlobalSize(handle)).to_vec();
            GlobalUnlock(handle);
            Ok((staged, reported))
        }
    }
    fn set(&self, format: u32, staged: WinStaged) -> bool {
        unsafe {
            let (handle, metafile) = staged.into_raw();
            if SetClipboardData(format, handle).is_null() {
                free_staged(handle, metafile);
                return false;
            }
            true
        }
    }
    fn report_health(&self, state: HealthState, reason: Option<ReasonCode>) {
        record_health(state, reason);
    }
}
/// Payloads allocated before trigger deletion; unused ones free on drop.
pub(super) struct PreparedClipboard<S> {
    handles: Vec<(u32, S)>,
    expected: ClipboardFormats,
}
// no-regression #1: restore only after sequence, owner, payload, format set
// and derived bytes re-verify.
pub(super) struct ClipboardLease<C: ClipboardBackend> {
    backend: C,
    backup: ClipboardFormats,
    owned_payload: ClipboardFormats,
    written: Option<u32>,
    dirty: bool,
    /// Set once this lease reported the component Degraded, so a later fast
    /// critical section of the same lease does not hide it.
    degraded: Cell<bool>,
    /// Formats present at backup time that will not be restored.
    skipped: Vec<u32>,
    restore_delay: Duration,
    /// What the verification of the last write established; every write
    /// (each `EmptyClipboard`) starts over at [`Capture::Untried`].
    baseline: Capture,
}
/// Trust in the clipboard the lease's last write left (B-08). It is
/// established once per write, by the first check of the clipboard that
/// gets it open: normally the capture right after a paste write, or the
/// restore's check when that capture never got it open. Every later check
/// only compares against what was recorded. [`ClipboardLease::verify_owned`]
/// makes every transition.
#[derive(Debug, PartialEq)]
enum Capture {
    /// No check has opened the clipboard since the lease last cleared it:
    /// nothing written yet, a write (paste or restore) whose `set` failed
    /// after the clear, or checks that never got the clipboard open (busy
    /// on every attempt, e.g. a clipboard manager reading the paste) and so
    /// read nothing. A failed clear leaves the clipboard, and this state,
    /// as they were. The next check that opens it (the restore's, Drop's)
    /// captures the way the paste write's capture would. Accepted residual
    /// risk, like the window between a write's CloseClipboard and its
    /// capture, which busy checks extend to the first successful open:
    /// another process rewriting a derived format in place in between is
    /// outside the threat model (see [`HANDLE_FORMATS`]); normal programs
    /// never do it without emptying the clipboard.
    Untried,
    /// Recorded by the first check that opened the clipboard: what every
    /// later check holds it to.
    Captured(Baseline),
    /// A check opened the clipboard and then refused (owner, format set,
    /// bytes, a derived format it could not read) or could not finish (the
    /// sequence never settled, a later attempt stayed busy), whether it was
    /// capturing or comparing. Nothing establishes trust later: what the
    /// check could not vouch for may have been rewritten since, and
    /// checking it again would adopt that. Every restore, explicit or on
    /// drop, refuses and the clipboard keeps what it holds.
    Failed,
}
/// What a restore holds the clipboard to (B-08), captured in the section
/// that verifies the paste write: its format set (the paste, the exclusion
/// markers and what the system synthesized from them), and the bytes of each
/// of those formats that is neither the lease's own (checked against
/// `owned_payload`) nor a [`HANDLE_FORMATS`] handle: CF_LOCALE, CF_TEXT,
/// CF_OEMTEXT, CF_DIBV5. A restore needs exactly this format set and these
/// bytes; the sequence may have moved.
#[derive(PartialEq)]
struct Baseline {
    formats: Vec<u32>,
    derived: ClipboardFormats,
}
/// What a lease read with the clipboard open.
struct Backup {
    /// Every enumerated format, to detect a changed format set.
    formats: Vec<u32>,
    data: ClipboardFormats,
    skipped: Vec<u32>,
}
/// Clipboard bytes in `Debug` (B-09): each format with `<redacted len=N>`, N
/// its byte count, never the content.
struct RedactedFormats<'a>(&'a ClipboardFormats);
impl std::fmt::Debug for RedactedFormats<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(
                self.0
                    .iter()
                    .map(|(format, bytes)| (format, quicker_domain::RedactedLen(bytes.len()))),
            )
            .finish()
    }
}
impl std::fmt::Debug for Baseline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Baseline")
            .field("formats", &self.formats)
            .field("derived", &RedactedFormats(&self.derived))
            .finish()
    }
}
impl std::fmt::Debug for Backup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backup")
            .field("formats", &self.formats)
            .field("data", &RedactedFormats(&self.data))
            .field("skipped", &self.skipped)
            .finish()
    }
}
fn refusal(too_large: bool) -> LeaseError {
    if too_large {
        LeaseError::new(
            ReasonCode::ClipboardTooLarge,
            "Clipboard format is too large to back up; trigger preserved",
        )
    } else {
        LeaseError::new(
            ReasonCode::ClipboardFormatSkipped,
            "Clipboard format cannot be safely backed up; trigger preserved",
        )
    }
}
/// Locked, on the backup worker: classifies and reads every format under
/// `policy`, checking the section's time after every read. BestEffort may
/// skip only the format table's skips and formats over `MAX_FORMAT_BYTES`;
/// any failed read refuses under every policy.
fn take_backup<R: ClipboardRead>(
    reader: &R,
    policy: RestorePolicy,
    section: &Section<'_>,
) -> Result<Backup, LeaseError> {
    let formats = reader.formats();
    let mut data = vec![];
    let mut skipped = vec![];
    let mut covered = vec![];
    let mut too_large = false;
    for &format in &formats {
        match handling(format, policy) {
            Handling::Ignore => continue,
            // Its sources may come later in enumeration order.
            Handling::Covered(sources) => {
                covered.push((format, sources));
                continue;
            }
            Handling::Unknown => {
                skipped.push(format);
                return Err(refusal(false).refusing(skipped));
            }
            Handling::Skip => {}
            Handling::Copy => {
                let read = reader.read_format(format, MAX_FORMAT_BYTES);
                if section.expired(reader) {
                    return Err(overrun());
                }
                match read {
                    FormatRead::Data(bytes) => {
                        data.push((format, bytes));
                        continue;
                    }
                    FormatRead::TooLarge => too_large = true,
                    // GetClipboardData returned nothing for an enumerated
                    // format, or its memory could not be read.
                    FormatRead::Missing | FormatRead::Unreadable => {
                        tracing::warn!(event = "clipboard.unreadable", format);
                        return Err(LeaseError::new(
                            ReasonCode::Internal,
                            format!(
                                "Clipboard format {format:#x} could not be read; trigger preserved"
                            ),
                        ));
                    }
                }
            }
        }
        skipped.push(format);
        // Full stops reading at the first loss.
        if policy == RestorePolicy::Full {
            return Err(refusal(too_large).refusing(skipped));
        }
    }
    let restored = |sources: &[u32]| data.iter().any(|(format, _)| sources.contains(format));
    let uncovered: Vec<u32> = covered
        .iter()
        .filter(|(_, sources)| !restored(sources))
        .map(|(format, _)| *format)
        .collect();
    if !uncovered.is_empty() {
        skipped.extend(uncovered);
        return Err(refusal(too_large).refusing(skipped));
    }
    // The restore would empty a clipboard that held data. TextOnly opted
    // into dropping everything that is not text.
    if policy != RestorePolicy::TextOnly && data.is_empty() && !formats.is_empty() {
        return Err(refusal(too_large).refusing(skipped));
    }
    Ok(Backup {
        formats,
        data,
        skipped,
    })
}
fn changed(message: &str) -> LeaseError {
    LeaseError::new(ReasonCode::ClipboardChanged, message)
}
fn unverifiable() -> LeaseError {
    LeaseError::new(ReasonCode::Internal, "Clipboard payload cannot be verified")
}
fn uncaptured() -> LeaseError {
    changed("Clipboard payload was never verified after the paste write; clipboard preserved")
}
/// A write found the clipboard changed since the lease last verified it;
/// nothing was cleared.
fn overwrite_refused() -> LeaseError {
    changed("Clipboard changed externally; stopped without overwriting it")
}
/// `EmptyClipboard` failed with the clipboard open: nothing was written.
fn clear_failed() -> LeaseError {
    LeaseError::new(ReasonCode::Internal, "Clipboard clear failed")
}
/// `SetClipboardData` failed after the clear.
fn write_failed() -> LeaseError {
    LeaseError::new(ReasonCode::Internal, "Clipboard write failed")
}
/// Locked: checks that `owner` still owns the clipboard, that its format set
/// passes `formats_ok` and that it holds exactly `expected`; `expired` is
/// checked after every read. Returns the format set it saw.
fn check_payload<R: ClipboardRead>(
    reader: &R,
    owner: u64,
    expected: &[(u32, Vec<u8>)],
    formats_ok: impl Fn(&[u32]) -> bool,
    expired: impl Fn() -> bool,
) -> Result<Vec<u32>, LeaseError> {
    if reader.owner() != owner {
        return Err(changed(
            "Clipboard owner changed externally; clipboard preserved",
        ));
    }
    let formats = reader.formats();
    if !formats_ok(&formats) {
        return Err(changed(
            "Clipboard formats changed externally; clipboard preserved",
        ));
    }
    check_bytes(reader, expected, expired)?;
    Ok(formats)
}
/// Locked: every format in `expected` holds exactly its bytes; `expired` is
/// checked after every read.
fn check_bytes<R: ClipboardRead>(
    reader: &R,
    expected: &[(u32, Vec<u8>)],
    expired: impl Fn() -> bool,
) -> Result<(), LeaseError> {
    for (format, bytes) in expected {
        let read = reader.read_format(*format, bytes.len());
        if expired() {
            return Err(overrun());
        }
        match read {
            FormatRead::Unreadable => return Err(unverifiable()),
            FormatRead::Data(current) if current == *bytes => {}
            _ => {
                return Err(changed(
                    "Clipboard payload changed externally; clipboard preserved",
                ))
            }
        }
    }
    Ok(())
}
/// Locked, on the lease thread, after the lease wrote `owned`: the lease
/// still owns the clipboard and holds exactly `owned`. With a `baseline`,
/// the format set is exactly the baseline's and its derived formats hold
/// their bytes. Without one (right after a write), every other format must
/// be one the system synthesizes from `owned`, and a baseline is captured:
/// the bytes of the derived formats are read, which renders them. Returns
/// the sequence, read after every read, and the captured baseline.
fn check_owned<R: ClipboardRead>(
    reader: &R,
    owner: u64,
    owned: &[(u32, Vec<u8>)],
    baseline: Option<&Baseline>,
) -> Result<(u32, Option<Baseline>), LeaseError> {
    let captured = match baseline {
        Some(baseline) => {
            let matches = |formats: &[u32]| formats_match(formats, &baseline.formats, owned);
            check_payload(reader, owner, owned, matches, || false)?;
            check_bytes(reader, &baseline.derived, || false)?;
            None
        }
        None => {
            let synthesized = |formats: &[u32]| formats_synthesized(formats, owned);
            let formats = check_payload(reader, owner, owned, synthesized, || false)?;
            let mut derived = vec![];
            for &format in &formats {
                if own(format, owned) || HANDLE_FORMATS.contains(&format) {
                    continue;
                }
                // Rendered from the lease's own data, never delay-rendered,
                // so the read cannot wait on another owner. A derived format
                // that cannot be read leaves nothing to hold a restore to.
                match reader.read_format(format, MAX_FORMAT_BYTES) {
                    FormatRead::Data(bytes) => derived.push((format, bytes)),
                    _ => return Err(unverifiable()),
                }
            }
            Some(Baseline { formats, derived })
        }
    };
    Ok((reader.sequence(), captured))
}
fn unverified() -> LeaseError {
    LeaseError::new(
        ReasonCode::ClipboardChanged,
        "Clipboard ownership could not be verified within the retry limit",
    )
}
/// Re-reads the backup on the worker so a copy racing it is detected, until
/// the sequence it was read at is still current. Returns the backup and
/// that sequence.
fn verify_backup<C: ClipboardBackend>(
    backend: &C,
    worker: &BackupWorker<C::Reader>,
    degraded: &Cell<bool>,
    owner: u64,
    mut backup: Backup,
) -> Result<(Backup, u32), LeaseError> {
    for _ in 0..VERIFY_ATTEMPTS {
        let (returned, checked) = read_section(
            backend,
            worker,
            degraded,
            (owner, backup),
            |reader, (owner, backup), section| {
                let checked = check_payload(
                    reader,
                    owner,
                    &backup.data,
                    |formats| {
                        formats.len() == backup.formats.len()
                            && formats.iter().all(|format| backup.formats.contains(format))
                    },
                    || section.expired(reader),
                )
                .map(|_| reader.sequence());
                (backup, checked)
            },
        )?;
        backup = returned;
        let sequence = checked?;
        if backend.sequence() == sequence && backend.owner() == owner {
            return Ok((backup, sequence));
        }
        backend.pause(VERIFY_RETRY_INTERVAL);
    }
    Err(unverified())
}
fn finish_clipboard_restore<T>(
    backup: &mut Vec<T>,
    result: Result<(), LeaseError>,
) -> Result<(), LeaseError> {
    result?;
    backup.clear();
    Ok(())
}
fn retry_clipboard_verification<T>(
    mut attempt: impl FnMut() -> Result<Option<T>, LeaseError>,
    mut pause: impl FnMut(),
) -> Result<T, LeaseError> {
    for _ in 0..VERIFY_ATTEMPTS {
        match attempt()? {
            Some(verified) => return Ok(verified),
            None => pause(),
        }
    }
    Err(unverified())
}
impl<C: ClipboardBackend> ClipboardLease<C> {
    /// Re-establishes the lease's own ownership after a write
    /// ([`check_owned`] against the baseline), retried until the sequence it
    /// verified at is still current once the clipboard is closed. A sequence
    /// that moved since the write is not a refusal by itself: the format set
    /// and every byte decide. Returns that sequence. Runs on the lease
    /// thread; the data is the lease's own, so no read waits on another
    /// owner.
    ///
    /// The only path to the clipboard after a write, for the paste write's
    /// capture and every restore alike, so each makes the same [`Capture`]
    /// transition. [`Capture::Failed`] refuses without opening the
    /// clipboard. Otherwise, verified: a baseline captured from
    /// [`Capture::Untried`] is kept ([`Capture::Captured`]), and every later
    /// check compares against it, never captures again. Failed after one of
    /// its sections opened the clipboard: [`Capture::Failed`], from either
    /// state. Never got it open (busy on every attempt, nothing read): the
    /// state stays as it was.
    fn verify_owned(&mut self) -> Result<u32, LeaseError> {
        let backend = &self.backend;
        let owner = backend.identity();
        let baseline = match &self.baseline {
            Capture::Untried => None,
            Capture::Captured(baseline) => Some(baseline),
            Capture::Failed => return Err(uncaptured()),
        };
        let opened = Cell::new(false);
        let verified = retry_clipboard_verification(
            || {
                let (sequence, captured) = critical(backend, &self.degraded, || {
                    opened.set(true);
                    check_owned(backend, owner, &self.owned_payload, baseline)
                })??;
                Ok((backend.sequence() == sequence && backend.owner() == owner)
                    .then_some((sequence, captured)))
            },
            || backend.pause(VERIFY_RETRY_INTERVAL),
        );
        match verified {
            Ok((sequence, captured)) => {
                // `None` only when it compared against a baseline it keeps.
                if let Some(captured) = captured {
                    self.baseline = Capture::Captured(captured);
                }
                Ok(sequence)
            }
            Err(error) => {
                if opened.get() {
                    self.baseline = Capture::Failed;
                }
                Err(error)
            }
        }
    }
    pub(super) fn verify_current(&self) -> Result<(), LeaseError> {
        if self
            .written
            .is_none_or(|seq| self.backend.sequence() != seq)
        {
            return Err(changed(
                "Clipboard changed externally before paste; no paste was sent",
            ));
        }
        Ok(())
    }
    /// The [`EXCLUSION_FORMATS`] markers. A name that cannot be registered is
    /// logged and left out: the paste itself does not depend on it.
    fn exclusion_markers(&self) -> ClipboardFormats {
        EXCLUSION_FORMATS
            .iter()
            .filter_map(|name| match self.backend.register_format(name) {
                0 => {
                    tracing::warn!(event = "clipboard.exclusion_unregistered", format = name);
                    None
                }
                format => Some((format, 0u32.to_le_bytes().to_vec())),
            })
            .collect()
    }
    /// Allocates `formats`, plus the exclusion markers, up front so
    /// allocation cannot fail after deletion. Restores never add markers:
    /// they bring back the original formats only.
    pub(super) fn prepare(
        &self,
        formats: ClipboardFormats,
    ) -> Result<PreparedClipboard<C::Staged>, LeaseError> {
        let mut prepared = PreparedClipboard {
            handles: vec![],
            expected: vec![],
        };
        for (format, bytes) in formats.into_iter().chain(self.exclusion_markers()) {
            let (staged, reported) = self.backend.stage(&bytes).map_err(|error| {
                LeaseError::new(ReasonCode::Internal, format!("{error} before deletion"))
            })?;
            prepared.expected.push((format, reported));
            prepared.handles.push((format, staged));
        }
        Ok(prepared)
    }
    pub(super) fn write_prepared(
        &mut self,
        prepared: &mut PreparedClipboard<C::Staged>,
    ) -> Result<(), LeaseError> {
        let Self {
            backend,
            owned_payload,
            written,
            dirty,
            degraded,
            baseline,
            ..
        } = self;
        critical(backend, degraded, || -> Result<(), LeaseError> {
            if written.is_some_and(|seq| backend.sequence() != seq) {
                return Err(overwrite_refused());
            }
            if !backend.clear() {
                return Err(clear_failed());
            }
            *dirty = true;
            owned_payload.clear();
            *baseline = Capture::Untried;
            mark_external_effect();
            while !prepared.handles.is_empty() {
                let (f, h) = prepared.handles.remove(0);
                if !backend.set(f, h) {
                    *written = Some(backend.sequence());
                    return Err(write_failed());
                }
                if let Some((_, bytes)) = prepared.expected.iter().find(|(format, _)| *format == f)
                {
                    owned_payload.push((f, bytes.clone()));
                }
            }
            *written = Some(backend.sequence());
            Ok(())
        })??;
        // The baseline is captured on the lease thread, in the section that
        // verifies the write: the data is the lease's own, so no read waits
        // on another owner (the backup worker only lives through `open`), and
        // one section makes payload, format set, derived bytes and sequence
        // one snapshot. Anything there but the paste, the markers and what
        // the system synthesizes from them was added by someone else: no
        // paste is sent and the restore refuses too. Accepted residual risk:
        // a format the system could synthesize, added or rewritten by another
        // process in the milliseconds between this write's CloseClipboard
        // and the capture, joins the baseline (outside the threat model for
        // the reasons at HANDLE_FORMATS). A capture that opened the clipboard
        // and refused or could not finish is final: every restore refuses
        // ([`Capture::Failed`]). One that never got it open (a clipboard
        // manager reading the paste can hold it through every attempt) read
        // nothing and stays [`Capture::Untried`]: the restore's check
        // captures in its place, so it can still bring the original back.
        // 待实测: whether CF_LOCALE is listed after CF_UNICODETEXT alone, and
        // whether rendering a synthesized format advances the sequence (a
        // move after the capture fails `verify_current` before the paste).
        self.written = Some(self.verify_owned()?);
        Ok(())
    }
    /// [`ClipboardLease::open`] with the app's options (`prefs.json`
    /// `app.clipboard`, set at startup; [`lease_options`]).
    pub(super) fn new(backend: C) -> Result<Self, LeaseError> {
        Self::open(backend, lease_options())
    }
    /// Backs up the clipboard under `options` (B-08), then verifies the
    /// backup in a second section so a copy racing it is detected. Every
    /// read of the other owner's data runs on a backup worker thread the
    /// lease waits on for at most [`READ_WAIT`] per section. Fails before
    /// anything is written: the trigger is preserved.
    pub(super) fn open(backend: C, options: LeaseOptions) -> Result<Self, LeaseError> {
        let degraded = Cell::new(false);
        let worker = BackupWorker::start(backend.reader())?;
        let mut taken = None;
        for _ in 0..OWNER_PROBES {
            // Probed with the clipboard closed: the probe may wait.
            let owner = backend.owner();
            if backend.owner_hung(owner) {
                // Its delay-rendered formats would block every read, and
                // emptying the clipboard would wait on it too.
                tracing::warn!(event = "clipboard.owner_hung");
                degrade(&backend, &degraded);
                return Err(LeaseError::new(
                    ReasonCode::ClipboardBusy,
                    "Clipboard owner is not responding; trigger preserved",
                ));
            }
            let backup = read_section(
                &backend,
                &worker,
                &degraded,
                (owner, options.policy),
                |reader, (owner, policy), section| {
                    (reader.owner() == owner).then(|| take_backup(reader, policy, section))
                },
            )?;
            if let Some(backup) = backup {
                let backup = backup.inspect_err(|error| {
                    tracing::warn!(event = "clipboard.lease_refused", reason = ?error.reason);
                })?;
                taken = Some((owner, backup));
                break;
            }
        }
        let Some((owner, backup)) = taken else {
            return Err(LeaseError::new(
                ReasonCode::ClipboardChanged,
                "Clipboard owner kept changing during backup; trigger preserved",
            ));
        };
        let (backup, sequence) = verify_backup(&backend, &worker, &degraded, owner, backup)?;
        let lease = Self {
            backend,
            backup: backup.data,
            owned_payload: vec![],
            written: Some(sequence),
            dirty: false,
            degraded,
            skipped: backup.skipped,
            restore_delay: options.restore_delay,
            baseline: Capture::Untried,
        };
        if !lease.skipped_formats().is_empty() {
            tracing::info!(
                event = "clipboard.skipped",
                formats = ?lease.skipped_formats(),
                policy = ?options.policy
            );
        }
        Ok(lease)
    }
    /// Formats on the clipboard at backup time that the restore will not
    /// bring back (B-08), for `HistoryEntry::skipped_formats`.
    pub(super) fn skipped_formats(&self) -> &[u32] {
        &self.skipped
    }
    /// Wait between the paste chord and the restore.
    pub(super) fn restore_delay(&self) -> Duration {
        self.restore_delay
    }
    fn write(&mut self, formats: ClipboardFormats) -> Result<(), LeaseError> {
        // Allocate before opening: the critical section only clears and sets.
        let mut staged = Vec::with_capacity(formats.len());
        for (format, bytes) in formats {
            let (handle, expected) = self
                .backend
                .stage_format(format, &bytes)
                .map_err(|error| LeaseError::new(ReasonCode::Internal, error))?;
            staged.push((format, handle, expected));
        }
        let Self {
            backend,
            owned_payload,
            written,
            degraded,
            baseline,
            ..
        } = self;
        critical(backend, degraded, || -> Result<(), LeaseError> {
            if let Some(seq) = *written {
                if backend.sequence() != seq {
                    return Err(overwrite_refused());
                }
            }
            if !backend.clear() {
                return Err(clear_failed());
            }
            mark_external_effect();
            owned_payload.clear();
            *baseline = Capture::Untried;
            *written = Some(backend.sequence());
            for (format, handle, expected) in staged {
                if !backend.set(format, handle) {
                    return Err(write_failed());
                }
                owned_payload.push((format, expected));
            }
            *written = Some(backend.sequence());
            Ok(())
        })?
    }
    pub(super) fn restore(&mut self) -> Result<(), LeaseError> {
        if !self.dirty {
            return Ok(());
        }
        if self.backend.owner() != self.backend.identity() {
            return Ok(());
        }
        // Closing the clipboard or a listener requesting a synthesized
        // format can advance the sequence after our writes. Re-establish
        // ownership from the exact payload, format set and derived bytes
        // under a lock; never adopt a new sequence solely because the
        // previous one is no longer current. Another application adding or
        // rewriting a format without emptying the clipboard keeps the owner
        // and the payload but not the baseline: that refuses, and the
        // clipboard stays as it is. So does a failed capture. From
        // [`Capture::Untried`] the check captures, exactly as the paste
        // write's would; what it records, or its failure, holds for every
        // later attempt, so a write that fails here (busy) leaves Drop
        // comparing against it. Drop runs this same check.
        self.written = Some(self.verify_owned()?);
        let result = self.write(self.backup.clone());
        finish_clipboard_restore(&mut self.backup, result)?;
        self.written = None;
        self.dirty = false;
        self.owned_payload.clear();
        Ok(())
    }
}
impl<C: ClipboardBackend> Drop for ClipboardLease<C> {
    fn drop(&mut self) {
        // The backend (owner window) is released after this restore attempt.
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::inject::SENT_EFFECT;
    #[test]
    fn busy_clipboard_restore_preserves_original_payload_for_drop_retry() {
        let original = vec![(13u32, vec![b'o', 0, b'l', 0, b'd', 0, 0, 0])];
        let mut backup = original.clone();
        assert!(finish_clipboard_restore(&mut backup, Err(busy())).is_err());
        assert_eq!(backup, original);
        let retried_payload = backup.clone();
        finish_clipboard_restore(&mut backup, Ok(())).unwrap();
        assert_eq!(retried_payload, original);
        assert!(backup.is_empty());
    }
    #[test]
    fn clipboard_listener_busy_retry_accepts_later_verified_owned_sequence() {
        let mut attempts = 0;
        let mut pauses = 0;
        let sequence = retry_clipboard_verification(
            || {
                attempts += 1;
                Ok(if attempts < 3 { None } else { Some(42u32) })
            },
            || pauses += 1,
        )
        .unwrap();
        assert_eq!(sequence, 42);
        assert_eq!(attempts, 3);
        assert_eq!(pauses, 2);
    }
    #[test]
    fn clipboard_retry_never_adopts_changed_owner_or_payload() {
        for reason in ["external owner", "modified payload"] {
            let changed = LeaseError::new(ReasonCode::ClipboardChanged, reason);
            let mut attempts = 0;
            let result = retry_clipboard_verification::<u32>(
                || {
                    attempts += 1;
                    if attempts == 1 {
                        Ok(None)
                    } else {
                        Err(changed.clone())
                    }
                },
                || {},
            );
            assert_eq!(result, Err(changed));
            assert_eq!(attempts, 2);
        }
        let mut attempts = 0;
        assert_eq!(
            retry_clipboard_verification::<u32>(
                || {
                    attempts += 1;
                    Ok(None)
                },
                || {}
            ),
            Err(unverified())
        );
        assert_eq!(attempts, VERIFY_ATTEMPTS);
    }
    #[test]
    fn unavailable_clipboard_rejects_paste_before_erasure_without_downgrade() {
        let plan = ActionPlan {
            steps: vec![Step::new(
                Operation::Text {
                    text: "中文\nemoji🙂".into(),
                    method: OutputMethod::Paste,
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        };
        SENT_EFFECT.with(|flag| flag.set(false));
        let result = acquire_clipboard::<(), _>(&plan, || Err(refusal(false)));
        assert!(result.is_err());
        assert!(matches!(&plan.steps[0].operation,
            Operation::Text { text, method: OutputMethod::Paste } if text == "中文\nemoji🙂"));
        assert!(!SENT_EFFECT.with(|flag| flag.get()));
    }
    #[test]
    fn restorable_clipboard_keeps_paste_and_rich_payload_never_silently_becomes_text() {
        let mut plan = ActionPlan {
            steps: vec![Step::new(
                Operation::Text {
                    text: "plain".into(),
                    method: OutputMethod::Paste,
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        };
        assert_eq!(
            acquire_clipboard::<_, String>(&plan, || Ok(42)).unwrap(),
            Some(42)
        );
        assert!(matches!(
            plan.steps[0].operation,
            Operation::Text {
                method: OutputMethod::Paste,
                ..
            }
        ));
        plan.steps.push(Step::new(
            Operation::Html {
                html: "<b>rich</b>".into(),
                plain_text: "rich".into(),
            },
            SourceSpan::default(),
        ));
        assert!(acquire_clipboard::<(), String>(&plan, || Err("cannot preserve".into())).is_err());
        assert!(matches!(
            plan.steps[0].operation,
            Operation::Text {
                method: OutputMethod::Paste,
                ..
            }
        ));
    }
    #[test]
    fn key_output_never_acquires_or_writes_clipboard() {
        let plan = ActionPlan {
            steps: vec![Step::new(
                Operation::Text {
                    text: "plain".into(),
                    method: OutputMethod::Keys,
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        };
        let lease = acquire_clipboard::<(), String>(&plan, || {
            panic!("key output must leave clipboard alone")
        })
        .unwrap();
        assert!(lease.is_none());
    }
    #[test]
    fn paste_is_written_before_erasure_only_after_pure_typing() {
        let typed = || Operation::Text {
            text: "typed".into(),
            method: OutputMethod::Keys,
        };
        let paste = || Operation::Text {
            text: "pasted".into(),
            method: OutputMethod::Paste,
        };
        let key = || Key::named("C");
        let cases: Vec<(Vec<Operation>, Option<usize>)> = vec![
            (vec![paste()], Some(0)),
            (vec![paste(), paste()], Some(0)),
            (vec![typed(), typed(), paste()], Some(2)),
            (vec![typed()], None),
            (vec![Operation::KeyPress { key: key() }, paste()], None),
            (vec![Operation::KeyDown { key: key() }, paste()], None),
            (vec![Operation::Hotkey { keys: vec![key()] }, paste()], None),
            (vec![Operation::Delay { ms: 1 }, paste()], None),
            (
                vec![typed(), Operation::KeyPress { key: key() }, paste()],
                None,
            ),
        ];
        for (operations, expected) in cases {
            let plan = ActionPlan {
                steps: operations
                    .into_iter()
                    .map(|operation| Step::new(operation, SourceSpan::default()))
                    .collect(),
                ..Default::default()
            };
            assert_eq!(written_before_erase(&plan), expected, "{plan:?}");
        }
    }
    #[test]
    fn clipboard_objects_are_native_formats() {
        let payload = clipboard_formats(&Operation::PasteFiles {
            paths: vec!["C:\\fixture.txt".into()],
        })
        .unwrap()
        .unwrap();
        assert_eq!(payload[0].0, 15);
        assert_eq!(&payload[0].1[..4], &20u32.to_le_bytes());
        assert_eq!(&payload[0].1[16..20], &1u32.to_le_bytes());
        let html = html_clipboard("😀 <b>合成</b>");
        let offset = html.find("StartFragment:").unwrap() + 14;
        let start: usize = html[offset..offset + 10].parse().unwrap();
        assert!(html[start..].starts_with("😀 <b>合成</b>"));
    }

    use crate::mock::MockClipboard;
    type Health = Option<(HealthState, Option<ReasonCode>)>;
    const HEALTHY: Health = Some((HealthState::Healthy, None));
    const BUSY: Health = Some((HealthState::Degraded, Some(ReasonCode::ClipboardBusy)));
    fn text(value: &str) -> ClipboardFormats {
        vec![(
            13,
            wide(value).iter().flat_map(|v| v.to_le_bytes()).collect(),
        )]
    }
    #[test]
    fn open_retries_a_busy_clipboard_within_the_bound() {
        let clipboard = MockClipboard::new(7, text("original"));
        clipboard.set_busy(OPEN_ATTEMPTS - 1);
        let lease = ClipboardLease::new(clipboard.clone()).unwrap();
        assert_eq!(
            clipboard.pauses(),
            vec![OPEN_RETRY_INTERVAL; OPEN_ATTEMPTS - 1]
        );
        assert_eq!(clipboard.health(), HEALTHY);
        drop(lease);
        assert_eq!(clipboard.clears(), 0);
    }
    #[test]
    fn clipboard_busy_for_every_attempt_degrades_and_preserves_trigger() {
        let clipboard = MockClipboard::new(7, text("original"));
        clipboard.set_busy(OPEN_ATTEMPTS);
        assert_eq!(
            ClipboardLease::new(clipboard.clone()).err(),
            Some(LeaseError::new(
                ReasonCode::ClipboardBusy,
                "Clipboard is busy; trigger preserved"
            ))
        );
        assert_eq!(clipboard.pauses().len(), OPEN_ATTEMPTS - 1);
        assert_eq!(clipboard.health(), BUSY);
        assert_eq!(clipboard.clears(), 0);
        assert_eq!(clipboard.content(), text("original"));
    }
    #[test]
    fn busy_backup_verification_is_reported_as_busy() {
        let clipboard = MockClipboard::new(7, text("original"));
        let script = clipboard.clone();
        // Lock 0 takes the backup; every attempt at verifying it is busy.
        clipboard.on_lock(move |index| {
            if index == 0 {
                script.set_busy(OPEN_ATTEMPTS);
            }
        });
        assert_eq!(
            ClipboardLease::new(clipboard.clone()).err(),
            Some(LeaseError::new(
                ReasonCode::ClipboardBusy,
                "Clipboard is busy; trigger preserved"
            ))
        );
        assert_eq!(clipboard.health(), BUSY);
    }
    #[test]
    fn slow_critical_section_degrades_for_the_rest_of_the_lease() {
        let clipboard = MockClipboard::new(7, text("original"));
        let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
        assert_eq!(clipboard.health(), HEALTHY);
        let mut prepared = lease.prepare(text("expansion")).unwrap();
        // Locks: 0 backup, 1 its verification, 2 paste write, 3 its
        // verification (slow, over our own data: reported, not enforced),
        // 4 restore verification (fast again), 5 restore write.
        clipboard.set_read_delay(LOCK_HOLD_BUDGET + Duration::from_millis(1));
        let script = clipboard.clone();
        clipboard.on_lock(move |index| {
            if index == 4 {
                script.set_read_delay(Duration::ZERO);
            }
        });
        lease.write_prepared(&mut prepared).unwrap();
        assert_eq!(clipboard.health(), BUSY);
        lease.restore().unwrap();
        assert_eq!(clipboard.health(), BUSY, "a fast section must not hide it");
        assert_eq!(clipboard.content(), text("original"));
        drop(lease);
        // The next lease whose sections are all fast reports Healthy again.
        drop(ClipboardLease::new(clipboard.clone()).unwrap());
        assert_eq!(clipboard.health(), HEALTHY);
    }
    #[test]
    fn blocked_read_times_out_and_the_worker_never_writes() {
        let mut original = text("original");
        original.push((0xC123, vec![1, 2, 3]));
        let clipboard = MockClipboard::new(7, original.clone());
        let sequence = clipboard.sequence_now();
        // The first read waits on an owner that does not render it.
        let release = clipboard.block_next_read();
        let error = refused(open_with(&clipboard, RestorePolicy::BestEffort));
        assert_eq!(error.reason, ReasonCode::ClipboardBusy);
        assert_eq!(
            error.message,
            "Clipboard data did not arrive in time; trigger preserved"
        );
        assert_eq!(clipboard.waits(), [READ_WAIT], "one bounded wait");
        assert_eq!(clipboard.health(), BUSY);
        // The abandoned worker still holds the clipboard open in that read.
        assert!(clipboard.read_parked());
        assert!(clipboard.is_open());
        assert!(clipboard.locked(|| ()).is_none());
        release.send(()).unwrap();
        assert!(clipboard.await_closed(Duration::from_secs(5)));
        // It stopped after that read and never wrote.
        assert_eq!(clipboard.reads(), 1);
        assert_eq!(clipboard.content(), original);
        assert_eq!(clipboard.sequence_now(), sequence);
        assert_eq!(clipboard.owner_now(), 7);
        assert_eq!(clipboard.clears(), 0);
    }
    #[test]
    fn reads_that_add_up_past_the_budget_stop_the_backup() {
        let mut original = text("original");
        original.extend([(0xC001, vec![1]), (0xC002, vec![2]), (0xC003, vec![3])]);
        let clipboard = MockClipboard::new(7, original.clone());
        // Each read alone is within the budget; the third crosses it.
        clipboard.set_read_delay(Duration::from_millis(100));
        for policy in [RestorePolicy::Full, RestorePolicy::BestEffort] {
            let reads = clipboard.reads();
            let error = refused(open_with(&clipboard, policy));
            assert_eq!(error.reason, ReasonCode::ClipboardBusy, "{policy:?}");
            assert_eq!(
                error.message,
                "Clipboard data took too long to read; trigger preserved"
            );
            assert_eq!(
                clipboard.reads() - reads,
                3,
                "{policy:?}: no partial backup"
            );
            assert_eq!(clipboard.health(), BUSY);
            assert!(!clipboard.is_open());
        }
        // The verification section is held to the same budget.
        let clipboard = MockClipboard::new(7, original.clone());
        let script = clipboard.clone();
        clipboard.on_lock(move |index| {
            if index == 1 {
                script.set_read_delay(Duration::from_millis(100));
            }
        });
        let error = refused(ClipboardLease::open(
            clipboard.clone(),
            LeaseOptions::default(),
        ));
        assert_eq!(error, overrun());
        assert_eq!(clipboard.health(), BUSY);
        assert_eq!(clipboard.clears(), 0);
        assert_eq!(clipboard.content(), original);
    }
    #[test]
    fn restore_retries_busy_clipboard_and_keeps_backup_for_drop() {
        let clipboard = MockClipboard::new(7, text("original"));
        let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
        let mut prepared = lease.prepare(text("expansion")).unwrap();
        lease.write_prepared(&mut prepared).unwrap();
        assert_eq!(clipboard.content(), pasted(&clipboard, "expansion"));
        clipboard.set_busy(OPEN_ATTEMPTS);
        assert_eq!(lease.restore(), Err(busy()));
        assert_eq!(clipboard.content(), pasted(&clipboard, "expansion"));
        assert_eq!(clipboard.health(), BUSY);
        clipboard.set_busy(OPEN_ATTEMPTS - 1);
        drop(lease);
        assert_eq!(clipboard.content(), text("original"));
        assert_eq!(clipboard.clears(), 2);
    }
    #[test]
    fn clipboard_busy_fault_holds_for_its_duration() {
        let start = Instant::now();
        let mut until = None;
        assert!(!fault_busy_window(None, &mut until, start));
        assert!(fault_busy_window(Some(250), &mut until, start));
        assert!(fault_busy_window(
            None,
            &mut until,
            start + Duration::from_millis(249)
        ));
        assert!(!fault_busy_window(
            None,
            &mut until,
            start + Duration::from_millis(250)
        ));
    }
    #[test]
    fn clipboard_health_keeps_entry_time_until_the_state_changes() {
        record_health(HealthState::Degraded, Some(ReasonCode::ClipboardBusy));
        let degraded = clipboard_health();
        assert_eq!(degraded.component, Component::Clipboard);
        assert_eq!(degraded.state, HealthState::Degraded);
        assert_eq!(degraded.reason, Some(ReasonCode::ClipboardBusy));
        record_health(HealthState::Degraded, Some(ReasonCode::ClipboardBusy));
        assert_eq!(clipboard_health().since_ms, degraded.since_ms);
        record_health(HealthState::Healthy, None);
        let healthy = clipboard_health();
        assert_eq!(
            (healthy.state, healthy.reason),
            (HealthState::Healthy, None)
        );
        assert!(healthy.since_ms >= degraded.since_ms);
    }
    /// The probe's read never waits (A2): `None` while a lease holds the
    /// health, the health itself otherwise.
    #[test]
    fn try_clipboard_health_never_waits_for_a_lease() {
        let held = health_cell().lock().unwrap_or_else(PoisonError::into_inner);
        let reader = std::thread::spawn(try_clipboard_health);
        assert!(reader.join().unwrap().is_none(), "held by a lease");
        drop(held);
        let read = try_clipboard_health().expect("free");
        assert_eq!(read.component, Component::Clipboard);
    }
    /// Before the health exists the probe's read is `None` without creating
    /// it (Codex B1): `get`, not `get_or_init`. A cell of its own, since the
    /// process-wide one is created by whichever test touches it first.
    #[test]
    fn try_health_never_creates_the_health() {
        let cell = OnceLock::new();
        assert!(try_health(&cell).is_none());
        assert!(cell.get().is_none(), "the read left it uncreated");
        let created = cell.get_or_init(|| {
            Mutex::new(ComponentHealth {
                component: Component::Clipboard,
                state: HealthState::Degraded,
                reason: Some(ReasonCode::ClipboardBusy),
                since_ms: 1,
            })
        });
        let read = try_health(&cell).expect("created and free");
        assert_eq!(
            (read.state, read.reason),
            (HealthState::Degraded, Some(ReasonCode::ClipboardBusy))
        );
        let held = created.lock().unwrap();
        let read = std::thread::scope(|scope| scope.spawn(|| try_health(&cell)).join().unwrap());
        assert!(read.is_none(), "held by a lease");
        drop(held);
    }

    fn open_with(
        clipboard: &MockClipboard,
        policy: RestorePolicy,
    ) -> Result<ClipboardLease<MockClipboard>, LeaseError> {
        ClipboardLease::open(
            clipboard.clone(),
            LeaseOptions {
                policy,
                ..LeaseOptions::default()
            },
        )
    }
    /// Pastes through `lease`, then restores the backup.
    fn paste_and_restore(mut lease: ClipboardLease<MockClipboard>, clipboard: &MockClipboard) {
        let mut prepared = lease.prepare(text("expansion")).unwrap();
        lease.write_prepared(&mut prepared).unwrap();
        assert_eq!(clipboard.content(), pasted(clipboard, "expansion"));
        lease.restore().unwrap();
    }
    /// What a paste write of `value` leaves: the text, then the exclusion
    /// markers, each a DWORD 0.
    fn pasted(clipboard: &MockClipboard, value: &str) -> ClipboardFormats {
        let mut formats = text(value);
        for name in EXCLUSION_FORMATS {
            formats.push((clipboard.registered(name).unwrap(), vec![0; 4]));
        }
        formats
    }
    fn refused(result: Result<ClipboardLease<MockClipboard>, LeaseError>) -> LeaseError {
        match result {
            Ok(_) => panic!("lease must be refused"),
            Err(error) => error,
        }
    }
    fn refusal_of(result: Result<ClipboardLease<MockClipboard>, LeaseError>) -> ReasonCode {
        refused(result).reason
    }
    #[test]
    fn handling_follows_the_b08_format_table() {
        use Handling::{Copy, Covered, Ignore, Skip, Unknown};
        let lossy = [RestorePolicy::Full, RestorePolicy::BestEffort];
        for policy in lossy {
            for format in [
                1, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 15, 16, 17, 0xC000, 0xFFFF,
            ] {
                assert_eq!(handling(format, policy), Copy, "{format:#x}");
            }
            for format in [9, 0x80, 0x81, 0x8E, 0x8F, 0x200, 0x2FF, 0x300, 0x3FF] {
                assert_eq!(handling(format, policy), Skip, "{format:#x}");
            }
            for format in [0, 18, 0x7F, 0x90, 0x1FF, 0x400, 0xBFFF] {
                assert_eq!(handling(format, policy), Unknown, "{format:#x}");
            }
            assert_eq!(handling(CF_BITMAP, policy), Covered(&[CF_DIB, CF_DIBV5]));
            assert_eq!(
                handling(CF_METAFILEPICT, policy),
                Covered(&[CF_ENHMETAFILE])
            );
        }
        for format in [CF_UNICODETEXT, CF_TEXT, CF_OEMTEXT, CF_LOCALE] {
            assert_eq!(handling(format, RestorePolicy::TextOnly), Copy);
        }
        for format in [2, 3, 8, 9, 14, 15, 17, 0x80, 0x200, 0x300, 0xC000] {
            assert_eq!(
                handling(format, RestorePolicy::TextOnly),
                Ignore,
                "{format:#x}"
            );
        }
    }
    #[test]
    fn best_effort_restores_byte_formats_and_records_handle_formats() {
        let dib = vec![(8, vec![40, 0, 0, 0, 1, 2, 3, 4]), (17, vec![124, 0, 9, 9])];
        let mut original = text("original");
        original.extend(dib.clone());
        original.extend([
            (CF_BITMAP, vec![0xB1]),
            (CF_PALETTE, vec![0x9A]),
            (0x82, vec![0xD5]),
            (0x200, vec![0x22]),
            (0x300, vec![0x33]),
            (0xC123, vec![5, 6, 7]),
        ]);
        let clipboard = MockClipboard::new(7, original);
        let lease = open_with(&clipboard, RestorePolicy::BestEffort).unwrap();
        assert_eq!(lease.skipped_formats(), [CF_PALETTE, 0x82, 0x200, 0x300]);
        paste_and_restore(lease, &clipboard);
        let mut restored = text("original");
        restored.extend(dib);
        restored.push((0xC123, vec![5, 6, 7]));
        assert_eq!(
            clipboard.content(),
            restored,
            "CF_DIB/CF_DIBV5 byte-identical"
        );
    }
    #[test]
    fn full_refuses_a_clipboard_it_cannot_restore_before_writing() {
        let mut original = text("original");
        original.push((CF_PALETTE, vec![0x9A]));
        let clipboard = MockClipboard::new(7, original.clone());
        let refused = open_with(&clipboard, RestorePolicy::Full);
        assert_eq!(refusal_of(refused), ReasonCode::ClipboardFormatSkipped);
        assert_eq!(clipboard.clears(), 0);
        assert_eq!(clipboard.content(), original);
        // CF_BITMAP is re-synthesized from CF_DIB: Full accepts the pair.
        let screenshot = vec![(CF_BITMAP, vec![0xB1]), (8, vec![40, 0, 0, 0, 7])];
        let clipboard = MockClipboard::new(7, screenshot);
        let lease = open_with(&clipboard, RestorePolicy::Full).unwrap();
        assert!(lease.skipped_formats().is_empty());
        paste_and_restore(lease, &clipboard);
        assert_eq!(clipboard.content(), vec![(8, vec![40, 0, 0, 0, 7])]);
    }
    #[test]
    fn text_only_keeps_text_formats_and_records_nothing() {
        let mut original = text("original");
        original.extend([
            (CF_TEXT, b"original\0".to_vec()),
            (8, vec![40, 0, 0, 0]),
            (CF_PALETTE, vec![0x9A]),
            (CF_LOCALE, 0x0411u32.to_le_bytes().to_vec()),
            (0xC123, vec![1]),
        ]);
        let clipboard = MockClipboard::new(7, original);
        let lease = open_with(&clipboard, RestorePolicy::TextOnly).unwrap();
        assert!(lease.skipped_formats().is_empty());
        paste_and_restore(lease, &clipboard);
        let mut restored = text("original");
        restored.extend([
            (CF_TEXT, b"original\0".to_vec()),
            (CF_LOCALE, 0x0411u32.to_le_bytes().to_vec()),
        ]);
        assert_eq!(clipboard.content(), restored);
    }
    #[test]
    fn enhanced_metafile_is_restored_and_covers_its_metafilepict() {
        let emf = (CF_ENHMETAFILE, vec![1, 0, 0, 0, 0x6C, 0, 0, 0]);
        let rich = vec![
            (0xC0F0, b"{\\rtf1}".to_vec()),
            emf.clone(),
            (CF_METAFILEPICT, vec![3]),
        ];
        for policy in [RestorePolicy::Full, RestorePolicy::BestEffort] {
            let clipboard = MockClipboard::new(7, rich.clone());
            let lease = open_with(&clipboard, policy).unwrap();
            assert!(lease.skipped_formats().is_empty(), "{policy:?}");
            paste_and_restore(lease, &clipboard);
            assert_eq!(clipboard.content(), rich[..2]);
        }
    }
    #[test]
    fn synthesized_format_without_its_source_refuses() {
        // Without the CF_ENHMETAFILE (or a CF_DIB/CF_DIBV5) nothing
        // re-synthesizes it, and BestEffort may not drop it.
        let lone = [(CF_METAFILEPICT, vec![3]), (CF_BITMAP, vec![0xB1])];
        for format in lone {
            let mut original = text("caption");
            original.push(format.clone());
            let clipboard = MockClipboard::new(7, original.clone());
            for policy in [RestorePolicy::Full, RestorePolicy::BestEffort] {
                assert_eq!(
                    refusal_of(open_with(&clipboard, policy)),
                    ReasonCode::ClipboardFormatSkipped,
                    "{:#x} {policy:?}",
                    format.0
                );
            }
            // TextOnly drops it with every other non-text format.
            let lease = open_with(&clipboard, RestorePolicy::TextOnly).unwrap();
            assert!(lease.skipped_formats().is_empty());
            drop(lease);
            assert_eq!(clipboard.clears(), 0);
            assert_eq!(clipboard.content(), original);
        }
        // A source that is only skipped (too large) covers nothing either.
        let mut original = text("caption");
        original.extend([
            (CF_ENHMETAFILE, vec![1, 0, 0, 0]),
            (CF_METAFILEPICT, vec![3]),
        ]);
        let clipboard = MockClipboard::new(7, original);
        clipboard.report_size(CF_ENHMETAFILE, MAX_FORMAT_BYTES + 1);
        assert_eq!(
            refusal_of(open_with(&clipboard, RestorePolicy::BestEffort)),
            ReasonCode::ClipboardTooLarge
        );
    }
    #[test]
    fn unknown_format_refuses_unless_text_only_drops_it() {
        let mut original = text("original");
        original.push((0x400, vec![4]));
        let clipboard = MockClipboard::new(7, original.clone());
        for policy in [RestorePolicy::Full, RestorePolicy::BestEffort] {
            assert_eq!(
                refusal_of(open_with(&clipboard, policy)),
                ReasonCode::ClipboardFormatSkipped,
                "{policy:?}"
            );
        }
        let lease = open_with(&clipboard, RestorePolicy::TextOnly).unwrap();
        assert!(lease.skipped_formats().is_empty());
        drop(lease);
        assert_eq!(clipboard.content(), original);
    }
    #[test]
    fn unreadable_format_refuses_under_every_policy() {
        // An enumerated format that renders no data (GetClipboardData NULL).
        let mut original = text("original");
        original.push((0xC123, vec![]));
        let clipboard = MockClipboard::new(7, original.clone());
        for policy in [RestorePolicy::Full, RestorePolicy::BestEffort] {
            let error = refused(open_with(&clipboard, policy));
            assert_eq!(error.reason, ReasonCode::Internal, "{policy:?}");
            assert_eq!(
                error.message,
                "Clipboard format 0xc123 could not be read; trigger preserved"
            );
        }
        // Data that cannot be locked, including a text format under TextOnly.
        let clipboard = MockClipboard::new(7, text("original"));
        clipboard.fail_read(CF_UNICODETEXT);
        for policy in [
            RestorePolicy::Full,
            RestorePolicy::BestEffort,
            RestorePolicy::TextOnly,
        ] {
            assert_eq!(
                refusal_of(open_with(&clipboard, policy)),
                ReasonCode::Internal,
                "{policy:?}"
            );
        }
        assert_eq!(clipboard.clears(), 0);
        assert_eq!(clipboard.content(), text("original"));
    }
    #[test]
    fn best_effort_refuses_to_restore_a_non_empty_clipboard_to_nothing() {
        // Only formats the table skips.
        let handles = vec![(CF_PALETTE, vec![0x9A]), (0x200, vec![0x22])];
        let clipboard = MockClipboard::new(7, handles.clone());
        assert_eq!(
            refusal_of(open_with(&clipboard, RestorePolicy::BestEffort)),
            ReasonCode::ClipboardFormatSkipped
        );
        assert_eq!(clipboard.content(), handles);
        // Only a format over the size limit.
        let clipboard = MockClipboard::new(7, vec![(0xC0C0, vec![7; 16])]);
        clipboard.report_size(0xC0C0, MAX_FORMAT_BYTES + 1);
        assert_eq!(
            refusal_of(open_with(&clipboard, RestorePolicy::BestEffort)),
            ReasonCode::ClipboardTooLarge
        );
        // TextOnly opted into dropping every non-text format.
        let lease = open_with(&clipboard, RestorePolicy::TextOnly).unwrap();
        assert!(lease.skipped_formats().is_empty());
        drop(lease);
        // An empty clipboard is restored to empty.
        let clipboard = MockClipboard::new(7, vec![]);
        let lease = open_with(&clipboard, RestorePolicy::BestEffort).unwrap();
        paste_and_restore(lease, &clipboard);
        assert_eq!(clipboard.content(), ClipboardFormats::new());
    }
    /// A refused backup names what it would have lost (for the history's
    /// skipped formats): the skips up to the refusal plus the formats that
    /// refused it. Every other failure names none.
    #[test]
    fn refusals_name_the_formats_they_would_have_lost() {
        let formats_of = |content: ClipboardFormats, policy, large: Option<u32>| {
            let clipboard = MockClipboard::new(7, content);
            if let Some(format) = large {
                clipboard.report_size(format, MAX_FORMAT_BYTES + 1);
            }
            let error = refused(open_with(&clipboard, policy));
            (error.reason, error.formats)
        };
        let with_text = |extra: &[(u32, Vec<u8>)]| {
            let mut content = text("original");
            content.extend_from_slice(extra);
            content
        };
        use RestorePolicy::{BestEffort, Full};
        // Full stops at its first loss.
        assert_eq!(
            formats_of(
                with_text(&[(CF_PALETTE, vec![0x9A]), (0x200, vec![0x22])]),
                Full,
                None
            ),
            (ReasonCode::ClipboardFormatSkipped, vec![CF_PALETTE])
        );
        assert_eq!(
            formats_of(with_text(&[(0xC0C0, vec![7; 16])]), Full, Some(0xC0C0)),
            (ReasonCode::ClipboardTooLarge, vec![0xC0C0])
        );
        // An unknown handle type, after a skip BestEffort accepted.
        assert_eq!(
            formats_of(
                with_text(&[(CF_PALETTE, vec![0x9A]), (0x400, vec![4])]),
                BestEffort,
                None
            ),
            (ReasonCode::ClipboardFormatSkipped, vec![CF_PALETTE, 0x400])
        );
        // A synthesized format without its source.
        assert_eq!(
            formats_of(with_text(&[(CF_METAFILEPICT, vec![3])]), BestEffort, None),
            (ReasonCode::ClipboardFormatSkipped, vec![CF_METAFILEPICT])
        );
        // ... whose source was too large to keep.
        assert_eq!(
            formats_of(
                with_text(&[
                    (CF_ENHMETAFILE, vec![1, 0, 0, 0]),
                    (CF_METAFILEPICT, vec![3])
                ]),
                BestEffort,
                Some(CF_ENHMETAFILE)
            ),
            (
                ReasonCode::ClipboardTooLarge,
                vec![CF_ENHMETAFILE, CF_METAFILEPICT]
            )
        );
        // Nothing restorable was left.
        assert_eq!(
            formats_of(
                vec![(CF_PALETTE, vec![0x9A]), (0x200, vec![0x22])],
                BestEffort,
                None
            ),
            (ReasonCode::ClipboardFormatSkipped, vec![CF_PALETTE, 0x200])
        );
        // Failures that are not refusals carry no formats.
        let unreadable = with_text(&[(0xC123, vec![])]);
        assert_eq!(
            formats_of(unreadable, BestEffort, None),
            (ReasonCode::Internal, vec![])
        );
        let clipboard = MockClipboard::new(7, text("original"));
        clipboard.set_busy(OPEN_ATTEMPTS);
        let error = refused(open_with(&clipboard, BestEffort));
        assert_eq!(
            (error.reason, error.formats),
            (ReasonCode::ClipboardBusy, vec![])
        );
    }
    #[test]
    fn hung_owner_refuses_before_opening_the_clipboard() {
        for policy in [
            RestorePolicy::Full,
            RestorePolicy::BestEffort,
            RestorePolicy::TextOnly,
        ] {
            let clipboard = MockClipboard::new(7, text("original"));
            clipboard.set_owner_hung(true);
            let error = refused(open_with(&clipboard, policy));
            assert_eq!(error.reason, ReasonCode::ClipboardBusy, "{policy:?}");
            assert_eq!(
                error.message,
                "Clipboard owner is not responding; trigger preserved"
            );
            assert_eq!(clipboard.health(), BUSY);
            assert_eq!(clipboard.reads(), 0);
            assert_eq!(clipboard.clears(), 0);
            assert_eq!(clipboard.content(), text("original"));
        }
    }
    #[test]
    fn owner_change_before_the_backup_lock_takes_a_fresh_probe() {
        let clipboard = MockClipboard::new(7, text("original"));
        let script = clipboard.clone();
        // A copy lands between the owner probe and the backup lock.
        clipboard.on_lock(move |index| {
            if index == 0 {
                script.take_over(8, text("newer"));
            }
        });
        let lease = ClipboardLease::new(clipboard.clone()).unwrap();
        paste_and_restore(lease, &clipboard);
        assert_eq!(clipboard.content(), text("newer"));
        // An owner that changes before every lock refuses the lease.
        let clipboard = MockClipboard::new(7, text("original"));
        let script = clipboard.clone();
        clipboard.on_lock(move |index| script.take_over(10 + index as u64, text("churn")));
        let refused = ClipboardLease::open(clipboard.clone(), LeaseOptions::default());
        assert_eq!(refusal_of(refused), ReasonCode::ClipboardChanged);
        assert_eq!(clipboard.clears(), 0);
    }
    #[test]
    fn lease_options_default_to_best_effort_after_300_ms() {
        let options = LeaseOptions::default();
        assert_eq!(options.policy, RestorePolicy::BestEffort);
        assert_eq!(options.restore_delay, Duration::from_millis(300));
        // `new` opens with the app's options; no unit test sets them (they
        // are process-wide), so they are still the defaults here. Setting
        // them is tested in tests/clipboard_options.rs, its own process.
        assert_eq!(lease_options(), options);
        let lease = ClipboardLease::new(MockClipboard::new(7, text("original"))).unwrap();
        assert_eq!(lease.restore_delay(), Duration::from_millis(300));
        assert!(lease.skipped_formats().is_empty());
        let names = [
            RestorePolicy::Full,
            RestorePolicy::BestEffort,
            RestorePolicy::TextOnly,
        ]
        .map(|policy| serde_json::to_string(&policy).unwrap());
        assert_eq!(names, ["\"full\"", "\"best_effort\"", "\"text_only\""]);
    }
    #[test]
    fn paste_write_carries_exclusion_markers_and_restore_does_not() {
        let clipboard = MockClipboard::new(7, text("original"));
        let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
        let mut prepared = lease.prepare(text("expansion")).unwrap();
        lease.write_prepared(&mut prepared).unwrap();
        let written = clipboard.content();
        assert_eq!(written.len(), 1 + EXCLUSION_FORMATS.len());
        for name in [
            "ExcludeClipboardContentFromMonitorProcessing",
            "CanIncludeInClipboardHistory",
            "CanUploadToCloudClipboard",
        ] {
            let id = clipboard.registered(name).unwrap();
            assert!(
                written.contains(&(id, 0u32.to_le_bytes().to_vec())),
                "{name}"
            );
        }
        lease.restore().unwrap();
        assert_eq!(clipboard.content(), text("original"));
    }

    fn formats_changed() -> LeaseError {
        LeaseError::new(
            ReasonCode::ClipboardChanged,
            "Clipboard formats changed externally; clipboard preserved",
        )
    }
    fn payload_changed() -> LeaseError {
        LeaseError::new(
            ReasonCode::ClipboardChanged,
            "Clipboard payload changed externally; clipboard preserved",
        )
    }
    /// A lease on `clipboard` that has written the paste of "expansion".
    fn written_lease(clipboard: &MockClipboard) -> ClipboardLease<MockClipboard> {
        synthesized_lease(clipboard, text("expansion"), vec![])
    }
    /// A lease on `clipboard` that has written the paste of `payload`, with
    /// the system listing `synthesized` once the write closed the clipboard.
    fn synthesized_lease(
        clipboard: &MockClipboard,
        payload: ClipboardFormats,
        synthesized: ClipboardFormats,
    ) -> ClipboardLease<MockClipboard> {
        let script = clipboard.clone();
        let mut synthesized = Some(synthesized);
        // Lock 2 writes the paste, lock 3 verifies it and captures the
        // baseline.
        clipboard.on_lock(move |index| {
            if index == 3 {
                for (format, bytes) in synthesized.take().unwrap_or_default() {
                    script.add_format(format, bytes);
                }
            }
        });
        let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
        let mut prepared = lease.prepare(payload).unwrap();
        lease.write_prepared(&mut prepared).unwrap();
        lease
    }
    /// What the system lists for a CF_UNICODETEXT "expansion".
    fn text_synthesized() -> ClipboardFormats {
        vec![
            (CF_LOCALE, 0x0409u32.to_le_bytes().to_vec()),
            (CF_TEXT, b"expansion\0".to_vec()),
            (CF_OEMTEXT, b"expansion\0".to_vec()),
        ]
    }
    #[test]
    fn synthesized_whitelist_covers_only_conversions_of_written_formats() {
        let unicode = text("x");
        for format in [CF_TEXT, CF_OEMTEXT, CF_LOCALE] {
            assert!(synthesized_from(format, &unicode), "{format}");
        }
        for format in [CF_UNICODETEXT, CF_BITMAP, CF_DIB, 0xC000] {
            assert!(!synthesized_from(format, &unicode), "{format}");
        }
        let dib = vec![(CF_DIB, vec![0; 40])];
        for format in [CF_BITMAP, CF_PALETTE, CF_DIBV5] {
            assert!(synthesized_from(format, &dib), "{format}");
        }
        // Nothing is synthesized from CF_HDROP or a registered format.
        let files = vec![(15, vec![0; 22]), (0xC000, vec![0; 4])];
        assert!(!(1..0x10000).any(|format| synthesized_from(format, &files)));
    }
    #[test]
    fn restore_refuses_a_format_added_without_emptying_the_clipboard() {
        // Explicit restore and then Drop, or Drop alone: neither overwrites.
        for explicit in [true, false] {
            let clipboard = MockClipboard::new(7, text("original"));
            let mut lease = written_lease(&clipboard);
            // Another application's SetClipboardData without EmptyClipboard:
            // the owner and the lease's payload stay as they were.
            let theirs = clipboard.register_format("Another application's format");
            clipboard.add_format(theirs, vec![1, 2, 3]);
            let mut expected = pasted(&clipboard, "expansion");
            expected.push((theirs, vec![1, 2, 3]));
            assert_eq!(clipboard.owner_now(), MockClipboard::LEASE);
            if explicit {
                assert_eq!(lease.restore(), Err(formats_changed()));
                // A comparison that opened the clipboard and refused is as
                // final as a failed capture.
                assert_eq!(lease.baseline, Capture::Failed);
                assert_eq!(clipboard.content(), expected);
            }
            drop(lease);
            assert_eq!(clipboard.content(), expected, "explicit: {explicit}");
            assert_eq!(clipboard.clears(), 1, "explicit: {explicit}");
        }
    }
    #[test]
    fn restore_accepts_formats_the_system_synthesizes_from_the_paste() {
        let clipboard = MockClipboard::new(7, text("original"));
        // Listed as soon as the write closes the clipboard, so the capture
        // records them with their bytes.
        let mut lease = synthesized_lease(&clipboard, text("expansion"), text_synthesized());
        let mut formats: Vec<u32> = pasted(&clipboard, "expansion")
            .iter()
            .map(|(format, _)| *format)
            .collect();
        formats.extend([CF_LOCALE, CF_TEXT, CF_OEMTEXT]);
        let baseline = Baseline {
            formats,
            derived: text_synthesized(),
        };
        assert_eq!(lease.baseline, Capture::Captured(baseline));
        // A later read (a listener rendering CF_TEXT) advances the sequence;
        // the format set and every byte are unchanged.
        clipboard.bump_sequence();
        lease.restore().unwrap();
        assert_eq!(clipboard.content(), text("original"));
        assert_eq!(clipboard.clears(), 2);
    }
    #[test]
    fn restore_refuses_a_derived_format_rewritten_in_place() {
        // Another application's SetClipboardData without EmptyClipboard on a
        // format the baseline holds: same format set, other bytes. Explicit
        // restore and then Drop, or Drop alone: neither overwrites.
        let rewrites = [
            (CF_TEXT, b"unrelated\0".to_vec()),
            (CF_LOCALE, 0x0804u32.to_le_bytes().to_vec()),
        ];
        for (format, bytes) in rewrites {
            for explicit in [true, false] {
                let case = format!("{format}, explicit: {explicit}");
                let clipboard = MockClipboard::new(7, text("original"));
                let mut lease =
                    synthesized_lease(&clipboard, text("expansion"), text_synthesized());
                clipboard.add_format(format, bytes.clone());
                let expected = clipboard.content();
                assert_eq!(clipboard.owner_now(), MockClipboard::LEASE);
                if explicit {
                    assert_eq!(lease.restore(), Err(payload_changed()), "{case}");
                    assert_eq!(clipboard.content(), expected, "{case}");
                }
                drop(lease);
                assert_eq!(clipboard.content(), expected, "{case}");
                assert_eq!(clipboard.clears(), 1, "{case}");
            }
        }
    }
    #[test]
    fn restore_refuses_a_synthesizable_format_added_after_the_baseline() {
        let clipboard = MockClipboard::new(7, text("original"));
        let mut lease = written_lease(&clipboard);
        // Exactly what the system would synthesize, but the baseline has no
        // CF_TEXT: it can only come from another application.
        clipboard.add_format(CF_TEXT, b"expansion\0".to_vec());
        let expected = clipboard.content();
        assert_eq!(lease.restore(), Err(formats_changed()));
        drop(lease);
        assert_eq!(clipboard.content(), expected);
        assert_eq!(clipboard.clears(), 1);
    }
    #[test]
    fn handle_formats_are_held_to_presence_only() {
        // An image paste: the system lists CF_BITMAP and CF_PALETTE (GDI
        // handles, never read as bytes) and CF_DIBV5 (bytes) from the CF_DIB.
        let image = vec![(CF_DIB, vec![40, 0, 0, 0, 0x10, 0x20])];
        let dibv5 = (CF_DIBV5, vec![124, 0, 0, 0, 0x10, 0x20]);
        let synthesized = vec![
            (CF_BITMAP, vec![0xB1]),
            (CF_PALETTE, vec![0x9A]),
            dibv5.clone(),
        ];
        for (rewritten, refused) in [(CF_BITMAP, false), (CF_PALETTE, false), (CF_DIBV5, true)] {
            let clipboard = MockClipboard::new(7, text("original"));
            let mut lease = synthesized_lease(&clipboard, image.clone(), synthesized.clone());
            let Capture::Captured(baseline) = &lease.baseline else {
                panic!("baseline captured");
            };
            assert_eq!(baseline.derived, vec![dibv5.clone()]);
            clipboard.add_format(rewritten, vec![0xEE]);
            if refused {
                assert_eq!(lease.restore(), Err(payload_changed()));
                assert_eq!(clipboard.clears(), 1);
            } else {
                lease.restore().unwrap();
                assert_eq!(clipboard.content(), text("original"), "{rewritten}");
            }
        }
    }
    #[test]
    fn restore_refuses_when_a_baseline_format_is_missing() {
        // Each of the paste's own formats: the text and every marker.
        for missing in 0..1 + EXCLUSION_FORMATS.len() {
            let clipboard = MockClipboard::new(7, text("original"));
            let mut lease = written_lease(&clipboard);
            let mut left = pasted(&clipboard, "expansion");
            left.remove(missing);
            clipboard.tamper(left.clone());
            assert_eq!(clipboard.owner_now(), MockClipboard::LEASE);
            assert_eq!(lease.restore(), Err(payload_changed()));
            drop(lease);
            assert_eq!(clipboard.content(), left, "missing #{missing}");
            assert_eq!(clipboard.clears(), 1, "missing #{missing}");
        }
        // A synthesized format the baseline recorded.
        let clipboard = MockClipboard::new(7, text("original"));
        let locale = vec![(CF_LOCALE, 0x0409u32.to_le_bytes().to_vec())];
        let mut lease = synthesized_lease(&clipboard, text("expansion"), locale);
        let left = pasted(&clipboard, "expansion");
        clipboard.tamper(left.clone());
        assert_eq!(lease.restore(), Err(formats_changed()));
        drop(lease);
        assert_eq!(clipboard.content(), left);
        assert_eq!(clipboard.clears(), 1);
    }
    #[test]
    fn paste_write_refuses_a_format_added_before_its_verification() {
        let clipboard = MockClipboard::new(7, text("original"));
        let theirs = clipboard.register_format("Another application's format");
        let script = clipboard.clone();
        // Lock 2 writes the paste, lock 3 verifies it.
        clipboard.on_lock(move |index| {
            if index == 3 {
                script.add_format(theirs, vec![1, 2, 3]);
            }
        });
        let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
        let mut prepared = lease.prepare(text("expansion")).unwrap();
        assert_eq!(lease.write_prepared(&mut prepared), Err(formats_changed()));
        assert_eq!(lease.baseline, Capture::Failed);
        assert!(lease.verify_current().is_err());
        drop(lease);
        let mut expected = pasted(&clipboard, "expansion");
        expected.push((theirs, vec![1, 2, 3]));
        assert_eq!(clipboard.content(), expected);
        assert_eq!(clipboard.clears(), 1);
    }
    #[test]
    fn paste_write_refuses_a_derived_format_it_cannot_read() {
        for explicit in [true, false] {
            let clipboard = MockClipboard::new(7, text("original"));
            let script = clipboard.clone();
            // Lock 2 writes the paste, lock 3 verifies it: CF_TEXT is listed
            // but its data cannot be read.
            clipboard.on_lock(move |index| {
                if index == 3 {
                    script.add_format(CF_TEXT, b"expansion\0".to_vec());
                    script.fail_read(CF_TEXT);
                }
            });
            let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
            let mut prepared = lease.prepare(text("expansion")).unwrap();
            assert_eq!(lease.write_prepared(&mut prepared), Err(unverifiable()));
            assert_eq!(lease.baseline, Capture::Failed);
            // Another application then replaces CF_TEXT in place with data
            // that reads, would pass the whitelist and is not the paste's.
            clipboard.add_format(CF_TEXT, b"unrelated\0".to_vec());
            let replaced = clipboard.content();
            if explicit {
                assert_eq!(lease.restore(), Err(uncaptured()));
                assert_eq!(clipboard.content(), replaced);
            }
            drop(lease);
            assert_eq!(clipboard.content(), replaced, "explicit: {explicit}");
            assert_eq!(clipboard.clears(), 1, "explicit: {explicit}");
        }
    }
    #[test]
    fn restore_after_a_partial_paste_write_checks_like_a_capture() {
        // The write fails partway, before any capture: untried, not failed,
        // and the restore still brings the original back.
        for explicit in [true, false] {
            let clipboard = MockClipboard::new(7, text("original"));
            clipboard.fail_set(1);
            let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
            let mut prepared = lease.prepare(text("expansion")).unwrap();
            assert_eq!(lease.write_prepared(&mut prepared), Err(write_failed()));
            assert_eq!(lease.baseline, Capture::Untried);
            if explicit {
                lease.restore().unwrap();
            }
            drop(lease);
            assert_eq!(
                clipboard.content(),
                text("original"),
                "explicit: {explicit}"
            );
            assert_eq!(clipboard.clears(), 2, "explicit: {explicit}");
        }
    }
    #[test]
    fn busy_capture_stays_untried_and_restores() {
        // The capture never gets the clipboard open, so it read nothing:
        // untried, not failed, and the restore still brings the original
        // back.
        for explicit in [true, false] {
            let clipboard = MockClipboard::new(7, text("original"));
            let script = clipboard.clone();
            // Lock 2 writes the paste; every attempt at capturing it is busy.
            clipboard.on_lock(move |index| {
                if index == 2 {
                    script.set_busy(OPEN_ATTEMPTS);
                }
            });
            let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
            let mut prepared = lease.prepare(text("expansion")).unwrap();
            assert_eq!(lease.write_prepared(&mut prepared), Err(busy()));
            assert_eq!(lease.baseline, Capture::Untried);
            assert_eq!(clipboard.health(), BUSY);
            if explicit {
                lease.restore().unwrap();
            }
            drop(lease);
            assert_eq!(
                clipboard.content(),
                text("original"),
                "explicit: {explicit}"
            );
            assert_eq!(clipboard.clears(), 2, "explicit: {explicit}");
        }
    }
    #[test]
    fn capture_busy_after_it_read_is_failed() {
        let clipboard = MockClipboard::new(7, text("original"));
        let script = clipboard.clone();
        // Lock 3 is the capture's first attempt: it opens and reads, the
        // sequence moves on release, and every attempt at the retry is busy.
        clipboard.on_lock(move |index| {
            if index == 3 {
                script.churn_sequence(true);
                script.set_busy(OPEN_ATTEMPTS);
            }
        });
        let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
        let mut prepared = lease.prepare(text("expansion")).unwrap();
        assert_eq!(lease.write_prepared(&mut prepared), Err(busy()));
        clipboard.churn_sequence(false);
        assert_eq!(lease.baseline, Capture::Failed);
        assert_eq!(lease.restore(), Err(uncaptured()));
        drop(lease);
        assert_eq!(clipboard.content(), pasted(&clipboard, "expansion"));
        assert_eq!(clipboard.clears(), 1);
    }
    #[test]
    fn restore_check_that_fails_after_a_busy_capture_is_final() {
        let clipboard = MockClipboard::new(7, text("original"));
        let script = clipboard.clone();
        // Lock 2 writes the paste and every attempt at capturing it is busy;
        // lock 3 is the explicit restore's check: CF_TEXT is listed but its
        // data cannot be read.
        clipboard.on_lock(move |index| match index {
            2 => script.set_busy(OPEN_ATTEMPTS),
            3 => {
                script.add_format(CF_TEXT, b"expansion\0".to_vec());
                script.fail_read(CF_TEXT);
            }
            _ => {}
        });
        let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
        let mut prepared = lease.prepare(text("expansion")).unwrap();
        assert_eq!(lease.write_prepared(&mut prepared), Err(busy()));
        assert_eq!(lease.baseline, Capture::Untried);
        assert_eq!(lease.restore(), Err(unverifiable()));
        assert_eq!(lease.baseline, Capture::Failed);
        // Another application then replaces CF_TEXT in place with data that
        // reads, would pass the whitelist and is not the paste's.
        clipboard.add_format(CF_TEXT, b"unrelated\0".to_vec());
        let replaced = clipboard.content();
        drop(lease);
        assert_eq!(clipboard.content(), replaced);
        assert_eq!(clipboard.clears(), 1);
    }
    #[test]
    fn restore_keeps_what_its_check_captured_when_its_write_is_busy() {
        // The paste write's capture never gets the clipboard open; the
        // explicit restore's check does and captures, then its write is
        // busy. Drop compares against that capture and never captures again.
        for rewritten in [true, false] {
            let clipboard = MockClipboard::new(7, text("original"));
            let script = clipboard.clone();
            // Lock 2 writes the paste and every attempt at capturing it is
            // busy; lock 3 is the restore's check, by which time the system
            // lists CF_TEXT, and every attempt at the restore write is busy.
            clipboard.on_lock(move |index| match index {
                2 => script.set_busy(OPEN_ATTEMPTS),
                3 => {
                    script.add_format(CF_TEXT, b"expansion\0".to_vec());
                    script.set_busy(OPEN_ATTEMPTS);
                }
                _ => {}
            });
            let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
            let mut prepared = lease.prepare(text("expansion")).unwrap();
            assert_eq!(lease.write_prepared(&mut prepared), Err(busy()));
            assert_eq!(lease.baseline, Capture::Untried);
            assert_eq!(lease.restore(), Err(busy()));
            let Capture::Captured(baseline) = &lease.baseline else {
                panic!("captured by the restore's check");
            };
            assert_eq!(baseline.derived, vec![(CF_TEXT, b"expansion\0".to_vec())]);
            if rewritten {
                // Rewritten in place: a fresh capture would adopt it.
                clipboard.add_format(CF_TEXT, b"unrelated\0".to_vec());
            }
            let expected = if rewritten {
                clipboard.content()
            } else {
                text("original")
            };
            drop(lease);
            assert_eq!(clipboard.content(), expected, "rewritten: {rewritten}");
            let clears = if rewritten { 1 } else { 2 };
            assert_eq!(clipboard.clears(), clears, "rewritten: {rewritten}");
        }
    }
    #[test]
    fn debug_never_shows_clipboard_bytes() {
        // B-09: a backup, a baseline and a format read print each format with
        // `<redacted len=N>` (N bytes), never the clipboard content.
        let secret = b"zqsecret\0".to_vec();
        let backup = Backup {
            formats: vec![CF_TEXT, CF_UNICODETEXT],
            data: vec![(CF_TEXT, secret.clone())],
            skipped: vec![CF_UNICODETEXT],
        };
        let baseline = Baseline {
            formats: vec![CF_TEXT],
            derived: vec![(CF_TEXT, secret.clone())],
        };
        let shown_backup = format!("{backup:?}");
        let shown_baseline = format!("{baseline:?}");
        let shown_capture = format!("{:?}", Capture::Captured(baseline));
        let shown_read = format!("{:?}", FormatRead::Data(secret));
        for shown in [&shown_backup, &shown_baseline, &shown_capture, &shown_read] {
            // Derived Debug would list the bytes ('z' 'q' = 122 113).
            assert!(!shown.contains("122, 113"), "{shown}");
            assert!(shown.contains("<redacted len=9>"), "{shown}");
        }
        let entry = format!("({CF_TEXT}, <redacted len=9>)");
        assert!(shown_backup.contains(&entry), "{shown_backup}");
        assert!(shown_backup.contains("skipped: [13]"), "{shown_backup}");
        assert_eq!(format!("{:?}", FormatRead::TooLarge), "TooLarge");
    }
}
