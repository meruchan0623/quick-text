//! Macro recording fed by the hooks.
use super::*;
struct RecordMeta {
    /// Capture time of the last recorded event (hook tick count, ms).
    at: u32,
    held: BTreeSet<Key>,
    buttons: Vec<MouseButton>,
    /// Nothing more goes into the current recording.
    stopped: bool,
    /// [`finish_recording`] closed the last recording: the next event or
    /// stop belongs to a new one.
    ended: bool,
}
impl RecordMeta {
    const fn new(ended: bool) -> Self {
        Self {
            at: 0,
            held: BTreeSet::new(),
            buttons: Vec::new(),
            stopped: false,
            ended,
        }
    }
    /// Drops what an earlier recording left behind when `rows` (the active
    /// recording) has nothing yet and either the last one was closed by
    /// [`finish_recording`] or nothing stopped. A stop before the first
    /// recorded event (M2) is kept, so it still holds; a closed recording
    /// that has not been taken yet stays stopped.
    fn begin(&mut self, rows: &[Step]) {
        if rows.is_empty() && (self.ended || !self.stopped) {
            *self = Self::new(false);
        }
    }
}
/// No recording has run yet.
static RECORD_META: Mutex<RecordMeta> = Mutex::new(RecordMeta::new(true));
/// The recording session: advanced each time a recording starts
/// ([`recording_started`]), so a stop requested during one recording never
/// ends a later one (M2). 0 before the first recording.
static SESSION: AtomicU64 = AtomicU64::new(0);
/// The current recording session; the hooks stamp a stop gesture with it.
/// One atomic load.
pub(super) fn recording_session() -> u64 {
    SESSION.load(Ordering::SeqCst)
}
/// A new recording session starts. `start_recording` calls this while it
/// holds the recording lock, just before it opens the recording, so a stop
/// checked under that lock always sees which session is current.
pub(crate) fn recording_started() {
    SESSION.fetch_add(1, Ordering::SeqCst);
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
/// Records `operation`, captured by a hook at tick `time`. The caller has
/// already dropped input aimed at our own windows. Delays come from capture
/// times, so a consumer that drains late keeps the user's timing (M1).
pub(super) fn record_event(shared: &Shared, operation: Operation, time: u32) {
    let Ok(mut recorded) = shared.recording.try_lock() else {
        return;
    };
    let Some(rows) = recorded.as_mut() else {
        return;
    };
    let mut meta = RECORD_META.lock().unwrap();
    meta.begin(rows);
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
    meta.begin(rows);
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
/// Stops the recording before the caller takes it, and marks it closed so
/// the next recording starts from scratch (src-tauri's `stop_recording`
/// always calls this first).
pub fn finish_recording(shared: &Arc<Shared>) {
    stop_record_events(shared);
    RECORD_META.lock().unwrap().ended = true;
}
