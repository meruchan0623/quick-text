//! L2 pipeline tests (S-23): the production match step, erasure, output and
//! clipboard lease running on `crate::mock` ports. Nothing here reaches
//! SendInput, the vendored injector or the real clipboard.
use super::actions::{execute_with_proof, Ports};
use super::executor::{Candidate, CandidateProof};
use super::inject::{erase, Erasure};
use super::rawinput::{detect_candidate, Buffer, Event, Kind, Stamp};
use super::*;
use crate::mock::{FakeClock, InjectCall, MockClipboard, MockForeground, MockInjector};
use crate::traits::Clock;
use std::sync::PoisonError;

const FOCUS: u64 = 0x2a;
const APP: &str = "notepad.exe";
const EXPANSION: &str = "by the way";
const CTRL_V: [u16; 2] = [VK_CONTROL, 0x56];

fn utf16(text: &str) -> Vec<u8> {
    wide(text).iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn clip(text: &str) -> Vec<(u32, Vec<u8>)> {
    vec![(13, utf16(text))]
}
fn rule(terminator: Option<&str>, method: OutputMethod) -> Rule {
    Rule {
        id: "btw".into(),
        match_spec: MatchSpec {
            kind: MatchKind::Literal,
            value: "btw".into(),
        },
        trigger_mode: if terminator.is_some() {
            TriggerMode::Terminator
        } else {
            TriggerMode::Immediate
        },
        terminator: terminator.map(|key| Terminator {
            keys: vec![key.into()],
            consume: false,
        }),
        action: Action::Text {
            method,
            source: ContentSource::inline(EXPANSION),
        },
        ..Default::default()
    }
}

/// One engine with mock ports. The clipboard starts with "original" owned by
/// another application (owner 7).
struct Rig {
    shared: Arc<Shared>,
    buffer: Buffer,
    generation: u64,
    injector: MockInjector,
    foreground: MockForeground,
    clock: FakeClock,
    clipboard: MockClipboard,
}
impl Rig {
    fn new(rule: Rule) -> Self {
        // Physical key state is still a global mirror (boundary): start with
        // nothing held so `physical()` never falls back to GetAsyncKeyState.
        keystate::PHYSICAL.mark_seeded();
        let shared = Shared::new();
        shared.paused.store(false, Ordering::SeqCst);
        let config = Config {
            rules: vec![rule],
            ..Default::default()
        };
        *shared.compiled.lock().unwrap() = compile_config(&config).unwrap();
        *shared.config.lock().unwrap() = config;
        Self {
            shared,
            buffer: Buffer::default(),
            generation: 0,
            injector: MockInjector::new(),
            foreground: MockForeground::new(MockForeground::window(FOCUS, APP)),
            clock: FakeClock::new(),
            clipboard: MockClipboard::new(7, clip("original")),
        }
    }
    /// The stamp the capture path records now (production `stamp` over the
    /// mock input context).
    fn stamp(&self) -> Stamp {
        rawinput::stamp(&self.shared, &self.foreground).expect("mock context is readable")
    }
    /// Delivers a text event captured at `generation` while the engine is at
    /// `current` (they differ when external input arrived in between).
    fn feed(
        &mut self,
        text: &str,
        stamp: Stamp,
        generation: u64,
        current: u64,
    ) -> Option<Candidate> {
        self.generation = current;
        self.shared.generation.store(current, Ordering::SeqCst);
        let event = Event {
            kind: Kind::Text(text.into()),
            stamp,
            generation,
            source: "fixture",
            captured_block: None,
            modified: false,
        };
        detect_candidate(&self.shared, &mut self.buffer, event, &self.foreground)
    }
    /// One contiguous hardware key, as the RawInput callback would submit it.
    fn key_in(&mut self, text: &str) -> Option<Candidate> {
        let next = self.generation + 1;
        self.feed(text, self.stamp(), next, next)
    }
    fn type_str(&mut self, text: &str) -> Option<Candidate> {
        text.chars()
            .fold(None, |_, ch| self.key_in(&ch.to_string()))
    }
    /// Runs a candidate the way the executor thread does, on the mock ports,
    /// through the production entry that installs and uninstalls the proof.
    fn execute(&self, candidate: &Candidate) -> Result<ExecutionResult, String> {
        // `physical()` reads the process-wide key mirror, which other tests
        // write: runs must not overlap them (see [`SERIAL`]).
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let config = self.shared.config.lock().unwrap().clone();
        let plan = expand_reusable(&candidate.rule.action, &config, &VariableContext::default())
            .map_err(|errors| format!("{} plan errors", errors.len()))?;
        let clipboard = self.clipboard.clone();
        execute_with_proof(
            &Ports {
                injector: &self.injector,
                foreground: &self.foreground,
                clock: &self.clock,
                clipboard: move || Ok(clipboard.clone()),
            },
            &self.shared,
            plan,
            candidate.focus.clone(),
            candidate.generation,
            Some(candidate.proof.clone()),
            if candidate.matched.consume_terminator {
                None
            } else {
                candidate.terminator.clone()
            },
            1.0,
        )
    }
    fn chords(&self) -> usize {
        self.injector
            .calls()
            .iter()
            .filter(|call| matches!(call, InjectCall::Chord(_)))
            .count()
    }
    /// Runs `change` on the clipboard when the paste chord is submitted.
    fn on_paste(&self, change: impl Fn(&MockClipboard) + Send + 'static) {
        let clipboard = self.clipboard.clone();
        self.injector.on_call(move |call| {
            if *call == InjectCall::Chord(CTRL_V.to_vec()) {
                change(&clipboard);
            }
        });
    }
    /// Runs `change` once, when the first backspace is submitted.
    fn on_first_backspace(&self, change: impl FnOnce() + Send + 'static) {
        let mut change = Some(change);
        self.injector.on_call(move |call| {
            if *call == InjectCall::Keys(vec![VK_BACK]) {
                if let Some(change) = change.take() {
                    change();
                }
            }
        });
    }
}

#[test]
fn match_erases_trigger_then_types_expansion() {
    let mut rig = Rig::new(rule(None, OutputMethod::Keys));
    assert!(rig.type_str("bt").is_none());
    let candidate = rig
        .key_in("w")
        .expect("immediate rule matches on the last key");
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(result.completed_steps, 1);
    assert!(result.side_effects);
    let calls = rig.injector.calls();
    assert!(calls[..3]
        .iter()
        .all(|call| *call == InjectCall::Keys(vec![VK_BACK])));
    assert_eq!(rig.injector.taps(VK_BACK), 3);
    assert_eq!(rig.injector.typed(), EXPANSION);
    assert_eq!(calls.len(), 3 + EXPANSION.len());
    // The settle delay before erasing runs on the injected clock.
    assert_eq!(rig.clock.sleeps().first(), Some(&Duration::from_millis(25)));
    assert_eq!(
        rig.clipboard.clears(),
        0,
        "key output never touches the clipboard"
    );
}

#[test]
fn space_terminator_erases_trigger_and_terminator_then_replays_it() {
    let mut rig = Rig::new(rule(Some("space"), OutputMethod::Keys));
    assert!(rig.type_str("btw").is_none());
    let candidate = rig.key_in(" ").expect("terminator rule matches");
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.injector.taps(VK_BACK), 4);
    assert_eq!(rig.injector.typed(), EXPANSION);
    assert_eq!(
        rig.injector.calls().last(),
        Some(&InjectCall::Keys(vec![VK_SPACE]))
    );
}

