//! Input-source facade: starts RawInput + hooks, runs the input consumer and
//! reports source health.
//!
//! B-02: the hook and RawInput callbacks hand fixed-size records to
//! [`Lane`]s (bounded lock-free queues) and return. One consumer thread
//! drains both lanes: macro recording, gate text, acceptance translation and
//! matching all happen here, never on a callback thread. Callback panics are
//! caught where they happen and logged from here (`hook.panic`).
//!
//! B-06: [`Sources`] owns the health of the four input components and
//! reattaches them; [`Liveness`] cross-checks the hook against RawInput on
//! the consumer and reattaches a source that stopped seeing keys. Every
//! system call goes through [`SourcePorts`], so tests run on fakes.
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
    hook::{mouse_operation, pointer_move, test_input_enabled, HookPipe, HookRecord, HookSet},
    ime::ime_active,
    keystate::{
        own_input, AsyncKeyProbe, KeyBits, ModifierWatch, OwnInput, SystemAsyncKeys,
        INJECTION_QUIET, PHYSICAL,
    },
    liveness::{component_name, reason_name, CrossCheck, HealthTable, Source, Verdict},
    rawinput::{
        acceptance_event, detect_candidate, event_from, note_source, shadow_step, Buffer, Event,
        RawRecord, Shadow,
    },
    recording::{record_event, stop_requested},
    session::{self, SessionSignals, Signal},
};
use crate::fault::{self, FaultPoint};
use crate::health::{Component, ComponentHealth, HealthState, ReasonCode};
use crate::traits::{Clock, ForegroundProbe, SystemClock};
use crossbeam_queue::ArrayQueue;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{MutexGuard, PoisonError};

pub fn input_status() -> serde_json::Value {
    let layout = unsafe {
        GetKeyboardLayout(GetWindowThreadProcessId(
            GetForegroundWindow(),
            std::ptr::null_mut(),
        )) as usize
    };
    serde_json::json!({"reason":LAST_GATE.get().map(|g|g.lock().unwrap_or_else(PoisonError::into_inner).clone()).unwrap_or_else(||"Engine starting".into()),"hooks_ready":engine_ready(),"layout":format!("0x{:04X}",layout & 0xffff),"layout_hkl":format!("0x{layout:016X}"),"external_injected_keys":if test_input_enabled(){"explicit_test_mode"}else{"filtered"},"ime_active":ime_active(),"target_observation":observer::status(),"physical_input":rawinput::status(),"espanso_input":rawinput::status(),"automatic_trigger_modes":["espanso_rawinput_immediate","espanso_rawinput_space","espanso_rawinput_semicolon"],"terminator_capture":"space/semicolon post-input; enter/tab disabled"})
}
/// Whether the keyboard, mouse and focus hooks are all installed and none of
/// them is known to be lost (B-06).
pub fn engine_ready() -> bool {
    SOURCES.hooks_ready()
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
    /// Wakes the consumer without queueing anything.
    pub(super) fn wake(&self) {
        (self.wake)();
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

/// What one source saw, for the liveness cross-check ([`Liveness::tick`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sighting {
    /// A physical key transition: (source, virtual key, down).
    Key(Source, u16, bool),
}

/// Where recorded input landed: input aimed at one of our own windows is
/// never recorded. Asked only while a recording is active, on the consumer.
#[derive(Clone, Copy)]
pub(super) struct OwnWindows {
    /// The foreground window belongs to this process.
    pub(super) foreground: fn() -> bool,
    /// The window under a screen point belongs to this process.
    pub(super) at: fn(i32, i32) -> bool,
}
impl Default for OwnWindows {
    fn default() -> Self {
        Self {
            foreground: own_foreground,
            at: own_window_at,
        }
    }
}
fn own_foreground() -> bool {
    // SAFETY: plain Win32 queries with no pointers we own.
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(GetForegroundWindow(), &mut pid);
        pid == GetCurrentProcessId()
    }
}
fn own_window_at(x: i32, y: i32) -> bool {
    // SAFETY: plain Win32 queries with no pointers we own.
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(WindowFromPoint(POINT { x, y }), &mut pid);
        pid == GetCurrentProcessId()
    }
}
fn recording(shared: &Shared) -> bool {
    shared.recording.try_lock().is_ok_and(|rows| rows.is_some())
}
/// Applies a stop gesture that overflowed the hook queue (M2) to the
/// recording session it was pressed in; stale ones are dropped.
fn take_overflowed_stop(shared: &Shared, hooks: &HookPipe) {
    let session = hooks.take_stop();
    if session != 0 {
        stop_requested(shared, session);
    }
}

