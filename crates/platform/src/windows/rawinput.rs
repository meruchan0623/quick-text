//! Thin bridge around the vendored Espanso Windows RawInput detector/injector.
//! Matching remains Quicker Text's domain compiler; UIA only supplies optional vetoes.
//!
//! B-02: the detector callback only reads atomics and the foreground input
//! context, pushes one fixed-size [`RawRecord`] onto a bounded lock-free
//! queue and returns. Text decoding and matching run on the input consumer.
//!
//! B-06: the detector window can be closed (WM_CLOSE from anyone) and the
//! process-wide registration can be replaced or removed. An unexpected
//! close is reported to the input facade at once; [`registration`],
//! [`register`] and [`rebuild`] let the facade check and restore both.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use super::{
    executor::{Candidate, CandidateProof},
    foreground::WinForeground,
    hook::{expected_key_text, KeyRecord},
    ime::ime_active,
    inject::Erasure,
    input::{self, Lane, Registration},
    keystate::{KeyBits, PHYSICAL},
    protection::{protection_block, protection_worker},
};
use crate::traits::{ForegroundProbe, InputContext};
use arc_swap::ArcSwapOption;
use std::ffi::c_void;
use std::sync::{atomic::AtomicUsize, PoisonError};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::{
    GetRegisteredRawInputDevices, RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_INPUTSINK,
    RIDEV_REMOVE,
};

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct InputEvent {
    event_type: i32,
    buffer: [u16; 24],
    buffer_len: i32,
    key_code: i32,
    variant: i32,
    status: i32,
    has_known_source: i32,
}
impl InputEvent {
    /// A key-down as the vendored detector reports it. `hardware` sets
    /// `has_known_source` (a RawInput device handle was present).
    #[cfg(test)]
    pub(super) fn key_down(vk: u16, text: &str, hardware: bool) -> Self {
        let mut buffer = [0u16; 24];
        let mut len = 0;
        for (slot, unit) in buffer.iter_mut().zip(text.encode_utf16()) {
            *slot = unit;
            len += 1;
        }
        Self {
            event_type: 1,
            buffer,
            buffer_len: len,
            key_code: i32::from(vk),
            variant: 0,
            status: 1,
            has_known_source: i32::from(hardware),
        }
    }
    /// The matching key-up (no text).
    #[cfg(test)]
    pub(super) fn key_up(vk: u16, hardware: bool) -> Self {
        Self {
            status: INPUT_STATUS_RELEASED,
            ..Self::key_down(vk, "", hardware)
        }
    }
}
/// The detector's `INPUT_EVENT_TYPE_KEYBOARD` and `INPUT_STATUS_*` (native.h).
const INPUT_EVENT_TYPE_KEYBOARD: i32 = 1;
const INPUT_STATUS_PRESSED: i32 = 1;
const INPUT_STATUS_RELEASED: i32 = 2;
#[repr(C)]
struct InitOptions {
    keyboard_layout_cache_interval: i32,
}
extern "C" {
    fn detect_initialize(
        context: *mut c_void,
        options: *mut InitOptions,
        error: *mut i32,
    ) -> *mut c_void;
    fn detect_eventloop(
        window: *mut c_void,
        callback: extern "C" fn(*mut c_void, InputEvent),
    ) -> i32;
    fn detect_destroy(window: *mut c_void) -> i32;
}
pub(super) static READY: AtomicBool = AtomicBool::new(false);
/// RawInput queue capacity in records (key-downs and key-ups). 待实测:
/// about a second of fast typing plus autorepeat while the consumer is
/// descheduled. A full queue drops the key; the generation a key-down
/// already took leaves a gap, so no later key joins the keys before it. A
/// dropped key-up only delays the cross-check (`liveness::REPEAT_RUN`).
pub(super) const RAW_QUEUE_CAPACITY: usize = 1024;
/// no-regression #7: the detector re-reads the keyboard layout on every key.
pub(super) const KEYBOARD_LAYOUT_CACHE_INTERVAL: i32 = 0;
static RAW_LANE: OnceLock<Lane<RawRecord>> = OnceLock::new();
/// The detector window while its event loop runs (0 otherwise).
static RAW_WINDOW: AtomicUsize = AtomicUsize::new(0);
/// Set before we close the detector window ourselves (shutdown, rebuild);
/// the detector thread consumes it and does not report the close as a loss.
static EXPECTED_CLOSE: AtomicBool = AtomicBool::new(false);
/// The window class the vendored detector registers (native.cpp).
const DETECTOR_CLASS: &str = "EspansoDetect";
/// How long startup waits for the detector window.
const DETECTOR_START_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a rebuild waits for the old detector thread to exit.
const DETECTOR_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// RawInput usage page and usages the detector registers.
const USAGE_PAGE_GENERIC: u16 = 0x01;
const USAGE_MOUSE: u16 = 0x02;
const USAGE_KEYBOARD: u16 = 0x06;
static LAST_SOURCE: Mutex<&str> = Mutex::new("waiting_for_input");
static LAST_PROCESS: Mutex<String> = Mutex::new(String::new());
thread_local! { static PROOF: std::cell::RefCell<Option<Proof>> = const {std::cell::RefCell::new(None)}; }

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    pub(super) focus: u64,
    pub(super) epoch: u64,
    pub(super) caret: u64,
    pub(super) revision: u64,
    pub(super) layout: usize,
}
#[derive(Clone)]
pub(super) struct Proof {
    stamp: Stamp,
    count: usize,
    at: Instant,
    ime_unverified: bool,
}
impl Proof {
    pub(super) fn erasure(&self) -> Erasure {
        Erasure {
            count: self.count,
            at: self.at,
            ime_unverified: self.ime_unverified,
        }
    }
}
#[derive(Clone)]
pub(super) enum Kind {
    Text(String),
    Backspace,
    Neutral,
    Reset,
}
/// The typed text is redacted (B-09).
impl std::fmt::Debug for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => f
                .debug_tuple("Text")
                .field(&quicker_domain::Redacted(text))
                .finish(),
            Self::Backspace => f.write_str("Backspace"),
            Self::Neutral => f.write_str("Neutral"),
            Self::Reset => f.write_str("Reset"),
        }
    }
}
pub(super) struct Event {
    pub(super) kind: Kind,
    pub(super) stamp: Stamp,
    pub(super) generation: u64,
    pub(super) source: &'static str,
    pub(super) captured_allowed: bool,
}
#[derive(Default)]
pub(super) struct Buffer {
    pub(super) text: String,
    stamp: Option<Stamp>,
    generation: u64,
}
/// The detection buffer is redacted (B-09).
impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
            .field("text", &quicker_domain::Redacted(&self.text))
            .field("stamp", &self.stamp)
            .field("generation", &self.generation)
            .finish()
    }
}
impl Buffer {
    // no-regression #3: stamp or generation discontinuity clears the buffer.
    fn push(&mut self, event: &Event, allowed: bool) {
        if !allowed
            || self.stamp.as_ref() != Some(&event.stamp)
            || (self.generation != 0 && self.generation.checked_add(1) != Some(event.generation))
        {
            self.text.clear();
        }
        self.stamp = Some(event.stamp.clone());
        self.generation = event.generation;
        if !allowed {
            return;
        }
        edit(&mut self.text, &event.kind);
    }
}
/// Applies one key to a typed-key buffer. The matcher's buffer and the busy
/// shadow (S-19) edit by the same rules.
fn edit(text: &mut String, kind: &Kind) {
    match kind {
        Kind::Text(typed) => text.push_str(typed),
        Kind::Backspace => {
            text.pop();
        }
        Kind::Neutral => {}
        Kind::Reset => text.clear(),
    }
    while text.chars().count() > 1024 {
        let Some(first) = text.chars().next() else {
            break;
        };
        text.drain(..first.len_utf8());
    }
}
fn kind(vkey: u16, text: Option<String>, modified: bool) -> Kind {
    match vkey {
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_CAPITAL | VK_CONTROL | VK_LCONTROL | VK_RCONTROL
        | VK_MENU | VK_LMENU | VK_RMENU => Kind::Neutral,
        VK_BACK if !modified => Kind::Backspace,
        VK_TAB | VK_RETURN | VK_ESCAPE | VK_DELETE | VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN
        | VK_HOME | VK_END | VK_PRIOR | VK_NEXT => Kind::Reset,
        _ if modified => Kind::Reset,
        _ => text
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
            .map(Kind::Text)
            .unwrap_or(Kind::Reset),
    }
}
/// Current transaction stamp: the probe's input context plus the compiled
/// configuration revision. `None` when either cannot be read.
pub(super) fn stamp(shared: &Shared, probe: &impl ForegroundProbe) -> Option<Stamp> {
    let revision = shared.compiled.try_lock().ok()?.config.revision;
    let context = probe.input_context()?;
    Some(stamp_of(context, revision))
}
fn stamp_of(context: InputContext, revision: u64) -> Stamp {
    Stamp {
        focus: context.focus,
        epoch: context.focus_epoch,
        caret: context.caret_epoch,
        revision,
        layout: context.layout as usize,
    }
}
/// One hardware key transition, captured by the detector callback. Fixed
/// size and `Copy`: building and queueing it never allocates.
#[derive(Clone, Copy)]
pub(super) struct RawRecord {
    vk: u16,
    /// A key-up only tells the hook cross-check (B-06, H4) that the next
    /// down of the key is a new press: no text, no generation of its own.
    down: bool,
    text: [u16; 24],
    len: usize,
    /// `None` when the foreground input context could not be read (always
    /// for a key-up); the key still counts for the hook cross-check (B-06)
    /// but never becomes text.
    context: Option<InputContext>,
    pub(super) generation: u64,
    modified: bool,
    /// Matching was paused when the key-down was captured.
    paused: bool,
    /// An action was running when the key-down was captured: the matcher
    /// drops the key, the busy shadow takes it (S-19).
    busy: bool,
    /// How many records the lane had dropped, or lost to a callback panic,
    /// when this one was queued ([`push_raw`]). A change between two
    /// records means a key between them is missing; the busy shadow breaks
    /// its prefix there (S-19), as the generation gap breaks the matcher's.
    lost: u64,
}
/// The key's text is redacted (B-09), N in UTF-16 units, and so is which key
/// it was.
impl std::fmt::Debug for RawRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawRecord")
            .field("vk", &quicker_domain::RedactedKey)
            .field("down", &self.down)
            .field("text", &quicker_domain::RedactedLen(self.len))
            .field("context", &self.context)
            .field("generation", &self.generation)
            .field("modified", &self.modified)
            .field("paused", &self.paused)
            .field("busy", &self.busy)
            .field("lost", &self.lost)
            .finish()
    }
}
impl RawRecord {
    /// The detector's virtual-key code (generic Shift/Ctrl/Alt).
    pub(super) fn vk(&self) -> u16 {
        self.vk
    }
    /// Key-down (true) or key-up.
    pub(super) fn down(&self) -> bool {
        self.down
    }
}
/// Callback body: filters the detector event and captures everything the
/// consumer needs. Atomics plus the probe's input context only. Every
/// accepted key-down takes the next generation, so a key that is later lost
/// leaves a gap instead of joining its neighbours. A key-up takes neither a
/// generation nor the input context.
pub(super) fn raw_record(
    shared: &Shared,
    keys: &KeyBits,
    event: &InputEvent,
    probe: &impl ForegroundProbe,
) -> Option<RawRecord> {
    let down = hardware_key(event.event_type, event.status, event.has_known_source)?;
    if !down {
        return Some(RawRecord {
            vk: event.key_code as u16,
            down: false,
            text: [0; 24],
            len: 0,
            context: None,
            generation: shared.generation.load(Ordering::SeqCst),
            modified: false,
            paused: false,
            busy: false,
            lost: 0,
        });
    }
    let generation = shared
        .generation
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    let context = probe.input_context();
    Some(RawRecord {
        vk: event.key_code as u16,
        down: true,
        text: event.buffer,
        len: usize::try_from(event.buffer_len.clamp(0, 24)).unwrap_or(0),
        context,
        generation,
        modified: keys.modified(),
        paused: shared.paused.load(Ordering::SeqCst),
        busy: shared.busy.load(Ordering::SeqCst),
        lost: 0,
    })
}
/// Queues a captured record on the RawInput lane, stamped with the lane's
/// loss count so far ([`RawRecord::lost`]). The detector callback is the
/// only producer, so the stamp is exact. Lock-free, no allocation.
pub(super) fn push_raw(lane: &Lane<RawRecord>, mut record: RawRecord) -> bool {
    record.lost = lane.dropped().wrapping_add(lane.panics());
    lane.push(record)
}
/// The RawInput lane; created once, before the detector starts.
pub(super) fn lane() -> &'static Lane<RawRecord> {
    RAW_LANE.get_or_init(|| Lane::new(RAW_QUEUE_CAPACITY, input::wake))
}
/// Consumer side: the matcher event for a captured hardware key-down (none
/// for a key-up). The configuration revision is read here; a poisoned
/// compiler or a missing input context fails closed (the key is dropped and
/// its generation gap clears the buffer).
pub(super) fn event_from(shared: &Shared, record: &RawRecord) -> Option<Event> {
    if !record.down {
        return None;
    }
    let context = record.context?;
    let revision = shared.compiled.lock().ok()?.config.revision;
    let text = record
        .text
        .get(..record.len)
        .and_then(|units| String::from_utf16(units).ok());
    Some(Event {
        kind: kind(record.vk, text, record.modified),
        stamp: stamp_of(context, revision),
        generation: record.generation,
        source: "rawinput_hardware",
        captured_allowed: !record.paused && !record.busy,
    })
}
/// Consumer side, acceptance mode only: the matcher event for an injected
/// key the hook captured.
pub(super) fn acceptance_event(shared: &Shared, key: &KeyRecord) -> Option<Event> {
    let acceptance = key.acceptance?;
    let revision = shared.compiled.lock().ok()?.config.revision;
    // SAFETY: keyboard-state reads and ToUnicodeEx on stack buffers.
    let text = unsafe {
        expected_key_text(
            key.vk,
            key.scan,
            acceptance.shift,
            acceptance.context.layout as usize as HKL,
        )
    };
    Some(Event {
        kind: kind(key.vk, text, acceptance.modified),
        stamp: stamp_of(acceptance.context, revision),
        generation: key.generation,
        source: "acceptance_sendinput_hook",
        captured_allowed: acceptance.allowed,
    })
}
/// Records which source fed the matcher last (diagnostics only).
pub(super) fn note_source(source: &'static str) {
    *LAST_SOURCE.lock().unwrap_or_else(PoisonError::into_inner) = source;
}
/// Key-down (`Some(true)`) or key-up (`Some(false)`) from a real device;
/// `None` for anything else, including our own injected keys, which reach
/// RawInput with no device handle.
fn hardware_key(event_type: i32, status: i32, known_source: i32) -> Option<bool> {
    if event_type != INPUT_EVENT_TYPE_KEYBOARD || known_source == 0 {
        return None;
    }
    match status {
        INPUT_STATUS_PRESSED => Some(true),
        INPUT_STATUS_RELEASED => Some(false),
        _ => None,
    }
}
fn match_buffer(
    compiler: &CompiledConfig,
    text: &str,
    previous_terminator: Option<(&str, &'static str)>,
    foreground_context: &ForegroundContext,
) -> Option<(MatchResult, Option<&'static str>)> {
    previous_terminator
        .and_then(|(input, key)| {
            compiler
                .match_input(input, Some(key), foreground_context)
                .map(|matched| (matched, Some(key)))
        })
        .or_else(|| {
            compiler
                .match_input(text, None, foreground_context)
                .map(|matched| (matched, None))
        })
}
fn validate_stamp(original: &Stamp, current: Option<&Stamp>) -> Result<(), String> {
    if current != Some(original) {
        Err("Focus, caret, layout or configuration changed; input stopped".into())
    } else {
        Ok(())
    }
}
extern "C" fn callback(context: *mut c_void, event: InputEvent) {
    // No panic may unwind across the C ABI; no COM, output, lock or
    // allocation here.
    let Some(lane) = RAW_LANE.get() else {
        return;
    };
    if context.is_null() {
        return;
    }
    // SAFETY: `context` is the boxed Arc<Shared> handed to detect_initialize;
    // the detector thread keeps it alive until its event loop returns.
    let shared = unsafe { &*(context as *const Arc<Shared>) };
    lane.contained((), || {
        if let Some(record) = raw_record(shared, &PHYSICAL, &event, &WinForeground) {
            push_raw(lane, record);
        }
    });
}
/// The terminator a key types, if it is one. Safe text terminators already
/// reach the target: erase shortcut plus terminator. RawInput never claims
/// to swallow Enter or Tab.
fn terminator_of(kind: &Kind) -> Option<&'static str> {
    match kind {
        Kind::Text(value) if value == " " => Some("space"),
        Kind::Text(value) if value == ";" => Some("semicolon"),
        _ => None,
    }
}
/// Whether a match typed under `stamp` may count in `target`: the focus is
/// still the one the key was typed into, the process is known and not ours,
/// and no protected control has it.
fn eligible(target: &Foreground, stamp: &Stamp, probe: &impl ForegroundProbe) -> bool {
    target.id == stamp.focus
        && !target.process.is_empty()
        && !target.process.eq_ignore_ascii_case("quicker-text.exe")
        && probe.protection().is_none()
}
/// A rule the user typed while an action was running, which the matcher
/// dropped because it was busy (S-19). Identifiers only: never the
/// abbreviation, the typed keys or any other text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DroppedBusy {
    /// The id of the matched rule.
    pub(crate) rule_id: String,
    /// The foreground process, from the same probe read that feeds
    /// `LAST_PROCESS`.
    pub(crate) process: String,
}
/// Where [`DroppedBusy`] reports go ([`set_dropped_busy_sink`]).
type DroppedBusySink = Box<dyn Fn(DroppedBusy) + Send + Sync>;
static DROPPED_BUSY_SINK: ArcSwapOption<DroppedBusySink> = ArcSwapOption::const_empty();
/// Installs the sink for busy drops (S-19), replacing any earlier one.
/// Until a sink is installed the busy shadow does nothing at all.
///
/// The sink is called synchronously on the input consumer thread, the one
/// that drains the RawInput lane, once per report. It must not block (the
/// runtime hands each report to a bounded queue); matching waits for it.
/// A rule retyped within one stamp is reported once; after any reset of the
/// busy shadow (a new busy period, a broken prefix) it is reported again.
/// Production: `start_executor` installs the executor's history sink.
pub(crate) fn set_dropped_busy_sink(sink: Box<dyn Fn(DroppedBusy) + Send + Sync>) {
    DROPPED_BUSY_SINK.store(Some(Arc::new(sink)));
}
/// Removes the sink again (tests only).
#[cfg(test)]
pub(super) fn clear_dropped_busy_sink() {
    DROPPED_BUSY_SINK.store(None);
}
/// The busy shadow (S-19): a second typed-key buffer, apart from the
/// matcher's, for the hardware keys the matcher drops only because an
/// action is running. It matches and never executes: a match produces no
/// candidate, is reported as [`DroppedBusy`] and forgotten. The matcher's
/// buffer is untouched by it (busy keys still clear it, so a word typed
/// during an action never triggers after it).
///
/// A key enters only when everything but busy would let the matcher take
/// it: a hardware key-down (RawInput sees no injected key, ours included),
/// captured busy, not paused at capture or now, with a readable input
/// context and configuration. The match conditions are the matcher's: same
/// stamp, focus unchanged, a known process that is not ours, no protected
/// control, the same configuration revision, the same terminator rule, and
/// the key still the latest input (its generation is current).
///
/// Prefix continuity (no-regression #3) applies here too, but not through
/// consecutive generations: while busy the keyboard hook also advances the
/// generation for every hardware key-down (so a running action stops at its
/// next step), on another thread, so two consecutive busy keys lie 1, 2 or
/// more generations apart depending on how the two threads interleave.
/// Between two keys the shadow requires instead the same stamp, whose caret
/// epoch every other prefix break advances together with the generation
/// (external injected input, clicks and wheel, the stop gesture, focus
/// events, session signals and lost or rebuilt sources), and the same lane
/// loss count ([`RawRecord::lost`]), so a key the lane dropped breaks it
/// like the gap it leaves in the matcher; a key that cannot become an event
/// breaks it too. Acceptance mode's injected keys advance only the
/// generation and do not break it (test harness only).
///
/// Whenever the shadow is reset it is reset whole, its reports included:
/// on a key captured while not busy (the busy period ended), a change of
/// stamp or loss count, a key that cannot become an event, a configuration
/// change. Reports are kept only so that one rule retyped within one stamp
/// does not flood the sink.
///
/// Busy is set when the consumer hands the executor a candidate, and also
/// by src-tauri's `execute_action` for actions started from the UI (its
/// [`ManualBusy`](super::ManualBusy) claim, under the same executor lock
/// as a candidate's, held from before the window hides until the run
/// returned), so the
/// invariant that keeps two busy periods apart is: between them there is
/// either a key captured while not busy or a change of stamp. A candidate
/// is completed by a key captured while not busy that came through here
/// first; an action started from the UI by mouse or focus alone follows a
/// click or a focus change, which advances the caret or focus epoch, so the
/// first key of its period finds a new stamp. Acceptance mode, whose
/// trigger keys come from the hook, is the exception (test harness only).
#[derive(Default)]
pub(super) struct Shadow {
    text: String,
    /// The stamp of the last key folded in; `None` once the shadow reset.
    stamp: Option<Stamp>,
    /// The lane loss count of the last key folded in.
    lost: u64,
    /// The rules already reported since the shadow last reset.
    reported: Vec<String>,
}
/// The shadow's typed text is redacted (B-09); reported rule ids are shown.
impl std::fmt::Debug for Shadow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shadow")
            .field("text", &quicker_domain::Redacted(&self.text))
            .field("stamp", &self.stamp)
            .field("lost", &self.lost)
            .field("reported", &self.reported)
            .finish()
    }
}
impl Shadow {
    /// The prefix broke or the busy period ended: the next key starts a
    /// new word, and its rule may be reported again.
    fn reset(&mut self) {
        self.text.clear();
        self.stamp = None;
        self.reported.clear();
    }
}
/// Folds one RawInput record into the busy shadow and returns the busy
/// drop it completes, if any ([`Shadow`]). `event` is what [`event_from`]
/// made of the record.
pub(super) fn shadow_match(
    shared: &Shared,
    shadow: &mut Shadow,
    record: &RawRecord,
    event: Option<&Event>,
    probe: &impl ForegroundProbe,
) -> Option<DroppedBusy> {
    if !record.down {
        return None;
    }
    if !record.busy {
        shadow.reset();
        return None;
    }
    let Some(event) = event.filter(|_| !record.paused && !shared.paused.load(Ordering::SeqCst))
    else {
        shadow.reset();
        return None;
    };
    let terminator = terminator_of(&event.kind);
    // no-regression #3, for the shadow: a terminator match needs the
    // contiguous, same-stamp prefix.
    let contiguous = shadow.stamp.as_ref() == Some(&event.stamp) && shadow.lost == record.lost;
    let previous = (terminator.is_some() && contiguous).then(|| shadow.text.clone());
    if !contiguous {
        shadow.reset();
    }
    shadow.stamp = Some(event.stamp.clone());
    shadow.lost = record.lost;
    edit(&mut shadow.text, &event.kind);
    if !matches!(event.kind, Kind::Text(_))
        || event.generation != shared.generation.load(Ordering::SeqCst)
    {
        return None;
    }
    let target = probe.foreground();
    if !eligible(&target, &event.stamp, probe) {
        return None;
    }
    let Ok(compiler) = shared.compiled.lock() else {
        return None;
    };
    if compiler.config.revision != event.stamp.revision {
        shadow.reset();
        return None;
    }
    let foreground_context = ForegroundContext {
        platform: Platform::Windows,
        process: target.process.clone(),
        bundle_id: String::new(),
    };
    let (matched, _) = match_buffer(
        &compiler,
        &shadow.text,
        previous.as_deref().zip(terminator),
        &foreground_context,
    )?;
    let rule_id = compiler
        .config
        .rules
        .iter()
        .find(|rule| rule.id == matched.rule_id)?
        .id
        .clone();
    drop(compiler);
    shadow.text.clear();
    if shadow.reported.contains(&rule_id) {
        return None;
    }
    shadow.reported.push(rule_id.clone());
    Some(DroppedBusy {
        rule_id,
        process: target.process,
    })
}
/// The consumer's busy-shadow step for one RawInput record: a completed
/// busy drop goes to the sink ([`set_dropped_busy_sink`]). Without a sink
/// nothing is matched or kept.
pub(super) fn shadow_step(
    shared: &Shared,
    shadow: &mut Shadow,
    record: &RawRecord,
    event: Option<&Event>,
    probe: &impl ForegroundProbe,
) {
    let sink = DROPPED_BUSY_SINK.load();
    let Some(sink) = sink.as_ref() else {
        *shadow = Shadow::default();
        return;
    };
    if let Some(dropped) = shadow_match(shared, shadow, record, event, probe) {
        tracing::debug!(event = "rawinput.dropped_busy");
        let sink: &DroppedBusySink = sink;
        sink(dropped);
    }
}
/// Match step: folds `event` into `buffer` and returns a candidate when a
/// rule matches the current, unprotected target.
pub(super) fn detect_candidate(
    shared: &Shared,
    buffer: &mut Buffer,
    event: Event,
    probe: &impl ForegroundProbe,
) -> Option<Candidate> {
    let allowed = event.captured_allowed
        && !shared.paused.load(Ordering::SeqCst)
        && !shared.busy.load(Ordering::SeqCst);
    let terminator = terminator_of(&event.kind);
    // no-regression #3: a terminator match needs the contiguous, same-stamp prefix.
    let previous = if terminator.is_some()
        && buffer.stamp.as_ref() == Some(&event.stamp)
        && buffer.generation.checked_add(1) == Some(event.generation)
    {
        Some(buffer.text.clone())
    } else {
        None
    };
    buffer.push(&event, allowed);
    if !allowed
        || !matches!(event.kind, Kind::Text(_))
        || event.generation != shared.generation.load(Ordering::SeqCst)
    {
        return None;
    }
    let target = probe.foreground();
    *LAST_PROCESS.lock().unwrap_or_else(PoisonError::into_inner) = target.process.clone();
    if !eligible(&target, &event.stamp, probe) {
        return None;
    }
    let Ok(compiler) = shared.compiled.lock() else {
        return None;
    };
    if compiler.config.revision != event.stamp.revision {
        buffer.text.clear();
        return None;
    }
    let foreground_context = ForegroundContext {
        platform: Platform::Windows,
        process: target.process.clone(),
        bundle_id: String::new(),
    };
    let (matched, terminator) = match_buffer(
        &compiler,
        &buffer.text,
        previous.as_deref().zip(terminator),
        &foreground_context,
    )?;
    let risky = ime_active() || (event.stamp.layout & 0x3ff) == 4;
    let rule = compiler
        .config
        .rules
        .iter()
        .find(|rule| rule.id == matched.rule_id)
        .cloned()?;
    let proof = Proof {
        stamp: event.stamp.clone(),
        count: matched.erase_chars + usize::from(terminator.is_some()),
        at: Instant::now(),
        ime_unverified: risky,
    };
    Some(Candidate {
        rule,
        matched,
        proof: CandidateProof::Espanso(proof),
        focus: target,
        generation: event.generation,
        terminator: terminator.map(str::to_owned),
    })
}
/// Starts the UIA protection sampler and the detector thread. The lane must
/// exist (see [`lane`]) before keys can be captured.
pub(super) fn start(shared: Arc<Shared>) -> Result<(), String> {
    protection_worker(shared.clone());
    spawn_detector(shared)
}
/// Starts one detector thread and waits until its window exists.
fn spawn_detector(shared: Arc<Shared>) -> Result<(), String> {
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("quicker-rawinput".into())
        .spawn(move || run_detector(shared, &startup_tx))
        .map_err(|_| "RawInput thread failed to start".to_string())?;
    startup_rx
        .recv_timeout(DETECTOR_START_TIMEOUT)
        .map_err(|_| "Espanso RawInput initialization timed out".to_string())?
}
/// Detector thread body: create the window, run its loop, and report a
/// close nobody asked for to the input facade (B-06). The loss no longer
/// pauses the engine; health reports the window missing until a rebuild.
fn run_detector(shared: Arc<Shared>, startup: &mpsc::SyncSender<Result<(), String>>) {
    let class = wide(DETECTOR_CLASS);
    // SAFETY: plain Win32 call; `class` outlives it. A previous detector
    // thread leaves its class registered (native.cpp never unregisters), and
    // RegisterClassEx would then fail; this fails harmlessly while a window
    // of the class still exists.
    unsafe {
        UnregisterClassW(class.as_ptr(), GetModuleHandleW(std::ptr::null()));
    }
    let context = Box::new(shared);
    let mut options = InitOptions {
        keyboard_layout_cache_interval: KEYBOARD_LAYOUT_CACHE_INTERVAL,
    };
    let mut error = 0;
    // SAFETY: `context` outlives the event loop below; the detector only
    // hands the pointer back to `callback` on this thread.
    let window = unsafe {
        detect_initialize(
            (&*context as *const Arc<Shared>).cast_mut().cast(),
            &mut options,
            &mut error,
        )
    };
    if window.is_null() {
        // native.cpp keeps its window when only the registration failed;
        // destroy it so the class can be registered again.
        // SAFETY: enumerates this thread's own windows.
        unsafe {
            EnumThreadWindows(GetCurrentThreadId(), Some(destroy_detector), 0);
        }
        let _ = startup.send(Err(format!(
            "Espanso RawInput initialization failed ({error})"
        )));
        return;
    }
    RAW_WINDOW.store(window as usize, Ordering::SeqCst);
    READY.store(true, Ordering::SeqCst);
    let _ = startup.send(Ok(()));
    // SAFETY: `window` came from detect_initialize on this thread.
    unsafe {
        detect_eventloop(window, callback);
    }
    let expected =
        EXPECTED_CLOSE.swap(false, Ordering::SeqCst) || context.shutdown.load(Ordering::SeqCst);
    READY.store(false, Ordering::SeqCst);
    // SAFETY: `window` belongs to this thread; IsWindow guards a window the
    // loop already destroyed.
    unsafe {
        if IsWindow(window as HWND) != 0 {
            detect_destroy(window);
        }
    }
    let _ = RAW_WINDOW.compare_exchange(window as usize, 0, Ordering::SeqCst, Ordering::SeqCst);
    tracing::info!(event = "rawinput.closed", expected);
    if !expected {
        input::raw_lost();
    }
}
/// EnumThreadWindows callback: destroys detector-class windows only.
unsafe extern "system" fn destroy_detector(window: HWND, _: LPARAM) -> windows_sys::core::BOOL {
    let mut name = [0u16; 32];
    // SAFETY: `name` holds the length we pass.
    let len = unsafe { GetClassNameW(window, name.as_mut_ptr(), name.len() as i32) };
    let ours = usize::try_from(len)
        .ok()
        .and_then(|len| name.get(..len))
        .is_some_and(|name| DETECTOR_CLASS.encode_utf16().eq(name.iter().copied()));
    if ours {
        // SAFETY: a window of this thread, found by enumeration.
        unsafe {
            DestroyWindow(window);
        }
    }
    1
}
/// Asks the detector window to close. `expected` for engine shutdown; the
/// RawinputClose fault passes `false` so the loss is reported like any
/// outside WM_CLOSE.
pub(super) fn close_window(expected: bool) {
    let window = RAW_WINDOW.load(Ordering::SeqCst);
    if window != 0 {
        if expected {
            EXPECTED_CLOSE.store(true, Ordering::SeqCst);
        }
        // SAFETY: posting to a stale handle fails harmlessly.
        unsafe {
            PostMessageW(window as HWND, WM_CLOSE, 0, 0);
        }
    }
}
/// Whether the detector window exists.
pub(super) fn alive() -> bool {
    let window = RAW_WINDOW.load(Ordering::SeqCst);
    // SAFETY: IsWindow accepts any handle value.
    window != 0 && unsafe { IsWindow(window as HWND) } != 0
}
fn device(usage: u16, flags: u32, target: HWND) -> RAWINPUTDEVICE {
    RAWINPUTDEVICE {
        usUsagePage: USAGE_PAGE_GENERIC,
        usUsage: usage,
        dwFlags: flags,
        hwndTarget: target,
    }
}
/// Where the process-wide keyboard RawInput registration points. Only one
/// target per usage exists per process and the last registration wins
/// (tao registers its own at event-loop creation, G6).
pub(super) fn registration() -> Registration {
    let window = RAW_WINDOW.load(Ordering::SeqCst);
    let mut devices = [device(0, 0, std::ptr::null_mut()); 16];
    let mut count = devices.len() as u32;
    // SAFETY: `devices` holds `count` entries of the size we pass.
    let found = unsafe {
        GetRegisteredRawInputDevices(
            devices.as_mut_ptr(),
            &mut count,
            size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    if found == u32::MAX {
        return Registration::Unknown;
    }
    let keyboard = devices
        .iter()
        .take(found as usize)
        .find(|entry| entry.usUsagePage == USAGE_PAGE_GENERIC && entry.usUsage == USAGE_KEYBOARD);
    match keyboard {
        None => Registration::Missing,
        Some(entry)
            if window != 0
                && entry.hwndTarget as usize == window
                && entry.dwFlags & RIDEV_INPUTSINK != 0 =>
        {
            Registration::Ours
        }
        Some(_) => Registration::Elsewhere,
    }
}
/// Registers keyboard and mouse RawInput for the detector window again, as
/// native.cpp does at startup. 待实测: registering from a thread that does
/// not own the window; the facade falls back to [`rebuild`] on failure.
pub(super) fn register() -> bool {
    let window = RAW_WINDOW.load(Ordering::SeqCst);
    if window == 0 {
        return false;
    }
    let devices = [
        device(USAGE_KEYBOARD, RIDEV_INPUTSINK, window as HWND),
        device(USAGE_MOUSE, RIDEV_INPUTSINK, window as HWND),
    ];
    // SAFETY: `devices` holds the count and entry size we pass.
    unsafe {
        RegisterRawInputDevices(
            devices.as_ptr(),
            devices.len() as u32,
            size_of::<RAWINPUTDEVICE>() as u32,
        ) != 0
    }
}
/// Removes the process keyboard and mouse registrations (RawinputUnregister
/// fault only).
pub(super) fn unregister() {
    let devices = [
        device(USAGE_KEYBOARD, RIDEV_REMOVE, std::ptr::null_mut()),
        device(USAGE_MOUSE, RIDEV_REMOVE, std::ptr::null_mut()),
    ];
    // SAFETY: `devices` holds the count and entry size we pass.
    unsafe {
        RegisterRawInputDevices(
            devices.as_ptr(),
            devices.len() as u32,
            size_of::<RAWINPUTDEVICE>() as u32,
        );
    }
}
/// Replaces the detector: closes a window that still exists, waits for its
/// thread to let go, and starts a new one (S-03). 待实测: rebuild latency.
pub(super) fn rebuild() -> bool {
    let Some(shared) = ENGINE.get() else {
        return false;
    };
    let old = RAW_WINDOW.load(Ordering::SeqCst);
    if old != 0 {
        EXPECTED_CLOSE.store(true, Ordering::SeqCst);
        // SAFETY: posting to a stale handle fails harmlessly.
        if unsafe { PostMessageW(old as HWND, WM_CLOSE, 0, 0) } == 0 {
            EXPECTED_CLOSE.store(false, Ordering::SeqCst);
        }
        let deadline = Instant::now() + DETECTOR_CLOSE_TIMEOUT;
        while RAW_WINDOW.load(Ordering::SeqCst) == old {
            if Instant::now() >= deadline {
                tracing::warn!(event = "rawinput.rebuild_stuck");
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    // A flag the old thread did not consume must not mark the next close
    // as expected.
    EXPECTED_CLOSE.store(false, Ordering::SeqCst);
    match spawn_detector(shared.clone()) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(event = "rawinput.rebuild_failed", error = %error);
            false
        }
    }
}
pub(super) fn install(proof: Option<Proof>) {
    PROOF.with(|p| *p.borrow_mut() = proof);
}
/// Fails when this thread runs under a match proof whose stamp no longer
/// equals the current one (focus, epochs, layout or revision changed).
pub(super) fn guard(shared: &Shared, probe: &impl ForegroundProbe) -> Result<(), String> {
    PROOF.with(|p| {
        if let Some(proof) = p.borrow().as_ref() {
            validate_stamp(&proof.stamp, stamp(shared, probe).as_ref())?;
        }
        Ok(())
    })
}
pub(super) fn status() -> serde_json::Value {
    let source = *LAST_SOURCE.lock().unwrap_or_else(PoisonError::into_inner);
    let process = LAST_PROCESS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    serde_json::json!({"backend":"espanso_win32_v2.4.1", "source":source,
        "ready":READY.load(Ordering::SeqCst), "available":READY.load(Ordering::SeqCst),
        "process":process, "reason":protection_block().unwrap_or_else(||"RawInput capture; delivery and IME composition are unverified".into()),
        "terminators":{"space":"post-input erasure; final IME consumption unknown","semicolon":"post-input erasure", "enter":"disabled","tab":"disabled"},
        "ime":"RawInput translated characters and backspace compatibility; composition and final delivery unverified"})
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
    fn original() -> Stamp {
        Stamp {
            focus: 1,
            epoch: 2,
            caret: 3,
            revision: 4,
            layout: 0x0804,
        }
    }
    fn event(text: &str, generation: u64) -> Event {
        Event {
            kind: Kind::Text(text.into()),
            stamp: original(),
            generation,
            source: "fixture",
            captured_allowed: true,
        }
    }
    #[test]
    fn production_buffer_preserves_case_digits_punctuation_and_shift() {
        let mut b = Buffer::default();
        b.push(&event("Q", 1), true);
        let mut neutral = event("", 2);
        neutral.kind = kind(VK_SHIFT, None, false);
        b.push(&neutral, true);
        b.push(&event("t12;", 3), true);
        assert_eq!(b.text, "Qt12;");
        let mut back = event("", 4);
        back.kind = kind(VK_BACK, None, false);
        b.push(&back, true);
        assert_eq!(b.text, "Qt12");
        assert!(matches!(kind(VK_CONTROL, None, true), Kind::Neutral));
        assert!(matches!(kind(0x43, Some("c".into()), true), Kind::Reset));
    }
    #[test]
    fn production_buffer_discards_focus_config_gaps_pause_and_navigation() {
        for change in 0..6 {
            let mut b = Buffer::default();
            b.push(&event("prefix", 1), true);
            let mut next = event("x", 2);
            match change {
                0 => next.stamp.focus += 1,
                1 => next.stamp.epoch += 1,
                2 => next.stamp.revision += 1,
                3 => next.generation += 1,
                4 => {}
                _ => next.kind = kind(VK_LEFT, None, false),
            };
            b.push(&next, change != 4);
            assert_eq!(b.text, if change >= 4 { "" } else { "x" });
        }
    }
    #[test]
    fn altgr_modifier_sequence_preserves_prefix_but_real_ctrl_chord_clears_it() {
        let mut buffer = Buffer::default();
        buffer.push(&event("mail", 1), true);
        for (v, g) in [(VK_CONTROL, 2), (VK_MENU, 3)] {
            let mut next = event("", g);
            next.kind = kind(v, None, true);
            buffer.push(&next, true);
        }
        let mut at = event("@", 4);
        at.kind = kind(0x51, Some("@".into()), false);
        buffer.push(&at, true);
        assert_eq!(buffer.text, "mail@");
        let mut copy = event("", 5);
        copy.kind = kind(0x43, Some("c".into()), true);
        buffer.push(&copy, true);
        assert!(buffer.text.is_empty());
    }
    #[test]
    fn raw_hardware_filter_rejects_own_or_external_orphan_input() {
        assert_eq!(hardware_key(1, 1, 1), Some(true));
        // H4: key-ups reach the hook cross-check (never the matcher).
        assert_eq!(hardware_key(1, 2, 1), Some(false));
        for args in [(1, 1, 0), (1, 2, 0), (2, 1, 1), (1, 0, 1), (0, 0, 0)] {
            assert_eq!(hardware_key(args.0, args.1, args.2), None, "{args:?}");
        }
    }
    #[test]
    fn transaction_stamp_rejects_every_context_change() {
        let s = original();
        assert!(validate_stamp(&s, Some(&s)).is_ok());
        assert!(validate_stamp(&s, None).is_err());
        for change in 0..5 {
            let mut other = s.clone();
            match change {
                0 => other.focus += 1,
                1 => other.epoch += 1,
                2 => other.caret += 1,
                3 => other.revision += 1,
                _ => other.layout += 1,
            };
            assert!(validate_stamp(&s, Some(&other)).is_err());
        }
    }
    #[test]
    fn semicolon_consumption_priority_and_immediate_fallback_preserve_contract() {
        let foreground_context = ForegroundContext {
            platform: Platform::Windows,
            process: "notepad.exe".into(),
            ..Default::default()
        };
        let immediate = Rule {
            id: "immediate".into(),
            match_spec: MatchSpec {
                kind: MatchKind::Literal,
                value: "Qt12;".into(),
            },
            trigger_mode: TriggerMode::Immediate,
            ..Default::default()
        };
        for consume in [true, false] {
            let terminator = Rule {
                id: "terminated".into(),
                match_spec: MatchSpec {
                    kind: MatchKind::Literal,
                    value: "Qt12".into(),
                },
                trigger_mode: TriggerMode::Terminator,
                terminator: Some(Terminator {
                    keys: vec!["semicolon".into()],
                    consume,
                }),
                ..Default::default()
            };
            let compiler = compile_config(&Config {
                // The longer immediate rule is first, but an explicitly matched
                // semicolon terminator still takes priority.
                rules: vec![immediate.clone(), terminator],
                ..Default::default()
            })
            .unwrap();
            let (matched, key) = match_buffer(
                &compiler,
                "Qt12;",
                Some(("Qt12", "semicolon")),
                &foreground_context,
            )
            .unwrap();
            assert_eq!(matched.rule_id, "terminated");
            assert_eq!(key, Some("semicolon"));
            assert_eq!(matched.consume_terminator, consume);
            assert_eq!(matched.erase_chars + usize::from(key.is_some()), 5);
            let returned = if matched.consume_terminator {
                None
            } else {
                key
            };
            assert_eq!(returned, if consume { None } else { Some("semicolon") });
        }
        let compiler = compile_config(&Config {
            rules: vec![immediate],
            ..Default::default()
        })
        .unwrap();
        let (matched, key) = match_buffer(
            &compiler,
            "Qt12;",
            Some(("Qt12", "semicolon")),
            &foreground_context,
        )
        .unwrap();
        assert_eq!(matched.rule_id, "immediate");
        assert_eq!(matched.matched, "Qt12;");
        assert_eq!(matched.erase_chars, 5);
        assert_eq!(key, None);
    }
    #[test]
    fn domain_case_and_space_consumption_keep_custom_contract() {
        let rule = Rule {
            id: "custom".into(),
            match_spec: MatchSpec {
                kind: MatchKind::Literal,
                value: "Qt12;".into(),
            },
            trigger_mode: TriggerMode::Terminator,
            terminator: Some(Terminator {
                keys: vec!["space".into()],
                consume: false,
            }),
            ..Default::default()
        };
        let compiler = compile_config(&Config {
            rules: vec![rule],
            ..Default::default()
        })
        .unwrap();
        let foreground_context = ForegroundContext {
            platform: Platform::Windows,
            process: "NOTEPAD.EXE".into(),
            ..Default::default()
        };
        let (matched, term) = match_buffer(
            &compiler,
            "Qt12; ",
            Some(("Qt12;", "space")),
            &foreground_context,
        )
        .unwrap();
        assert_eq!(term, Some("space"));
        assert!(!matched.consume_terminator);
        assert_eq!(matched.erase_chars, 5);
        assert!(match_buffer(
            &compiler,
            "qt12; ",
            Some(("qt12;", "space")),
            &foreground_context
        )
        .is_none());
        assert!(match_buffer(&compiler, "Qt12;", None, &foreground_context).is_none());
        assert!(match_buffer(
            &compiler,
            "Qt12; ",
            Some(("Qt12;", "space")),
            &ForegroundContext {
                process: "other.exe".into(),
                ..foreground_context
            }
        )
        .is_some());
        for key in [VK_TAB, VK_RETURN] {
            assert!(matches!(kind(key, Some(" ".into()), false), Kind::Reset));
        }
    }
    #[test]
    fn debug_never_shows_the_typed_text() {
        // B-09: the detection buffer, the busy shadow, a key's text and a
        // captured record print `<redacted len=N>`, never what was typed.
        let mut buffer = Buffer::default();
        buffer.push(&event("zqsecret", 1), true);
        assert_eq!(buffer.text, "zqsecret");
        let mut shadow = Shadow::default();
        edit(&mut shadow.text, &Kind::Text("zqsecret".into()));
        shadow.reported.push("rule-a".into());
        let mut text = [0u16; 24];
        for (slot, unit) in text.iter_mut().zip("zqsecret".encode_utf16()) {
            *slot = unit;
        }
        let record = RawRecord {
            vk: 0x5A,
            down: true,
            text,
            len: 8,
            context: None,
            generation: 1,
            modified: false,
            paused: false,
            busy: false,
            lost: 0,
        };
        let shown_buffer = format!("{buffer:?}");
        let shown_shadow = format!("{shadow:?}");
        let shown_kind = format!("{:?}", event("zqsecret", 2).kind);
        let shown_record = format!("{record:?}");
        for shown in [&shown_buffer, &shown_shadow, &shown_kind, &shown_record] {
            assert!(!shown.contains("secret"), "{shown}");
            assert!(shown.contains("<redacted len=8>"), "{shown}");
        }
        // Derived Debug would list the record's UTF-16 units ('z' 'q' = 122 113).
        assert!(!shown_record.contains("122, 113"), "{shown_record}");
        assert!(shown_shadow.contains("rule-a"), "{shown_shadow}");
        // Which key it was is redacted too (vk 0x5A = 90, 'Z').
        assert!(shown_record.contains("vk: <redacted>"), "{shown_record}");
        assert!(!shown_record.contains("90"), "{shown_record}");
        assert!(!format!("{record:#?}").contains("90"), "{record:#?}");
    }
}
