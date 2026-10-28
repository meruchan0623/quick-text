//! The macOS backend's decisions that need no Apple API: how a key code edits
//! the typed-key buffer, which conditions pause detection for now, which
//! event a replayed mouse move becomes, how a Windows wheel delta scrolls,
//! what a clipboard backup keeps, and the history verdict of a run. They are
//! compiled into the tests of every platform, so they are checked on the
//! Windows CI too; `macos.rs` only applies them.
use crate::health::{Outcome, ReasonCode};
use crate::traits::RestorePolicy;
use quicker_domain::{ActionPlan, KeyClass, Operation, OutputMethod, WheelAxis, WheelUnit};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// Class of a macOS virtual key code (Carbon `kVK_*`) for the shared buffer
/// rules ([`quicker_domain::key_edit`]), matching the Windows table.
pub(crate) fn key_class(code: u16) -> KeyClass {
    match code {
        // Cmd (54, 55), Shift (56, 60), Caps Lock (57), Option (58, 61),
        // Control (59, 62), Fn (63).
        54..=63 => KeyClass::Modifier,
        51 => KeyClass::Backspace,
        // Return, keypad Enter, Tab, Esc, forward Delete, Home, Page Up,
        // End, Page Down, arrows.
        36 | 76 | 48 | 53 | 117 | 115 | 116 | 119 | 121 | 123..=126 => KeyClass::Navigation,
        _ => KeyClass::Other,
    }
}

/// Why the platform blocks detection right now, if it does
/// (`PauseReasons::PLATFORM`). These conditions come and go on their own:
/// the backend sets the reason while one holds and clears it after, so they
/// neither become a manual pause nor count against the input health.
pub(crate) fn platform_block(
    secure_input: bool,
    layout_safe: bool,
    tap_restarting: bool,
) -> Option<&'static str> {
    if tap_restarting {
        Some("macOS disabled the event tap; it is being reinstalled")
    } else if secure_input {
        Some("Secure Input is active; native input is paused until it ends")
    } else if !layout_safe {
        Some("Automatic capture requires ABC or U.S. keyboard layout; IME/dead-key layouts are paused")
    } else {
        None
    }
}

/// The Quartz event type and button number a replayed mouse move is posted
/// as. With a task-held button it is that button's drag (left 6, right 7,
/// other 27), so a recorded drag selects or moves in the target; otherwise a
/// plain move (5). The left button wins over the right, both over others.
pub(crate) fn move_event(held: &BTreeSet<u32>) -> (u32, u32) {
    match held.iter().next() {
        None => (5, 0),
        Some(0) => (6, 0),
        Some(1) => (7, 1),
        Some(&other) => (27, other),
    }
}

/// One wheel notch in Windows `WHEEL_DELTA` units.
const WHEEL_DELTA: i32 = 120;

/// Turns the 1/120-notch deltas of a Windows recording into the line
/// steps macOS scrolls by. A precision touchpad sends many small deltas; the
/// remainder carries over to the next step on the same axis, so a run of
/// them still scrolls as far as the recording did, instead of each rounding
/// to nothing.
#[derive(Debug, Default)]
pub(crate) struct WheelSteps {
    vertical: i32,
    horizontal: i32,
}

impl WheelSteps {
    /// Lines to post for one wheel step (0: nothing to post yet).
    pub(crate) fn lines(&mut self, axis: WheelAxis, amount: i32, unit: WheelUnit) -> i32 {
        if unit == WheelUnit::Clicks {
            return amount;
        }
        let residue = match axis {
            WheelAxis::Vertical => &mut self.vertical,
            WheelAxis::Horizontal => &mut self.horizontal,
        };
        let total = residue.saturating_add(amount);
        // Truncates toward zero, so the sign of what is left stays the sign
        // of the scroll and opposite deltas cancel.
        let lines = total / WHEEL_DELTA;
        *residue = total % WHEEL_DELTA;
        lines
    }
}