/// Consumer state: the typed-key buffer, the logged lane counters and the
/// physical key transitions each source saw since the last liveness tick.
#[derive(Default)]
pub(super) struct Consumer {
    buffer: Buffer,
    /// Keys the matcher drops only because an action is running (S-19).
    shadow: Shadow,
    reported: Reported,
    seen: Vec<Sighting>,
    own: OwnWindows,
}
impl Consumer {
    /// A consumer that asks `own` where recorded input landed.
    #[cfg(test)]
    pub(super) fn with_windows(own: OwnWindows) -> Self {
        Self {
            own,
            ..Self::default()
        }
    }
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
            // M2: a stop gesture that overflowed the queue ends its
            // recording before this record is accepted. Checked after the
            // pop: a record pushed after the stop then always sees it.
            take_overflowed_stop(shared, hooks);
            if let HookRecord::Key(key) = &record {
                if key.hardware {
                    self.seen
                        .push(Sighting::Key(Source::Hook, key.vk, key.down));
                }
            }
            self.hook_record(shared, record, probe, enqueue);
        }
        // ...and with nothing queued behind it.
        take_overflowed_stop(shared, hooks);
        while let Some(record) = raw.pop() {
            handled += 1;
            if record.mouse() {
                // F5-2: a mouse button event. The callback already
                // invalidated the prefix; it is neither text nor a key.
                continue;
            }
            // Every other record is a hardware key transition, with or
            // without context; only key-downs become text.
            self.seen
                .push(Sighting::Key(Source::Raw, record.vk(), record.down()));
            let event = event_from(shared, &record);
            // S-19: matched in the busy shadow, reported, never executed.
            shadow_step(shared, &mut self.shadow, &record, event.as_ref(), probe);
            if let Some(event) = event {
                self.detect(shared, event, probe, enqueue);
            }
        }
        self.report(hooks.lane(), raw);
        handled
    }
    /// Key transitions for [`Liveness::tick`], which drains them.
    pub(super) fn seen(&mut self) -> &mut Vec<Sighting> {
        &mut self.seen
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
            HookRecord::Cancel { session } => stop_requested(shared, session),
            HookRecord::Key(key) => {
                let code = Key::WindowsCode { code: key.vk };
                self.record_key(
                    shared,
                    if key.down {
                        Operation::KeyDown { key: code }
                    } else {
                        Operation::KeyUp { key: code }
                    },
                    key.time,
                    key.session,
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
            HookRecord::Mouse {
                msg,
                data,
                x,
                y,
                time,
                session,
            } => {
                if let Some(operation) = mouse_operation(msg, data) {
                    self.record_pointer(shared, operation, (x, y), time, session);
                }
            }
            HookRecord::Move {
                x,
                y,
                time,
                session,
            } => self.record_pointer(shared, pointer_move(x, y), (x, y), time, session),
        }
    }
    /// Records a key operation captured at `time` during recording session
    /// `session` unless one of our own windows has the focus.
    fn record_key(&self, shared: &Shared, operation: Operation, time: u32, session: u64) {
        if recording(shared) && !(self.own.foreground)() {
            record_event(shared, operation, time, session);
        }
    }
    /// Records a pointer operation captured at `time` during recording
    /// session `session` unless the pointer (at `x`, `y`) is over, or the
    /// focus is in, one of our own windows. Hit-testing runs only while a
    /// recording is active, never on the hook.
    fn record_pointer(
        &self,
        shared: &Shared,
        operation: Operation,
        (x, y): (i32, i32),
        time: u32,
        session: u64,
    ) {
        if recording(shared) && !(self.own.at)(x, y) && !(self.own.foreground)() {
            record_event(shared, operation, time, session);
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
        self.shadow = Shadow::default();
        self.seen.clear();
    }
}
/// Second resync after a session signal, for key transitions that were
/// still in flight when the first one ran (B-05).
pub(super) const RESYNC_SECOND_PASS: Duration = Duration::from_millis(200);

/// Ends the current word and every candidate proof built on it: the next
/// key takes a generation with a gap, and the caret epoch in each stamp
/// moves. Used whenever input may have gone unseen (session signals, a lost
/// or reattached input source; H3).
pub(super) fn invalidate_prefix(shared: &Shared) {
    shared.generation.fetch_add(1, Ordering::SeqCst);
    observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
}

/// The earlier of two optional deadlines.
fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// When our own injections last changed, as the consumer observed them
/// (H1): the asynchronous key state is only sampled after
/// [`INJECTION_QUIET`] without any.
struct Quiet {
    /// [`OwnInput::seq`] at the last observation.
    seen: u64,
    /// When an injection was last seen starting, ending or in flight;
    /// `None` when none has been seen since the monitor started.
    since: Option<Instant>,
}
impl Quiet {
    fn observe(&mut self, own: &OwnInput, now: Instant) {
        let seq = own.seq();
        if seq != self.seen || own.active() {
            self.seen = seq;
            self.since = Some(now);
        }
    }
    /// `None` when sampling may start now, else when to look again.
    fn wait(&self, now: Instant) -> Option<Instant> {
        self.since
            .and_then(|since| since.checked_add(INJECTION_QUIET))
            .filter(|&ready| now < ready)
    }
}

/// Consumer-side timers (B-05): the key-mirror resync after session signals
/// and the stuck-modifier check. All time comes from `clock`; every system
/// read goes through `ports`.
///
/// H1: a resync never imports our own output. It waits until no injection
/// was in flight for [`INJECTION_QUIET`], masks the keys `own` holds, and
/// is retried when an injection overlapped its sample.
pub(super) struct Monitor<'a, P: AsyncKeyProbe, C: Clock> {
    keys: &'a KeyBits,
    ports: &'a P,
    clock: &'a C,
    signals: &'a SessionSignals,
    own: &'a OwnInput,
    watch: ModifierWatch,
    second_pass: Option<Instant>,
    /// A resync waiting for our injections to settle.
    pending: Option<&'static str>,
    quiet: Quiet,
}
impl<'a, P: AsyncKeyProbe, C: Clock> Monitor<'a, P, C> {
    pub(super) fn new(
        keys: &'a KeyBits,
        ports: &'a P,
        clock: &'a C,
        signals: &'a SessionSignals,
        own: &'a OwnInput,
    ) -> Self {
        Self {
            keys,
            ports,
            clock,
            signals,
            own,
            watch: ModifierWatch::default(),
            second_pass: None,
            pending: None,
            quiet: Quiet {
                seen: own.seq(),
                since: None,
            },
        }
    }
    /// Handles pending session signals and due checks. Returns when it next
    /// needs to run, if anything is scheduled.
    pub(super) fn tick(&mut self, shared: &Shared) -> Option<Instant> {
        let now = self.clock.now();
        self.quiet.observe(self.own, now);
        let mut signalled = false;
        for signal in self.signals.take() {
            tracing::info!(event = "session.signal", signal = signal.name());
            signalled = true;
        }
        if signalled {
            // Typing before and after a session change is not one word.
            invalidate_prefix(shared);
            self.pending = Some("first");
            self.second_pass = None;
        } else if self.second_pass.is_some_and(|due| now >= due) {
            // Mirror only: a word typed right after an unlock stays intact.
            self.second_pass = None;
            self.pending.get_or_insert("second");
        }
        let retry = self.pending.and_then(|pass| self.run_pending(pass, now));
        let modifiers = self.watch.check(self.keys, self.ports, now);
        earliest(earliest(self.second_pass, retry), modifiers)
    }
    /// Whether a candidate detected at `generation` may go to the executor
    /// (H3). A signal raised since the last tick (an input source came back,
    /// the session changed) is handled first, so the word boundary it
    /// stands for applies to this candidate too.
    pub(super) fn admits(&mut self, shared: &Shared, generation: u64) -> bool {
        if !self.signals.pending() {
            return true;
        }
        self.tick(shared);
        generation == shared.generation.load(Ordering::SeqCst)
    }
    /// Runs the pending resync if our injections have settled. Returns when
    /// to try again otherwise.
    fn run_pending(&mut self, pass: &'static str, now: Instant) -> Option<Instant> {
        if let Some(ready) = self.quiet.wait(now) {
            return Some(ready);
        }
        if !self
            .keys
            .resync_unless_injected(self.ports, self.own, self.quiet.seen)
        {
            // An injection overlapped the sample: wait for quiet again,
            // counted from now (one still in flight keeps extending it).
            tracing::info!(event = "keystate.resync_deferred", pass);
            self.quiet.seen = self.own.seq();
            self.quiet.since = Some(now);
            return now.checked_add(INJECTION_QUIET);
        }
        self.pending = None;
        self.watch.reset();
        tracing::info!(event = "keystate.resync", pass);
        if pass == "first" {
            self.second_pass = now.checked_add(RESYNC_SECOND_PASS);
        }
        None
    }
}

