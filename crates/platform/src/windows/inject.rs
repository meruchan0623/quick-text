//! Output layer over the [`Injector`] port with fail-stop accounting of
//! accepted events, plus the Windows adapter (SendInput and the vendored
//! Espanso injector).
use super::keystate::{own_input, physical, Injecting};
use super::*;
use crate::traits::{Clock, Injector};

extern "C" {
    fn inject_string(text: *const u16) -> u32;
    fn inject_separate_vkeys(keys: *const i32, len: i32) -> u32;
    fn inject_vkeys_combination(keys: *const i32, len: i32) -> u32;
}
thread_local! {
    pub(super) static SENT_EFFECT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Production [`Injector`]: text, taps and chords go through the vendored
/// injector; single key transitions and mouse events through `SendInput`.
pub(super) struct WinInjector;
fn send_input(input: INPUT) -> u32 {
    unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) }
}
fn vkeys(keys: &[u16]) -> Vec<i32> {
    keys.iter().map(|v| i32::from(*v)).collect()
}
impl Injector for WinInjector {
    fn send_text(&self, text: &str) -> u32 {
        let encoded = wide(text);
        unsafe { inject_string(encoded.as_ptr()) }
    }
    fn send_keys(&self, keys: &[u16]) -> u32 {
        let keys = vkeys(keys);
        unsafe { inject_separate_vkeys(keys.as_ptr(), keys.len() as i32) }
    }
    fn send_chord(&self, keys: &[u16]) -> u32 {
        let keys = vkeys(keys);
        unsafe { inject_vkeys_combination(keys.as_ptr(), keys.len() as i32) }
    }
    fn send_key(&self, key: u16, up: bool) -> u32 {
        let mut input: INPUT = unsafe { zeroed() };
        input.r#type = INPUT_KEYBOARD;
        input.Anonymous.ki = KEYBDINPUT {
            wVk: key,
            wScan: 0,
            dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
            time: 0,
            dwExtraInfo: MARK,
        };
        send_input(input)
    }
    fn send_mouse(&self, flags: u32, data: u32, x: i32, y: i32) -> u32 {
        let mut input: INPUT = unsafe { zeroed() };
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi = MOUSEINPUT {
            dx: x,
            dy: y,
            mouseData: data,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: MARK,
        };
        send_input(input)
    }
}

