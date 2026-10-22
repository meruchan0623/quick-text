//! Input-source facade: starts RawInput + hooks, runs the input consumer and
//! reports source health.
//!
//! B-02: the hook and RawInput callbacks hand fixed-size records to
//! [`Lane`]s (bounded lock-free queues) and return. One consumer thread
//! drains both lanes: macro recording, gate text, acceptance translation and
//! matching all happen here, never on a callback thread. Callback panics are
//! caught where they happen and logged from here (`hook.panic`).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use super::{
    executor::{enqueue, Candidate},
    foreground::WinForeground,
    hook::{mouse_operation, pointer_move, test_input_enabled, HookPipe, HookRecord, HOOK_READY},
    ime::ime_active,
    rawinput::{
        acceptance_event, detect_candidate, event_from, note_source, Buffer, Event, RawRecord,
    },
    recording::{record_event, stop_record_events},
};
use crate::health::{Component, ComponentHealth, HealthState, ReasonCode};
use crate::traits::ForegroundProbe;
use crossbeam_queue::ArrayQueue;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::PoisonError;

pub fn input_status() -> serde_json::Value {
    let layout = unsafe {
        GetKeyboardLayout(GetWindowThreadProcessId(
            GetForegroundWindow(),
            std::ptr::null_mut(),
        )) as usize
    };
    serde_json::json!({"reason":LAST_GATE.get().map(|g|g.lock().unwrap_or_else(PoisonError::into_inner).clone()).unwrap_or_else(||"Engine starting".into()),"hooks_ready":engine_ready(),"layout":format!("0x{:04X}",layout & 0xffff),"layout_hkl":format!("0x{layout:016X}"),"external_injected_keys":if test_input_enabled(){"explicit_test_mode"}else{"filtered"},"ime_active":ime_active(),"target_observation":observer::status(),"physical_input":rawinput::status(),"espanso_input":rawinput::status(),"automatic_trigger_modes":["espanso_rawinput_immediate","espanso_rawinput_space","espanso_rawinput_semicolon"],"terminator_capture":"space/semicolon post-input; enter/tab disabled"})
}
pub fn engine_ready() -> bool {
    HOOK_READY.load(Ordering::SeqCst)
}
pub fn native_ready() -> Result<(), String> {
    if !engine_ready() {
        return Err("Native keyboard/mouse hooks are unavailable".into());
    }
    Ok(())
}

/// A bounded lock-free queue from a callback thread to the consumer.
///
/// `push` never blocks or allocates: a full queue drops the record and
/// counts it. `contained` runs a callback body with panics caught and
/// counted, so the callback can still return normally.
pub(super) struct Lane<T> {
    queue: ArrayQueue<T>,
    dropped: AtomicU64,
    panics: AtomicU64,
    wake: fn(),
}
impl<T> Lane<T> {
    pub(super) fn new(capacity: usize, wake: fn()) -> Self {
        Self {
            queue: ArrayQueue::new(capacity.max(1)),
            dropped: AtomicU64::new(0),
            panics: AtomicU64::new(0),
            wake,
        }
    }
    /// Queues `record` and wakes the consumer. Returns false when the queue
    /// was full and the record was dropped.
    pub(super) fn push(&self, record: T) -> bool {
        let queued = self.queue.push(record).is_ok();
        if !queued {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
        (self.wake)();
        queued
    }
    pub(super) fn pop(&self) -> Option<T> {
        self.queue.pop()
    }
    /// Runs a callback body; a panic is counted and `fallback` returned.
    pub(super) fn contained<R>(&self, fallback: R, body: impl FnOnce() -> R) -> R {
        match catch_unwind(AssertUnwindSafe(body)) {
            Ok(value) => value,
            Err(_) => {
                self.panics.fetch_add(1, Ordering::SeqCst);
                (self.wake)();
                fallback
            }
        }
    }
    pub(super) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }
    pub(super) fn panics(&self) -> u64 {
        self.panics.load(Ordering::SeqCst)
    }
}

static CONSUMER: OnceLock<std::thread::Thread> = OnceLock::new();
/// Upper bound on how long the idle consumer sleeps between shutdown checks
/// (one wakeup per second keeps the idle budget).
const IDLE_PARK: Duration = Duration::from_secs(1);
/// Wakes the input consumer. Lock- and allocation-free; safe on callback
/// threads.
pub(super) fn wake() {
    if let Some(thread) = CONSUMER.get() {
        thread.unpark();
    }
}

/// Lane counters already logged.
#[derive(Default)]
struct Reported {
    hook_panics: u64,
    raw_panics: u64,
    hook_dropped: u64,
    raw_dropped: u64,
}
fn delta(seen: &mut u64, now: u64) -> u64 {
    let new = now.saturating_sub(*seen);
    *seen = now;
    new
}