/// Where the process-wide RawInput keyboard registration points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Registration {
    /// Our detector window, with input sink.
    Ours,
    /// This process has no keyboard registration.
    Missing,
    /// Another window took it over (tao registers its own, G6).
    Elsewhere,
    /// The registration could not be read.
    Unknown,
}

/// The system calls behind the input sources (B-06). Production uses
/// [`WinPorts`]; tests use fakes, so no test installs a hook, registers
/// RawInput or creates a window.
pub(super) trait SourcePorts: Send + Sync {
    /// (Re)installs `which` and returns the requested hooks now installed.
    /// Blocks up to the hook thread's answer timeout.
    fn install_hooks(&self, which: HookSet) -> HookSet;
    /// Whether the RawInput detector window exists.
    fn raw_alive(&self) -> bool;
    fn raw_registration(&self) -> Registration;
    /// Registers RawInput for the existing detector window again.
    fn register_raw(&self) -> bool;
    /// Replaces the detector window and thread; true when the new one runs.
    fn rebuild_raw(&self) -> bool;
    /// Fault injection only (HookUnhook): drop the low-level hooks silently.
    fn drop_hooks(&self);
    /// Fault injection only (RawinputUnregister).
    fn unregister_raw(&self);
    /// Fault injection only (RawinputClose): close the detector window as
    /// an outside WM_CLOSE would.
    fn close_raw(&self);
}
pub(super) struct WinPorts;
impl SourcePorts for WinPorts {
    fn install_hooks(&self, which: HookSet) -> HookSet {
        hook::install(which)
    }
    fn raw_alive(&self) -> bool {
        rawinput::alive()
    }
    fn raw_registration(&self) -> Registration {
        rawinput::registration()
    }
    fn register_raw(&self) -> bool {
        rawinput::register()
    }
    fn rebuild_raw(&self) -> bool {
        rawinput::rebuild()
    }
    fn drop_hooks(&self) {
        hook::drop_hooks();
    }
    fn unregister_raw(&self) {
        rawinput::unregister();
    }
    fn close_raw(&self) {
        rawinput::close_window(false);
    }
}