// no-regression #6: an injected-count mismatch fails the step and stops the action.
pub(super) fn record_send_result(accepted: u32, requested: u32) -> Result<(), String> {
    if accepted > 0 {
        mark_external_effect();
    }
    if accepted != requested {
        return Err(format!("SendInput accepted {accepted}/{requested} events (possibly elevated target / UIPI); partial effects and target consumption unknown"));
    }
    Ok(())
}
pub(super) fn mark_external_effect() {
    SENT_EFFECT.with(|flag| flag.set(true));
}
pub(super) fn release_owned(
    owned: &mut BTreeSet<u16>,
    v: u16,
    physically_held: bool,
    release: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if !owned.contains(&v) {
        return Err("Cannot release an unowned key".into());
    }
    if physically_held {
        return Err("Physical key conflicts with release; action stopped".into());
    }
    release()?;
    owned.remove(&v);
    Ok(())
}
// H1: every injector call below runs inside `own_input().begin()`, and the
// key and button transitions the system accepted are noted (for a batch,
// the accepted prefix, before any cleanup), so the input consumer never
// imports our own output into the physical key mirror.
pub(super) fn key(injector: &impl Injector, v: u16, up: bool) -> Result<(), String> {
    let injecting = own_input().begin();
    submitted(&injecting, &[(v, up)], injector.send_key(v, up))
}
pub(super) fn press(injector: &impl Injector, v: u16) -> Result<(), String> {
    press_keys(injector, &[v])
}
pub(super) fn press_keys(injector: &impl Injector, keys: &[u16]) -> Result<(), String> {
    let injecting = own_input().begin();
    let sequence = taps(keys);
    let result = submitted(&injecting, &sequence, injector.send_keys(keys));
    cleanup_submission(keys, result, physical, |v| key(injector, v, true))
}
pub(super) fn combination(injector: &impl Injector, keys: &[u16]) -> Result<(), String> {
    let injecting = own_input().begin();
    let sequence = chord(keys);
    let result = submitted(&injecting, &sequence, injector.send_chord(keys));
    cleanup_submission(keys, result, physical, |v| key(injector, v, true))
}
/// The transitions [`Injector::send_keys`] submits, in order, as
/// `(key, up)`: each key pressed, then released.
fn taps(keys: &[u16]) -> Vec<(u16, bool)> {
    keys.iter().flat_map(|&v| [(v, false), (v, true)]).collect()
}
/// The transitions [`Injector::send_chord`] submits, in order, as
/// `(key, up)`: every key pressed, then all released in reverse.
fn chord(keys: &[u16]) -> Vec<(u16, bool)> {
    let presses = keys.iter().map(|&v| (v, false));
    presses
        .chain(keys.iter().rev().map(|&v| (v, true)))
        .collect()
}
/// Notes what the system accepted of one batch `sequence` before anything
/// else runs, so a partial submission keeps ownership of the presses it
/// made (H1). `SendInput` inserts the events serially and returns how many
/// it inserted, so `accepted` events are the first ones of `sequence`; the
/// vendored injector returns that count unchanged. A count beyond the
/// request cannot be mapped to events: every press of the batch is then
/// kept as ours, until an accepted key-up of ours or a key-up the hooks see
/// clears it.
fn submitted(
    injecting: &Injecting<'_>,
    sequence: &[(u16, bool)],
    accepted: u32,
) -> Result<(), String> {
    let requested = u32::try_from(sequence.len()).unwrap_or(u32::MAX);
    if accepted > requested {
        for &(v, up) in sequence {
            if !up {
                injecting.pressed(v);
            }
        }
    } else {
        // Accepted residual risk: this thread can be preempted between
        // SendInput returning and the loop below, while the hook already
        // handles the user's physical key-up of a key we just pressed.
        // `released_elsewhere` then finds no bit to clear, and the bit set
        // here outlives the key. This thread cannot tell the two orders
        // apart (the hook runs asynchronously to SendInput's return), so it
        // sets the bit anyway. Wrongly not setting it leaves a press of ours
        // unprotected: a resync imports it as physical, `release_owned`
        // skips it and the key stays down. Wrongly setting it only hides
        // the key from resync until its next key-up that is not ours, which
        // `released_elsewhere` always clears. Visible harm needs all of: a
        // batch ending with the key down (partial acceptance or a KeyDown
        // step), a failed cleanup release, the key-up inside this window,
        // and a later physical press the hooks miss.
        for &(v, up) in sequence.iter().take(accepted as usize) {
            if up {
                injecting.released(v);
            } else {
                injecting.pressed(v);
            }
        }
    }
    record_send_result(accepted, requested)
}
fn cleanup_submission(
    keys: &[u16],
    result: Result<(), String>,
    mut held: impl FnMut(u16) -> bool,
    mut release: impl FnMut(u16) -> Result<(), String>,
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
pub(super) fn text(injector: &impl Injector, value: &str) -> Result<(), String> {
    let requested = value
        .encode_utf16()
        .count()
        .checked_mul(2)
        .ok_or("Text too long")? as u32;
    if value.contains('\0') {
        return Err("Unicode injection cannot contain NUL".into());
    }
    let _injecting = own_input().begin();
    record_send_result(injector.send_text(value), requested)
}

pub(super) fn send_terminator(injector: &impl Injector, t: &str) -> Result<(), String> {
    if t == "semicolon" {
        return text(injector, ";");
    }
    press(
        injector,
        match t {
            "space" => VK_SPACE,
            "tab" => VK_TAB,
            "enter" => VK_RETURN,
            _ => return Ok(()),
        },
    )
}
/// Types `value` one scalar at a time, running `guard` before each one.
pub(super) fn unicode(
    injector: &impl Injector,
    value: &str,
    mut guard: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    // Preserve Keys scalar order, without per-character UIA readback.
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        guard()?;
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                press(injector, VK_RETURN)?;
            }
            '\n' => press(injector, VK_RETURN)?,
            '\t' => press(injector, VK_TAB)?,
            _ => text(injector, &ch.to_string())?,
        }
    }
    Ok(())
}

