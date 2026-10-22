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
/// Stops the active recording (Ctrl+Shift+Esc or [`finish_recording`]):
/// drops the gesture's own Ctrl/Shift presses and releases whatever is
/// still held. Once per recording; a second stop adds nothing. A stop
/// before anything was recorded holds as well (M2).
pub(super) fn stop_record_events(shared: &Shared) {
    let mut recorded = shared.recording.lock().unwrap();
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
