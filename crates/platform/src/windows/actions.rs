//! Action plans: preflight and execution (erase, output steps, owned-key
//! cleanup, terminator and clipboard restore).
use super::*;
use super::{
    clipboard::{acquire_clipboard, clipboard_formats, ClipboardLease, PreparedClipboard},
    executor::CandidateProof,
    inject::{
        button, hotkey, key, mouse_input, press, release_owned, send_terminator, unicode, vk,
        SENT_EFFECT,
    },
    keystate::{modifiers_held, physical},
    protection::{native_input_protection, protection_block},
};
thread_local! {
    static AUTOMATIC_EXECUTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn open(target: &str, args: &[String], cwd: &Option<String>) -> Result<(), String> {
    if args.is_empty() {
        unsafe {
            let result = ShellExecuteW(
                std::ptr::null_mut(),
                wide("open").as_ptr(),
                wide(target).as_ptr(),
                std::ptr::null(),
                cwd.as_ref()
                    .map(|s| wide(s))
                    .as_ref()
                    .map(|w| w.as_ptr())
                    .unwrap_or(std::ptr::null()),
                SW_SHOWNORMAL,
            );
            if result as usize <= 32 {
                return Err("Windows could not open target".into());
            }
        }
        Ok(())
    } else {
        let mut cmd = Command::new(target);
        cmd.args(args);
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
    }
}

pub fn preflight(plan: &ActionPlan) -> Result<(), String> {
    let diagnostics = validate_plan(plan);
    if !diagnostics.is_empty() {
        return Err(diagnostics
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
            .join("; "));
    }
    for step in &plan.steps {
        match &step.operation {
            Operation::Text {
                text,
                method: OutputMethod::Keys,
            } if text.contains('\0') => {
                return Err("Unicode key output cannot contain NUL".into());
            }
            Operation::KeyDown { key } | Operation::KeyUp { key } | Operation::KeyPress { key } => {
                vk(key)?;
            }
            Operation::Hotkey { keys } => {
                for k in keys {
                    vk(k)?;
                }
            }
            Operation::PasteFiles { paths } => {
                for p in paths {
                    if !std::path::Path::new(p).exists() {
                        return Err(format!("File does not exist: {p}"));
                    }
                }
            }
            Operation::PasteImage { path } => {
                image::open(path).map_err(|e| format!("Image preflight failed: {e}"))?;
            }
            Operation::Open {
                target,
                args,
                working_directory,
            } => {
                if !(target.starts_with("https://")
                    || target.starts_with("http://")
                    || std::path::Path::new(target).exists())
                {
                    return Err(
                        "Open target must be an existing absolute path or HTTP(S) URL".into(),
                    );
                }
                if !args.is_empty() && target.starts_with("http") {
                    return Err("URL arguments are not supported".into());
                }
                if let Some(c) = working_directory {
                    if !std::path::Path::new(c).is_dir() {
                        return Err("Working directory does not exist".into());
                    }
                }
            }
            Operation::Reusable { .. } => {
                return Err("Reusable action id is not registered; map it explicitly".into())
            }
            Operation::Common { function } => {
                common_keys(function)?;
            }
            _ => {}
        }
    }
    Ok(())
}
fn common_keys(name: &str) -> Result<Vec<Key>, String> {
    let names: Vec<&str> = match name {
        "copy" => vec!["CTRL", "C"],
        "paste" => vec!["CTRL", "V"],
        "cut" => vec!["CTRL", "X"],
        "undo" => vec!["CTRL", "Z"],
        "redo" => vec!["CTRL", "Y"],
        "select_all" => vec!["CTRL", "A"],
        "volume_up" => vec!["VOLUMEUP"],
        "volume_down" => vec!["VOLUMEDOWN"],
        "volume_mute" => vec!["VOLUMEMUTE"],
        "media_play_pause" => vec!["MEDIAPLAYPAUSE"],
        "media_next" => vec!["MEDIANEXTTRACK"],
        "media_previous" => vec!["MEDIAPREVIOUSTRACK"],
        "browser_back" => vec!["BROWSERBACK"],
        "browser_forward" => vec!["BROWSERFORWARD"],
        _ => {
            return Err(format!(
                "Common function {name} has no adapter; choose a supported function"
            ))
        }
    };
    Ok(names.into_iter().map(Key::named).collect())
}
struct FenceLease;
impl Drop for FenceLease {
    fn drop(&mut self) {
        rawinput::install(None);
        AUTOMATIC_EXECUTION.with(|automatic| automatic.set(false));
    }
}
// no-regression #6: every step re-checks pause/cancel/generation/focus/protection.
fn guarded_context(s: &Shared, target: &Foreground, generation: u64) -> Result<(), String> {
    if pause_blocks_execution(
        s.paused.load(Ordering::SeqCst),
        AUTOMATIC_EXECUTION.with(|automatic| automatic.get()),
    ) {
        return Err("Engine paused; input stopped".into());
    }
    if s.cancel.load(Ordering::SeqCst) {
        return Err("Cancelled".into());
    }
    if s.generation.load(Ordering::SeqCst) != generation {
        return Err("User input changed during action; stopped safely".into());
    }
    let now = foreground();
    if now.id != target.id || !now.process.eq_ignore_ascii_case(&target.process) {
        return Err("Foreground window/process changed; stopped safely".into());
    }
    rawinput::guard(s)?;
    if let Some(error) = native_input_protection().or_else(protection_block) {
        return Err(error);
    }
    Ok(())
}
pub(super) fn guarded(s: &Shared, target: &Foreground, generation: u64) -> Result<(), String> {
    guarded_context(s, target, generation)?;
    if physical(VK_CONTROL)
        || physical(VK_MENU)
        || physical(VK_SHIFT)
        || physical(VK_LWIN)
        || physical(VK_RWIN)
    {
        return Err("Physical modifier is held; input stopped".into());
    }
    Ok(())
}
fn pause_blocks_execution(paused: bool, automatic: bool) -> bool {
    paused && automatic
}
pub fn execute(
    s: Arc<Shared>,
    plan: ActionPlan,
    target: Foreground,
    generation: u64,
    erase: usize,
    terminator: Option<String>,
    speed: f64,
) -> Result<ExecutionResult, String> {
    if erase != 0 {
        return Err("Automatic erasure requires observed text proof".into());
    }
    execute_inner(s, plan, target, generation, None, terminator, speed)
}
pub(super) fn execute_inner(
    s: Arc<Shared>,
    plan: ActionPlan,
    mut target: Foreground,
    generation: u64,
    proof: Option<CandidateProof>,
    terminator: Option<String>,
    speed: f64,
) -> Result<ExecutionResult, String> {
    SENT_EFFECT.with(|flag| flag.set(false));
    AUTOMATIC_EXECUTION.with(|automatic| automatic.set(proof.is_some()));
    rawinput::install(proof.as_ref().map(|proof| match proof {
        CandidateProof::Espanso(proof) => proof.clone(),
    }));
    let _fence_lease = FenceLease;
    if !speed.is_finite() || speed <= 0.0 {
        return Err("Replay speed must be positive and finite".into());
    }
    preflight(&plan)?;
    // A shifted or AltGr trigger can match before the modifier is released.
    // Wait for release while keeping every cancellation and focus check active.
    if proof.is_some() {
        let wait = Instant::now();
        while modifiers_held() && wait.elapsed() < Duration::from_secs(1) {
            guarded_context(&s, &target, generation)?;
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    guarded(&s, &target, generation)?;
    // All potentially failing source/format checks happen before trigger deletion.
    let mut clipboard = acquire_clipboard(&plan, ClipboardLease::new)?;
    let mut payloads = std::collections::BTreeMap::new();
    if clipboard.is_some() {
        for (index, step) in plan.steps.iter().enumerate() {
            if let Some(formats) = clipboard_formats(&step.operation)? {
                payloads.insert(index, PreparedClipboard::new(formats)?);
            }
        }
    }
    guarded(&s, &target, generation)?;
    if let Some(lease) = clipboard.as_ref() {
        lease.verify_current()?;
    }
    if let Some(proof) = &proof {
        let erased = match proof {
            CandidateProof::Espanso(proof) => rawinput::erase(&s, &target, generation, proof),
        };
        if let Err(message) = erased {
            return Ok(ExecutionResult {
                id: "erase".into(),
                status: "failed".into(),
                message,
                completed_steps: 0,
                side_effects: SENT_EFFECT.with(|flag| flag.get()),
            });
        }
    }
    let mut owned = BTreeSet::new();
    let mut owned_mouse = vec![];
    let mut completed = 0;
    let started = Instant::now();
    let mut failure = None;
    for step in &plan.steps {
        if started.elapsed() > Duration::from_secs(120) {
            failure = Some("Execution timeout".into());
            break;
        }
        if let TargetPolicy::Declared { process } = &step.target {
            let now = foreground();
            if !now.process.eq_ignore_ascii_case(process) {
                failure = Some("Declared next target is not foreground".into());
                break;
            }
            if proof.is_some()
                && (now.id != target.id || !now.process.eq_ignore_ascii_case(&target.process))
            {
                failure = Some(
                    "Automatic shortcut action changed its declared target; stopped safely".into(),
                );
                break;
            }
            target = now;
        }
        if let Err(e) = guarded(&s, &target, generation) {
            failure = Some(e);
            break;
        }
        let result = (|| -> Result<(), String> {
            match &step.operation {
                Operation::Text {
                    text,
                    method: OutputMethod::Keys,
                } => {
                    unicode(text, &s, &target, generation)?;
                }
                Operation::Text {
                    method: OutputMethod::Paste,
                    ..
                }
                | Operation::Html { .. }
                | Operation::PasteFiles { .. }
                | Operation::PasteImage { .. } => {
                    if let Some(prepared) = payloads.get_mut(&completed) {
                        guarded(&s, &target, generation)?;
                        clipboard.as_mut().unwrap().write_prepared(prepared)?;
                    }
                    // Clipboard preparation/retry may take time: revalidate
                    // immediately before submitting the paste chord.
                    clipboard.as_ref().unwrap().verify_current()?;
                    guarded(&s, &target, generation)?;
                    hotkey(&[Key::named("CTRL"), Key::named("V")])?;
                    std::thread::sleep(Duration::from_millis(150));
                }
                Operation::KeyDown { key: k } => {
                    let v = vk(k)?;
                    if physical(v) {
                        return Err("Requested key is physically held".into());
                    }
                    key(v, false)?;
                    owned.insert(v);
                }
                Operation::KeyUp { key: k } => {
                    let v = vk(k)?;
                    release_owned(&mut owned, v, physical(v), || key(v, true))?;
                }
                Operation::KeyPress { key: k } => {
                    let v = vk(k)?;
                    if physical(v) {
                        return Err("Requested key is physically held".into());
                    }
                    press(v)?;
                }
                Operation::Hotkey { keys } => hotkey(keys)?,

                Operation::Delay { ms } => {
                    let until = Instant::now() + Duration::from_millis(*ms);
                    while Instant::now() < until {
                        if started.elapsed() > Duration::from_secs(120) {
                            return Err("Execution timeout".into());
                        }
                        guarded(&s, &target, generation)?;
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                Operation::MouseMove {
                    x,
                    y,
                    relative,
                    percent,
                } => {
                    if *relative {
                        mouse_input(MOUSEEVENTF_MOVE, 0, *x as i32, *y as i32)?
                    } else {
                        unsafe {
                            let (left, top, w, h) = (
                                GetSystemMetrics(SM_XVIRTUALSCREEN),
                                GetSystemMetrics(SM_YVIRTUALSCREEN),
                                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                                GetSystemMetrics(SM_CYVIRTUALSCREEN),
                            );
                            let (px, py) = if *percent {
                                (
                                    *x * (GetSystemMetrics(SM_CXSCREEN) - 1) as f64 / 100.0,
                                    *y * (GetSystemMetrics(SM_CYSCREEN) - 1) as f64 / 100.0,
                                )
                            } else {
                                (*x, *y)
                            };
                            mouse_input(
                                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                                0,
                                ((px - left as f64) * 65535.0 / (w - 1) as f64) as i32,
                                ((py - top as f64) * 65535.0 / (h - 1) as f64) as i32,
                            )?
                        }
                    }
                }
                Operation::MouseButton { button: b, state } => {
                    let physical_key = match b {
                        MouseButton::Left => VK_LBUTTON,
                        MouseButton::Right => VK_RBUTTON,
                        MouseButton::Middle => VK_MBUTTON,
                        MouseButton::X1 => VK_XBUTTON1,
                        MouseButton::X2 => VK_XBUTTON2,
                    };
                    if physical(physical_key) {
                        return Err("Physical mouse button conflicts with action".into());
                    }
                    button(*b, *state)?;
                    if *state == ButtonState::Down {
                        owned_mouse.push(*b)
                    }
                    if *state == ButtonState::Up {
                        owned_mouse.retain(|x| x != b)
                    }
                }
                Operation::Wheel { axis, amount, unit } => mouse_input(
                    if *axis == WheelAxis::Vertical {
                        MOUSEEVENTF_WHEEL
                    } else {
                        MOUSEEVENTF_HWHEEL
                    },
                    if *unit == WheelUnit::Clicks {
                        amount.saturating_mul(120)
                    } else {
                        *amount
                    } as u32,
                    0,
                    0,
                )?,
                Operation::Open {
                    target,
                    args,
                    working_directory,
                } => open(target, args, working_directory)?,
                Operation::Common { function } => {
                    hotkey(&common_keys(function)?)?;
                }
                Operation::Reusable { .. } => return Err("Unregistered reusable action".into()),
            }
            Ok(())
        })();
        match result {
            Ok(()) => completed += 1,
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    for v in owned {
        if !physical(v) {
            if let Err(error) = key(v, true) {
                failure.get_or_insert_with(|| format!("Owned key cleanup failed: {error}"));
            }
        }
    }
    for b in owned_mouse {
        let v = match b {
            MouseButton::Left => VK_LBUTTON,
            MouseButton::Right => VK_RBUTTON,
            MouseButton::Middle => VK_MBUTTON,
            MouseButton::X1 => VK_XBUTTON1,
            MouseButton::X2 => VK_XBUTTON2,
        };
        if !physical(v) {
            if let Err(error) = button(b, ButtonState::Up) {
                failure
                    .get_or_insert_with(|| format!("Owned mouse button cleanup failed: {error}"));
            }
        }
    }
    if failure.is_none() {
        if let Some(t) = terminator {
            if let Err(error) = guarded(&s, &target, generation).and_then(|_| send_terminator(&t)) {
                failure = Some(error);
            }
        }
    }
    if let Some(mut lease) = clipboard {
        if let Err(e) = lease.restore() {
            failure = Some(format!("Clipboard restore failed after input: {e}"));
        }
    }
    let result = ExecutionResult {
        id: format!("run-{}", s.generation.load(Ordering::SeqCst)),
        status: if failure.as_deref() == Some("Cancelled") {
            "cancelled"
        } else if failure.is_some() {
            "failed"
        } else {
            "unknown"
        }
        .into(),
        message: failure
            .unwrap_or_else(|| "Input was sent; target consumption cannot be confirmed".into()),
        completed_steps: completed,
        side_effects: SENT_EFFECT.with(|flag| flag.get()) || completed > 0,
    };
    *s.last.lock().unwrap() = Some(result.clone());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pause_blocks_only_automatic_actions() {
        assert!(pause_blocks_execution(true, true));
        assert!(!pause_blocks_execution(true, false));
        assert!(!pause_blocks_execution(false, true));
    }
    #[test]
    fn missing_file_rejected_before_executor() {
        let plan = ActionPlan {
            steps: vec![Step::new(
                Operation::PasteFiles {
                    paths: vec!["C:\\nonexistent-quicker-text-fixture\\missing.txt".into()],
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        };
        assert!(preflight(&plan).is_err());
    }
    #[test]
    fn unknown_native_key_fails_preflight() {
        let plan = ActionPlan {
            steps: vec![Step::new(
                Operation::KeyPress {
                    key: Key::named("CMD"),
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        };
        assert!(preflight(&plan).is_err());
    }
}
