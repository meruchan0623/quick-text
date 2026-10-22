//! Clipboard lease over the [`ClipboardBackend`] port: backup, verified
//! writes and verified restore, plus the Windows adapter.
use super::inject::mark_external_effect;
use super::*;
use crate::fault::{self, FaultPoint};
use crate::health::{Component, ComponentHealth, HealthState, ReasonCode};
use crate::traits::ClipboardBackend;
use std::cell::Cell;
use std::sync::PoisonError;

/// Bounded `OpenClipboard` retry (B-08): clipboard managers, remote-desktop
/// clients and listeners open the clipboard briefly after every change.
pub(super) const OPEN_ATTEMPTS: usize = 10;
/// Pause between `OpenClipboard` attempts, always with the clipboard closed.
pub(super) const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(15);
/// A critical section holding the clipboard open longer than this degrades
/// the clipboard component: every other `OpenClipboard` fails meanwhile.
pub(super) const LOCK_HOLD_BUDGET: Duration = Duration::from_millis(250);
/// Pause between ownership re-verification attempts (clipboard closed).
const VERIFY_RETRY_INTERVAL: Duration = Duration::from_millis(5);

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
/// bounded retry or held it open too long; Healthy again after the next
/// lease whose critical sections all completed in time.
pub fn clipboard_health() -> ComponentHealth {
    health_cell()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
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
/// message the action result has always carried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LeaseError {
    pub(super) reason: ReasonCode,
    pub(super) message: String,
}
impl LeaseError {
    fn new(reason: ReasonCode, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}
impl From<LeaseError> for String {
    fn from(error: LeaseError) -> Self {
        error.message
    }
}

/// Runs `f` with the clipboard open. The critical section is only
/// open → read/write → close: callers never sleep, wait or inject input in
/// `f`. A busy clipboard is retried `OPEN_ATTEMPTS` times with the clipboard
/// closed in between. Staying busy, or holding it open longer than
/// `LOCK_HOLD_BUDGET`, reports the component Degraded (`ClipboardBusy`) and
/// keeps it Degraded for the rest of the lease (`degraded`).
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
            if held > LOCK_HOLD_BUDGET {
                tracing::warn!(
                    event = "clipboard.slow",
                    held_ms = held.as_millis() as u64,
                    budget_ms = LOCK_HOLD_BUDGET.as_millis() as u64
                );
                degraded.set(true);
                backend.report_health(HealthState::Degraded, Some(ReasonCode::ClipboardBusy));
            } else if !degraded.get() {
                backend.report_health(HealthState::Healthy, None);
            }
            return Ok(result);
        }
    }
    tracing::warn!(event = "clipboard.busy", attempts = OPEN_ATTEMPTS as u64);
    degraded.set(true);
    backend.report_health(HealthState::Degraded, Some(ReasonCode::ClipboardBusy));
    Err(LeaseError::new(
        ReasonCode::ClipboardBusy,
        "Clipboard is busy",
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
pub(super) fn acquire_clipboard<T>(
    plan: &ActionPlan,
    acquire: impl FnOnce() -> Result<T, String>,
) -> Result<Option<T>, String> {
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
            let img = image::open(path).map_err(|e| e.to_string())?.to_rgba8();
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
                wide("QuickerText Clipboard").as_ptr(),
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
/// Global allocation not yet handed to the clipboard; freed on drop.
pub(super) struct WinStaged(HGLOBAL);
impl WinStaged {
    fn into_raw(mut self) -> HGLOBAL {
        std::mem::replace(&mut self.0, std::ptr::null_mut())
    }
}
impl Drop for WinStaged {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                GlobalFree(self.0);
            }
        }
    }
}
impl ClipboardBackend for WinClipboard {
    type Staged = WinStaged;
    fn identity(&self) -> u64 {
        self.window as u64
    }
    fn owner(&self) -> u64 {
        unsafe { GetClipboardOwner() as u64 }
    }
    fn sequence(&self) -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }
    fn locked<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let fault_busy = fault_busy_window(
            fault::take(FaultPoint::ClipboardBusy),
            &mut FAULT_BUSY_UNTIL
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Instant::now(),
        );
        if fault_busy {
            return None;
        }
        unsafe {
            if OpenClipboard(self.window) == 0 {
                return None;
            }
            let result = f();
            CloseClipboard();
            Some(result)
        }
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
    fn read(&self, format: u32, limit: usize) -> Result<Option<Vec<u8>>, String> {
        unsafe {
            let handle = GetClipboardData(format);
            if handle.is_null() {
                return Ok(None);
            }
            let size = GlobalSize(handle);
            if size == 0 || size > limit {
                return Ok(None);
            }
            let pointer = GlobalLock(handle);
            if pointer.is_null() {
                return Err("Clipboard data lock failed".into());
            }
            let bytes = std::slice::from_raw_parts(pointer as *const u8, size).to_vec();
            GlobalUnlock(handle);
            Ok(Some(bytes))
        }
    }
    fn clear(&self) -> bool {
        unsafe { EmptyClipboard() != 0 }
    }
    fn stage(&self, bytes: &[u8]) -> Result<(WinStaged, Vec<u8>), String> {
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len());
            if handle.is_null() {
                return Err("Clipboard allocation failed".into());
            }
            let staged = WinStaged(handle);
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
            let handle = staged.into_raw();
            if SetClipboardData(format, handle).is_null() {
                GlobalFree(handle);
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
// no-regression #1: restore only after sequence, owner and payload re-verify.
pub(super) struct ClipboardLease<C: ClipboardBackend> {
    backend: C,
    backup: ClipboardFormats,
    owned_payload: ClipboardFormats,
    written: Option<u32>,
    dirty: bool,
    /// Set once this lease reported the component Degraded, so a later fast
    /// critical section of the same lease does not hide it.
    degraded: Cell<bool>,
}
fn finish_clipboard_restore<T>(
    backup: &mut Vec<T>,
    result: Result<(), String>,
) -> Result<(), String> {
    result?;
    backup.clear();
    Ok(())
}
fn retry_clipboard_verification<T>(
    mut attempt: impl FnMut() -> Result<Option<T>, String>,
    mut pause: impl FnMut(),
) -> Result<T, String> {
    for _ in 0..8 {
        match attempt()? {
            Some(verified) => return Ok(verified),
            None => pause(),
        }
    }
    Err("Clipboard ownership could not be verified within the retry limit".into())
}
impl<C: ClipboardBackend> ClipboardLease<C> {
    fn verified_sequence(
        &self,
        owner: u64,
        expected: &[(u32, Vec<u8>)],
        exact_formats: bool,
    ) -> Result<u32, LeaseError> {
        let backend = &self.backend;
        // An exhausted retry means the sequence never settled.
        let mut reason = ReasonCode::ClipboardChanged;
        let verified = retry_clipboard_verification(
            || {
                let checked = critical(backend, &self.degraded, || {
                    if backend.owner() != owner {
                        return Err((
                            ReasonCode::ClipboardChanged,
                            "Clipboard owner changed externally; clipboard preserved",
                        ));
                    }
                    if exact_formats {
                        let formats = backend.formats();
                        if formats.len() != expected.len()
                            || formats
                                .iter()
                                .any(|format| !expected.iter().any(|(wanted, _)| wanted == format))
                        {
                            return Err((
                                ReasonCode::ClipboardChanged,
                                "Clipboard formats changed externally; clipboard preserved",
                            ));
                        }
                    }
                    for (format, bytes) in expected {
                        match backend.read(*format, bytes.len()) {
                            Err(_) => {
                                return Err((
                                    ReasonCode::Internal,
                                    "Clipboard payload cannot be verified",
                                ))
                            }
                            Ok(Some(current)) if current == *bytes => {}
                            Ok(_) => {
                                return Err((
                                    ReasonCode::ClipboardChanged,
                                    "Clipboard payload changed externally; clipboard preserved",
                                ))
                            }
                        }
                    }
                    Ok(backend.sequence())
                });
                let sequence = match checked {
                    Ok(Ok(sequence)) => sequence,
                    Ok(Err((code, message))) => {
                        reason = code;
                        return Err(message.to_string());
                    }
                    Err(busy) => {
                        reason = busy.reason;
                        return Err(busy.message);
                    }
                };
                if backend.sequence() == sequence && backend.owner() == owner {
                    return Ok(Some(sequence));
                }
                Ok(None)
            },
            || backend.pause(VERIFY_RETRY_INTERVAL),
        );
        verified.map_err(|message| LeaseError::new(reason, message))
    }
    pub(super) fn verify_current(&self) -> Result<(), String> {
        if self
            .written
            .is_none_or(|seq| self.backend.sequence() != seq)
        {
            return Err("Clipboard changed externally before paste; no paste was sent".into());
        }
        Ok(())
    }
    /// Allocates `formats` up front so allocation cannot fail after deletion.
    pub(super) fn prepare(
        &self,
        formats: ClipboardFormats,
    ) -> Result<PreparedClipboard<C::Staged>, String> {
        let mut prepared = PreparedClipboard {
            handles: vec![],
            expected: vec![],
        };
        for (format, bytes) in formats {
            let (staged, reported) = self
                .backend
                .stage(&bytes)
                .map_err(|error| format!("{error} before deletion"))?;
            prepared.expected.push((format, reported));
            prepared.handles.push((format, staged));
        }
        Ok(prepared)
    }
    pub(super) fn write_prepared(
        &mut self,
        prepared: &mut PreparedClipboard<C::Staged>,
    ) -> Result<(), String> {
        let Self {
            backend,
            owned_payload,
            written,
            dirty,
            degraded,
            ..
        } = self;
        critical(backend, degraded, || -> Result<(), String> {
            if written.is_some_and(|seq| backend.sequence() != seq) {
                return Err("Clipboard changed externally; stopped without overwriting it".into());
            }
            if !backend.clear() {
                return Err("Clipboard clear failed".into());
            }
            *dirty = true;
            owned_payload.clear();
            mark_external_effect();
            while !prepared.handles.is_empty() {
                let (f, h) = prepared.handles.remove(0);
                if !backend.set(f, h) {
                    *written = Some(backend.sequence());
                    return Err("Clipboard write failed".into());
                }
                if let Some((_, bytes)) = prepared.expected.iter().find(|(format, _)| *format == f)
                {
                    owned_payload.push((f, bytes.clone()));
                }
            }
            *written = Some(backend.sequence());
            Ok(())
        })??;
        self.written =
            Some(self.verified_sequence(self.backend.identity(), &prepared.expected, false)?);
        Ok(())
    }
    pub(super) fn new(backend: C) -> Result<Self, String> {
        const BUSY: &str = "Clipboard is busy; trigger preserved";
        let degraded = Cell::new(false);
        let result = critical(&backend, &degraded, || -> Result<(u64, ClipboardFormats, u32), String> {
            let original_owner = backend.owner();
            let mut backup = vec![];
            for format in backend.formats() {
                if matches!(format, 2 | 3 | 9 | 14 | 0x80..=0x8f) {
                    return Err("Existing clipboard contains handle-based rich formats that cannot be safely restored; trigger preserved".into());
                }
                match backend.read(format, 32 * 1024 * 1024) {
                    Ok(Some(bytes)) => backup.push((format, bytes)),
                    Ok(None) => {
                        return Err(
                            "Clipboard format cannot be safely backed up; trigger preserved".into(),
                        )
                    }
                    Err(_) => return Err("Clipboard backup failed".into()),
                }
            }
            Ok((original_owner, backup, backend.sequence()))
        })
        .map_err(|_| BUSY.to_string())?;
        let (original_owner, backup, sequence) = result?;
        let mut lease = Self {
            backend,
            backup,
            owned_payload: vec![],
            written: Some(sequence),
            dirty: false,
            degraded,
        };
        match lease.verified_sequence(original_owner, &lease.backup, true) {
            Ok(sequence) => {
                lease.written = Some(sequence);
                Ok(lease)
            }
            Err(error) => {
                lease.written = None;
                Err(if error.reason == ReasonCode::ClipboardBusy {
                    BUSY.into()
                } else {
                    error.message
                })
            }
        }
    }
    fn write(&mut self, formats: ClipboardFormats) -> Result<(), String> {
        // Allocate before opening: the critical section only clears and sets.
        let mut staged = Vec::with_capacity(formats.len());
        for (format, bytes) in formats {
            let (handle, expected) = self.backend.stage(&bytes)?;
            staged.push((format, handle, expected));
        }
        let Self {
            backend,
            owned_payload,
            written,
            degraded,
            ..
        } = self;
        critical(backend, degraded, || -> Result<(), String> {
            if let Some(seq) = *written {
                if backend.sequence() != seq {
                    return Err(
                        "Clipboard changed externally; stopped without overwriting it".into(),
                    );
                }
            }
            if !backend.clear() {
                return Err("Clipboard clear failed".into());
            }
            mark_external_effect();
            owned_payload.clear();
            *written = Some(backend.sequence());
            for (format, handle, expected) in staged {
                if !backend.set(format, handle) {
                    return Err("Clipboard write failed".into());
                }
                owned_payload.push((format, expected));
            }
            *written = Some(backend.sequence());
            Ok(())
        })?
    }
    pub(super) fn restore(&mut self) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        if self.backend.owner() != self.backend.identity() {
            return Ok(());
        }
        // Closing the clipboard or a listener requesting a synthesized
        // format can advance the sequence after our writes. Re-establish
        // ownership from the exact payload under a lock; never adopt a new
        // sequence solely because the previous one is no longer current.
        self.written =
            Some(self.verified_sequence(self.backend.identity(), &self.owned_payload, false)?);
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
        assert!(finish_clipboard_restore(&mut backup, Err("clipboard busy".into())).is_err());
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
            let mut attempts = 0;
            let result = retry_clipboard_verification::<u32>(
                || {
                    attempts += 1;
                    if attempts == 1 {
                        Ok(None)
                    } else {
                        Err(reason.into())
                    }
                },
                || {},
            );
            assert_eq!(result, Err(reason.into()));
            assert_eq!(attempts, 2);
        }
        let mut attempts = 0;
        assert!(retry_clipboard_verification::<u32>(
            || {
                attempts += 1;
                Ok(None)
            },
            || {}
        )
        .is_err());
        assert_eq!(attempts, 8);
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
        let result = acquire_clipboard::<()>(&plan, || {
            Err("Existing clipboard has an unsupported handle format".into())
        });
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
        assert_eq!(acquire_clipboard(&plan, || Ok(42)).unwrap(), Some(42));
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
        assert!(acquire_clipboard::<()>(&plan, || Err("cannot preserve".into())).is_err());
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
        let lease =
            acquire_clipboard::<()>(&plan, || panic!("key output must leave clipboard alone"))
                .unwrap();
        assert!(lease.is_none());
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
            ClipboardLease::new(clipboard.clone()).err().as_deref(),
            Some("Clipboard is busy; trigger preserved")
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
            ClipboardLease::new(clipboard.clone()).err().as_deref(),
            Some("Clipboard is busy; trigger preserved")
        );
        assert_eq!(clipboard.health(), BUSY);
    }
    #[test]
    fn slow_critical_section_degrades_for_the_rest_of_the_lease() {
        let clipboard = MockClipboard::new(7, text("original"));
        clipboard.set_read_delay(LOCK_HOLD_BUDGET + Duration::from_millis(1));
        let script = clipboard.clone();
        // Only the backup (lock 0) is slow; its verification is fast.
        clipboard.on_lock(move |index| {
            if index == 1 {
                script.set_read_delay(Duration::ZERO);
            }
        });
        let lease = ClipboardLease::new(clipboard.clone()).unwrap();
        assert_eq!(clipboard.health(), BUSY, "a fast section must not hide it");
        drop(lease);
        // The next lease whose sections are all fast reports Healthy again.
        drop(ClipboardLease::new(clipboard.clone()).unwrap());
        assert_eq!(clipboard.health(), HEALTHY);
    }
    #[test]
    fn restore_retries_busy_clipboard_and_keeps_backup_for_drop() {
        let clipboard = MockClipboard::new(7, text("original"));
        let mut lease = ClipboardLease::new(clipboard.clone()).unwrap();
        let mut prepared = lease.prepare(text("expansion")).unwrap();
        lease.write_prepared(&mut prepared).unwrap();
        assert_eq!(clipboard.content(), text("expansion"));
        clipboard.set_busy(OPEN_ATTEMPTS);
        assert_eq!(lease.restore(), Err("Clipboard is busy".into()));
        assert_eq!(clipboard.content(), text("expansion"));
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
}
