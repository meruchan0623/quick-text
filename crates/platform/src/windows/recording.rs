//! Macro recording fed by the hooks.
use super::*;
struct RecordMeta {
    /// Capture time of the last recorded event (hook tick count, ms).
    at: u32,
    held: BTreeSet<Key>,
    buttons: Vec<MouseButton>,
    /// Nothing more goes into the current recording. Set by the first stop
    /// and cleared only when the next recording starts
    /// ([`recording_started`]): a stopped recording stays stopped until it
    /// is taken, even when it has no rows (M2).
    stopped: bool,
}
impl RecordMeta {
    const fn new() -> Self {
        Self {
            at: 0,
            held: BTreeSet::new(),
            buttons: Vec::new(),
            stopped: false,
        }
    }
}
/// The bookkeeping of the current recording, reset only by
/// [`recording_started`] (never inferred from what the recording holds).
/// Lock order: the recording lock, then this one.
static RECORD_META: Mutex<RecordMeta> = Mutex::new(RecordMeta::new());
/// The recording session: advanced each time a recording starts
/// ([`recording_started`]), so a stop requested during one recording never
/// ends a later one (M2). 0 before the first recording.
static SESSION: AtomicU64 = AtomicU64::new(0);
/// The current recording session; the hooks stamp a stop gesture with it.
/// One atomic load.
pub(super) fn recording_session() -> u64 {
    SESSION.load(Ordering::SeqCst)
}
/// A new recording session starts: the session advances and the recorder's
/// bookkeeping starts from scratch. The only place either is reset (M2).
/// `start_recording` calls this while it holds the recording lock, just
/// before it opens the recording, so a stop or an event handled under that
/// lock always sees which session is current.
pub(crate) fn recording_started() {
    SESSION.fetch_add(1, Ordering::SeqCst);
    *RECORD_META.lock().unwrap() = RecordMeta::new();
}
/// The delay to record between two events captured at hook ticks `from`
/// and `to` (ms). The tick count wraps, so the gap is taken modulo 2^32; a
/// gap of more than half the range means `to` came first and counts as no
/// delay. Capped at [`MAX_DELAY_MS`].
pub(super) fn capture_gap(from: u32, to: u32) -> u64 {
    let gap = to.wrapping_sub(from);
    if gap > u32::MAX / 2 {
        0
    } else {
        u64::from(gap).min(MAX_DELAY_MS)
    }
}
/// Records `operation`, captured by a hook at tick `time` during recording
/// session `session`. The caller has already dropped input aimed at our own
/// windows. Delays come from capture times, so a consumer that drains late
/// keeps the user's timing (M1). Input from another session (captured
/// during an earlier recording, or between two, and drained only now) is
/// dropped before it touches anything, so it can neither enter this
/// recording nor leave a held key or button for the stop to release (M2).
pub(super) fn record_event(shared: &Shared, operation: Operation, time: u32, session: u64) {
    let Ok(mut recorded) = shared.recording.try_lock() else {
        return;
    };
    let Some(rows) = recorded.as_mut() else {
        return;
    };
    let current = SESSION.load(Ordering::SeqCst);
    if session != current {
        tracing::debug!(event = "recording.event_stale", session, current);
        return;
    }
    let mut meta = RECORD_META.lock().unwrap();
    if meta.stopped || rows.len() + 2 >= MAX_STEPS {
        return;
    }
    match &operation {
        Operation::KeyDown { key } => {
            if !meta.held.insert(key.clone()) {
                return;
            }
        }
        Operation::KeyUp { key } => {
            if !meta.held.remove(key) {
                return;
            }
        }
        Operation::MouseButton {
            button,
            state: ButtonState::Down,
        } => meta.buttons.push(*button),
        Operation::MouseButton {
            button,
            state: ButtonState::Up,
        } => {
            if !meta.buttons.contains(button) {
                return;
            }
            meta.buttons.retain(|b| b != button)
        }
        _ => {}
    }
    let delay = capture_gap(meta.at, time);
    meta.at = time;
    if delay > 0 && !rows.is_empty() {
        rows.push(Step::new(
            Operation::Delay { ms: delay },
            SourceSpan::default(),
        ));
    }
    rows.push(Step::new(operation, SourceSpan::default()));
}
/// The stop gesture (Ctrl+Shift+Esc) pressed during recording session
/// `session` reached the consumer: stops the recording if that session is
/// still the current one. A stop from an earlier session is stale (its
/// recording was finished and a new one started before the consumer got
/// to it) and is dropped (M2).
pub(super) fn stop_requested(shared: &Shared, session: u64) {
    let mut recorded = shared.recording.lock().unwrap();
    let current = SESSION.load(Ordering::SeqCst);
    if session != current {
        tracing::debug!(event = "recording.stop_stale", session, current);
        return;
    }
    stop_rows(&mut recorded);
}
/// Stops the active recording ([`finish_recording`]).
pub(super) fn stop_record_events(shared: &Shared) {
    stop_rows(&mut shared.recording.lock().unwrap());
}
/// Stops `recorded`: drops the stop gesture's own Ctrl/Shift presses and
/// releases whatever is still held. Once per recording; a second stop adds
/// nothing. A stop before anything was recorded holds as well (M2).
fn stop_rows(recorded: &mut Option<Vec<Step>>) {
    let Some(rows) = recorded.as_mut() else {
        return;
    };
    let mut meta = RECORD_META.lock().unwrap();
    if meta.stopped {
        return;
    }
    meta.stopped = true;
    let stop_keys = [
        VK_CONTROL,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_SHIFT,
        VK_LSHIFT,
        VK_RSHIFT,
    ];
    rows.retain(|s|!matches!(&s.operation,Operation::KeyDown{key:Key::WindowsCode{code}} if stop_keys.contains(code)&&meta.held.contains(&Key::WindowsCode{code:*code})));
    for key in &meta.held {
        if !matches!(key,Key::WindowsCode{code}if stop_keys.contains(code)) {
            rows.push(Step::new(
                Operation::KeyUp { key: key.clone() },
                SourceSpan::default(),
            ));
        }
    }
    for button in &meta.buttons {
        rows.push(Step::new(
            Operation::MouseButton {
                button: *button,
                state: ButtonState::Up,
            },
            SourceSpan::default(),
        ));
    }
}
/// Stops the recording before the caller takes it. It stays stopped until
/// the next recording starts, so nothing reaches it before the take.
/// Prefer [`finish_and_take`], which stops and takes in one critical
/// section.
pub fn finish_recording(shared: &Arc<Shared>) {
    stop_record_events(shared);
}
/// Ends the active recording and takes it, in one critical section under
/// the recording lock: stops it like [`finish_recording`] (drops the stop
/// gesture's Ctrl/Shift, releases what is still held), then takes the rows
/// as a replay plan, so no event can land between the two (M2). The
/// Windows replacement for `finish_recording` followed by
/// `stop_recording`; same errors as `stop_recording`.
pub fn finish_and_take(shared: &Arc<Shared>) -> Result<ActionPlan, String> {
    let mut recorded = shared.recording.lock().map_err(|_| "recording lock")?;
    stop_rows(&mut recorded);
    let steps = recorded.take().ok_or("Recording is not active")?;
    Ok(ActionPlan {
        steps,
        dialect: Dialect::Replay,
        ..Default::default()
    })
}