/// Waits up to `wait` for `held` to turn false (a key or modifier the user
/// is still pressing), checking every 10 ms. Whether it came up in time.
pub(crate) fn released_within(wait: Duration, mut held: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + wait;
    while held() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Whether the plan's first clipboard step (`first`) may be written before
/// the trigger is erased, as on Windows (`written_before_erase`): only when
/// every step before it just types text. After a key, chord or click the
/// target may copy or cut, so the write waits for its own step.
pub(crate) fn written_before_erase(plan: &ActionPlan, first: usize) -> bool {
    plan.steps[..first.min(plan.steps.len())]
        .iter()
        .all(|step| {
            matches!(
                step.operation,
                Operation::Text {
                    method: OutputMethod::Keys,
                    ..
                }
            )
        })
}

/// UTIs whose data is text, the only ones `TextOnly` backs up.
const TEXT_TYPES: &[&str] = &[
    "public.utf8-plain-text",
    "public.utf16-plain-text",
    "public.utf16-external-plain-text",
    "public.plain-text",
    "NSStringPboardType",
];

/// What a clipboard backup does with one pasteboard type (B-08).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backup {
    /// Back up its bytes and restore them.
    Copy,
    /// Not restorable (a promised or private type whose data only its owner
    /// can provide): left out and counted as skipped.
    Skip,
    /// Left out without being counted (TextOnly's non-text types).
    Ignore,
    /// Refuse the action before the trigger is erased.
    Refuse,
}

/// The backup decision for a type: `readable` is whether NSPasteboard gave
/// its data. Under `Full` every unreadable type refuses, as before.
pub(crate) fn backup(policy: RestorePolicy, kind: &str, readable: bool) -> Backup {
    if policy == RestorePolicy::TextOnly && !TEXT_TYPES.contains(&kind) {
        return Backup::Ignore;
    }
    match (readable, policy) {
        (true, _) => Backup::Copy,
        (false, RestorePolicy::Full) => Backup::Refuse,
        (false, _) => Backup::Skip,
    }
}

/// Whether a finished backup must still refuse: `BestEffort` never restores
/// an empty clipboard over one that held data (Windows does the same);
/// `TextOnly` opted into dropping everything that is not text.
pub(crate) fn backup_refused(policy: RestorePolicy, had_types: bool, kept_types: bool) -> bool {
    policy == RestorePolicy::BestEffort && had_types && !kept_types
}

/// Reason codes recognised in the macOS run messages; the first matching
/// fragment wins. Every fragment is a text `macos.rs` produces (`guarded`,
/// `native_ready`, the clipboard lease and the action steps).
const REASONS: &[(&str, ReasonCode)] = &[
    ("Cancelled", ReasonCode::Cancelled),
    ("Execution timeout", ReasonCode::ExecutorTimeout),
    ("Secure Input", ReasonCode::ProtectedControl),
    ("Grant ", ReasonCode::EngineDown),
    ("macOS denied synthetic input", ReasonCode::EngineDown),
    ("event tap", ReasonCode::EngineDown),
    ("Physical input or focus changed", ReasonCode::StaleInput),
    ("revision changed", ReasonCode::StaleInput),
    ("Revision changed", ReasonCode::StaleInput),
    ("IME/composition", ReasonCode::StaleInput),
    ("keyboard layout", ReasonCode::StaleInput),
    (
        "Focused application/window/control changed",
        ReasonCode::FocusChanged,
    ),
    ("not the current foreground", ReasonCode::FocusChanged),
    ("modifier is held", ReasonCode::ModifierHeld),
    ("physically held", ReasonCode::ModifierHeld),
    ("already held", ReasonCode::ModifierHeld),
    ("Physical mouse button", ReasonCode::ModifierHeld),
    ("Physical key conflicts", ReasonCode::ModifierHeld),
    ("exceeds 64 MB", ReasonCode::ClipboardTooLarge),
    ("too many objects", ReasonCode::ClipboardTooLarge),
    ("could not be preserved", ReasonCode::ClipboardFormatSkipped),
    (
        "No clipboard format could be preserved",
        ReasonCode::ClipboardFormatSkipped,
    ),
    ("Clipboard changed", ReasonCode::ClipboardChanged),
    ("Clipboard ownership", ReasonCode::ClipboardChanged),
    ("Clipboard data is unreadable", ReasonCode::ClipboardBusy),
    ("NSPasteboard rejected", ReasonCode::ClipboardBusy),
];

