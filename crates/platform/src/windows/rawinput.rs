//! Thin bridge around the vendored Espanso Windows RawInput detector/injector.
//! Matching remains Quicker Text's domain compiler; UIA only supplies optional vetoes.
//!
//! B-02: the detector callback only reads atomics and the foreground input
//! context, pushes one fixed-size [`RawRecord`] onto a bounded lock-free
//! queue and returns. Text decoding and matching run on the input consumer.
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
    input::{self, Lane},
    keystate::{KeyBits, PHYSICAL},
    protection::{protection_block, protection_worker},
};
use crate::traits::{ForegroundProbe, InputContext};
use std::ffi::c_void;
use std::sync::{atomic::AtomicUsize, PoisonError};

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
}
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
/// RawInput queue capacity in records. 待实测: about a second of fast typing
/// plus autorepeat while the consumer is descheduled. A full queue drops the
/// key; the generation it already took leaves a gap, so no later key joins
/// the keys before it.
pub(super) const RAW_QUEUE_CAPACITY: usize = 1024;
/// no-regression #7: the detector re-reads the keyboard layout on every key.
pub(super) const KEYBOARD_LAYOUT_CACHE_INTERVAL: i32 = 0;
static RAW_LANE: OnceLock<Lane<RawRecord>> = OnceLock::new();
/// The detector window while its event loop runs (0 otherwise).
static RAW_WINDOW: AtomicUsize = AtomicUsize::new(0);
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
#[derive(Clone, Debug)]
pub(super) enum Kind {
    Text(String),
    Backspace,
    Neutral,
    Reset,
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
        match &event.kind {
            Kind::Text(text) => self.text.push_str(text),
            Kind::Backspace => {
                self.text.pop();
            }
            Kind::Neutral => {}
            Kind::Reset => self.text.clear(),
        }
        while self.text.chars().count() > 1024 {
            let Some(first) = self.text.chars().next() else {
                break;
            };
            self.text.drain(..first.len_utf8());
        }
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
/// One hardware key-down, captured by the detector callback. Fixed size and
/// `Copy`: building and queueing it never allocates.
#[derive(Clone, Copy, Debug)]
pub(super) struct RawRecord {
    vk: u16,
    text: [u16; 24],
    len: usize,
    context: InputContext,
    pub(super) generation: u64,
    modified: bool,
    allowed: bool,
}
/// Callback body: filters the detector event and captures everything the
/// consumer needs. Atomics plus the probe's input context only. Every
/// accepted key takes the next generation, so a key that is later lost
/// leaves a gap instead of joining its neighbours.
pub(super) fn raw_record(
    shared: &Shared,
    keys: &KeyBits,
    event: &InputEvent,
    probe: &impl ForegroundProbe,
) -> Option<RawRecord> {
    if !hardware_keydown(event.event_type, event.status, event.has_known_source) {
        return None;
    }
    let generation = shared
        .generation
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    let context = probe.input_context()?;
    Some(RawRecord {
        vk: event.key_code as u16,
        text: event.buffer,
        len: usize::try_from(event.buffer_len.clamp(0, 24)).unwrap_or(0),
        context,
        generation,
        modified: keys.modified(),
        allowed: !shared.paused.load(Ordering::SeqCst) && !shared.busy.load(Ordering::SeqCst),
    })
}
/// The RawInput lane; created once, before the detector starts.
pub(super) fn lane() -> &'static Lane<RawRecord> {
    RAW_LANE.get_or_init(|| Lane::new(RAW_QUEUE_CAPACITY, input::wake))
}
/// Consumer side: the matcher event for a captured hardware key. The
/// configuration revision is read here; a poisoned compiler fails closed
/// (the key is dropped and its generation gap clears the buffer).
pub(super) fn event_from(shared: &Shared, record: &RawRecord) -> Option<Event> {
    let revision = shared.compiled.lock().ok()?.config.revision;
    let text = record
        .text
        .get(..record.len)
        .and_then(|units| String::from_utf16(units).ok());
    Some(Event {
        kind: kind(record.vk, text, record.modified),
        stamp: stamp_of(record.context, revision),
        generation: record.generation,
        source: "rawinput_hardware",
        captured_allowed: record.allowed,
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
fn hardware_keydown(event_type: i32, status: i32, known_source: i32) -> bool {
    event_type == 1 && status == 1 && known_source != 0
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
            lane.push(record);
        }
    });
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
    // Safe text terminators already reach the target: erase shortcut plus
    // terminator. RawInput never claims to swallow Enter or Tab.
    let terminator = match &event.kind {
        Kind::Text(value) if value == " " => Some("space"),
        Kind::Text(value) if value == ";" => Some("semicolon"),
        _ => None,
    };
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
    if target.id != event.stamp.focus
        || target.process.is_empty()
        || target.process.eq_ignore_ascii_case("quicker-text.exe")
        || probe.protection().is_some()
    {
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
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || unsafe {
        let context = Box::new(shared);
        let mut options = InitOptions {
            keyboard_layout_cache_interval: KEYBOARD_LAYOUT_CACHE_INTERVAL,
        };
        let mut error = 0;
        let window = detect_initialize(
            (&*context as *const Arc<Shared>).cast_mut().cast(),
            &mut options,
            &mut error,
        );
        if window.is_null() {
            let _ = startup_tx.send(Err(format!(
                "Espanso RawInput initialization failed ({error})"
            )));
            return;
        }
        RAW_WINDOW.store(window as usize, Ordering::SeqCst);
        READY.store(true, Ordering::SeqCst);
        let _ = startup_tx.send(Ok(()));
        detect_eventloop(window, callback);
        READY.store(false, Ordering::SeqCst);
        RAW_WINDOW.store(0, Ordering::SeqCst);
        context.paused.store(true, Ordering::SeqCst);
        if IsWindow(window as HWND) != 0 {
            detect_destroy(window);
        }
    });
    startup_rx
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "Espanso RawInput initialization timed out".to_string())?
}
/// Asks the detector window to close (engine shutdown).
pub(super) fn close_window() {
    let window = RAW_WINDOW.load(Ordering::SeqCst);
    if window != 0 {
        // SAFETY: posting to a stale handle fails harmlessly.
        unsafe {
            PostMessageW(window as HWND, WM_CLOSE, 0, 0);
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
    fn raw_hardware_filter_rejects_own_or_external_orphan_input_and_keyup() {
        assert!(hardware_keydown(1, 1, 1));
        for args in [(1, 1, 0), (1, 2, 1), (2, 1, 1), (0, 0, 0)] {
            assert!(!hardware_keydown(args.0, args.1, args.2));
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
}
