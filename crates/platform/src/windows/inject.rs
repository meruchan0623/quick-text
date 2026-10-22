//! Output primitives (SendInput and the vendored injector) with fail-stop
//! accounting of accepted events.
use super::*;
use super::{actions::guarded, keystate::physical};
thread_local! {
    pub(super) static SENT_EFFECT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn send(inputs: &[INPUT]) -> Result<(), String> {
    unsafe {
        let accepted = SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        );
        record_send_result(accepted, inputs.len() as u32)?;
    }
    Ok(())
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
pub(super) fn key(v: u16, up: bool) -> Result<(), String> {
    let mut input: INPUT = unsafe { zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: v,
        wScan: 0,
        dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
        time: 0,
        dwExtraInfo: MARK,
    };
    send(&[input])
}
pub(super) fn press(v: u16) -> Result<(), String> {
    rawinput::press_keys(&[v])
}

pub(super) fn send_terminator(t: &str) -> Result<(), String> {
    if t == "semicolon" {
        return rawinput::text(";");
    }
    press(match t {
        "space" => VK_SPACE,
        "tab" => VK_TAB,
        "enter" => VK_RETURN,
        _ => return Ok(()),
    })
}
pub(super) fn unicode(
    text: &str,
    s: &Shared,
    target: &Foreground,
    generation: u64,
) -> Result<(), String> {
    // Preserve Keys scalar order, without per-character UIA readback.
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        guarded(s, target, generation)?;
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                press(VK_RETURN)?;
            }
            '\n' => press(VK_RETURN)?,
            '\t' => press(VK_TAB)?,
            _ => rawinput::text(&ch.to_string())?,
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
pub(super) fn hotkey(keys: &[Key]) -> Result<(), String> {
    let values = keys.iter().map(vk).collect::<Result<Vec<_>, _>>()?;
    if values.iter().any(|v| physical(*v)) {
        return Err("Physical key conflicts with requested hotkey; action stopped".into());
    }
    rawinput::combination(&values)
}

pub(super) fn mouse_input(flags: u32, data: u32, x: i32, y: i32) -> Result<(), String> {
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
    send(&[input])
}
pub(super) fn button(button: MouseButton, state: ButtonState) -> Result<(), String> {
    let (down, up, data) = match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        MouseButton::X1 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1),
        MouseButton::X2 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2),
    };
    match state {
        ButtonState::Down => mouse_input(down, data, 0, 0),
        ButtonState::Up => mouse_input(up, data, 0, 0),
        ButtonState::Click => {
            mouse_input(down, data, 0, 0)?;
            mouse_input(up, data, 0, 0)
        }
        ButtonState::DoubleClick => {
            button_fn(down, up, data)?;
            std::thread::sleep(Duration::from_millis(80));
            button_fn(down, up, data)
        }
    }
}
fn button_fn(d: u32, u: u32, data: u32) -> Result<(), String> {
    mouse_input(d, data, 0, 0)?;
    mouse_input(u, data, 0, 0)
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
}