#[test]
fn paste_output_restores_original_clipboard() {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.injector.taps(VK_BACK), 3);
    assert_eq!(rig.injector.typed(), "");
    assert_eq!(
        rig.injector.calls().last(),
        Some(&InjectCall::Chord(CTRL_V.to_vec()))
    );
    assert_eq!(rig.clipboard.content(), clip("original"));
    assert_eq!(rig.clipboard.clears(), 2, "paste write plus restore write");
}

#[test]
fn short_count_during_erase_stops_before_any_output() {
    let mut rig = Rig::new(rule(None, OutputMethod::Keys));
    let candidate = rig.type_str("btw").unwrap();
    rig.injector.short_count(1, 1);
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.id, "erase");
    assert_eq!(result.status, "failed");
    assert!(
        result.message.contains("accepted 1/2"),
        "{}",
        result.message
    );
    assert_eq!(result.completed_steps, 0);
    assert!(result.side_effects);
    // Two backspaces, then release of the partially submitted key; no text.
    assert_eq!(
        rig.injector.calls(),
        vec![
            InjectCall::Keys(vec![VK_BACK]),
            InjectCall::Keys(vec![VK_BACK]),
            InjectCall::Key {
                key: VK_BACK,
                up: true
            },
        ]
    );
}

#[test]
fn short_count_during_output_skips_remaining_text_and_terminator() {
    let mut rig = Rig::new(rule(Some("space"), OutputMethod::Keys));
    rig.type_str("btw");
    let candidate = rig.key_in(" ").unwrap();
    // Calls 0-3 erase "btw ", call 4 types 'b', call 5 ('y') is cut short.
    rig.injector.short_count(5, 1);
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.status, "failed");
    assert!(
        result.message.contains("accepted 1/2"),
        "{}",
        result.message
    );
    assert_eq!(result.completed_steps, 0);
    assert_eq!(rig.injector.typed(), "by");
    assert_eq!(
        rig.injector.calls().len(),
        6,
        "nothing after the short call"
    );
    assert_eq!(rig.injector.taps(VK_SPACE), 0, "terminator not replayed");
}