/// Consumer state: the typed-key buffer and the logged lane counters.
#[derive(Default)]
pub(super) struct Consumer {
    buffer: Buffer,
    reported: Reported,
}
impl Consumer {
    /// Drains both lanes once and returns how many records were handled.
    /// Candidates go to `enqueue`; the buffer is cleared when it accepts.
    pub(super) fn drain(
        &mut self,
        shared: &Shared,
        hooks: &HookPipe,
        raw: &Lane<RawRecord>,
        probe: &impl ForegroundProbe,
        enqueue: &mut impl FnMut(Candidate) -> bool,
    ) -> usize {
        let mut handled = 0;
        while let Some(record) = hooks.lane().pop() {
            handled += 1;
            self.hook_record(shared, record, probe, enqueue);
        }
        while let Some(record) = raw.pop() {
            handled += 1;
            if let Some(event) = event_from(shared, &record) {
                self.detect(shared, event, probe, enqueue);
            }
        }
        self.report(hooks.lane(), raw);
        handled
    }
    fn hook_record(
        &mut self,
        shared: &Shared,
        record: HookRecord,
        probe: &impl ForegroundProbe,
        enqueue: &mut impl FnMut(Candidate) -> bool,
    ) {
        match record {
            HookRecord::External => gate("External simulated input filtered; matching invalidated"),
            HookRecord::Cancel => stop_record_events(shared),
            HookRecord::Key(key) => {
                let code = Key::WindowsCode { code: key.vk };
                record_event(
                    shared,
                    if key.down {
                        Operation::KeyDown { key: code }
                    } else {
                        Operation::KeyUp { key: code }
                    },
                );
                if !key.down {
                    return;
                }
                if let Some(event) = acceptance_event(shared, &key) {
                    self.detect(shared, event, probe, enqueue);
                }
                gate(if shared.paused.load(Ordering::SeqCst) {
                    "Paused"
                } else {
                    "Espanso RawInput capture active"
                });
            }
            HookRecord::Mouse { msg, data, x, y } => {
                if let Some(operation) = mouse_operation(msg, data) {
                    record_pointer(shared, operation, x, y);
                }
            }
            HookRecord::Move { x, y } => record_pointer(shared, pointer_move(x, y), x, y),
        }
    }
    fn detect(
        &mut self,
        shared: &Shared,
        event: Event,
        probe: &impl ForegroundProbe,
        enqueue: &mut impl FnMut(Candidate) -> bool,
    ) {
        note_source(event.source);
        if let Some(candidate) = detect_candidate(shared, &mut self.buffer, event, probe) {
            if enqueue(candidate) {
                self.buffer.text.clear();
            }
        }
    }
    /// Logs lane counters that moved since the last report. Counts only.
    fn report(&mut self, hooks: &Lane<HookRecord>, raw: &Lane<RawRecord>) {
        let seen = &mut self.reported;
        let count = delta(&mut seen.hook_panics, hooks.panics());
        if count > 0 {
            tracing::error!(event = "hook.panic", count);
        }
        let count = delta(&mut seen.raw_panics, raw.panics());
        if count > 0 {
            tracing::error!(event = "rawinput.panic", count);
        }
        let count = delta(&mut seen.hook_dropped, hooks.dropped());
        if count > 0 {
            tracing::warn!(event = "input.queue_full", source = "hook", count);
        }
        let count = delta(&mut seen.raw_dropped, raw.dropped());
        if count > 0 {
            tracing::warn!(event = "input.queue_full", source = "rawinput", count);
        }
    }
    fn reset(&mut self) {
        self.buffer = Buffer::default();
    }
}
/// Records a pointer operation unless the pointer is over our own window.
/// Hit-testing runs only while a recording is active, never on the hook.
fn record_pointer(shared: &Shared, operation: Operation, x: i32, y: i32) {
    if !shared.recording.try_lock().is_ok_and(|rows| rows.is_some()) {
        return;
    }
    // SAFETY: plain Win32 queries with no pointers we own.
    let own = unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(WindowFromPoint(POINT { x, y }), &mut pid);
        pid == GetCurrentProcessId()
    };
    if !own {
        record_event(shared, operation);
    }
}
fn run_consumer(
    shared: Arc<Shared>,
    hooks: &'static HookPipe<'static>,
    raw: &'static Lane<RawRecord>,
) {
    let mut consumer = Consumer::default();
    while !shared.shutdown.load(Ordering::SeqCst) {
        let drained = catch_unwind(AssertUnwindSafe(|| {
            consumer.drain(&shared, hooks, raw, &WinForeground, &mut |candidate| {
                enqueue(&shared, candidate)
            })
        }));
        let handled = match drained {
            Ok(handled) => handled,
            Err(_) => {
                tracing::error!(event = "input.consumer_panic");
                consumer.reset();
                shared.generation.fetch_add(1, Ordering::SeqCst);
                1
            }
        };
        if handled == 0 {
            std::thread::park_timeout(IDLE_PARK);
        }
    }
    rawinput::close_window();
}