pub(super) fn vk(k: &Key) -> Result<u16, String> {
    match k {
        Key::WindowsCode { code } => Ok(*code),
        Key::PlatformPrimary => Ok(VK_CONTROL),
        Key::Named { name } => {
            let n = name.to_ascii_uppercase();
            let v = match n.as_str() {
                "CTRL" | "CONTROL" => VK_CONTROL,
                "SHIFT" => VK_SHIFT,
                "ALT" | "MENU" => VK_MENU,
                "WIN" | "LWIN" | "META" => VK_LWIN,
                "ENTER" | "RETURN" => VK_RETURN,
                "TAB" => VK_TAB,
                "ESC" | "ESCAPE" => VK_ESCAPE,
                "BACKSPACE" | "BACK" | "BS" => VK_BACK,
                "DELETE" | "DEL" => VK_DELETE,
                "INSERT" | "INS" => VK_INSERT,
                "SPACE" => VK_SPACE,
                "LEFT" => VK_LEFT,
                "RIGHT" => VK_RIGHT,
                "UP" => VK_UP,
                "DOWN" => VK_DOWN,
                "HOME" => VK_HOME,
                "END" => VK_END,
                "PGUP" | "PAGEUP" => VK_PRIOR,
                "PGDN" | "PAGEDOWN" => VK_NEXT,
                "CAPSLOCK" => VK_CAPITAL,
                "NUMLOCK" => VK_NUMLOCK,
                "SCROLLLOCK" => VK_SCROLL,
                "PRINTSCREEN" | "PRTSC" => VK_SNAPSHOT,
                "PAUSE" => VK_PAUSE,
                "VOLUMEUP" => VK_VOLUME_UP,
                "VOLUMEDOWN" => VK_VOLUME_DOWN,
                "VOLUMEMUTE" => VK_VOLUME_MUTE,
                "MEDIAPLAYPAUSE" => VK_MEDIA_PLAY_PAUSE,
                "MEDIANEXTTRACK" => VK_MEDIA_NEXT_TRACK,
                "MEDIAPREVIOUSTRACK" => VK_MEDIA_PREV_TRACK,
                "BROWSERBACK" => VK_BROWSER_BACK,
                "BROWSERFORWARD" => VK_BROWSER_FORWARD,
                // Fixed texts (F5-1 H): never the rule's key name.
                _ if n.starts_with('F') => {
                    let num = n[1..]
                        .parse::<u16>()
                        .map_err(|_| "Unknown key name".to_string())?;
                    if !(1..=24).contains(&num) {
                        return Err("Unknown key name".into());
                    }
                    VK_F1 + num - 1
                }
                _ if n.len() == 2 && n.starts_with('D') && n.as_bytes()[1].is_ascii_digit() => {
                    n.as_bytes()[1] as u16
                }
                _ if n.len() == 1 && n.as_bytes()[0].is_ascii_alphanumeric() => {
                    n.as_bytes()[0] as u16
                }
                _ => return Err("Unknown native key name".into()),
            };
            Ok(v)
        }
    }
}
pub(super) fn hotkey(injector: &impl Injector, keys: &[Key]) -> Result<(), String> {
    let values = keys.iter().map(vk).collect::<Result<Vec<_>, _>>()?;
    if values.iter().any(|v| physical(*v)) {
        return Err("Physical key conflicts with requested hotkey; action stopped".into());
    }
    combination(injector, &values)
}