/// Hook kinds behind each hook component.
const HOOK_COMPONENTS: [(Component, HookSet); 3] = [
    (Component::KeyboardHook, HookSet::KEYBOARD),
    (Component::MouseHook, HookSet::MOUSE),
    (Component::FocusEvents, HookSet::FOCUS),
];

/// Health and recovery of the input sources (B-06). The supervisor reads
/// [`Sources::health`] and calls [`Sources::reattach`] for every component
/// that is not healthy; [`Liveness`] calls it when the cross-check finds a
/// source lost. Input sources never report `Failed`: the supervisor's
/// circuit breaker decides when to give up.
pub(super) struct Sources<'a, P: SourcePorts, C: Clock> {
    ports: &'a P,
    clock: &'a C,
    signals: &'a SessionSignals,
    table: Mutex<HealthTable>,
    /// Serialises start and reattach (one install request at a time).
    work: Mutex<()>,
    started: AtomicBool,
    /// Bumped by every successful reattach.
    epoch: AtomicU64,
    /// The engine whose typed prefix a lost or reattached source
    /// invalidates (H3); bound once, before the sources start.
    engine: OnceLock<Arc<Shared>>,
    /// Published by [`Liveness::tick`]: RawInput saw at least one physical
    /// key the keyboard hook missed since the two last agreed on a key
    /// ([`CrossCheck::missed`]), even if too few for a verdict. Read by the
    /// `hook_heartbeat` self-test check without any lock (review A5).
    hook_gap: AtomicBool,
}
impl<'a, P: SourcePorts, C: Clock> Sources<'a, P, C> {
    pub(super) const fn new(ports: &'a P, clock: &'a C, signals: &'a SessionSignals) -> Self {
        Self {
            ports,
            clock,
            signals,
            table: Mutex::new(HealthTable::new()),
            work: Mutex::new(()),
            started: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            engine: OnceLock::new(),
            hook_gap: AtomicBool::new(false),
        }
    }
    /// Whether the cross-check holds evidence that the keyboard hook missed
    /// a physical key since it last delivered one RawInput also saw (see
    /// `hook_gap`). An atomic load: never waits for the consumer.
    pub(super) fn hook_gap(&self) -> bool {
        self.hook_gap.load(Ordering::SeqCst)
    }
    /// Binds the engine whose prefix a source loss invalidates.
    pub(super) fn bind(&self, shared: &Arc<Shared>) {
        let _ = self.engine.set(shared.clone());
    }
    /// H3: input may have gone unseen, or is about to flow again after a
    /// gap. Synchronous, on whichever thread noticed, so no key captured
    /// afterwards can join the keys before it.
    fn invalidate(&self) {
        if let Some(shared) = self.engine.get() {
            invalidate_prefix(shared);
        }
    }
    fn table(&self) -> MutexGuard<'_, HealthTable> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn set(&self, component: Component, state: HealthState, reason: Option<ReasonCode>) {
        self.table()
            .set(component, state, reason, self.clock.wall_ms());
    }
    /// Records what an install left: healthy, or recovering because the
    /// install failed.
    fn hooks_installed(&self, installed: HookSet, which: HookSet) {
        for (component, kind) in HOOK_COMPONENTS {
            if !which.contains(kind) {
                continue;
            }
            if installed.contains(kind) {
                self.set(component, HealthState::Healthy, None);
            } else {
                self.set(
                    component,
                    HealthState::Recovering,
                    Some(ReasonCode::HookInstallFailed),
                );
            }
        }
    }
    /// Installs every hook and records the state of the detector window
    /// `rawinput::start` created. Returns the hooks installed.
    pub(super) fn start(&self) -> HookSet {
        let _work = self.work.lock().unwrap_or_else(PoisonError::into_inner);
        let installed = self.ports.install_hooks(HookSet::ALL);
        self.hooks_installed(installed, HookSet::ALL);
        if self.ports.raw_alive() {
            self.set(Component::RawInput, HealthState::Healthy, None);
            // G6: the last RawInput registration in the process wins. Check
            // once more now that the detector runs, as insurance against a
            // registration made while it started. If it cannot be taken back
            // here, RawInput is reported missing and the supervisor's
            // reattach rebuilds the detector.
            if !self.registered() {
                let ok = self.ports.register_raw() && self.registered();
                tracing::warn!(
                    event = "input.reattach",
                    component = component_name(Component::RawInput),
                    reason = reason_name(Some(ReasonCode::RawInputMissing)),
                    ok
                );
                if !ok {
                    self.lost(Component::RawInput, ReasonCode::RawInputMissing);
                }
            }
        } else {
            self.set(
                Component::RawInput,
                HealthState::Recovering,
                Some(ReasonCode::RawInputMissing),
            );
        }
        self.started.store(true, Ordering::SeqCst);
        installed
    }
    /// The input components' health. Before [`Sources::start`] everything is
    /// `Starting` and no system call is made; afterwards a detector window
    /// that disappeared is reported at once.
    pub(super) fn health(&self) -> Vec<ComponentHealth> {
        let table = self.table();
        self.health_locked(table)
    }
    /// [`Sources::health`] without waiting for the table: `None` while
    /// another thread holds it (the supervisor adapter then reuses the last
    /// value, see the `InputSources` contract).
    pub(super) fn try_health(&self) -> Option<Vec<ComponentHealth>> {
        let table = match self.table.try_lock() {
            Ok(table) => table,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        Some(self.health_locked(table))
    }
    fn health_locked(&self, mut table: MutexGuard<'_, HealthTable>) -> Vec<ComponentHealth> {
        let now = self.clock.wall_ms();
        let raw_healthy = table
            .get(Component::RawInput)
            .is_some_and(|entry| entry.state == HealthState::Healthy);
        if self.started.load(Ordering::SeqCst) && raw_healthy && !self.ports.raw_alive() {
            table.set(
                Component::RawInput,
                HealthState::Recovering,
                Some(ReasonCode::RawInputMissing),
                now,
            );
            self.invalidate();
        }
        table.snapshot(now)
    }
    /// Whether every hook is installed and none is known to be lost.
    pub(super) fn hooks_ready(&self) -> bool {
        let table = self.table();
        HOOK_COMPONENTS.iter().all(|(component, _)| {
            table
                .get(*component)
                .is_some_and(|entry| entry.state == HealthState::Healthy)
        })
    }
    /// Marks `component` lost until a reattach succeeds, and invalidates the
    /// typed prefix (H3): keys, clicks or focus changes it missed never
    /// reached the buffer.
    fn lost(&self, component: Component, reason: ReasonCode) {
        self.set(component, HealthState::Recovering, Some(reason));
        self.invalidate();
    }
    /// The detector window closed without being asked to.
    pub(super) fn raw_lost(&self) {
        if self.started.load(Ordering::SeqCst) {
            self.lost(Component::RawInput, ReasonCode::RawInputMissing);
        }
    }
    /// Whether the keyboard registration points at our detector (or cannot
    /// be read, which is not evidence that it moved).
    fn registered(&self) -> bool {
        matches!(
            self.ports.raw_registration(),
            Registration::Ours | Registration::Unknown
        )
    }
    /// Restores RawInput: re-register while the window exists, rebuild the
    /// window when it is gone or registering does not stick.
    fn restore_raw(&self) -> bool {
        if self.ports.raw_alive() && self.ports.register_raw() && self.registered() {
            return true;
        }
        self.ports.rebuild_raw() && self.ports.raw_alive()
    }
    /// Re-installs one input source: both low-level hooks for either hook
    /// component, the focus hook, or RawInput. One attempt; the caller
    /// retries with backoff. Logs `input.reattach` with the reason the
    /// component had; on success the key mirror is resynchronised (key-ups
    /// may have gone unseen) through [`Signal::InputReattached`]. A
    /// component that is healthy by the time the call gets its turn (the
    /// other hook component's reattach also replaced it) is left alone.
    ///
    /// H3: the prefix is invalidated *before* delivery is restored, so a key
    /// captured the moment the source is back already leaves a generation
    /// gap, even if the consumer drains it before it sees the signal.
    pub(super) fn reattach(&self, component: Component) -> Result<(), String> {
        if !self.started.load(Ordering::SeqCst) {
            return Err("input sources have not started".into());
        }
        let _work = self.work.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((state, prior)) = self
            .table()
            .get(component)
            .map(|entry| (entry.state, entry.reason))
        else {
            return Err(format!("{component:?} is not an input source"));
        };
        if state == HealthState::Healthy
            && (component != Component::RawInput || self.ports.raw_alive())
        {
            return Ok(());
        }
        self.invalidate();
        let ok = match component {
            Component::RawInput => {
                let ok = self.restore_raw();
                if ok {
                    self.set(Component::RawInput, HealthState::Healthy, None);
                } else {
                    self.lost(Component::RawInput, ReasonCode::RawInputMissing);
                }
                ok
            }
            _ => {
                let which = if component == Component::FocusEvents {
                    HookSet::FOCUS
                } else {
                    HookSet::LOW_LEVEL
                };
                let installed = self.ports.install_hooks(which);
                self.hooks_installed(installed, which);
                HOOK_COMPONENTS
                    .iter()
                    .any(|(hooked, kind)| *hooked == component && installed.contains(*kind))
            }
        };
        tracing::warn!(
            event = "input.reattach",
            component = component_name(component),
            reason = reason_name(prior),
            ok
        );
        if !ok {
            return Err(format!(
                "{} could not be reattached",
                component_name(component)
            ));
        }
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.signals.raise(Signal::InputReattached);
        wake();
        Ok(())
    }
}
static SOURCES: Sources<'static, WinPorts, SystemClock> =
    Sources::new(&WinPorts, &SystemClock, &session::SIGNALS);