#[test]
fn clipboard_write_failure_fails_without_typing_fallback() {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    rig.clipboard.fail_set(1);
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.status, "failed");
    assert_eq!(result.message, "Clipboard write failed");
    assert!(result.side_effects);
    assert_eq!(rig.injector.typed(), "", "paste never downgrades to typing");
    assert_eq!(rig.chords(), 0, "no paste chord after a failed write");
    assert_eq!(rig.clipboard.content(), clip("original"));
}

#[test]
fn busy_clipboard_rejects_paste_before_erasure() {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    rig.clipboard.set_busy(super::clipboard::OPEN_ATTEMPTS);
    assert_eq!(
        rig.execute(&candidate).unwrap_err(),
        "Clipboard is busy; trigger preserved"
    );
    assert!(rig.injector.calls().is_empty());
    assert_eq!(rig.clipboard.content(), clip("original"));
}

#[test]
fn restore_never_overwrites_when_sequence_keeps_changing() {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    rig.on_paste(|clipboard| clipboard.churn_sequence(true));
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.status, "failed");
    assert_eq!(
        result.message,
        "Clipboard restore failed after input: Clipboard ownership could not be verified within the retry limit"
    );
    assert_eq!(rig.clipboard.content(), pasted(&rig.clipboard, EXPANSION));
    assert_eq!(rig.clipboard.clears(), 1, "only the paste write");
}

#[test]
fn restore_never_overwrites_when_owner_changed() {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    rig.on_paste(|clipboard| clipboard.take_over(9, clip("theirs")));
    let result = rig.execute(&candidate).unwrap();
    // Another owner's data is left alone; that is not an execution failure.
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.clipboard.content(), clip("theirs"));
    assert_eq!(rig.clipboard.owner_now(), 9);
    assert_eq!(rig.clipboard.clears(), 1);
}

#[test]
fn restore_never_overwrites_when_content_changed() {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    rig.on_paste(|clipboard| clipboard.tamper(clip("edited")));
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.status, "failed");
    assert_eq!(
        result.message,
        "Clipboard restore failed after input: Clipboard payload changed externally; clipboard preserved"
    );
    assert_eq!(rig.clipboard.content(), clip("edited"));
    assert_eq!(rig.clipboard.clears(), 1);
}

#[test]
fn restore_reestablishes_lone_sequence_bump_with_identical_payload() {
    // By design: a listener or synthesized format may advance the sequence
    // once; the exact payload under our ownership is re-verified, then restored.
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    rig.on_paste(MockClipboard::bump_sequence);
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.clipboard.content(), clip("original"));
    assert_eq!(rig.clipboard.clears(), 2);
}