pub(super) fn mouse_input(
    injector: &impl Injector,
    flags: u32,
    data: u32,
    x: i32,
    y: i32,
) -> Result<(), String> {
    let injecting = own_input().begin();
    let accepted = injector.send_mouse(flags, data, x, y);
    if accepted == 1 {
        note_buttons(&injecting, flags, data);
    }
    record_send_result(accepted, 1)
}
/// Notes the mouse buttons one accepted `send_mouse` call pressed or
/// released, as the virtual-key codes the asynchronous state reports.
/// 「待实测」: with swapped mouse buttons, which code an injected
/// `MOUSEEVENTF_LEFTDOWN` sets has not been checked on a real machine.
fn note_buttons(injecting: &Injecting<'_>, flags: u32, data: u32) {
    let xbutton = match data {
        1 => VK_XBUTTON1,
        2 => VK_XBUTTON2,
        _ => 0,
    };
    for (down, up, vk) in [
        (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, VK_LBUTTON),
        (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, VK_RBUTTON),
        (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, VK_MBUTTON),
        (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, xbutton),
    ] {
        if vk == 0 {
            continue;
        }
        if flags & down != 0 {
            injecting.pressed(vk);
        }
        if flags & up != 0 {
            injecting.released(vk);
        }
    }
}
pub(super) fn button(
    injector: &impl Injector,
    clock: &impl Clock,
    button: MouseButton,
    state: ButtonState,
) -> Result<(), String> {
    let (down, up, data) = match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        MouseButton::X1 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1),
        MouseButton::X2 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2),
    };
    match state {
        ButtonState::Down => mouse_input(injector, down, data, 0, 0),
        ButtonState::Up => mouse_input(injector, up, data, 0, 0),
        ButtonState::Click => {
            mouse_input(injector, down, data, 0, 0)?;
            mouse_input(injector, up, data, 0, 0)
        }
        ButtonState::DoubleClick => {
            button_fn(injector, down, up, data)?;
            clock.sleep(Duration::from_millis(80));
            button_fn(injector, down, up, data)
        }
    }
}
fn button_fn(injector: &impl Injector, d: u32, u: u32, data: u32) -> Result<(), String> {
    mouse_input(injector, d, data, 0, 0)?;
    mouse_input(injector, u, data, 0, 0)
}