/// The detector thread reports a window nobody asked to close.
pub(super) fn raw_lost() {
    SOURCES.raw_lost();
    wake();
}

/// The consumer side of B-06: feeds the key transitions each source saw
/// into a [`CrossCheck`] and acts on its verdicts. One reattach per
/// verdict; the supervisor retries what stays unhealthy.
pub(super) struct Liveness<'a, P: SourcePorts, C: Clock> {
    sources: &'a Sources<'a, P, C>,
    check: CrossCheck,
    epoch: u64,
}
impl<'a, P: SourcePorts, C: Clock> Liveness<'a, P, C> {
    pub(super) fn new(sources: &'a Sources<'a, P, C>) -> Self {
        Self {
            sources,
            check: CrossCheck::default(),
            epoch: sources.epoch.load(Ordering::SeqCst),
        }
    }
    /// Takes armed input faults, drains `seen` into the cross-check and
    /// handles due verdicts. Returns when the next sighting expires. Keys
    /// seen before [`Sources::start`] (RawInput runs before the hooks are
    /// installed) are dropped.
    pub(super) fn tick(&mut self, shared: &Shared, seen: &mut Vec<Sighting>) -> Option<Instant> {
        self.inject_faults();
        if !self.sources.started.load(Ordering::SeqCst) {
            seen.clear();
            return None;
        }
        let epoch = self.sources.epoch.load(Ordering::SeqCst);
        if epoch != self.epoch {
            self.epoch = epoch;
            self.check.forget();
        }
        let now = self.sources.clock.now();
        for sighting in seen.drain(..) {
            match sighting {
                Sighting::Key(source, vk, true) => {
                    self.check.saw(source, vk, now);
                }
                Sighting::Key(source, vk, false) => self.check.released(source, vk),
            }
        }
        while let Some(verdict) = self.check.expire(now) {
            self.act(shared, verdict);
        }
        // Published after the verdicts, which are unchanged: a verdict (and
        // its reattach) starts the miss count again.
        self.sources
            .hook_gap
            .store(self.check.missed(Source::Hook), Ordering::SeqCst);
        self.check.next_due()
    }
    fn inject_faults(&self) {
        let ports = self.sources.ports;
        if fault::take(FaultPoint::HookUnhook).is_some() {
            tracing::warn!(event = "fault.injected", point = "hook_unhook");
            ports.drop_hooks();
        }
        if fault::take(FaultPoint::RawinputUnregister).is_some() {
            tracing::warn!(event = "fault.injected", point = "rawinput_unregister");
            ports.unregister_raw();
        }
        if fault::take(FaultPoint::RawinputClose).is_some() {
            tracing::warn!(event = "fault.injected", point = "rawinput_close");
            ports.close_raw();
        }
    }
    fn act(&mut self, shared: &Shared, verdict: Verdict) {
        let sources = self.sources;
        // Either source missed keys the other saw: what was typed around
        // the gap is not one word, whatever the cause turns out to be (H3).
        invalidate_prefix(shared);
        match verdict {
            Verdict::HookLost => {
                tracing::warn!(
                    event = "input.check",
                    source = "keyboard_hook",
                    result = "lost"
                );
                sources.lost(Component::KeyboardHook, ReasonCode::HookMissing);
                let _ = sources.reattach(Component::KeyboardHook);
            }
            Verdict::RawLost => {
                if !sources.ports.raw_alive() {
                    // Rebuilding the window is the supervisor's job.
                    tracing::warn!(
                        event = "input.check",
                        source = "raw_input",
                        result = "window_missing"
                    );
                    sources.raw_lost();
                    return;
                }
                match sources.ports.raw_registration() {
                    Registration::Missing | Registration::Elsewhere => {
                        tracing::warn!(
                            event = "input.check",
                            source = "raw_input",
                            result = "unregistered"
                        );
                        sources.lost(Component::RawInput, ReasonCode::RawInputMissing);
                        let _ = sources.reattach(Component::RawInput);
                    }
                    Registration::Ours | Registration::Unknown => {
                        // 待实测: remote sessions deliver keys with no
                        // device handle, which the detector drops.
                        tracing::info!(
                            event = "input.check",
                            source = "raw_input",
                            result = "registered"
                        );
                    }
                }
            }
        }
    }
}