#[test]
fn external_input_during_erase_stops_before_output() {
    let mut rig = Rig::new(rule(None, OutputMethod::Keys));
    let candidate = rig.type_str("btw").unwrap();
    // What the keyboard hook does for external injected input (#4).
    let shared = rig.shared.clone();
    rig.on_first_backspace(move || {
        shared.generation.fetch_add(1, Ordering::SeqCst);
    });
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.id, "erase");
    assert_eq!(
        result.message,
        "User input changed during action; stopped safely"
    );
    assert_eq!(rig.injector.taps(VK_BACK), 1);
    assert_eq!(rig.injector.typed(), "");
}

#[test]
fn external_input_invalidates_pending_match() {
    // A generation gap (input the buffer never saw) discards the prefix.
    let mut rig = Rig::new(rule(None, OutputMethod::Keys));
    rig.type_str("bt");
    let stamp = rig.stamp();
    assert!(rig.feed("w", stamp, 4, 4).is_none());
    // An event overtaken by newer input never matches.
    let mut rig = Rig::new(rule(None, OutputMethod::Keys));
    rig.type_str("bt");
    let stamp = rig.stamp();
    assert!(rig.feed("w", stamp, 3, 4).is_none());
}

#[test]
fn terminator_match_requires_contiguous_same_stamp_prefix() {
    let mut rig = Rig::new(rule(Some("space"), OutputMethod::Keys));
    rig.type_str("btw");
    let candidate = rig.key_in(" ").expect("contiguous prefix matches");
    assert_eq!(
        match &candidate.proof {
            CandidateProof::Espanso(proof) => proof.erasure().count,
        },
        4
    );
    // Caret moved before the terminator.
    let mut rig = Rig::new(rule(Some("space"), OutputMethod::Keys));
    rig.type_str("btw");
    let mut moved = rig.stamp();
    moved.caret += 1;
    assert!(rig.feed(" ", moved, 4, 4).is_none());
    // A key the buffer never saw arrived before the terminator.
    let mut rig = Rig::new(rule(Some("space"), OutputMethod::Keys));
    rig.type_str("btw");
    let stamp = rig.stamp();
    assert!(rig.feed(" ", stamp, 5, 5).is_none());
}

#[test]
fn protected_control_blocks_match_and_stops_output() {
    let mut rig = Rig::new(rule(None, OutputMethod::Keys));
    rig.foreground
        .set_protection(Some("Password field; input stopped"));
    assert!(rig.type_str("btw").is_none());

    let mut rig = Rig::new(rule(None, OutputMethod::Keys));
    let candidate = rig.type_str("btw").unwrap();
    let foreground = rig.foreground.clone();
    rig.on_first_backspace(move || {
        foreground.set_protection(Some("Password field; input stopped"))
    });
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.id, "erase");
    assert_eq!(result.message, "Password field; input stopped");
    assert_eq!(rig.injector.taps(VK_BACK), 1);
    assert_eq!(rig.injector.typed(), "");
}

#[test]
fn erase_waits_only_for_the_remaining_settle_time() {
    let injector = MockInjector::new();
    let clock = FakeClock::new();
    let erasure = Erasure {
        count: 2,
        at: clock.now(),
        ime_unverified: false,
    };
    clock.advance(Duration::from_millis(10));
    erase(&injector, &clock, &erasure, || Ok(())).unwrap();
    assert_eq!(clock.sleeps(), vec![Duration::from_millis(15)]);
    assert_eq!(injector.taps(VK_BACK), 2);
}

/// Fail-stop message of the stamp guard (`rawinput::validate_stamp`). The
/// result carries no reason code yet; B-35 classifies it as
/// `ReasonCode::StaleInput`.
const STAMP_CHANGED: &str = "Focus, caret, layout or configuration changed; input stopped";

/// One component of the transaction stamp.
#[derive(Clone, Copy, Debug)]
enum StampField {
    Caret,
    FocusEpoch,
    Layout,
    Revision,
}

