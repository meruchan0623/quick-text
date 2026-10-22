//! Output layer over the [`Injector`] port with fail-stop accounting of
//! accepted events, plus the Windows adapter (SendInput and the vendored
//! Espanso injector).
use super::keystate::physical;
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
pub(super) fn key(injector: &impl Injector, v: u16, up: bool) -> Result<(), String> {
    record_send_result(injector.send_key(v, up), 1)
}
pub(super) fn press(injector: &impl Injector, v: u16) -> Result<(), String> {
    press_keys(injector, &[v])
}
pub(super) fn press_keys(injector: &impl Injector, keys: &[u16]) -> Result<(), String> {
    let result = record_send_result(injector.send_keys(keys), keys.len() as u32 * 2);
    cleanup_submission(keys, result, physical, |v| key(injector, v, true))
}
pub(super) fn combination(injector: &impl Injector, keys: &[u16]) -> Result<(), String> {
    let result = record_send_result(injector.send_chord(keys), keys.len() as u32 * 2);
    cleanup_submission(keys, result, physical, |v| key(injector, v, true))
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
                _ if n.starts_with('F') => {
                    let num = n[1..]
                        .parse::<u16>()
                        .map_err(|_| format!("Unknown key {name}"))?;
                    if !(1..=24).contains(&num) {
                        return Err(format!("Unknown key {name}"));
                    }
                    VK_F1 + num - 1
                }
                _ if n.len() == 2 && n.starts_with('D') && n.as_bytes()[1].is_ascii_digit() => {
                    n.as_bytes()[1] as u16
                }
                _ if n.len() == 1 && n.as_bytes()[0].is_ascii_alphanumeric() => {
                    n.as_bytes()[0] as u16
                }
                _ => return Err(format!("Unknown native key {name}")),
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
    record_send_result(injector.send_mouse(flags, data, x, y), 1)
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
    use super::*;
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
