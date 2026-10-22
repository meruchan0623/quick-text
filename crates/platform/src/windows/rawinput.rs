//! Thin bridge around the vendored Espanso Windows RawInput detector/injector.
//! Matching remains Quicker Text's domain compiler; UIA only supplies optional vetoes.
use super::*;
use super::{
    executor::{enqueue, Candidate, CandidateProof},
    foreground::WinForeground,
    hook::expected_key_text,
    ime::ime_active,
    inject::Erasure,
    keystate::physical,
    protection::{protection_block, protection_worker},
};
use crate::traits::ForegroundProbe;
use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    event_type: i32,
    buffer: [u16; 24],
    buffer_len: i32,
    key_code: i32,
    variant: i32,
    status: i32,
    has_known_source: i32,
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
static EVENTS: OnceLock<mpsc::SyncSender<Event>> = OnceLock::new();
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
            self.text
                .drain(..self.text.chars().next().unwrap().len_utf8());
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
    Some(Stamp {
        focus: context.focus,
        epoch: context.focus_epoch,
        caret: context.caret_epoch,
        revision,
        layout: context.layout as usize,
    })
}
fn submit(shared: &Shared, kind: Kind, generation: u64, source: &'static str) {
    let Some(stamp) = stamp(shared, &WinForeground) else {
        return;
    };
    if EVENTS.get().is_none_or(|tx| {
        tx.try_send(Event {
            kind,
            stamp,
            generation,
            source,
            captured_allowed: !shared.paused.load(Ordering::SeqCst)
                && !shared.busy.load(Ordering::SeqCst),
        })
        .is_err()
    }) {
        // A dropped event must break contiguity before another suffix can match.
        shared.generation.fetch_add(1, Ordering::SeqCst);
    }
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
    // No panic may unwind across the C ABI; no COM, output or provider read here.
    let _ = std::panic::catch_unwind(|| {
        let shared = unsafe { &*(context as *const Arc<Shared>) };
        if !hardware_keydown(event.event_type, event.status, event.has_known_source) {
            return;
        }
        let len = event.buffer_len.clamp(0, 24) as usize;
        let text = String::from_utf16(&event.buffer[..len]).ok();
        let modified =
            (physical(VK_CONTROL) ^ physical(VK_MENU)) || physical(VK_LWIN) || physical(VK_RWIN);
        submit(
            shared,
            kind(event.key_code as u16, text, modified),
            shared.generation.fetch_add(1, Ordering::SeqCst) + 1,
            "rawinput_hardware",
        );
    });
}
pub(super) unsafe fn acceptance_key(shared: &Shared, event: &KBDLLHOOKSTRUCT, generation: u64) {
    let modified =
        (physical(VK_CONTROL) ^ physical(VK_MENU)) || physical(VK_LWIN) || physical(VK_RWIN);
    submit(
        shared,
        kind(event.vkCode as u16, expected_key_text(event), modified),
        generation,
        "acceptance_sendinput_hook",
    );
}
fn dispatch(shared: Arc<Shared>, rx: mpsc::Receiver<Event>, probe: impl ForegroundProbe) {
    let mut buffer = Buffer::default();
    while !shared.shutdown.load(Ordering::SeqCst) {
        let event = match rx.recv_timeout(Duration::from_millis(30)) {
            Ok(e) => e,
            Err(_) => continue,
        };
        *LAST_SOURCE.lock().unwrap() = event.source;
        let Some(candidate) = detect_candidate(&shared, &mut buffer, event, &probe) else {
            continue;
        };
        if enqueue(&shared, candidate) {
            buffer.text.clear();
        }
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
    *LAST_PROCESS.lock().unwrap() = target.process.clone();
    if target.id != event.stamp.focus
        || target.process.is_empty()
        || target.process.eq_ignore_ascii_case("quicker-text.exe")
        || probe.protection().is_some()
    {
        return None;
    }
    let compiler = shared.compiled.lock().unwrap();
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
pub(super) fn start(shared: Arc<Shared>) -> Result<(), String> {
    let (tx, rx) = mpsc::sync_channel(256);
    EVENTS
        .set(tx)
        .map_err(|_| "Espanso dispatcher already started")?;
    let dispatch_shared = shared.clone();
    std::thread::spawn(move || dispatch(dispatch_shared, rx, WinForeground));
    protection_worker(shared.clone());
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || unsafe {
        let context = Box::new(shared);
        let mut options = InitOptions {
            // no-regression #7: re-read the keyboard layout on every key.
            keyboard_layout_cache_interval: 0,
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
        READY.store(true, Ordering::SeqCst);
        let _ = startup_tx.send(Ok(()));
        let shutdown_shared = context.clone();
        let window_id = window as usize;
        std::thread::spawn(move || {
            while !shutdown_shared.shutdown.load(Ordering::SeqCst) && READY.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(30));
            }
            if shutdown_shared.shutdown.load(Ordering::SeqCst) {
                PostMessageW(window_id as HWND, WM_CLOSE, 0, 0);
            }
        });
        detect_eventloop(window, callback);
        READY.store(false, Ordering::SeqCst);
        context.paused.store(true, Ordering::SeqCst);
        if IsWindow(window as HWND) != 0 {
            detect_destroy(window);
        }
    });
    startup_rx
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "Espanso RawInput initialization timed out".to_string())?
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
    serde_json::json!({"backend":"espanso_win32_v2.4.1", "source":*LAST_SOURCE.lock().unwrap(),
        "ready":READY.load(Ordering::SeqCst), "available":READY.load(Ordering::SeqCst),
        "process":*LAST_PROCESS.lock().unwrap(), "reason":protection_block().unwrap_or_else(||"RawInput capture; delivery and IME composition are unverified".into()),
        "terminators":{"space":"post-input erasure; final IME consumption unknown","semicolon":"post-input erasure", "enter":"disabled","tab":"disabled"},
        "ime":"RawInput translated characters and backspace compatibility; composition and final delivery unverified"})
}

#[cfg(test)]
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