/// Creates both lanes, starts the consumer, then the RawInput detector
/// (which also starts the UIA protection sampler) and the hook thread.
pub(crate) fn start_input_sources(shared: Arc<Shared>) -> Result<(), String> {
    let hooks = hook::pipe(test_input_enabled());
    let raw = rawinput::lane();
    let consumer_shared = shared.clone();
    let consumer = std::thread::Builder::new()
        .name("quicker-input".into())
        .spawn(move || run_consumer(consumer_shared, hooks, raw))
        .map_err(|error| format!("Input consumer could not start: {error}"))?;
    CONSUMER.set(consumer.thread().clone()).ok();
    rawinput::start(shared)?;
    hook::spawn_hooks();
    Ok(())
}
/// Health of the input sources, derived from the existing readiness flags.
/// Until the supervisor (B-06) tracks transitions, "not ready" is reported as
/// `Starting` (install failure and not-yet-started are indistinguishable) and
/// `since_ms` is the observation time.
#[allow(dead_code, reason = "first caller is the supervisor (B-06)")]
pub(crate) fn input_health() -> Vec<ComponentHealth> {
    let now = chrono::Utc::now().timestamp_millis();
    let hooks = HOOK_READY.load(Ordering::SeqCst);
    let raw = rawinput::READY.load(Ordering::SeqCst);
    let entry = |component, ready: bool, missing| ComponentHealth {
        component,
        state: if ready {
            HealthState::Healthy
        } else {
            HealthState::Starting
        },
        reason: (!ready).then_some(missing),
        since_ms: now,
    };
    vec![
        entry(Component::KeyboardHook, hooks, ReasonCode::HookMissing),
        entry(Component::MouseHook, hooks, ReasonCode::HookMissing),
        entry(Component::FocusEvents, hooks, ReasonCode::HookMissing),
        entry(Component::RawInput, raw, ReasonCode::RawInputMissing),
    ]
}
/// Re-installs one input source after a failure.
#[allow(dead_code, reason = "first caller is the supervisor (B-06)")]
pub(crate) fn reattach(component: Component) -> Result<(), String> {
    // TODO(B-06): reinstall the hook / RawInput window for `component` on its
    // own thread with backoff; until then the supervisor cannot recover it.
    Err(format!("reattach {component:?} is not implemented"))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    #[test]
    fn input_health_reports_unstarted_sources_without_claiming_health() {
        let health = input_health();
        assert_eq!(
            health.iter().map(|h| h.component).collect::<Vec<_>>(),
            [
                Component::KeyboardHook,
                Component::MouseHook,
                Component::FocusEvents,
                Component::RawInput
            ]
        );
        // Tests never install hooks or start RawInput.
        assert!(health.iter().all(|h| h.state == HealthState::Starting));
        assert!(reattach(Component::KeyboardHook).is_err());
    }

    fn nothing() {}

    #[test]
    fn a_full_lane_drops_without_blocking_and_counts() {
        let lane = Lane::new(2, nothing);
        assert!(lane.push(1));
        assert!(lane.push(2));
        assert!(!lane.push(3), "the third record is dropped");
        assert_eq!(lane.dropped(), 1);
        assert_eq!(
            (lane.pop(), lane.pop(), lane.pop()),
            (Some(1), Some(2), None)
        );
        assert!(lane.push(4), "space frees up after a drain");
    }

    #[test]
    fn contained_panics_return_the_fallback_and_count() {
        let lane: Lane<u8> = Lane::new(1, nothing);
        assert_eq!(lane.contained(0, || 7), 7);
        assert_eq!(lane.contained(0, || panic!("callback bug")), 0);
        assert_eq!(lane.panics(), 1);
        assert_eq!(lane.contained(0, || 8), 8, "the next call still runs");
    }

    #[test]
    fn counter_deltas_report_each_increase_once() {
        let mut seen = 0;
        assert_eq!(delta(&mut seen, 0), 0);
        assert_eq!(delta(&mut seen, 3), 3);
        assert_eq!(delta(&mut seen, 3), 0);
        assert_eq!(delta(&mut seen, 5), 2);
    }
}