/// Reason of a run that stopped with `message`. Unrecognised texts are
/// `Internal`, which counts as an engine failure.
pub(crate) fn reason_for(message: &str) -> ReasonCode {
    REASONS
        .iter()
        .find(|(fragment, _)| message.contains(fragment))
        .map_or(ReasonCode::Internal, |(_, reason)| *reason)
}

/// Where the worker stopped a run that did not get to execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// Expanding the rule's action failed.
    Resolve,
    /// The plan was refused before anything was sent.
    Preflight,
    /// `execute` returned an error (nothing typed; the lease restored the
    /// clipboard).
    Execute,
}

/// History verdict of one automatic run: outcome, reason, completed steps
/// and side effects. A finished result keeps its own counts; an error had
/// no side effects. The outcome follows the Windows rule
/// ([`crate::history::outcome_for`]).
pub(crate) fn verdict(
    result: &Result<crate::ExecutionResult, (Stop, String)>,
) -> (Outcome, ReasonCode, usize, bool) {
    match result {
        Ok(run) => {
            let reason = match run.status.as_str() {
                "cancelled" => Some(ReasonCode::Cancelled),
                "failed" => Some(reason_for(&run.message)),
                _ => None,
            };
            let (outcome, reason) = reason.map_or((Outcome::Succeeded, ReasonCode::Ok), |reason| {
                (
                    crate::history::outcome_for(reason, run.side_effects),
                    reason,
                )
            });
            (outcome, reason, run.completed_steps, run.side_effects)
        }
        Err((stop, message)) => {
            let reason = match stop {
                Stop::Resolve => ReasonCode::ResolveFailed,
                Stop::Preflight => ReasonCode::PreflightFailed,
                Stop::Execute => reason_for(message),
            };
            (crate::history::outcome_for(reason, false), reason, 0, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExecutionResult;
    use quicker_domain::{key_edit, KeyEdit, SourceSpan, Step};

    #[test]
    fn mac_key_codes_edit_the_buffer_like_windows() {
        let edit =
            |code, text: &str, modified| key_edit(key_class(code), Some(text.into()), modified);
        // Shift, Caps Lock and the other modifiers keep the buffer.
        for code in 54..=63 {
            assert_eq!(edit(code, "", false), KeyEdit::Neutral, "{code}");
        }
        assert_eq!(edit(51, "\u{8}", false), KeyEdit::Backspace);
        assert_eq!(edit(51, "\u{8}", true), KeyEdit::Reset);
        // Return, Tab and the caret keys reset; they are not terminators.
        for code in [36, 76, 48, 53, 117, 115, 116, 119, 121, 123, 124, 125, 126] {
            assert_eq!(edit(code, "\r", false), KeyEdit::Reset, "{code}");
        }
        // Any character the layout types counts, not only ASCII.
        assert_eq!(edit(14, "é", false), KeyEdit::Text("é".into()));
        assert_eq!(edit(49, " ", false), KeyEdit::Text(" ".into()));
        assert_eq!(edit(8, "c", true), KeyEdit::Reset);
    }

    #[test]
    fn transient_conditions_name_their_block() {
        assert_eq!(platform_block(false, true, false), None);
        assert!(platform_block(true, true, false)
            .unwrap()
            .contains("Secure Input"));
        assert!(platform_block(false, false, false)
            .unwrap()
            .contains("layout"));
        assert!(platform_block(true, false, true)
            .unwrap()
            .contains("event tap"));
    }

    #[test]
    fn a_held_key_is_waited_for_up_to_the_limit() {
        let mut polls = 0;
        assert!(released_within(Duration::from_millis(200), || {
            polls += 1;
            polls < 3
        }));
        assert_eq!(polls, 3);
        assert!(released_within(Duration::ZERO, || false));
        assert!(!released_within(Duration::from_millis(30), || true));
    }

    #[test]
    fn moves_with_a_held_button_are_drags() {
        let held = |buttons: &[u32]| buttons.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(move_event(&held(&[])), (5, 0));
        assert_eq!(move_event(&held(&[0])), (6, 0));
        assert_eq!(move_event(&held(&[1])), (7, 1));
        assert_eq!(move_event(&held(&[2])), (27, 2));
        assert_eq!(move_event(&held(&[4, 1, 0])), (6, 0));
        assert_eq!(move_event(&held(&[3, 1])), (7, 1));
    }

    #[test]
    fn wheel_deltas_become_lines_and_keep_their_remainder() {
        let mut steps = WheelSteps::default();
        assert_eq!(steps.lines(WheelAxis::Vertical, 3, WheelUnit::Clicks), 3);
        assert_eq!(steps.lines(WheelAxis::Vertical, 240, WheelUnit::Delta), 2);
        assert_eq!(steps.lines(WheelAxis::Vertical, -120, WheelUnit::Delta), -1);
        // A touchpad's small deltas add up to whole lines.
        let lines: i32 = (0..15)
            .map(|_| steps.lines(WheelAxis::Vertical, 16, WheelUnit::Delta))
            .sum();
        assert_eq!(lines, 2);
        // The axes keep separate remainders; opposite deltas cancel.
        assert_eq!(steps.lines(WheelAxis::Horizontal, 100, WheelUnit::Delta), 0);
        assert_eq!(
            steps.lines(WheelAxis::Horizontal, -100, WheelUnit::Delta),
            0
        );
        assert_eq!(steps.lines(WheelAxis::Horizontal, 20, WheelUnit::Delta), 0);
        assert_eq!(steps.lines(WheelAxis::Horizontal, 100, WheelUnit::Delta), 1);
        assert_eq!(
            steps.lines(WheelAxis::Vertical, i32::MAX, WheelUnit::Delta),
            i32::MAX / 120
        );
    }

    fn plan(operations: Vec<Operation>) -> ActionPlan {
        ActionPlan {
            steps: operations
                .into_iter()
                .map(|op| Step::new(op, SourceSpan::default()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_first_paste_is_written_early_only_after_typed_text() {
        let typed = || Operation::Text {
            text: "a".into(),
            method: OutputMethod::Keys,
        };
        let paste = || Operation::Text {
            text: "b".into(),
            method: OutputMethod::Paste,
        };
        assert!(written_before_erase(&plan(vec![paste()]), 0));
        assert!(written_before_erase(
            &plan(vec![typed(), typed(), paste()]),
            2
        ));
        let copy_first = plan(vec![
            Operation::Hotkey {
                keys: vec![
                    quicker_domain::Key::PlatformPrimary,
                    quicker_domain::Key::named("C"),
                ],
            },
            paste(),
        ]);
        assert!(!written_before_erase(&copy_first, 1));
        let key_first = plan(vec![
            typed(),
            Operation::KeyPress {
                key: quicker_domain::Key::named("Tab"),
            },
            paste(),
        ]);
        assert!(!written_before_erase(&key_first, 2));
    }

    #[test]
    fn unreadable_types_follow_the_restore_policy() {
        use RestorePolicy::*;
        let text = "public.utf8-plain-text";
        let promised = "com.example.promised";
        assert_eq!(backup(Full, promised, true), Backup::Copy);
        assert_eq!(backup(Full, promised, false), Backup::Refuse);
        assert_eq!(backup(BestEffort, promised, false), Backup::Skip);
        assert_eq!(backup(BestEffort, text, false), Backup::Skip);
        assert_eq!(backup(TextOnly, promised, true), Backup::Ignore);
        assert_eq!(backup(TextOnly, promised, false), Backup::Ignore);
        assert_eq!(backup(TextOnly, text, true), Backup::Copy);
        assert_eq!(backup(TextOnly, text, false), Backup::Skip);
        assert!(backup_refused(BestEffort, true, false));
        assert!(!backup_refused(BestEffort, true, true));
        assert!(!backup_refused(BestEffort, false, false));
        assert!(!backup_refused(TextOnly, true, false));
        assert!(!backup_refused(Full, true, false));
    }

    fn run(status: &str, message: &str, side_effects: bool) -> ExecutionResult {
        ExecutionResult {
            id: "r".into(),
            status: status.into(),
            message: message.into(),
            completed_steps: 1,
            side_effects,
        }
    }

    #[test]
    fn runs_get_the_windows_verdicts() {
        assert_eq!(
            verdict(&Ok(run("unknown", "", true))),
            (Outcome::Succeeded, ReasonCode::Ok, 1, true)
        );
        assert_eq!(
            verdict(&Ok(run("cancelled", "Cancelled", true))),
            (Outcome::Cancelled, ReasonCode::Cancelled, 1, true)
        );
        assert_eq!(
            verdict(&Ok(run(
                "failed",
                "Physical input or focus changed; execution stopped",
                false
            ))),
            (Outcome::Skipped, ReasonCode::StaleInput, 1, false)
        );
        assert_eq!(
            verdict(&Ok(run(
                "failed",
                "A physical modifier is held; execution stopped",
                true
            ))),
            (Outcome::Failed, ReasonCode::ModifierHeld, 1, true)
        );
        assert_eq!(
            verdict(&Ok(run("failed", "Execution timeout", false))),
            (Outcome::Failed, ReasonCode::ExecutorTimeout, 1, false)
        );
        assert_eq!(
            verdict(&Err((Stop::Resolve, "bad".into()))),
            (Outcome::Skipped, ReasonCode::ResolveFailed, 0, false)
        );
        assert_eq!(
            verdict(&Err((Stop::Preflight, "Secure Input".into()))),
            (Outcome::Skipped, ReasonCode::PreflightFailed, 0, false)
        );
        assert_eq!(
            verdict(&Err((
                Stop::Execute,
                "A promised/private clipboard format could not be preserved; paste refused before erasure"
                    .into()
            ))),
            (Outcome::Skipped, ReasonCode::ClipboardFormatSkipped, 0, false)
        );
        assert_eq!(
            verdict(&Err((Stop::Execute, "something new".into()))),
            (Outcome::Failed, ReasonCode::Internal, 0, false)
        );
    }

    /// Every fragment matches a message `macos.rs` (or [`platform_block`])
    /// really produces; a reworded message fails here instead of becoming
    /// `Internal`.
    #[test]
    fn reason_fragments_match_the_mac_messages() {
        let mut source = include_str!("macos.rs").to_string();
        for (secure, safe, restarting) in [
            (true, true, false),
            (false, false, false),
            (false, true, true),
        ] {
            source.push_str(platform_block(secure, safe, restarting).unwrap());
        }
        for (fragment, reason) in REASONS {
            assert!(
                source.contains(fragment),
                "{fragment:?} ({reason:?}) not in macos.rs"
            );
        }
        for message in [
            "Focused application/window/control changed or cannot be verified",
            "Clipboard changed before paste; user's new content preserved",
        ] {
            assert!(source.contains(message), "{message}");
            assert_ne!(reason_for(message), ReasonCode::Internal, "{message}");
        }
    }
}