/// When, after the match, the stamp changes.
#[derive(Clone, Copy, Debug)]
enum Moment {
    /// While the clipboard lease takes its backup, before any backspace.
    BeforeErase,
    /// Right after backspace number `k` (1-based) was accepted.
    AfterBackspace(usize),
}

impl Rig {
    /// The change the focus/caret hooks, a layout switch or a config reload
    /// would make to one stamp component.
    fn stamp_change(&self, field: StampField) -> impl Fn() + Send + 'static {
        let foreground = self.foreground.clone();
        let shared = self.shared.clone();
        move || match field {
            StampField::Caret => foreground.update_input_context(|c| c.caret_epoch += 1),
            StampField::FocusEpoch => foreground.update_input_context(|c| c.focus_epoch += 1),
            StampField::Layout => foreground.update_input_context(|c| c.layout = 0x0804),
            StampField::Revision => shared.compiled.lock().unwrap().config.revision += 1,
        }
    }
}

fn assert_stamp_change_fails_stop(moment: Moment, field: StampField) {
    let case = format!("{field:?} changed {moment:?}");
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    let change = rig.stamp_change(field);
    let backspaces = match moment {
        Moment::BeforeErase => {
            // Lock 0 is the lease reading its backup.
            rig.clipboard.on_lock(move |index| {
                if index == 0 {
                    change();
                }
            });
            0
        }
        Moment::AfterBackspace(k) => {
            let mut seen = 0;
            rig.injector.on_call(move |call| {
                if *call == InjectCall::Keys(vec![VK_BACK]) {
                    seen += 1;
                    if seen == k {
                        change();
                    }
                }
            });
            k
        }
    };
    let sequence = rig.clipboard.sequence_now();
    let outcome = rig.execute(&candidate);
    let message = match moment {
        // The pre-erase guard rejects the run before anything is sent.
        Moment::BeforeErase => outcome.expect_err(&case),
        Moment::AfterBackspace(_) => {
            let result = outcome.expect(&case);
            assert_eq!(result.id, "erase", "{case}");
            assert_eq!(result.status, "failed", "{case}");
            assert_eq!(result.completed_steps, 0, "{case}");
            assert!(result.side_effects, "{case}");
            result.message
        }
    };
    assert_eq!(message, STAMP_CHANGED, "{case}");
    // No input after the change: only the backspaces accepted before it.
    assert_eq!(
        rig.injector.calls(),
        vec![InjectCall::Keys(vec![VK_BACK]); backspaces],
        "{case}"
    );
    match moment {
        // #1: the lease never wrote, so sequence, owner and content are intact.
        Moment::BeforeErase => {
            assert_eq!(rig.clipboard.clears(), 0, "{case}");
            assert_eq!(rig.clipboard.sequence_now(), sequence, "{case}");
            assert_eq!(rig.clipboard.owner_now(), 7, "{case}");
        }
        // The paste was written before erasure (plan §5.4 principle 4) and
        // is restored: paste write plus restore write.
        Moment::AfterBackspace(_) => assert_eq!(rig.clipboard.clears(), 2, "{case}"),
    }
    assert_eq!(rig.clipboard.content(), clip("original"), "{case}");
}

macro_rules! stamp_change_cases {
    ($($name:ident => $moment:expr, $field:expr;)+) => {
        $(
            #[test]
            fn $name() {
                assert_stamp_change_fails_stop($moment, $field);
            }
        )+
    };
}

stamp_change_cases! {
    caret_change_before_erase_fails_stop => Moment::BeforeErase, StampField::Caret;
    focus_epoch_change_before_erase_fails_stop => Moment::BeforeErase, StampField::FocusEpoch;
    layout_change_before_erase_fails_stop => Moment::BeforeErase, StampField::Layout;
    revision_change_before_erase_fails_stop => Moment::BeforeErase, StampField::Revision;
    caret_change_mid_erase_fails_stop => Moment::AfterBackspace(2), StampField::Caret;
    focus_epoch_change_mid_erase_fails_stop => Moment::AfterBackspace(2), StampField::FocusEpoch;
    layout_change_mid_erase_fails_stop => Moment::AfterBackspace(2), StampField::Layout;
    revision_change_mid_erase_fails_stop => Moment::AfterBackspace(2), StampField::Revision;
}

