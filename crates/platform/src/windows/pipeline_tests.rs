//! L2 pipeline tests (S-23): the production match step, erasure, output and
//! clipboard lease running on `crate::mock` ports. Nothing here reaches
//! SendInput, the vendored injector or the real clipboard.
use super::actions::{run, Ports};
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
        keystate::PHYSICAL.set(Mutex::new(BTreeSet::new())).ok();
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
    fn stamp(&self) -> Stamp {
        Stamp {
            focus: FOCUS,
            epoch: 1,
            caret: 1,
            revision: self.shared.config.lock().unwrap().revision,
            layout: 0x0409,
        }
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
            captured_allowed: true,
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
    /// Runs a candidate the way the executor thread does, on the mock ports.
    fn execute(&self, candidate: &Candidate) -> Result<ExecutionResult, String> {
        // `physical()` reports "held" when its mirror is contended, so runs
        // from parallel tests must not overlap (production has one executor).
        static SERIAL: Mutex<()> = Mutex::new(());
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let config = self.shared.config.lock().unwrap().clone();
        let plan = expand_reusable(&candidate.rule.action, &config, &VariableContext::default())
            .map_err(|errors| format!("{} plan errors", errors.len()))?;
        let erasure = match &candidate.proof {
            CandidateProof::Espanso(proof) => proof.erasure(),
        };
        let clipboard = self.clipboard.clone();
        run(
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
            Some(erasure),
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
    rig.clipboard.set_busy(1);
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
    assert_eq!(rig.clipboard.content(), clip(EXPANSION));
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
