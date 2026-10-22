//! Macro recording fed by the hooks.
use super::*;
struct RecordMeta {
    at: Instant,
    held: BTreeSet<Key>,
    buttons: Vec<MouseButton>,
    stopped: bool,
}
static RECORD_META: OnceLock<Mutex<RecordMeta>> = OnceLock::new();
pub(super) fn record_event(shared: &Shared, operation: Operation) {
    let Ok(mut recorded) = shared.recording.try_lock() else {
        return;
    };
    let Some(rows) = recorded.as_mut() else {
        return;
    };
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(GetForegroundWindow(), &mut pid);
        if pid == GetCurrentProcessId() {
            return;
        }
    }
    let mut meta = RECORD_META
        .get_or_init(|| {
            Mutex::new(RecordMeta {
                at: Instant::now(),
                held: BTreeSet::new(),
                buttons: vec![],
                stopped: false,
            })
        })
        .lock()
        .unwrap();
    if rows.is_empty() {
        meta.at = Instant::now();
        meta.held.clear();
        meta.buttons.clear();
        meta.stopped = false;
    }
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
    let delay = meta.at.elapsed().as_millis().min(MAX_DELAY_MS as u128) as u64;
    meta.at = Instant::now();
    if delay > 0 && !rows.is_empty() {
        rows.push(Step::new(
            Operation::Delay { ms: delay },
            SourceSpan::default(),
        ));
    }
    rows.push(Step::new(operation, SourceSpan::default()));
}
pub(super) fn stop_record_events(shared: &Shared) {
    let mut recorded = shared.recording.lock().unwrap();
    if let Some(rows) = recorded.as_mut() {
        if let Some(meta) = RECORD_META.get() {
            let mut meta = meta.lock().unwrap();
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
    }
}
pub fn finish_recording(shared: &Arc<Shared>) {
    stop_record_events(shared);
}
