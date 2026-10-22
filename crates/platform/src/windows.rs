use super::*;
use std::{
    collections::BTreeSet,
    mem::{size_of, zeroed},
    process::Command,
    sync::{mpsc, OnceLock},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{DataExchange::*, Memory::*, Threading::*},
    UI::{
        Input::{Ime::*, KeyboardAndMouse::*},
        Shell::*,
        WindowsAndMessaging::*,
    },
};
#[path = "windows_espanso.rs"]
mod espanso;
#[path = "windows_observer.rs"]
mod observer;
const MARK: usize = 0x51545854;
static HOOK_READY: AtomicBool = AtomicBool::new(false);
static LAST_GATE: OnceLock<Mutex<String>> = OnceLock::new();
struct RecordMeta {
    at: Instant,
    held: BTreeSet<Key>,
    buttons: Vec<MouseButton>,
    stopped: bool,
}
static RECORD_META: OnceLock<Mutex<RecordMeta>> = OnceLock::new();
fn record_event(shared: &Shared, operation: Operation) {
    let Ok(mut recorded) = shared.recording.try_lock() else {
        return;
    };
    let Some(rows) = recorded.as_mut() else {
        return;
    };
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(GetForegroundWindow(), &mut pid);
        if pid == GetCurrentProcessId() {
            return;
        }
    }
    let mut meta = RECORD_META
        .get_or_init(|| {
            Mutex::new(RecordMeta {
                at: Instant::now(),
                held: BTreeSet::new(),
                buttons: vec![],
                stopped: false,
            })
        })
        .lock()
        .unwrap();
    if rows.is_empty() {
        meta.at = Instant::now();
        meta.held.clear();
        meta.buttons.clear();
        meta.stopped = false;
    }
    if meta.stopped || rows.len() + 2 >= MAX_STEPS {
        return;
    }
    match &operation {
        Operation::KeyDown { key } => {
            if !meta.held.insert(key.clone()) {
                return;
            }
        }
        Operation::KeyUp { key } => {
            if !meta.held.remove(key) {
                return;
            }
        }
        Operation::MouseButton {
            button,
            state: ButtonState::Down,
        } => meta.buttons.push(*button),
        Operation::MouseButton {
            button,
            state: ButtonState::Up,
        } => {
            if !meta.buttons.contains(button) {
                return;
            }
            meta.buttons.retain(|b| b != button)
        }
        _ => {}
    }
    let delay = meta.at.elapsed().as_millis().min(MAX_DELAY_MS as u128) as u64;
    meta.at = Instant::now();
    if delay > 0 && !rows.is_empty() {
        rows.push(Step::new(
            Operation::Delay { ms: delay },
            SourceSpan::default(),
        ));
    }
    rows.push(Step::new(operation, SourceSpan::default()));
}
fn stop_record_events(shared: &Shared) {
    let mut recorded = shared.recording.lock().unwrap();
    if let Some(rows) = recorded.as_mut() {
        if let Some(meta) = RECORD_META.get() {
            let mut meta = meta.lock().unwrap();
            meta.stopped = true;
            let stop_keys = [
                VK_CONTROL,
                VK_LCONTROL,
                VK_RCONTROL,
                VK_SHIFT,
                VK_LSHIFT,
                VK_RSHIFT,
            ];
            rows.retain(|s|!matches!(&s.operation,Operation::KeyDown{key:Key::WindowsCode{code}} if stop_keys.contains(code)&&meta.held.contains(&Key::WindowsCode{code:*code})));
            for key in &meta.held {
                if !matches!(key,Key::WindowsCode{code}if stop_keys.contains(code)) {
                    rows.push(Step::new(
                        Operation::KeyUp { key: key.clone() },
                        SourceSpan::default(),
                    ));
                }
            }
            for button in &meta.buttons {
                rows.push(Step::new(
                    Operation::MouseButton {
                        button: *button,
                        state: ButtonState::Up,
                    },
                    SourceSpan::default(),
                ));
            }
        }
    }
}
pub fn finish_recording(shared: &Arc<Shared>) {
    stop_record_events(shared);
}
fn gate(reason: &str) {
    if let Some(g) = LAST_GATE.get() {
        if let Ok(mut state) = g.try_lock() {
            *state = reason.into();
        }
    }
}
pub fn input_status() -> serde_json::Value {
    let layout = unsafe {
        GetKeyboardLayout(GetWindowThreadProcessId(
            GetForegroundWindow(),
            std::ptr::null_mut(),
        )) as usize
    };
    serde_json::json!({"reason":LAST_GATE.get().map(|g|g.lock().unwrap().clone()).unwrap_or_else(||"Engine starting".into()),"hooks_ready":engine_ready(),"layout":format!("0x{:04X}",layout & 0xffff),"layout_hkl":format!("0x{layout:016X}"),"external_injected_keys":if test_input_enabled(){"explicit_test_mode"}else{"filtered"},"ime_active":ime_active(),"target_observation":observer::status(),"physical_input":espanso::status(),"espanso_input":espanso::status(),"automatic_trigger_modes":["espanso_rawinput_immediate","espanso_rawinput_space","espanso_rawinput_semicolon"],"terminator_capture":"space/semicolon post-input; enter/tab disabled"})
}
pub fn engine_ready() -> bool {
    HOOK_READY.load(Ordering::SeqCst)
}
pub fn native_ready() -> Result<(), String> {
    if !engine_ready() {
        return Err("Native keyboard/mouse hooks are unavailable".into());
    }
    Ok(())
}
static ENGINE: OnceLock<Arc<Shared>> = OnceLock::new();
static SENDER: OnceLock<mpsc::SyncSender<Candidate>> = OnceLock::new();
fn test_input_enabled() -> bool {
    acceptance_test_input_enabled()
}
unsafe fn expected_key_text(event: &KBDLLHOOKSTRUCT) -> Option<String> {
    if event.vkCode == VK_PACKET as u32 {
        return char::from_u32(event.scanCode).map(|ch| ch.to_string());
    }
    let mut state = [0u8; 256];
    if physical(VK_SHIFT) {
        state[VK_SHIFT as usize] = 0x80;
    }
    if GetKeyState(VK_CAPITAL as i32) & 1 != 0 {
        state[VK_CAPITAL as usize] = 1;
    }
    let thread = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
    let mut units = [0u16; 8];
    // This acceptance adapter preserves dead-key state; hardware translation
    // uses the vendored detector and its own upstream flags.
    let len = ToUnicodeEx(
        event.vkCode,
        event.scanCode,
        state.as_ptr(),
        units.as_mut_ptr(),
        units.len() as i32,
        4,
        GetKeyboardLayout(thread),
    );
    (len > 0 && len as usize <= units.len())
        .then(|| String::from_utf16(&units[..len as usize]).ok())
        .flatten()
}
static PHYSICAL: OnceLock<Mutex<BTreeSet<u16>>> = OnceLock::new();
struct Candidate {
    rule: Rule,
    matched: MatchResult,
    proof: CandidateProof,
    focus: Foreground,
    generation: u64,
    terminator: Option<String>,
}
#[derive(Clone)]
enum CandidateProof {
    Espanso(espanso::Proof),
}
fn enqueue(shared: &Shared, candidate: Candidate) -> bool {
    if shared
        .busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }
    shared.cancel.store(false, Ordering::SeqCst);
    if SENDER
        .get()
        .is_none_or(|sender| sender.try_send(candidate).is_err())
    {
        shared.busy.store(false, Ordering::SeqCst);
        return false;
    }
    true
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub fn cursor() -> serde_json::Value {
    unsafe {
        let mut p: POINT = zeroed();
        GetCursorPos(&mut p);
        serde_json::json!({"x":p.x,"y":p.y,"width":GetSystemMetrics(SM_CXVIRTUALSCREEN),"height":GetSystemMetrics(SM_CYVIRTUALSCREEN),"left":GetSystemMetrics(SM_XVIRTUALSCREEN),"top":GetSystemMetrics(SM_YVIRTUALSCREEN)})
    }
}
pub fn capture_selection() -> Result<String, String> {
    use std::{io::Read, os::windows::process::CommandExt, process::Stdio};
    let target = foreground();
    if password_control() {
        return Err("Password or inaccessible focused control cannot be read".into());
    }
    let script = r#"[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); Add-Type -AssemblyName UIAutomationClient; Add-Type -AssemblyName UIAutomationTypes; $element=[System.Windows.Automation.AutomationElement]::FocusedElement; if($null -eq $element -or $element.Current.IsPassword){exit 2}; try{$pattern=$element.GetCurrentPattern([System.Windows.Automation.TextPattern]::Pattern);$ranges=$pattern.GetSelection();$text=($ranges|ForEach-Object{$_.GetText(-1)}) -join '';[Console]::Write($text)}catch{exit 3}"#;
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x08000000)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Selection provider could not start")?;
    let pipe = child.stdout.take().ok_or("Selection pipe unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = vec![];
        pipe.take(1_048_577).read_to_end(&mut bytes).map(|_| bytes)
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("Selected text provider timed out; no clipboard was changed".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let bytes = reader
        .join()
        .map_err(|_| "Selection reader failed")?
        .map_err(|_| "Selection read failed")?;
    if !status.success() {
        return Err("Focused application does not expose selected text through UI Automation; clipboard unchanged".into());
    }
    if bytes.len() > 1_048_576 {
        return Err("Selection exceeds 1 MiB".into());
    }
    if foreground().id != target.id {
        return Err("Focus changed while reading selection".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "Selection provider returned invalid UTF-8")?;
    if text.is_empty() {
        return Err("No selected text is available".into());
    }
    Ok(text)
}
pub fn foreground() -> Foreground {
    unsafe {
        let h = GetForegroundWindow();
        let mut title = [0u16; 512];
        GetWindowTextW(h, title.as_mut_ptr(), 512);
        let mut pid = 0;
        GetWindowThreadProcessId(h, &mut pid);
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        let mut path = [0u16; 32768];
        let mut count = path.len() as u32;
        let process = if !p.is_null()
            && QueryFullProcessImageNameW(p, 0, path.as_mut_ptr(), &mut count) != 0
        {
            std::path::Path::new(&String::from_utf16_lossy(&path[..count as usize]))
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            String::new()
        };
        if !p.is_null() {
            CloseHandle(p);
        }
        Foreground {
            id: focus_handle(h) as u64,
            platform: "windows".into(),
            bundle_id: String::new(),
            process,
            title: String::from_utf16_lossy(
                &title[..title.iter().position(|x| *x == 0).unwrap_or(512)],
            ),
        }
    }
}
fn focus_handle(window: HWND) -> HWND {
    unsafe {
        let tid = GetWindowThreadProcessId(window, std::ptr::null_mut());
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(tid, &mut info) != 0 && !info.hwndFocus.is_null() {
            info.hwndFocus
        } else {
            window
        }
    }
}
fn physical(v: u16) -> bool {
    if let Some(set) = PHYSICAL.get() {
        let Ok(set) = set.try_lock() else {
            return true;
        };
        match v {
            VK_CONTROL => {
                set.contains(&VK_LCONTROL) || set.contains(&VK_RCONTROL) || set.contains(&v)
            }
            VK_SHIFT => set.contains(&VK_LSHIFT) || set.contains(&VK_RSHIFT) || set.contains(&v),
            VK_MENU => set.contains(&VK_LMENU) || set.contains(&VK_RMENU) || set.contains(&v),
            _ => set.contains(&v),
        }
    } else {
        unsafe { GetAsyncKeyState(v as i32) & i16::MIN != 0 }
    }
}
fn ime_active() -> bool {
    unsafe {
        let h = GetForegroundWindow();
        let ctx = ImmGetContext(h);
        if ctx.is_null() {
            return false;
        }
        let active = ImmGetOpenStatus(ctx) != 0;
        ImmReleaseContext(h, ctx);
        active
    }
}
fn password_control() -> bool {
    unsafe {
        let h = GetForegroundWindow();
        let tid = GetWindowThreadProcessId(h, std::ptr::null_mut());
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(tid, &mut info) == 0 {
            return true;
        }
        let mut cls = [0u16; 64];
        GetClassNameW(info.hwndFocus, cls.as_mut_ptr(), 64);
        let class =
            String::from_utf16_lossy(&cls[..cls.iter().position(|x| *x == 0).unwrap_or(64)]);
        class.eq_ignore_ascii_case("edit")
            && (GetWindowLongW(info.hwndFocus, GWL_STYLE) & 0x20) != 0
    }
}
fn native_edit_protection(class: &str, style: i32) -> Option<&'static str> {
    let class = class.to_ascii_lowercase();
    if class != "edit" && !class.starts_with("richedit") {
        return None;
    }
    if style & 0x20 != 0 {
        Some("Password controls are blocked")
    } else if style & 0x800 != 0 {
        Some("Read-only text control blocked")
    } else if style & 0x08000000 != 0 {
        Some("Disabled controls are blocked")
    } else {
        None
    }
}
fn native_input_protection() -> Option<String> {
    native_input_verdict().and_then(Result::err)
}
/// Only genuine native edit classes have styles that independently establish
/// password, enabled and editable state. Browser HWND styles prove nothing.
fn native_input_verdict() -> Option<Result<(), String>> {
    unsafe {
        let focus = focus_handle(GetForegroundWindow());
        let mut class = [0u16; 128];
        let len = GetClassNameW(focus, class.as_mut_ptr(), class.len() as i32);
        if len <= 0 {
            return None;
        }
        let class = String::from_utf16_lossy(&class[..len as usize]);
        if !native_edit_class(&class) {
            return None;
        }
        Some(
            match native_edit_protection(&class, GetWindowLongW(focus, GWL_STYLE)) {
                Some(reason) => Err(reason.to_owned()),
                None => Ok(()),
            },
        )
    }
}
fn native_edit_class(class: &str) -> bool {
    let class = class.to_ascii_lowercase();
    class == "edit" || class.starts_with("richedit")
}
unsafe extern "system" fn keyboard(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, w, l);
    }
    let event = &*(l as *const KBDLLHOOKSTRUCT);
    if event.dwExtraInfo == MARK {
        return CallNextHookEx(std::ptr::null_mut(), code, w, l);
    }
    let Some(shared) = ENGINE.get() else {
        return CallNextHookEx(std::ptr::null_mut(), code, w, l);
    };
    if event.flags & LLKHF_INJECTED != 0 && !test_input_enabled() {
        shared.generation.fetch_add(1, Ordering::SeqCst);
        observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
        gate("External simulated input filtered; matching invalidated");
        return CallNextHookEx(std::ptr::null_mut(), code, w, l);
    }
    let down = w as u32 == WM_KEYDOWN || w as u32 == WM_SYSKEYDOWN;
    if let Some(set) = PHYSICAL.get() {
        if let Ok(mut set) = set.try_lock() {
            if down {
                set.insert(event.vkCode as u16);
            } else {
                set.remove(&(event.vkCode as u16));
            }
        }
    }
    if down && event.vkCode == VK_ESCAPE as u32 && physical(VK_CONTROL) && physical(VK_SHIFT) {
        shared.cancel.store(true, Ordering::SeqCst);
        shared.generation.fetch_add(1, Ordering::SeqCst);
        observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
        stop_record_events(shared);
        return 1;
    }
    record_event(
        shared,
        if down {
            Operation::KeyDown {
                key: Key::WindowsCode {
                    code: event.vkCode as u16,
                },
            }
        } else {
            Operation::KeyUp {
                key: Key::WindowsCode {
                    code: event.vkCode as u16,
                },
            }
        },
    );
    if !down {
        return CallNextHookEx(std::ptr::null_mut(), code, w, l);
    }
    let generation = if event.flags & LLKHF_INJECTED != 0 || shared.busy.load(Ordering::SeqCst) {
        shared.generation.fetch_add(1, Ordering::SeqCst) + 1
    } else {
        shared.generation.load(Ordering::SeqCst)
    };
    // Only explicit acceptance mode adapts external SendInput into the dispatcher.
    if event.flags & LLKHF_INJECTED != 0 && test_input_enabled() {
        espanso::acceptance_key(shared, event, generation);
    }
    gate(if shared.paused.load(Ordering::SeqCst) {
        "Paused"
    } else {
        "Espanso RawInput capture active"
    });

    CallNextHookEx(std::ptr::null_mut(), code, w, l)
}
unsafe extern "system" fn mouse(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        let ev = &*(l as *const MSLLHOOKSTRUCT);
        if ev.dwExtraInfo != MARK {
            if let Some(s) = ENGINE.get() {
                if let Some(set) = PHYSICAL.get() {
                    let mut set = set.lock().unwrap();
                    match w as u32 {
                        WM_LBUTTONDOWN => {
                            set.insert(VK_LBUTTON);
                        }
                        WM_LBUTTONUP => {
                            set.remove(&VK_LBUTTON);
                        }
                        WM_RBUTTONDOWN => {
                            set.insert(VK_RBUTTON);
                        }
                        WM_RBUTTONUP => {
                            set.remove(&VK_RBUTTON);
                        }
                        WM_MBUTTONDOWN => {
                            set.insert(VK_MBUTTON);
                        }
                        WM_MBUTTONUP => {
                            set.remove(&VK_MBUTTON);
                        }
                        WM_XBUTTONDOWN => {
                            set.insert(if ev.mouseData >> 16 == 1 {
                                VK_XBUTTON1
                            } else {
                                VK_XBUTTON2
                            });
                        }
                        WM_XBUTTONUP => {
                            set.remove(&if ev.mouseData >> 16 == 1 {
                                VK_XBUTTON1
                            } else {
                                VK_XBUTTON2
                            });
                        }
                        _ => {}
                    }
                }
                // Pointer movement alone cannot move the caret; only clicks and
                // wheel reset the typed-key buffer (as in espanso).
                if w as u32 != WM_MOUSEMOVE {
                    s.generation.fetch_add(1, Ordering::SeqCst);
                    observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
                }
                {
                    let op = match w as u32 {
                        WM_MOUSEMOVE => Some(Operation::MouseMove {
                            x: ev.pt.x as f64,
                            y: ev.pt.y as f64,
                            relative: false,
                            percent: false,
                        }),
                        WM_LBUTTONDOWN => Some(Operation::MouseButton {
                            button: MouseButton::Left,
                            state: ButtonState::Down,
                        }),
                        WM_LBUTTONUP => Some(Operation::MouseButton {
                            button: MouseButton::Left,
                            state: ButtonState::Up,
                        }),
                        WM_RBUTTONDOWN => Some(Operation::MouseButton {
                            button: MouseButton::Right,
                            state: ButtonState::Down,
                        }),
                        WM_RBUTTONUP => Some(Operation::MouseButton {
                            button: MouseButton::Right,
                            state: ButtonState::Up,
                        }),
                        WM_MBUTTONDOWN => Some(Operation::MouseButton {
                            button: MouseButton::Middle,
                            state: ButtonState::Down,
                        }),
                        WM_MBUTTONUP => Some(Operation::MouseButton {
                            button: MouseButton::Middle,
                            state: ButtonState::Up,
                        }),
                        WM_XBUTTONDOWN => Some(Operation::MouseButton {
                            button: if ev.mouseData >> 16 == 1 {
                                MouseButton::X1
                            } else {
                                MouseButton::X2
                            },
                            state: ButtonState::Down,
                        }),
                        WM_XBUTTONUP => Some(Operation::MouseButton {
                            button: if ev.mouseData >> 16 == 1 {
                                MouseButton::X1
                            } else {
                                MouseButton::X2
                            },
                            state: ButtonState::Up,
                        }),
                        WM_MOUSEWHEEL => Some(Operation::Wheel {
                            axis: WheelAxis::Vertical,
                            amount: (ev.mouseData >> 16) as i16 as i32,
                            unit: WheelUnit::Delta,
                        }),
                        WM_MOUSEHWHEEL => Some(Operation::Wheel {
                            axis: WheelAxis::Horizontal,
                            amount: (ev.mouseData >> 16) as i16 as i32,
                            unit: WheelUnit::Delta,
                        }),
                        _ => None,
                    };
                    if let Some(op) = op {
                        let hovered = WindowFromPoint(ev.pt);
                        let mut pid = 0;
                        GetWindowThreadProcessId(hovered, &mut pid);
                        if pid != GetCurrentProcessId() {
                            record_event(s, op);
                        }
                    }
                }
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, w, l)
}
unsafe extern "system" fn focus_event(
    _: windows_sys::Win32::UI::Accessibility::HWINEVENTHOOK,
    _: u32,
    window: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    // WinEvent hooks are global. Background accessibility focus notifications
    // must not invalidate the active foreground transaction.
    let foreground = GetForegroundWindow();
    if window.is_null()
        || foreground.is_null()
        || GetAncestor(window, GA_ROOT) != GetAncestor(foreground, GA_ROOT)
    {
        return;
    }
    observer::FOCUS_EPOCH.fetch_add(1, Ordering::SeqCst);
    observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
}
pub fn start_engine(shared: Arc<Shared>) -> Result<(), String> {
    LAST_GATE.set(Mutex::new("后台默认暂停".into())).ok();
    ENGINE
        .set(shared.clone())
        .map_err(|_| "Engine already started")?;
    let initial = (1u16..=255)
        .filter(|v| unsafe { GetAsyncKeyState(*v as i32) & i16::MIN != 0 })
        .collect();
    PHYSICAL.set(Mutex::new(initial)).ok();
    let (tx, rx) = mpsc::sync_channel::<Candidate>(1);
    SENDER.set(tx).ok();
    std::thread::spawn(move || {
        for c in rx {
            let mut ctx = VariableContext::default();
            ctx.clipboard = arboard::Clipboard::new()
                .ok()
                .and_then(|mut cb| cb.get_text().ok());
            let selection_needed = serde_json::to_string(&c.rule.action)
                .unwrap_or_default()
                .contains("{selection}")
                || matches!(c.rule.action, Action::Reusable { .. })
                    && serde_json::to_string(&*shared.config.lock().unwrap())
                        .unwrap_or_default()
                        .contains("{selection}");
            if selection_needed {
                ctx.selection = capture_selection().ok();
            }
            ctx.parameters.insert(
                "action_parameter".into(),
                c.matched
                    .parameter
                    .clone()
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
            let config = shared.config.lock().unwrap().clone();
            let plan = expand_reusable(&c.rule.action, &config, &ctx);
            let result = match plan {
                Ok(plan) => execute_inner(
                    shared.clone(),
                    plan,
                    c.focus.clone(),
                    c.generation,
                    Some(c.proof.clone()),
                    if c.matched.consume_terminator {
                        None
                    } else {
                        c.terminator.clone()
                    },
                    match c.rule.action {
                        Action::Replay { speed, .. } => speed,
                        _ => 1.0,
                    },
                ),
                Err(errors) => Err(errors
                    .iter()
                    .map(|e| e.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ")),
            };
            if let Ok(completed) = &result {
                let mut result = completed.clone();
                result.id = format!("{}:{}", c.rule.id, c.generation);
                *shared.last.lock().unwrap() = Some(result);
            }
            if let Err(message) = result {
                *shared.last.lock().unwrap() = Some(ExecutionResult {
                    id: format!("{}:{}", c.rule.id, c.generation),
                    status: "failed".into(),
                    message,
                    completed_steps: 0,
                    side_effects: false,
                })
            }
            shared.busy.store(false, Ordering::SeqCst);
        }
    });
    // Independent protection sampling is started by the RawInput bridge.
    espanso::start(ENGINE.get().unwrap().clone())?;
    std::thread::spawn(|| unsafe {
        let focus_hook = windows_sys::Win32::UI::Accessibility::SetWinEventHook(
            EVENT_OBJECT_FOCUS,
            EVENT_OBJECT_FOCUS,
            std::ptr::null_mut(),
            Some(focus_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        let keyboard_hook =
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), std::ptr::null_mut(), 0);
        let mouse_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), std::ptr::null_mut(), 0);
        if keyboard_hook.is_null() || mouse_hook.is_null() || focus_hook.is_null() {
            if !keyboard_hook.is_null() {
                UnhookWindowsHookEx(keyboard_hook);
            }
            if !mouse_hook.is_null() {
                UnhookWindowsHookEx(mouse_hook);
            }
            if !focus_hook.is_null() {
                windows_sys::Win32::UI::Accessibility::UnhookWinEvent(focus_hook);
            }
            if let Some(s) = ENGINE.get() {
                s.paused.store(true, Ordering::SeqCst);
                *s.last.lock().unwrap() = Some(ExecutionResult {
                    id: "engine".into(),
                    status: "failed".into(),
                    message: "Windows keyboard/mouse hook installation failed; automatic input remains paused".into(),
                    completed_steps: 0,
                    side_effects: false,
                })
            }
            return;
        }
        HOOK_READY.store(true, Ordering::SeqCst);
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(keyboard_hook);
        windows_sys::Win32::UI::Accessibility::UnhookWinEvent(focus_hook);
        if !mouse_hook.is_null() {
            UnhookWindowsHookEx(mouse_hook);
        }
    });
    Ok(())
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
fn record_send_result(accepted: u32, requested: u32) -> Result<(), String> {
    if accepted > 0 {
        mark_external_effect();
    }
    if accepted != requested {
        return Err(format!("SendInput accepted {accepted}/{requested} events (possibly elevated target / UIPI); partial effects and target consumption unknown"));
    }
    Ok(())
}
fn mark_external_effect() {
    SENT_EFFECT.with(|flag| flag.set(true));
}
fn release_owned(
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
fn key(v: u16, up: bool) -> Result<(), String> {
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
fn press(v: u16) -> Result<(), String> {
    espanso::press_keys(&[v])
}

fn send_terminator(t: &str) -> Result<(), String> {
    if t == "semicolon" {
        return espanso::text(";");
    }
    press(match t {
        "space" => VK_SPACE,
        "tab" => VK_TAB,
        "enter" => VK_RETURN,
        _ => return Ok(()),
    })
}
fn unicode(text: &str, s: &Shared, target: &Foreground, generation: u64) -> Result<(), String> {
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
            _ => espanso::text(&ch.to_string())?,
        }
    }
    Ok(())
}

fn vk(k: &Key) -> Result<u16, String> {
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
fn hotkey(keys: &[Key]) -> Result<(), String> {
    let values = keys.iter().map(vk).collect::<Result<Vec<_>, _>>()?;
    if values.iter().any(|v| physical(*v)) {
        return Err("Physical key conflicts with requested hotkey; action stopped".into());
    }
    espanso::combination(&values)
}

fn mouse_input(flags: u32, data: u32, x: i32, y: i32) -> Result<(), String> {
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
fn button(button: MouseButton, state: ButtonState) -> Result<(), String> {
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
thread_local! {
    static SENT_EFFECT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static AUTOMATIC_EXECUTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
struct FenceLease;
impl Drop for FenceLease {
    fn drop(&mut self) {
        espanso::install(None);
        AUTOMATIC_EXECUTION.with(|automatic| automatic.set(false));
    }
}
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
    espanso::guard(s)?;
    if let Some(error) = native_input_protection().or_else(espanso::protection_block) {
        return Err(error);
    }
    Ok(())
}
fn modifiers_held() -> bool {
    physical(VK_CONTROL)
        || physical(VK_MENU)
        || physical(VK_SHIFT)
        || physical(VK_LWIN)
        || physical(VK_RWIN)
}
fn guarded(s: &Shared, target: &Foreground, generation: u64) -> Result<(), String> {
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
fn clipboard_operation(operation: &Operation) -> bool {
    matches!(
        operation,
        Operation::Text {
            method: OutputMethod::Paste,
            ..
        } | Operation::Html { .. }
            | Operation::PasteFiles { .. }
            | Operation::PasteImage { .. }
    )
}

fn acquire_clipboard<T>(
    plan: &ActionPlan,
    acquire: impl FnOnce() -> Result<T, String>,
) -> Result<Option<T>, String> {
    if plan
        .steps
        .iter()
        .any(|step| clipboard_operation(&step.operation))
    {
        // Output method is a user contract. Never turn a failed Paste into
        // typed keys (especially multiline text that could submit a chat).
        acquire().map(Some)
    } else {
        Ok(None)
    }
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
fn execute_inner(
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
    espanso::install(proof.as_ref().map(|proof| match proof {
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
            CandidateProof::Espanso(proof) => espanso::erase(&s, &target, generation, proof),
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
fn html_clipboard(fragment: &str) -> String {
    let prefix = "<html><body><!--StartFragment-->";
    let suffix = "<!--EndFragment--></body></html>";
    let head="Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
    let start = head.len();
    let start_fragment = start + prefix.len();
    format!("Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n{}{}{}\0",start,start+prefix.len()+fragment.len()+suffix.len(),start_fragment,start_fragment+fragment.len(),prefix,fragment,suffix)
}
fn clipboard_formats(op: &Operation) -> Result<Option<Vec<(u32, Vec<u8>)>>, String> {
    Ok(Some(match op {
        Operation::Text {
            text,
            method: OutputMethod::Paste,
        } => vec![(
            13,
            wide(text).iter().flat_map(|v| v.to_le_bytes()).collect(),
        )],
        Operation::Html { html, plain_text } => vec![
            (
                unsafe { RegisterClipboardFormatW(wide("HTML Format").as_ptr()) },
                html_clipboard(html).into_bytes(),
            ),
            (
                13,
                wide(plain_text)
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect(),
            ),
        ],
        Operation::PasteFiles { paths } => {
            let mut b = vec![0u8; 20];
            b[..4].copy_from_slice(&20u32.to_le_bytes());
            b[16..20].copy_from_slice(&1u32.to_le_bytes());
            for p in paths {
                for v in wide(p) {
                    b.extend(v.to_le_bytes())
                }
            }
            b.extend([0, 0]);
            vec![(15, b)]
        }
        Operation::PasteImage { path } => {
            let img = image::open(path).map_err(|e| e.to_string())?.to_rgba8();
            let (w, h) = img.dimensions();
            let mut b = vec![0u8; 40];
            b[..4].copy_from_slice(&40u32.to_le_bytes());
            b[4..8].copy_from_slice(&(w as i32).to_le_bytes());
            b[8..12].copy_from_slice(&(-(h as i32)).to_le_bytes());
            b[12..14].copy_from_slice(&1u16.to_le_bytes());
            b[14..16].copy_from_slice(&32u16.to_le_bytes());
            for p in img.pixels() {
                b.extend([p[2], p[1], p[0], p[3]])
            }
            vec![(8, b)]
        }
        _ => return Ok(None),
    }))
}
struct PreparedClipboard {
    handles: Vec<(u32, HGLOBAL)>,
    expected: Vec<(u32, Vec<u8>)>,
}
impl PreparedClipboard {
    fn new(formats: Vec<(u32, Vec<u8>)>) -> Result<Self, String> {
        let mut result = Self {
            handles: vec![],
            expected: vec![],
        };
        for (f, b) in formats {
            unsafe {
                let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, b.len());
                if h.is_null() {
                    return Err("Clipboard allocation failed before deletion".into());
                }
                let p = GlobalLock(h);
                if p.is_null() {
                    GlobalFree(h);
                    return Err("Clipboard allocation lock failed before deletion".into());
                }
                std::ptr::copy_nonoverlapping(b.as_ptr(), p as *mut u8, b.len());
                result.expected.push((
                    f,
                    std::slice::from_raw_parts(p as *const u8, GlobalSize(h)).to_vec(),
                ));
                GlobalUnlock(h);
                result.handles.push((f, h));
            }
        }
        Ok(result)
    }
}
impl Drop for PreparedClipboard {
    fn drop(&mut self) {
        for (_, h) in self.handles.drain(..) {
            unsafe {
                GlobalFree(h);
            }
        }
    }
}
struct ClipboardLease {
    backup: Vec<(u32, Vec<u8>)>,
    owned_payload: Vec<(u32, Vec<u8>)>,
    written: Option<u32>,
    owner: HWND,
    dirty: bool,
}
fn finish_clipboard_restore<T>(
    backup: &mut Vec<T>,
    result: Result<(), String>,
) -> Result<(), String> {
    result?;
    backup.clear();
    Ok(())
}
fn retry_clipboard_verification<T>(
    mut attempt: impl FnMut() -> Result<Option<T>, String>,
    mut pause: impl FnMut(),
) -> Result<T, String> {
    for _ in 0..8 {
        match attempt()? {
            Some(verified) => return Ok(verified),
            None => pause(),
        }
    }
    Err("Clipboard ownership could not be verified within the retry limit".into())
}
impl ClipboardLease {
    unsafe fn verified_sequence(
        &self,
        owner: HWND,
        expected: &[(u32, Vec<u8>)],
        exact_formats: bool,
    ) -> Result<u32, String> {
        retry_clipboard_verification(
            || {
                if OpenClipboard(self.owner) == 0 {
                    return Ok(None);
                }
                let verified: Result<u32, String> = (|| {
                    if GetClipboardOwner() != owner {
                        return Err(
                            "Clipboard owner changed externally; clipboard preserved".into()
                        );
                    }
                    if exact_formats {
                        let mut formats = Vec::new();
                        let mut format = 0;
                        loop {
                            format = EnumClipboardFormats(format);
                            if format == 0 {
                                break;
                            }
                            formats.push(format);
                        }
                        if formats.len() != expected.len()
                            || formats
                                .iter()
                                .any(|format| !expected.iter().any(|(wanted, _)| wanted == format))
                        {
                            return Err(
                                "Clipboard formats changed externally; clipboard preserved".into(),
                            );
                        }
                    }
                    for (format, bytes) in expected {
                        let handle = GetClipboardData(*format);
                        if handle.is_null() || GlobalSize(handle) != bytes.len() {
                            return Err(
                                "Clipboard payload changed externally; clipboard preserved".into(),
                            );
                        }
                        let pointer = GlobalLock(handle);
                        if pointer.is_null() {
                            return Err("Clipboard payload cannot be verified".into());
                        }
                        let matches =
                            std::slice::from_raw_parts(pointer as *const u8, bytes.len()) == bytes;
                        GlobalUnlock(handle);
                        if !matches {
                            return Err(
                                "Clipboard payload changed externally; clipboard preserved".into(),
                            );
                        }
                    }
                    Ok(GetClipboardSequenceNumber())
                })();
                CloseClipboard();
                let sequence = verified?;
                if GetClipboardSequenceNumber() == sequence && GetClipboardOwner() == owner {
                    return Ok(Some(sequence));
                }
                Ok(None)
            },
            || std::thread::sleep(Duration::from_millis(5)),
        )
    }
    fn verify_current(&self) -> Result<(), String> {
        if self
            .written
            .is_none_or(|seq| unsafe { GetClipboardSequenceNumber() } != seq)
        {
            return Err("Clipboard changed externally before paste; no paste was sent".into());
        }
        Ok(())
    }
    fn write_prepared(&mut self, prepared: &mut PreparedClipboard) -> Result<(), String> {
        unsafe {
            if OpenClipboard(self.owner) == 0 {
                return Err("Clipboard is busy".into());
            }
            let result: Result<(), String> = (|| {
                if self
                    .written
                    .is_some_and(|seq| GetClipboardSequenceNumber() != seq)
                {
                    return Err(
                        "Clipboard changed externally; stopped without overwriting it".into(),
                    );
                }
                if EmptyClipboard() == 0 {
                    return Err("Clipboard clear failed".into());
                }
                self.dirty = true;
                self.owned_payload.clear();
                mark_external_effect();
                self.written = Some(GetClipboardSequenceNumber());
                while !prepared.handles.is_empty() {
                    let (f, h) = prepared.handles.remove(0);
                    if SetClipboardData(f, h).is_null() {
                        GlobalFree(h);
                        self.written = Some(GetClipboardSequenceNumber());
                        return Err("Clipboard write failed".into());
                    }
                    if let Some((_, bytes)) =
                        prepared.expected.iter().find(|(format, _)| *format == f)
                    {
                        self.owned_payload.push((f, bytes.clone()));
                    }
                }
                self.written = Some(GetClipboardSequenceNumber());
                Ok(())
            })();
            CloseClipboard();
            result?;
            self.written = Some(self.verified_sequence(self.owner, &prepared.expected, false)?);
            Ok(())
        }
    }
    fn new() -> Result<Self, String> {
        unsafe {
            let owner = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide("QuickerText Clipboard").as_ptr(),
                0,
                0,
                0,
                0,
                0,
                (-3isize) as HWND,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if owner.is_null() {
                return Err("Clipboard owner window creation failed".into());
            }
            if OpenClipboard(owner) == 0 {
                DestroyWindow(owner);
                return Err("Clipboard is busy; trigger preserved".into());
            }
            let original_owner = GetClipboardOwner();
            let result = (|| {
                let mut backup = vec![];
                let mut format = 0;
                loop {
                    format = EnumClipboardFormats(format);
                    if format == 0 {
                        break;
                    }
                    if matches!(format, 2 | 3 | 9 | 14 | 0x80..=0x8f) {
                        return Err("Existing clipboard contains handle-based rich formats that cannot be safely restored; trigger preserved".into());
                    }
                    let h = GetClipboardData(format);
                    let n = GlobalSize(h);
                    if n == 0 || n > 32 * 1024 * 1024 {
                        return Err(
                            "Clipboard format cannot be safely backed up; trigger preserved".into(),
                        );
                    }
                    let p = GlobalLock(h);
                    if p.is_null() {
                        return Err("Clipboard backup failed".into());
                    }
                    let bytes = std::slice::from_raw_parts(p as *const u8, n).to_vec();
                    GlobalUnlock(h);
                    backup.push((format, bytes));
                }
                Ok(Self {
                    backup,
                    owned_payload: vec![],
                    written: Some(GetClipboardSequenceNumber()),
                    owner,
                    dirty: false,
                })
            })();
            CloseClipboard();
            match result {
                Ok(mut lease) => match lease.verified_sequence(original_owner, &lease.backup, true)
                {
                    Ok(sequence) => {
                        lease.written = Some(sequence);
                        Ok(lease)
                    }
                    Err(reason) => {
                        lease.written = None;
                        Err(reason)
                    }
                },
                Err(reason) => {
                    DestroyWindow(owner);
                    Err(reason)
                }
            }
        }
    }
    fn write(&mut self, formats: Vec<(u32, Vec<u8>)>) -> Result<(), String> {
        unsafe {
            if OpenClipboard(self.owner) == 0 {
                return Err("Clipboard is busy".into());
            }
            let result = (|| {
                if let Some(seq) = self.written {
                    if GetClipboardSequenceNumber() != seq {
                        return Err(
                            "Clipboard changed externally; stopped without overwriting it".into(),
                        );
                    }
                }
                if EmptyClipboard() == 0 {
                    return Err("Clipboard clear failed".into());
                }
                mark_external_effect();
                self.owned_payload.clear();
                self.written = Some(GetClipboardSequenceNumber());
                for (format, bytes) in formats {
                    let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len());
                    if h.is_null() {
                        return Err("Clipboard allocation failed".into());
                    }
                    let p = GlobalLock(h);
                    if p.is_null() {
                        GlobalFree(h);
                        return Err("Clipboard lock failed".into());
                    }
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
                    let expected =
                        std::slice::from_raw_parts(p as *const u8, GlobalSize(h)).to_vec();
                    GlobalUnlock(h);
                    if SetClipboardData(format, h).is_null() {
                        GlobalFree(h);
                        return Err("Clipboard write failed".into());
                    }
                    self.owned_payload.push((format, expected));
                }
                self.written = Some(GetClipboardSequenceNumber());
                Ok(())
            })();
            CloseClipboard();
            result
        }
    }
    fn restore(&mut self) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        unsafe {
            if GetClipboardOwner() != self.owner {
                return Ok(());
            }
            // Closing the clipboard or a listener requesting a synthesized
            // format can advance the sequence after our writes. Re-establish
            // ownership from the exact payload under a lock; never adopt a new
            // sequence solely because the previous one is no longer current.
            self.written = Some(self.verified_sequence(self.owner, &self.owned_payload, false)?);
        }
        let result = self.write(self.backup.clone());
        finish_clipboard_restore(&mut self.backup, result)?;
        self.written = None;
        self.dirty = false;
        self.owned_payload.clear();
        Ok(())
    }
}
impl Drop for ClipboardLease {
    fn drop(&mut self) {
        let _ = self.restore();
        unsafe {
            DestroyWindow(self.owner);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_protection_covers_edit_and_richedit_without_misreading_browser_styles() {
        for class in ["Edit", "RICHEDIT50W", "RichEdit20A"] {
            assert_eq!(
                native_edit_protection(class, 0x20),
                Some("Password controls are blocked")
            );
            assert_eq!(
                native_edit_protection(class, 0x800),
                Some("Read-only text control blocked")
            );
            assert_eq!(
                native_edit_protection(class, 0x08000000),
                Some("Disabled controls are blocked")
            );
            assert_eq!(native_edit_protection(class, 0), None);
        }
        assert_eq!(
            native_edit_protection("Chrome_RenderWidgetHostHWND", 0x820),
            None
        );
    }
    #[test]
    fn pause_blocks_only_automatic_actions() {
        assert!(pause_blocks_execution(true, true));
        assert!(!pause_blocks_execution(true, false));
        assert!(!pause_blocks_execution(false, true));
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
    fn busy_clipboard_restore_preserves_original_payload_for_drop_retry() {
        let original = vec![(13u32, vec![b'o', 0, b'l', 0, b'd', 0, 0, 0])];
        let mut backup = original.clone();
        assert!(finish_clipboard_restore(&mut backup, Err("clipboard busy".into())).is_err());
        assert_eq!(backup, original);
        let retried_payload = backup.clone();
        finish_clipboard_restore(&mut backup, Ok(())).unwrap();
        assert_eq!(retried_payload, original);
        assert!(backup.is_empty());
    }
    #[test]
    fn clipboard_listener_busy_retry_accepts_later_verified_owned_sequence() {
        let mut attempts = 0;
        let mut pauses = 0;
        let sequence = retry_clipboard_verification(
            || {
                attempts += 1;
                Ok(if attempts < 3 { None } else { Some(42u32) })
            },
            || pauses += 1,
        )
        .unwrap();
        assert_eq!(sequence, 42);
        assert_eq!(attempts, 3);
        assert_eq!(pauses, 2);
    }
    #[test]
    fn clipboard_retry_never_adopts_changed_owner_or_payload() {
        for reason in ["external owner", "modified payload"] {
            let mut attempts = 0;
            let result = retry_clipboard_verification::<u32>(
                || {
                    attempts += 1;
                    if attempts == 1 {
                        Ok(None)
                    } else {
                        Err(reason.into())
                    }
                },
                || {},
            );
            assert_eq!(result, Err(reason.into()));
            assert_eq!(attempts, 2);
        }
        let mut attempts = 0;
        assert!(retry_clipboard_verification::<u32>(
            || {
                attempts += 1;
                Ok(None)
            },
            || {}
        )
        .is_err());
        assert_eq!(attempts, 8);
    }
    #[test]
    fn clipboard_clear_is_effect_even_when_no_input_was_accepted() {
        SENT_EFFECT.with(|flag| flag.set(false));
        assert!(record_send_result(0, 2).is_err());
        mark_external_effect(); // EmptyClipboard succeeded before SetClipboardData failed.
        assert!(SENT_EFFECT.with(|flag| flag.get()));
        SENT_EFFECT.with(|flag| flag.set(false));
    }
    #[test]
    fn unavailable_clipboard_rejects_paste_before_erasure_without_downgrade() {
        let plan = ActionPlan {
            steps: vec![Step::new(
                Operation::Text {
                    text: "中文\nemoji🙂".into(),
                    method: OutputMethod::Paste,
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        };
        SENT_EFFECT.with(|flag| flag.set(false));
        let result = acquire_clipboard::<()>(&plan, || {
            Err("Existing clipboard has an unsupported handle format".into())
        });
        assert!(result.is_err());
        assert!(matches!(&plan.steps[0].operation,
            Operation::Text { text, method: OutputMethod::Paste } if text == "中文\nemoji🙂"));
        assert!(!SENT_EFFECT.with(|flag| flag.get()));
    }
    #[test]
    fn restorable_clipboard_keeps_paste_and_rich_payload_never_silently_becomes_text() {
        let mut plan = ActionPlan {
            steps: vec![Step::new(
                Operation::Text {
                    text: "plain".into(),
                    method: OutputMethod::Paste,
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        };
        assert_eq!(acquire_clipboard(&plan, || Ok(42)).unwrap(), Some(42));
        assert!(matches!(
            plan.steps[0].operation,
            Operation::Text {
                method: OutputMethod::Paste,
                ..
            }
        ));
        plan.steps.push(Step::new(
            Operation::Html {
                html: "<b>rich</b>".into(),
                plain_text: "rich".into(),
            },
            SourceSpan::default(),
        ));
        assert!(acquire_clipboard::<()>(&plan, || Err("cannot preserve".into())).is_err());
        assert!(matches!(
            plan.steps[0].operation,
            Operation::Text {
                method: OutputMethod::Paste,
                ..
            }
        ));
    }
    #[test]
    fn key_output_never_acquires_or_writes_clipboard() {
        let plan = ActionPlan {
            steps: vec![Step::new(
                Operation::Text {
                    text: "plain".into(),
                    method: OutputMethod::Keys,
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        };
        let lease =
            acquire_clipboard::<()>(&plan, || panic!("key output must leave clipboard alone"))
                .unwrap();
        assert!(lease.is_none());
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
    fn clipboard_objects_are_native_formats() {
        let payload = clipboard_formats(&Operation::PasteFiles {
            paths: vec!["C:\\fixture.txt".into()],
        })
        .unwrap()
        .unwrap();
        assert_eq!(payload[0].0, 15);
        assert_eq!(&payload[0].1[..4], &20u32.to_le_bytes());
        assert_eq!(&payload[0].1[16..20], &1u32.to_le_bytes());
        let html = html_clipboard("😀 <b>合成</b>");
        let offset = html.find("StartFragment:").unwrap() + 14;
        let start: usize = html[offset..offset + 10].parse().unwrap();
        assert!(html[start..].starts_with("😀 <b>合成</b>"));
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