fn run_consumer(
    shared: Arc<Shared>,
    hooks: &'static HookPipe<'static>,
    raw: &'static Lane<RawRecord>,
) {
    let clock = SystemClock;
    let mut consumer = Consumer::default();
    let mut monitor = Monitor::new(
        &PHYSICAL,
        &SystemAsyncKeys,
        &clock,
        &session::SIGNALS,
        own_input(),
    );
    let mut liveness = Liveness::new(&SOURCES);
    while !shared.shutdown.load(Ordering::SeqCst) {
        let drained = catch_unwind(AssertUnwindSafe(|| {
            session::inject_fault(&session::SESSION_END);
            let handled = consumer.drain(&shared, hooks, raw, &WinForeground, &mut |candidate| {
                // H3: a reattach or session signal is handled before any
                // candidate; one that crosses it is not a word.
                if !monitor.admits(&shared, candidate.generation) {
                    tracing::info!(event = "input.refused", reason = "signal");
                    return false;
                }
                // A lost hook leaves the key mirror and focus epochs stale:
                // nothing is injected until it is back (B-06).
                if !engine_ready() {
                    tracing::warn!(event = "input.refused", reason = "hook_missing");
                    return false;
                }
                enqueue(&shared, candidate)
            });
            let checked = liveness.tick(&shared, consumer.seen());
            let next = match (monitor.tick(&shared), checked) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            (handled, next)
        }));
        let (handled, next) = match drained {
            Ok(drained) => drained,
            Err(_) => {
                tracing::error!(event = "input.consumer_panic");
                consumer.reset();
                shared.generation.fetch_add(1, Ordering::SeqCst);
                (1, None)
            }
        };
        if handled == 0 {
            let wait = next.map_or(IDLE_PARK, |due| {
                due.saturating_duration_since(clock.now()).min(IDLE_PARK)
            });
            std::thread::park_timeout(wait);
        }
    }
    rawinput::close_window(true);
}

