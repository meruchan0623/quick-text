//! Thin bridge around the vendored Espanso Windows RawInput detector/injector.
//! Matching remains Quicker Text's domain compiler; UIA only supplies optional vetoes.
use super::*;
use ::windows::Win32::{
    System::{Com::*, Variant::*},
    UI::Accessibility::*,
};
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
    fn inject_string(text: *const u16) -> u32;
    fn inject_separate_vkeys(keys: *const i32, len: i32) -> u32;
    fn inject_vkeys_combination(keys: *const i32, len: i32) -> u32;
}
static READY: AtomicBool = AtomicBool::new(false);
static EVENTS: OnceLock<mpsc::SyncSender<Event>> = OnceLock::new();
static LAST_SOURCE: Mutex<&str> = Mutex::new("waiting_for_input");
static LAST_PROCESS: Mutex<String> = Mutex::new(String::new());
static PROTECTION: Mutex<Option<(u64, u64, String)>> = Mutex::new(None);
thread_local! { static PROOF: std::cell::RefCell<Option<Proof>> = const {std::cell::RefCell::new(None)}; }

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    focus: u64,
    epoch: u64,
    caret: u64,
    revision: u64,
    layout: usize,
}
#[derive(Clone)]
pub(super) struct Proof {
    stamp: Stamp,
    count: usize,
    at: Instant,
    ime_unverified: bool,
}
#[derive(Clone, Debug)]
enum Kind {
    Text(String),
    Backspace,
    Neutral,
    Reset,
}
struct Event {
    kind: Kind,
    stamp: Stamp,
    generation: u64,
    source: &'static str,
    captured_allowed: bool,
}
#[derive(Default)]
struct Buffer {
    text: String,
    stamp: Option<Stamp>,
    generation: u64,
}
impl Buffer {
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
fn stamp(shared: &Shared) -> Option<Stamp> {
    let revision = shared.compiled.try_lock().ok()?.config.revision;
    unsafe {
        let top = GetForegroundWindow();
        Some(Stamp {
            focus: focus_handle(top) as u64,
            epoch: observer::FOCUS_EPOCH.load(Ordering::SeqCst),
            caret: observer::CARET_EPOCH.load(Ordering::SeqCst),
            revision,
            layout: GetKeyboardLayout(GetWindowThreadProcessId(top, std::ptr::null_mut())) as usize,
        })
    }
}
fn submit(shared: &Shared, kind: Kind, generation: u64, source: &'static str) {
    let Some(stamp) = stamp(shared) else {
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
fn erase_count(
    count: usize,
    mut guard: impl FnMut() -> Result<(), String>,
    mut send: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    for _ in 0..count {
        guard()?;
        send()?;
    }
    Ok(())
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
fn dispatch(shared: Arc<Shared>, rx: mpsc::Receiver<Event>) {
    let mut buffer = Buffer::default();
    while !shared.shutdown.load(Ordering::SeqCst) {
        let event = match rx.recv_timeout(Duration::from_millis(30)) {
            Ok(e) => e,
            Err(_) => continue,
        };
        *LAST_SOURCE.lock().unwrap() = event.source;
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
            continue;
        }
        let target = foreground();
        *LAST_PROCESS.lock().unwrap() = target.process.clone();
        if target.id != event.stamp.focus
            || target.process.is_empty()
            || target.process.eq_ignore_ascii_case("quicker-text.exe")
            || native_input_protection()
                .or_else(protection_block)
                .is_some()
        {
            continue;
        }
        let compiler = shared.compiled.lock().unwrap();
        if compiler.config.revision != event.stamp.revision {
            buffer.text.clear();
            continue;
        }
        let foreground_context = ForegroundContext {
            platform: Platform::Windows,
            process: target.process.clone(),
            bundle_id: String::new(),
        };
        let found = match_buffer(
            &compiler,
            &buffer.text,
            previous.as_deref().zip(terminator),
            &foreground_context,
        );
        let Some((matched, terminator)) = found else {
            continue;
        };
        let risky = ime_active() || (event.stamp.layout & 0x3ff) == 4;
        let Some(rule) = compiler
            .config
            .rules
            .iter()
            .find(|rule| rule.id == matched.rule_id)
            .cloned()
        else {
            continue;
        };
        let proof = Proof {
            stamp: event.stamp.clone(),
            count: matched.erase_chars + usize::from(terminator.is_some()),
            at: Instant::now(),
            ime_unverified: risky,
        };
        drop(compiler);
        if enqueue(
            &shared,
            Candidate {
                rule,
                matched,
                proof: CandidateProof::Espanso(proof),
                focus: target,
                generation: event.generation,
                terminator: terminator.map(str::to_owned),
            },
        ) {
            buffer.text.clear();
        }
    }
}
pub(super) fn start(shared: Arc<Shared>) -> Result<(), String> {
    let (tx, rx) = mpsc::sync_channel(256);
    EVENTS
        .set(tx)
        .map_err(|_| "Espanso dispatcher already started")?;
    let dispatch_shared = shared.clone();
    std::thread::spawn(move || dispatch(dispatch_shared, rx));
    protection_worker(shared.clone());
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || unsafe {
        let context = Box::new(shared);
        let mut options = InitOptions {
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
pub(super) fn guard(shared: &Shared) -> Result<(), String> {
    PROOF.with(|p| {
        if let Some(proof) = p.borrow().as_ref() {
            validate_stamp(&proof.stamp, stamp(shared).as_ref())?;
        }
        Ok(())
    })
}
pub(super) fn erase(
    shared: &Shared,
    target: &Foreground,
    generation: u64,
    proof: &Proof,
) -> Result<(), String> {
    // Let the foreground process consume the final key; no UIA readback loop.
    if let Some(remaining) = Duration::from_millis(25).checked_sub(proof.at.elapsed()) {
        std::thread::sleep(remaining);
    }
    erase_count(
        proof.count,
        || guarded(shared, target, generation),
        || press_keys(&[VK_BACK]),
    )?;
    if proof.ime_unverified {
        gate("IME physical-letter erasure submitted; target composition/consumption unknown");
    }
    Ok(())
}
pub(super) fn text(value: &str) -> Result<(), String> {
    let requested = value
        .encode_utf16()
        .count()
        .checked_mul(2)
        .ok_or("Text too long")? as u32;
    if value.contains('\0') {
        return Err("Unicode injection cannot contain NUL".into());
    }
    let encoded = wide(value);
    record_send_result(unsafe { inject_string(encoded.as_ptr()) }, requested)
}
pub(super) fn press_keys(keys: &[u16]) -> Result<(), String> {
    let keys: Vec<i32> = keys.iter().map(|v| i32::from(*v)).collect();
    let result = record_send_result(
        unsafe { inject_separate_vkeys(keys.as_ptr(), keys.len() as i32) },
        keys.len() as u32 * 2,
    );
    cleanup_submission(
        &keys,
        result,
        |v| physical(v as u16),
        |v| key(v as u16, true),
    )
}
pub(super) fn combination(keys: &[u16]) -> Result<(), String> {
    let keys: Vec<i32> = keys.iter().map(|v| i32::from(*v)).collect();
    let result = record_send_result(
        unsafe { inject_vkeys_combination(keys.as_ptr(), keys.len() as i32) },
        keys.len() as u32 * 2,
    );
    cleanup_submission(
        &keys,
        result,
        |v| physical(v as u16),
        |v| key(v as u16, true),
    )
}
fn cleanup_submission(
    keys: &[i32],
    result: Result<(), String>,
    mut held: impl FnMut(i32) -> bool,
    mut release: impl FnMut(i32) -> Result<(), String>,
) -> Result<(), String> {
    if let Err(mut error) = result {
        for &v in keys.iter().rev() {
            if !held(v) {
                if let Err(cleanup) = release(v) {
                    error.push_str(&format!("; key release failed: {cleanup}"));
                }
            }
        }
        Err(error)
    } else {
        Ok(())
    }
}
pub(super) fn protection_block() -> Option<String> {
    let (focus, epoch, reason) = PROTECTION.try_lock().ok()?.clone()?;
    (focus == focus_handle(unsafe { GetForegroundWindow() }) as u64
        && epoch == observer::FOCUS_EPOCH.load(Ordering::SeqCst))
    .then_some(reason)
}
unsafe fn text_readonly(element: &IUIAutomationElement) -> Option<&'static str> {
    let pattern = element
        .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
        .ok()?;
    let range = pattern.DocumentRange().ok()?;
    let mut value = range.GetAttributeValue(UIA_IsReadOnlyAttributeId).ok()?;
    let readonly = value.Anonymous.Anonymous.vt == VT_BOOL
        && value.Anonymous.Anonymous.Anonymous.boolVal.0 != 0;
    let _ = VariantClear(&mut value);
    readonly.then_some("Read-only text control blocked")
}
fn protection_worker(shared: Arc<Shared>) {
    std::thread::spawn(move || unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            return;
        }
        if let Ok(automation) =
            CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
        {
            while !shared.shutdown.load(Ordering::SeqCst) {
                let focus = focus_handle(GetForegroundWindow()) as u64;
                let epoch = observer::FOCUS_EPOCH.load(Ordering::SeqCst);
                if let Ok(element) = automation.GetFocusedElement() {
                    let reason = if element.CurrentIsPassword().is_ok_and(|b| b.as_bool()) {
                        Some("Password controls are blocked")
                    } else if element.CurrentIsEnabled().is_ok_and(|b| !b.as_bool()) {
                        Some("Disabled controls are blocked")
                    } else if element
                        .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                        .ok()
                        .is_some_and(|p| p.CurrentIsReadOnly().is_ok_and(|b| b.as_bool()))
                    {
                        Some("Read-only ValuePattern control blocked")
                    } else if element
                        .GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                            UIA_LegacyIAccessiblePatternId,
                        )
                        .ok()
                        .is_some_and(|p| {
                            p.CurrentState()
                                .is_ok_and(|state| state & (0x40 | 0x20000000) != 0)
                        })
                    {
                        Some("Read-only or protected accessibility control blocked")
                    } else {
                        text_readonly(&element)
                    };
                    if focus == focus_handle(GetForegroundWindow()) as u64
                        && epoch == observer::FOCUS_EPOCH.load(Ordering::SeqCst)
                    {
                        *PROTECTION.lock().unwrap() =
                            reason.map(|reason| (focus, epoch, reason.to_owned()));
                    }
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        CoUninitialize();
    });
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
    fn deletion_stops_on_submission_failure_or_cancel_before_more_backspaces() {
        let mut sent = 0;
        let result = erase_count(
            5,
            || Ok(()),
            || {
                sent += 1;
                if sent == 2 {
                    Err("send failed".into())
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(sent, 2);
        let mut sent = 0;
        let result = erase_count(
            5,
            || Err("cancelled".into()),
            || {
                sent += 1;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(sent, 0);
    }
    #[test]
    fn partial_combination_releases_unheld_keys_in_reverse_and_reports_cleanup_failure() {
        let mut released = vec![];
        let result = cleanup_submission(
            &[17, 65],
            Err("accepted 1/4".into()),
            |v| v == 65,
            |v| {
                released.push(v);
                Ok(())
            },
        );
        assert_eq!(released, vec![17]);
        assert_eq!(result, Err("accepted 1/4".into()));
        let mut released = vec![];
        let result = cleanup_submission(
            &[17, 65],
            Err("accepted 1/4".into()),
            |_| false,
            |v| {
                released.push(v);
                Err("blocked".into())
            },
        );
        assert_eq!(released, vec![65, 17]);
        assert!(result.unwrap_err().contains("key release failed"));
        let result = cleanup_submission(
            &[17, 65],
            Ok(()),
            |_| panic!("no cleanup on success"),
            |_| panic!("no cleanup on success"),
        );
        assert!(result.is_ok());
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