#[test]
fn stamp_change_after_paste_write_restores_clipboard_without_paste() {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    let change = rig.stamp_change(StampField::Caret);
    // Locks: 0 backup, 1 backup verification, 2 paste write.
    rig.clipboard.on_lock(move |index| {
        if index == 2 {
            change();
        }
    });
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.status, "failed");
    assert_eq!(result.message, STAMP_CHANGED);
    // The paste is written before erasure (plan §5.4 principle 4).
    assert!(
        rig.injector.calls().is_empty(),
        "trigger preserved, no paste chord after the change"
    );
    // #1: our payload re-verified under our ownership, then the original
    // was written back.
    assert_eq!(rig.clipboard.content(), clip("original"));
    assert_eq!(rig.clipboard.clears(), 2);
}

#[test]
fn unchanged_stamp_passes_guard_and_completes() {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    let reads = rig.foreground.input_context_reads();
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.injector.taps(VK_BACK), 3);
    assert_eq!(rig.chords(), 1);
    assert_eq!(rig.clipboard.content(), clip("original"));
    // The guard compared live stamps: 3 pre-erase checks (one after the
    // paste write), 3 backspaces and the step check; the proof is dropped
    // once the first output is about to be sent.
    let compared = rig.foreground.input_context_reads() - reads;
    assert!(compared >= 7, "stamp read {compared} times");
}

#[test]
fn proof_is_installed_only_for_the_run() {
    let mut rig = Rig::new(rule(None, OutputMethod::Keys));
    let candidate = rig.type_str("btw").unwrap();
    let during = Arc::new(Mutex::new(None));
    let (shared, foreground, seen) = (rig.shared.clone(), rig.foreground.clone(), during.clone());
    rig.on_first_backspace(move || {
        // Probe the guard from inside the run with a changed context.
        foreground.update_input_context(|c| c.caret_epoch += 1);
        *seen.lock().unwrap() = Some(rawinput::guard(&shared, &foreground));
        foreground.update_input_context(|c| c.caret_epoch -= 1);
    });
    rig.execute(&candidate).unwrap();
    assert_eq!(
        during.lock().unwrap().clone(),
        Some(Err(STAMP_CHANGED.to_string()))
    );
    rig.foreground.update_input_context(|c| c.caret_epoch += 1);
    assert_eq!(rawinput::guard(&rig.shared, &rig.foreground), Ok(()));
}

#[test]
fn proof_is_uninstalled_when_the_run_unwinds() {
    let mut rig = Rig::new(rule(None, OutputMethod::Keys));
    let candidate = rig.type_str("btw").unwrap();
    rig.on_first_backspace(|| panic!("simulated injector panic"));
    let unwound =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.execute(&candidate)));
    assert!(unwound.is_err());
    rig.foreground.update_input_context(|c| c.caret_epoch += 1);
    assert_eq!(rawinput::guard(&rig.shared, &rig.foreground), Ok(()));
}

/// What a paste write of `text` leaves on the clipboard: the payload, then
/// the history/cloud exclusion markers (B-08), each a DWORD 0.
fn pasted(clipboard: &MockClipboard, text: &str) -> Vec<(u32, Vec<u8>)> {
    let mut formats = clip(text);
    for name in super::clipboard::EXCLUSION_FORMATS {
        let id = clipboard.registered(name).expect("marker registered");
        formats.push((id, 0u32.to_le_bytes().to_vec()));
    }
    formats
}

#[path = "pipeline_tests_clipboard.rs"]
mod clipboard_cases;

/// Serializes the pipeline tests that share process-wide input state, so
/// their runs never overlap (production has one executor): the physical key
/// mirror `keystate::PHYSICAL`, which every run reads through `physical()`,
/// the executor tests hold keys in and some input tests resync, and the
/// global fault points. Shared by `pipeline_tests_executor` and
/// `pipeline_tests_input`.
pub(super) static SERIAL: Mutex<()> = Mutex::new(());