/// Trigger erasure derived from a match proof: how many characters to
/// delete and when the final trigger key was observed.
#[derive(Clone, Debug)]
pub(super) struct Erasure {
    pub(super) count: usize,
    pub(super) at: Instant,
    pub(super) ime_unverified: bool,
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
/// Deletes the trigger with backspaces, running `guard` before each one.
pub(super) fn erase(
    injector: &impl Injector,
    clock: &impl Clock,
    erasure: &Erasure,
    guard: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    // Let the foreground process consume the final key; no UIA readback loop.
    let elapsed = clock.now().saturating_duration_since(erasure.at);
    if let Some(remaining) = Duration::from_millis(25).checked_sub(elapsed) {
        clock.sleep(remaining);
    }
    erase_count(erasure.count, guard, || press_keys(injector, &[VK_BACK]))?;
    if erasure.ime_unverified {
        gate("IME physical-letter erasure submitted; target composition/consumption unknown");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::keystate::{track_own_input, OwnInput, PHYSICAL};
    use super::*;
    use crate::mock::{FakeClock, MockInjector};

    #[test]
    fn injections_note_the_keys_and_buttons_the_system_accepted() {
        let own: &'static OwnInput = Box::leak(Box::new(OwnInput::new()));
        let _track = track_own_input(own);
        let injector = MockInjector::new();
        let in_flight = Arc::new(AtomicU64::new(0));
        let calls = in_flight.clone();
        injector.on_call(move |_| {
            assert!(own.active(), "every call runs inside an injection");
            calls.fetch_add(1, Ordering::SeqCst);
        });
        let seen = own.seq();
        key(&injector, 0x41, false).unwrap();
        assert!(own.holds(0x41) && !own.active() && !own.settled(seen));
        // A rejected key-up leaves the key ours; a rejected key-down is not.
        injector.short_count(1, 0);
        assert!(key(&injector, 0x41, true).is_err());
        assert!(own.holds(0x41));
        key(&injector, 0x41, true).unwrap();
        assert!(!own.holds(0x41));
        injector.short_count(3, 0);
        assert!(key(&injector, 0x42, false).is_err());
        assert!(!own.holds(0x42));
        let clock = FakeClock::new();
        button(&injector, &clock, MouseButton::X2, ButtonState::Down).unwrap();
        button(&injector, &clock, MouseButton::Left, ButtonState::Down).unwrap();
        assert!(own.holds(VK_XBUTTON2) && own.holds(VK_LBUTTON) && !own.holds(VK_XBUTTON1));
        button(&injector, &clock, MouseButton::X2, ButtonState::Up).unwrap();
        button(&injector, &clock, MouseButton::Left, ButtonState::Click).unwrap();
        assert!(!own.holds(VK_XBUTTON2) && !own.holds(VK_LBUTTON));
        // Taps, chords and text leave nothing held but still count.
        let seen = own.seq();
        press_keys(&injector, &[VK_BACK]).unwrap();
        combination(&injector, &[VK_CONTROL, 0x56]).unwrap();
        text(&injector, "x").unwrap();
        mouse_input(&injector, MOUSEEVENTF_MOVE, 0, 1, 1).unwrap();
        assert!(!own.settled(seen) && !own.active() && !own.holds(VK_CONTROL));
        assert_eq!(in_flight.load(Ordering::SeqCst), 13);
    }
    #[test]
    fn partial_batches_keep_the_presses_the_system_accepted() {
        // The cleanup's `physical()` reads the global mirror; once seeded it
        // never falls back to the real asynchronous state.
        PHYSICAL.mark_seeded();
        let own: &'static OwnInput = Box::leak(Box::new(OwnInput::new()));
        let _track = track_own_input(own);
        // Taps A, B: A down, A up and B down get in, and B's cleanup release
        // is rejected. B stays ours; A is not.
        let injector = MockInjector::new();
        injector.short_count(0, 3);
        injector.short_count(1, 0);
        assert!(press_keys(&injector, &[0x41, 0x42]).is_err());
        assert!(own.holds(0x42) && !own.holds(0x41));
        // Chord Shift+X: both presses and X's release get in; Shift's
        // cleanup release is rejected.
        let injector = MockInjector::new();
        injector.short_count(0, 3);
        injector.short_count(2, 0);
        assert!(combination(&injector, &[VK_SHIFT, 0x58]).is_err());
        assert!(own.holds(VK_SHIFT) && !own.holds(0x58));
        // A count beyond the request maps to no prefix: every press of the
        // batch stays ours until a release of ours gets in.
        let injector = MockInjector::new();
        injector.short_count(0, 7);
        injector.short_count(1, 0);
        injector.short_count(2, 0);
        assert!(combination(&injector, &[VK_MENU, 0x43]).is_err());
        assert!(
            own.holds(VK_MENU) && own.holds(0x43),
            "both cleanup releases failed"
        );
        let injector = MockInjector::new();
        injector.short_count(0, 2);
        assert!(key(&injector, VK_MENU, true).is_err());
        assert!(own.holds(VK_MENU), "an impossible count releases nothing");
        injector.short_count(1, 2);
        assert!(key(&injector, 0x44, false).is_err());
        assert!(own.holds(0x44), "but takes the press");
    }
    #[test]
    fn failed_key_release_retains_ownership_for_cleanup() {
        let mut held = BTreeSet::from([VK_CONTROL]);
        assert!(release_owned(&mut held, VK_CONTROL, false, || Err("rejected".into())).is_err());
        assert!(held.contains(&VK_CONTROL));
        assert!(release_owned(&mut held, VK_CONTROL, true, || panic!(
            "must not release physical key"
        ))
        .is_err());
        assert!(held.contains(&VK_CONTROL));
        release_owned(&mut held, VK_CONTROL, false, || Ok(())).unwrap();
        assert!(held.is_empty());
    }
    #[test]
    fn partial_unicode_submission_reports_side_effects() {
        SENT_EFFECT.with(|flag| flag.set(false));
        assert!(record_send_result(0, 2).is_err());
        assert!(!SENT_EFFECT.with(|flag| flag.get()));
        assert!(record_send_result(1, 2).is_err());
        assert!(SENT_EFFECT.with(|flag| flag.get()));
        SENT_EFFECT.with(|flag| flag.set(false));
    }
    #[test]
    fn clipboard_clear_is_effect_even_when_no_input_was_accepted() {
        SENT_EFFECT.with(|flag| flag.set(false));
        assert!(record_send_result(0, 2).is_err());
        mark_external_effect(); // EmptyClipboard succeeded before SetClipboardData failed.
        assert!(SENT_EFFECT.with(|flag| flag.get()));
        SENT_EFFECT.with(|flag| flag.set(false));
    }
    // Moved from rawinput.rs with the output functions (S-23).
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
    // Moved from rawinput.rs with the output functions (S-23).
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
}