/// Creates both lanes, starts the consumer and the session window, then the
/// RawInput detector (which also starts the UIA protection sampler) and the
/// hooks. A hook that fails to install keeps the engine paused; the
/// supervisor can reattach it later.
pub(crate) fn start_input_sources(shared: Arc<Shared>) -> Result<(), String> {
    let hooks = hook::pipe(test_input_enabled());
    let raw = rawinput::lane();
    let consumer_shared = shared.clone();
    let consumer = std::thread::Builder::new()
        .name("quicker-input".into())
        .spawn(move || run_consumer(consumer_shared, hooks, raw))
        .map_err(|error| format!("Input consumer could not start: {error}"))?;
    CONSUMER.set(consumer.thread().clone()).ok();
    SOURCES.bind(&shared);
    session::start();
    rawinput::start(shared.clone())?;
    if SOURCES.start() != HookSet::ALL {
        shared.paused.store(true, Ordering::SeqCst);
        *shared.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(ExecutionResult {
            id: "engine".into(),
            status: "failed".into(),
            message:
                "Windows keyboard/mouse hook installation failed; automatic input remains paused"
                    .into(),
            completed_steps: 0,
            side_effects: false,
        });
    }
    Ok(())
}
/// Health of the four input components (keyboard hook, mouse hook, focus
/// events, RawInput) for the supervisor (B-06). `Starting` until the engine
/// starts; `Recovering` with `HookInstallFailed`, `HookMissing` or
/// `RawInputMissing` while a source is down. Never `Failed`.
pub fn input_health() -> Vec<ComponentHealth> {
    SOURCES.health()
}
/// [`input_health`] for the supervisor's tick, which must not block: `None`
/// while the health table is held elsewhere.
pub fn try_input_health() -> Option<Vec<ComponentHealth>> {
    SOURCES.try_health()
}
/// [`Sources::hook_gap`] of the production sources, for the
/// `hook_heartbeat` self-test check.
pub(super) fn hook_gap() -> bool {
    SOURCES.hook_gap()
}
/// Re-installs one input component (one attempt; blocks up to a few
/// seconds). Errors for components that are not input sources and before
/// the engine starts.
pub fn reattach(component: Component) -> Result<(), String> {
    SOURCES.reattach(component)
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

    /// The supervisor's read never waits for the health table (P5).
    #[test]
    fn try_input_health_does_not_wait_for_the_table() {
        let held = SOURCES.table();
        assert!(try_input_health().is_none());
        drop(held);
        let states = |health: Vec<ComponentHealth>| {
            health
                .into_iter()
                .map(|h| (h.component, h.state, h.reason))
                .collect::<Vec<_>>()
        };
        assert_eq!(try_input_health().map(states), Some(states(input_health())));
    }
}
