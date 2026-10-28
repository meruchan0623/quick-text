//! macOS native adapter. One suppressible event tap feeds the shared Rust matcher.
//! Event callbacks do not read files, run actions, or query Accessibility.
// macOS stays experimental (ADR-f); its unwrap/expect are exempt from the 0.4
// lint ramp until the backend is reworked.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::health::{PauseReasons, TriggerKind};
use crate::history::Record;
use crate::macos_rules::{Backup, Stop};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{c_char, c_void, CString},
    process::Command,
    ptr,
    sync::{mpsc, OnceLock},
    time::{Duration, Instant},
};
type Id = *mut c_void;
type CF = *const c_void;
type Event = *mut c_void;
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Size {
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    origin: Point,
    size: Size,
}
type Callback = unsafe extern "C" fn(Id, u32, Event, Id) -> Event;
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> CF;
    fn AXUIElementCopyAttributeValue(element: CF, attribute: CF, value: *mut CF) -> i32;
    fn AXUIElementSetMessagingTimeout(element: CF, seconds: f32) -> i32;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGEventTapCreate(
        location: u32,
        placement: u32,
        options: u32,
        mask: u64,
        callback: Callback,
        user: Id,
    ) -> Id;
    fn CGEventTapEnable(tap: Id, enable: bool);
    fn CGEventCreate(source: CF) -> Event;
    fn CGEventCreateKeyboardEvent(source: CF, key: u16, down: bool) -> Event;
    fn CGEventCreateMouseEvent(source: CF, kind: u32, point: Point, button: u32) -> Event;
    fn CGEventCreateScrollWheelEvent(source: CF, unit: u32, count: u32, ...) -> Event;
    fn CGEventKeyboardSetUnicodeString(event: Event, length: usize, text: *const u16);
    fn CGEventKeyboardGetUnicodeString(event: Event, max: usize, count: *mut usize, text: *mut u16);
    fn CGEventGetIntegerValueField(event: Event, field: u32) -> i64;
    fn CGEventSetIntegerValueField(event: Event, field: u32, value: i64);
    fn CGEventGetFlags(event: Event) -> u64;
    fn CGEventSetFlags(event: Event, flags: u64);
    fn CGEventGetLocation(event: Event) -> Point;
    fn CGEventPost(location: u32, event: Event);
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> Rect;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: CF);
    fn CFRetain(value: CF) -> CF;
    fn CFHash(value: CF) -> usize;
    fn CFEqual(a: CF, b: CF) -> bool;
    fn CFStringCreateWithBytes(
        allocator: CF,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external: bool,
    ) -> CF;
    fn CFStringGetCString(value: CF, bytes: *mut c_char, size: isize, encoding: u32) -> bool;
    fn CFStringGetLength(value: CF) -> isize;
    fn CFMachPortCreateRunLoopSource(allocator: CF, port: Id, order: isize) -> CF;
    fn CFMachPortInvalidate(port: Id);
    fn CFRunLoopGetCurrent() -> CF;
    fn CFRunLoopAddSource(runloop: CF, source: CF, mode: CF);
    fn CFRunLoopRemoveSource(runloop: CF, source: CF, mode: CF);
    fn CFRunLoopRunInMode(mode: CF, seconds: f64, return_after_source: bool) -> i32;
    static kCFRunLoopDefaultMode: CF;
}
#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
    fn TISCopyCurrentKeyboardInputSource() -> CF;
    fn TISGetInputSourceProperty(source: CF, property: CF) -> CF;
    static kTISPropertyInputSourceType: CF;
    static kTISPropertyInputSourceID: CF;
    static kTISTypeKeyboardLayout: CF;
}
#[link(name = "AppKit", kind = "framework")]
extern "C" {}
#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Id;
    fn objc_msgSend();
}
macro_rules! msg {
    ($obj:expr, $sel:literal => $ret:ty $(, $arg:expr => $ty:ty)*) => {{
        let function: unsafe extern "C" fn(Id, Id $(, $ty)*) -> $ret = std::mem::transmute(objc_msgSend as *const ());
        function($obj, sel_registerName(concat!($sel, "\0").as_ptr().cast()) $(, $arg)*)
    }};
}
unsafe fn class(name: &str) -> Id {
    objc_getClass(CString::new(name).unwrap().as_ptr())
}
struct Pool(Id);
impl Pool {
    unsafe fn new() -> Self {
        Self(msg!(msg!(class("NSAutoreleasePool"), "alloc" => Id), "init" => Id))
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        unsafe {
            msg!(self.0, "drain" => ());
        }
    }
}
struct StringRef(CF);
impl StringRef {
    fn new(s: &str) -> Self {
        unsafe {
            Self(CFStringCreateWithBytes(
                ptr::null(),
                s.as_ptr(),
                s.len() as isize,
                0x08000100,
                false,
            ))
        }
    }
}
impl Drop for StringRef {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}
unsafe fn string(value: CF) -> String {
    if value.is_null() {
        return String::new();
    }
    let size = CFStringGetLength(value).saturating_mul(4).saturating_add(1);
    let mut bytes = vec![0u8; size as usize];
    if !CFStringGetCString(value, bytes.as_mut_ptr().cast(), size, 0x08000100) {
        return String::new();
    }
    bytes.truncate(bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len()));
    String::from_utf8_lossy(&bytes).into_owned()
}
unsafe fn attribute(element: CF, name: &str) -> Option<CF> {
    let name = StringRef::new(name);
    let mut value = ptr::null();
    (AXUIElementCopyAttributeValue(element, name.0, &mut value) == 0 && !value.is_null())
        .then_some(value)
}
const MARK: i64 = 0x51545854;
const SHIFT: u64 = 1 << 17;
const CTRL: u64 = 1 << 18;
const ALT: u64 = 1 << 19;
const CMD: u64 = 1 << 20;
/// How long an action waits for held modifiers (the trigger's Shift) to come up.
const MODIFIER_RELEASE_WAIT: Duration = Duration::from_millis(500);
static ENGINE: OnceLock<Arc<Shared>> = OnceLock::new();
static SENDER: OnceLock<mpsc::SyncSender<Candidate>> = OnceLock::new();
static BUFFER: OnceLock<Mutex<Buffer>> = OnceLock::new();
static ENVIRONMENT: OnceLock<Mutex<(Foreground, bool)>> = OnceLock::new();
static PHYSICAL: OnceLock<Mutex<BTreeSet<u16>>> = OnceLock::new();
static BUTTONS: OnceLock<Mutex<BTreeSet<u32>>> = OnceLock::new();
static READY: AtomicBool = AtomicBool::new(false);
/// Set from the moment macOS disables the tap (timeout or user input) until
/// it is reinstalled: the input health stays up meanwhile, and
/// `PauseReasons::PLATFORM` holds detection.
static RESTARTING: AtomicBool = AtomicBool::new(false);
static RECORD_ERROR: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static CLIPBOARD_TOKEN: AtomicU64 = AtomicU64::new(0);
struct Buffer {
    value: String,
    focus: u64,
    record_at: Instant,
}
struct Candidate {
    rule: Rule,
    parameter: Option<String>,
    focus: Foreground,
    generation: u64,
    revision: u64,
    erase: usize,
    terminator: Option<String>,
    consume: bool,
}
/// Whether the input backend can run (resume, recording, the Health probe):
/// see [`hook_ready`].
pub fn engine_ready() -> bool {
    hook_ready().is_ok()
}
/// The input health (supervisor probe): the permissions are granted and the
/// event tap is installed, or being reinstalled after macOS disabled it.
/// Secure Input and an unsafe input source are not part of it: they come
/// and go on their own and set `PauseReasons::PLATFORM` instead (the monitor
/// thread in [`start_engine`]), so they neither fail the health nor become a
/// manual pause.
pub fn hook_ready() -> Result<(), String> {
    permissions()?;
    if !READY.load(Ordering::SeqCst) && !RESTARTING.load(Ordering::SeqCst) {
        return Err(
            "macOS event tap is not installed or was disabled; engine remains paused".into(),
        );
    }
    Ok(())
}
fn permissions() -> Result<(), String> {
    unsafe {
        if !AXIsProcessTrusted() {
            return Err("Grant Accessibility to Quicktext in System Settings, then resume".into());
        }
        if !CGPreflightListenEventAccess() {
            return Err(
                "Grant Input Monitoring to Quicktext in System Settings; restart if required"
                    .into(),
            );
        }
        if !CGPreflightPostEventAccess() {
            return Err("macOS denied synthetic input permission (Accessibility)".into());
        }
    }
    Ok(())
}
/// Whether synthetic input may be sent right now: [`hook_ready`], the tap
/// installed, and no platform block (Secure Input, unsafe input source).
pub fn native_ready() -> Result<(), String> {
    permissions()?;
    if let Some(block) = crate::macos_rules::platform_block(
        unsafe { IsSecureEventInputEnabled() },
        layout_safe(),
        RESTARTING.load(Ordering::SeqCst),
    ) {
        return Err(block.into());
    }
    if !READY.load(Ordering::SeqCst) {
        return Err(
            "macOS event tap is not installed or was disabled; engine remains paused".into(),
        );
    }
    Ok(())
}
pub fn input_status() -> serde_json::Value {
    let ready = native_ready();
    let focus = foreground();
    unsafe {
        serde_json::json!({
            "platform":"macos", "hook_installed":READY.load(Ordering::SeqCst),
            "accessibility":AXIsProcessTrusted(), "input_monitoring":CGPreflightListenEventAccess(),
            "post_event_access":CGPreflightPostEventAccess(), "secure_input":IsSecureEventInputEnabled(),
            "keyboard_layout_safe":layout_safe(), "focus_known":focus.id != 0,
            "ready":ready.is_ok(), "reason":ready.err(), "ime_committed_text_supported":false
        })
    }
}
/// Read the selected text through AX, without Cmd+C or clipboard mutation.
pub fn capture_selection() -> Result<String, String> {
    native_ready()?;
    let before = foreground();
    if before.id == 0 {
        return Err("Focused control cannot be verified or is a secure field".into());
    }
    let result = unsafe {
        let _pool = Pool::new();
        let workspace = msg!(class("NSWorkspace"), "sharedWorkspace" => Id);
        let application = msg!(workspace, "frontmostApplication" => Id);
        let app = AXUIElementCreateApplication(msg!(application, "processIdentifier" => i32));
        if app.is_null() {
            return Err("Accessibility application is unavailable".into());
        }
        AXUIElementSetMessagingTimeout(app, 0.02);
        let selected = if let Some(element) = attribute(app, "AXFocusedUIElement") {
            let selected = attribute(element, "AXSelectedText").map(|value| {
                let selected = string(value);
                CFRelease(value);
                selected
            });
            CFRelease(element);
            selected
        } else {
            None
        };
        CFRelease(app);
        selected.ok_or_else(|| {
            "Focused app does not expose selected text through Accessibility".to_string()
        })
    }?;
    if foreground().id != before.id {
        return Err("Focus changed while reading selection".into());
    }
    Ok(result)
}
pub fn foreground() -> Foreground {
    unsafe {
        let _pool = Pool::new();
        let workspace = msg!(class("NSWorkspace"), "sharedWorkspace" => Id);
        let application = msg!(workspace, "frontmostApplication" => Id);
        if application.is_null() {
            return Foreground {
                platform: "macos".into(),
                bundle_id: String::new(),
                id: 0,
                process: String::new(),
                title: String::new(),
            };
        }
        let pid = msg!(application, "processIdentifier" => i32);
        let bundle = string(msg!(application, "bundleIdentifier" => CF));
        let mut focus = Foreground {
            platform: "macos".into(),
            bundle_id: bundle.clone(),
            id: 0,
            process: bundle,
            title: String::new(),
        };
        if pid == std::process::id() as i32 || !AXIsProcessTrusted() {
            return focus;
        }
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return focus;
        }
        AXUIElementSetMessagingTimeout(app, 0.02);
        let window = attribute(app, "AXFocusedWindow");
        let element = attribute(app, "AXFocusedUIElement");
        if let (Some(window), Some(element)) = (window, element) {
            let subrole = attribute(element, "AXSubrole");
            let secure = subrole
                .map(|v| {
                    let secure = string(v) == "AXSecureTextField";
                    CFRelease(v);
                    secure
                })
                .unwrap_or(false);
            let editable_role = attribute(element, "AXRole")
                .map(|role| {
                    let allowed = matches!(string(role).as_str(), "AXTextArea" | "AXTextField");
                    CFRelease(role);
                    allowed
                })
                .unwrap_or(false);
            if !secure && editable_role {
                focus.id = (pid as u64).wrapping_mul(0x9e3779b97f4a7c15)
                    ^ (CFHash(window) as u64).rotate_left(19)
                    ^ CFHash(element) as u64;
                if let Some(title) = attribute(window, "AXTitle") {
                    focus.title = string(title);
                    CFRelease(title)
                }
            }
        }
        if let Some(window) = window {
            CFRelease(window);
        }
        if let Some(element) = element {
            CFRelease(element);
        }
        CFRelease(app);
        focus
    }
}
fn layout_safe() -> bool {
    unsafe {
        let input = TISCopyCurrentKeyboardInputSource();
        if input.is_null() {
            return false;
        }
        let kind = TISGetInputSourceProperty(input, kTISPropertyInputSourceType);
        let id = TISGetInputSourceProperty(input, kTISPropertyInputSourceID);
        let id = string(id);
        let safe = !kind.is_null()
            && CFEqual(kind, kTISTypeKeyboardLayout)
            && matches!(
                id.as_str(),
                "com.apple.keylayout.ABC" | "com.apple.keylayout.US"
            );
        CFRelease(input);
        safe
    }
}
fn location() -> Result<Point, String> {
    unsafe {
        let event = CGEventCreate(ptr::null());
        if event.is_null() {
            return Err("Could not read cursor".into());
        }
        let p = CGEventGetLocation(event);
        CFRelease(event);
        Ok(p)
    }
}
pub fn cursor() -> serde_json::Value {
    let Ok(p) = location() else {
        return serde_json::json!({"available":false});
    };
    unsafe {
        let mut displays = [0u32; 32];
        let mut count = 0;
        let mut items = vec![];
        if CGGetActiveDisplayList(32, displays.as_mut_ptr(), &mut count) == 0 {
            for id in &displays[..count.min(32) as usize] {
                let r = CGDisplayBounds(*id);
                items.push(serde_json::json!({"id":id,"left":r.origin.x,"top":r.origin.y,"width":r.size.width,"height":r.size.height}));
            }
        }
        let r = CGDisplayBounds(CGMainDisplayID());
        serde_json::json!({"available":true,"x":p.x,"y":p.y,"width":r.size.width,"height":r.size.height,"left":r.origin.x,"top":r.origin.y,"coordinate_unit":"Quartz display points","displays":items})
    }
}
/// Stops pending work and reports why as the last result. It never writes
/// `Shared::paused` (the runtime would read that as a manual pause): the
/// Health probe ([`hook_ready`]) and `PauseReasons::PLATFORM` hold detection.
fn error_status(shared: &Shared, message: String) {
    shared.generation.fetch_add(1, Ordering::SeqCst);
    *shared.last.lock().unwrap() = Some(ExecutionResult {
        id: "engine".into(),
        status: "failed".into(),
        message,
        completed_steps: 0,
        side_effects: false,
    });
}
fn record(shared: &Shared, buffer: &mut Buffer, op: Operation) {
    if shared.cancel.load(Ordering::SeqCst) {
        return;
    }
    if let Ok(mut record) = shared.recording.try_lock() {
        if let Some(steps) = record.as_mut() {
            let ms = buffer.record_at.elapsed().as_millis().min(120_000) as u64;
            buffer.record_at = Instant::now();
            if steps.is_empty() { /* First event starts the recording clock. */
            } else if steps.len() + 2 <= MAX_STEPS {
                steps.push(Step::new(Operation::Delay { ms }, SourceSpan::default()));
            }
            if steps.len() < MAX_STEPS {
                steps.push(Step::new(op, SourceSpan::default()));
            } else {
                shared.cancel.store(true, Ordering::SeqCst);
                if let Ok(mut error) = RECORD_ERROR.get().unwrap().try_lock() {
                    *error =
                        Some("Recording reached the event limit; record a shorter sequence".into());
                }
            }
        }
    }
}
unsafe extern "C" fn tap(_: Id, kind: u32, event: Event, _: Id) -> Event {
    let Some(shared) = ENGINE.get() else {
        return event;
    };
    if kind == 0xffff_fffe || kind == 0xffff_ffff {
        // macOS disabled the tap (timeout or user input). The tap thread
        // reinstalls it; meanwhile PLATFORM holds detection.
        RESTARTING.store(true, Ordering::SeqCst);
        READY.store(false, Ordering::SeqCst);
        shared.generation.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut buffer) = BUFFER.get().unwrap().try_lock() {
            buffer.value.clear();
        }
        return event;
    }
    if CGEventGetIntegerValueField(event, 42) == MARK {
        return event;
    }
    // Hardware-originated events have no user process source. Foreign injection
    // invalidates pending work and must not create matches or recorded macros.
    if CGEventGetIntegerValueField(event, 41) > 0 {
        shared.generation.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut buffer) = BUFFER.get().unwrap().try_lock() {
            buffer.value.clear();
        }
        return event;
    }
    let Ok(mut buffer) = BUFFER.get().unwrap().try_lock() else {
        shared.generation.fetch_add(1, Ordering::SeqCst);
        return event;
    };
    let flags = CGEventGetFlags(event);
    let code = CGEventGetIntegerValueField(event, 9) as u16;
    let down = if kind == 12 {
        CGEventSourceKeyState(1, code)
    } else {
        kind == 10
    };
    if matches!(kind, 10..=12) {
        if let Ok(mut physical) = PHYSICAL.get().unwrap().try_lock() {
            if down {
                physical.insert(code);
            } else {
                physical.remove(&code);
            }
        } else {
            buffer.value.clear();
            shared.generation.fetch_add(1, Ordering::SeqCst);
            return event;
        }
        if kind == 10 && code == 53 && flags & (CTRL | SHIFT) == CTRL | SHIFT {
            shared.cancel.store(true, Ordering::SeqCst);
            shared.generation.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut recording) = shared.recording.try_lock() {
                if let Some(steps) = recording.as_mut() {
                    while steps.last().is_some_and(|step| match &step.operation {
                        Operation::Delay { .. } => true,
                        Operation::KeyDown { key } => {
                            keycode(key).is_ok_and(|code| matches!(code, 56 | 60 | 59 | 62))
                        }
                        _ => false,
                    }) {
                        steps.pop();
                    }
                }
            }
            buffer.value.clear();
            return ptr::null_mut();
        }
    }
    // Until the monitor thread sets PLATFORM, the tap itself stays out of a
    // Secure Input session.
    if IsSecureEventInputEnabled() {
        buffer.value.clear();
        shared.generation.fetch_add(1, Ordering::SeqCst);
        return event;
    }
    if kind == 14 {
        let native = msg!(class("NSEvent"), "eventWithCGEvent:" => Id, event => Event);
        if !native.is_null() && msg!(native, "subtype" => i16) == 8 {
            let data = msg!(native, "data1" => isize);
            let code = 256 + ((data >> 16) & 0xffff) as u16;
            let down = (data >> 8) & 0xff == 0x0a;
            if let Ok(mut physical) = PHYSICAL.get().unwrap().try_lock() {
                if down {
                    physical.insert(code);
                } else {
                    physical.remove(&code);
                }
            }
            if let Some(name) = system_key_name(code - 256) {
                record(
                    shared,
                    &mut buffer,
                    if down {
                        Operation::KeyDown {
                            key: Key::named(name),
                        }
                    } else {
                        Operation::KeyUp {
                            key: Key::named(name),
                        }
                    },
                );
            }
        }
        shared.generation.fetch_add(1, Ordering::SeqCst);
        buffer.value.clear();
        return event;
    }
    if !matches!(kind, 10..=12) {
        // A click or a scroll may move the caret; a pointer move or drag
        // does not, so it keeps the buffer and pending work (as on Windows).
        if !matches!(kind, 5 | 6 | 7 | 27) {
            shared.generation.fetch_add(1, Ordering::SeqCst);
            buffer.value.clear();
        }
        let p = CGEventGetLocation(event);
        let b = CGEventGetIntegerValueField(event, 3) as u32;
        if let Ok(mut buttons) = BUTTONS.get().unwrap().try_lock() {
            if matches!(kind, 1 | 3 | 25) {
                buttons.insert(b);
            } else if matches!(kind, 2 | 4 | 26) {
                buttons.remove(&b);
            }
        }
        let op = match kind {
            5 | 6 | 7 | 27 => Some(Operation::MouseMove {
                x: p.x,
                y: p.y,
                relative: false,
                percent: false,
            }),
            1 | 2 | 3 | 4 | 25 | 26 => mouse_name(b).map(|button| Operation::MouseButton {
                button,
                state: if matches!(kind, 1 | 3 | 25) {
                    ButtonState::Down
                } else {
                    ButtonState::Up
                },
            }),
            22 => {
                let vertical = CGEventGetIntegerValueField(event, 11) as i32;
                let horizontal = CGEventGetIntegerValueField(event, 12) as i32;
                if vertical != 0 {
                    record(
                        shared,
                        &mut buffer,
                        Operation::Wheel {
                            axis: WheelAxis::Vertical,
                            amount: vertical,
                            unit: WheelUnit::Clicks,
                        },
                    );
                }
                if horizontal != 0 {
                    Some(Operation::Wheel {
                        axis: WheelAxis::Horizontal,
                        amount: horizontal,
                        unit: WheelUnit::Clicks,
                    })
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(op) = op {
            if matches!(op, Operation::MouseButton { .. }) {
                record(
                    shared,
                    &mut buffer,
                    Operation::MouseMove {
                        x: p.x,
                        y: p.y,
                        relative: false,
                        percent: false,
                    },
                );
            }
            record(shared, &mut buffer, op)
        }
        return event;
    }
    if let Some(name) = key_name(code) {
        record(
            shared,
            &mut buffer,
            if down {
                Operation::KeyDown {
                    key: Key::named(name),
                }
            } else {
                Operation::KeyUp {
                    key: Key::named(name),
                }
            },
        );
    } else if matches!(kind, 10..=12)
        && shared
            .recording
            .try_lock()
            .is_ok_and(|record| record.is_some())
    {
        if let Ok(mut error) = RECORD_ERROR.get().unwrap().try_lock() {
            *error = Some(format!(
                "Mac hardware keycode {code} cannot be represented safely in portable replay"
            ));
        }
        shared.cancel.store(true, Ordering::SeqCst);
    }
    // Shift (56, 60) and Caps Lock (57) only change the case of the next
    // character: they keep the buffer and the generation, so `Hi` or `x:`
    // can match and run.
    if kind == 11 || kind == 12 && matches!(code, 56 | 57 | 60) {
        return event;
    }
    let generation = shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
    // Control, Option and Cmd alone keep the buffer too (`KeyEdit::Neutral`);
    // the next key typed with one of them held resets it.
    if kind == 12 {
        return event;
    }
    let Ok(env) = ENVIRONMENT.get().unwrap().try_lock() else {
        buffer.value.clear();
        return event;
    };
    if shared.paused.load(Ordering::SeqCst)
        || shared.busy.load(Ordering::SeqCst)
        || !env.1
        || env.0.id == 0
    {
        buffer.value.clear();
        return event;
    }
    let focus = env.0.clone();
    drop(env);
    if buffer.focus != focus.id {
        buffer.value.clear()
    }
    buffer.focus = focus.id;
    // The buffer rules are shared with Windows (`quicker_domain::key_edit`):
    // any character the layout types counts, Backspace edits, caret keys
    // and shortcuts reset, and only space and `;` are terminators.
    let mut chars = [0u16; 8];
    let mut len = 0;
    CGEventKeyboardGetUnicodeString(event, 8, &mut len, chars.as_mut_ptr());
    let text = String::from_utf16_lossy(&chars[..len.min(8)]);
    let edit = key_edit(
        crate::macos_rules::key_class(code),
        Some(text),
        flags & (CTRL | ALT | CMD) != 0,
    );
    let previous = typed_terminator(&edit).map(|key| (buffer.value.clone(), key));
    apply_key_edit(&mut buffer.value, &edit);
    if !matches!(edit, KeyEdit::Text(_)) {
        return event;
    }
    let Ok(config) = shared.config.try_lock() else {
        buffer.value.clear();
        return event;
    };
    let Ok(compiled) = shared.compiled.try_lock() else {
        buffer.value.clear();
        return event;
    };
    let foreground_context = ForegroundContext {
        platform: Platform::Macos,
        process: focus.process.clone(),
        bundle_id: focus.process.clone(),
    };
    let Some((matched, terminator)) = match_typed(
        &compiled,
        &buffer.value,
        previous.as_ref().map(|(text, key)| (text.as_str(), *key)),
        &foreground_context,
    ) else {
        return event;
    };
    if let Some(rule) = config.rules.iter().find(|r| r.id == matched.rule_id) {
        let candidate = Candidate {
            rule: rule.clone(),
            parameter: matched.parameter,
            focus,
            generation,
            revision: config.revision,
            erase: matched.erase_chars,
            terminator: terminator.map(str::to_string),
            consume: rule.terminator.as_ref().map(|t| t.consume).unwrap_or(true),
        };
        buffer.value.clear();
        if shared
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            shared.cancel.store(false, Ordering::SeqCst);
            if SENDER.get().unwrap().try_send(candidate).is_ok() {
                // The terminator is held back; the worker sends it again
                // after the output when the rule keeps it.
                if terminator.is_some() {
                    return ptr::null_mut();
                }
            } else {
                shared.busy.store(false, Ordering::SeqCst);
            }
        }
    }
    event
}
pub fn start_engine(shared: Arc<Shared>) -> Result<(), String> {
    ENGINE
        .set(shared.clone())
        .map_err(|_| "Engine already started")?;
    BUFFER
        .set(Mutex::new(Buffer {
            value: String::new(),
            focus: 0,
            record_at: Instant::now(),
        }))
        .ok();
    ENVIRONMENT
        .set(Mutex::new((
            Foreground {
                platform: "macos".into(),
                bundle_id: String::new(),
                id: 0,
                process: String::new(),
                title: String::new(),
            },
            false,
        )))
        .ok();
    PHYSICAL
        .set(Mutex::new(
            (0..128)
                .filter(|key| unsafe { CGEventSourceKeyState(1, *key) })
                .collect(),
        ))
        .ok();
    BUTTONS
        .set(Mutex::new(
            (0..5)
                .filter(|button| unsafe { CGEventSourceButtonState(1, *button) })
                .collect(),
        ))
        .ok();
    RECORD_ERROR.set(Mutex::new(None)).ok();
    let (tx, rx) = mpsc::sync_channel::<Candidate>(1);
    SENDER.set(tx).ok();
    let worker = shared.clone();
    std::thread::spawn(move || loop {
        let candidate = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) if !worker.shutdown.load(Ordering::SeqCst) => {
                continue
            }
            _ => break,
        };
        // Let the non-suppressed final immediate key reach the target before deletion.
        std::thread::sleep(Duration::from_millis(15));
        let config = worker.config.lock().unwrap().clone();
        let action = &candidate.rule.action;
        // Built first so `now` is still taken before the clipboard and selection reads.
        let base = VariableContext::default();
        let mut context = VariableContext {
            clipboard: crate::clipboard_variable(action_uses_variable(action, &config, "cliptext")),
            selection: action_uses_variable(action, &config, "selection")
                .then(capture_selection)
                .and_then(Result::ok),
            ..base
        };
        context.parameters.insert(
            "action_parameter".into(),
            candidate
                .parameter
                .clone()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        let result = if config.revision != candidate.revision {
            Err((
                Stop::Execute,
                "Revision changed before native action".into(),
            ))
        } else {
            expand_reusable(&candidate.rule.action, &config, &context)
                .map_err(|e| {
                    let message = e
                        .iter()
                        .map(|d| d.message.clone())
                        .collect::<Vec<_>>()
                        .join("; ");
                    (Stop::Resolve, message)
                })
                .and_then(|plan| {
                    preflight(&plan)
                        .map(|()| plan)
                        .map_err(|message| (Stop::Preflight, message))
                })
                .and_then(|plan| {
                    execute(
                        worker.clone(),
                        plan,
                        candidate.focus.clone(),
                        candidate.generation,
                        candidate.erase,
                        if candidate.consume {
                            None
                        } else {
                            candidate.terminator.clone()
                        },
                        match candidate.rule.action {
                            Action::Replay { speed, .. } => speed,
                            _ => 1.0,
                        },
                    )
                    .map_err(|message| (Stop::Execute, message))
                })
        };
        // The run's history entry (B-35), with the Windows verdict rules.
        let (outcome, reason, completed_steps, side_effects) = crate::macos_rules::verdict(&result);
        crate::runtime::current().record(Record {
            process: Some(candidate.focus.process.clone()).filter(|name| !name.is_empty()),
            completed_steps,
            side_effects,
            ..Record::new(
                candidate.rule.id.clone(),
                match candidate.rule.trigger_mode {
                    TriggerMode::Immediate => TriggerKind::Immediate,
                    TriggerMode::Terminator => TriggerKind::Terminator,
                },
                outcome,
                reason,
            )
        });
        match result {
            Ok(mut result) => {
                result.id = format!("{}:run-{}", candidate.rule.id, candidate.generation);
                *worker.last.lock().unwrap() = Some(result);
            }
            Err((_, message)) => {
                if guarded(
                    &worker,
                    &candidate.focus,
                    candidate.generation,
                    candidate.revision,
                )
                .is_ok()
                {
                    if let Some(term) = candidate.terminator {
                        let _ = send_terminator(&term);
                    }
                }
                *worker.last.lock().unwrap() = Some(ExecutionResult {
                    id: format!("{}:run-{}", candidate.rule.id, candidate.generation),
                    status: "failed".into(),
                    message,
                    completed_steps: 0,
                    side_effects: false,
                });
            }
        }
        worker.busy.store(false, Ordering::SeqCst);
    });
    let monitor = shared.clone();
    std::thread::spawn(move || {
        let mut old = 0;
        // `None` until the first pass, so a PLATFORM pause left by an earlier
        // engine run is written over at once.
        let mut blocked: Option<Option<&str>> = None;
        while !monitor.shutdown.load(Ordering::SeqCst) {
            let focus = foreground();
            let safe = layout_safe();
            if old != focus.id && !monitor.busy.load(Ordering::SeqCst) {
                monitor.generation.fetch_add(1, Ordering::SeqCst);
            }
            if old != focus.id {
                if let Ok(mut buffer) = BUFFER.get().unwrap().try_lock() {
                    buffer.value.clear();
                }
            }
            old = focus.id;
            *ENVIRONMENT.get().unwrap().lock().unwrap() = (focus, safe);
            // Transient conditions hold detection through their own pause
            // reason, set and cleared as they come and go.
            let block = crate::macos_rules::platform_block(
                unsafe { IsSecureEventInputEnabled() },
                safe,
                RESTARTING.load(Ordering::SeqCst),
            );
            if blocked != Some(block) {
                if let Some(message) = block {
                    error_status(&monitor, message.into());
                    if let Ok(mut buffer) = BUFFER.get().unwrap().try_lock() {
                        buffer.value.clear();
                    }
                }
                crate::runtime::current().set_pause(PauseReasons::PLATFORM, block.is_some());
                blocked = Some(block);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        crate::runtime::current().set_pause(PauseReasons::PLATFORM, false);
    });
    std::thread::spawn(move || unsafe {
        let mut last_error = String::new();
        while !shared.shutdown.load(Ordering::SeqCst) {
            if !AXIsProcessTrusted()
                || !CGPreflightListenEventAccess()
                || !CGPreflightPostEventAccess()
            {
                let message = "macOS permissions required: System Settings → Privacy & Security → Accessibility and Input Monitoring; grant Quicktext and resume".to_string();
                RESTARTING.store(false, Ordering::SeqCst);
                if last_error != message {
                    error_status(&shared, message.clone());
                    last_error = message;
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            let mask = [1u32, 2, 3, 4, 5, 6, 7, 10, 11, 12, 14, 22, 25, 26, 27]
                .into_iter()
                .fold(0u64, |mask, n| mask | (1 << n));
            let port = CGEventTapCreate(1, 0, 0, mask, tap, ptr::null_mut());
            if port.is_null() {
                RESTARTING.store(false, Ordering::SeqCst);
                if last_error != "tap" {
                    error_status(
                        &shared,
                        "CGEventTapCreate failed; check Input Monitoring permissions and restart"
                            .into(),
                    );
                    last_error = "tap".into();
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            let source = CFMachPortCreateRunLoopSource(ptr::null(), port, 0);
            if source.is_null() {
                CFMachPortInvalidate(port);
                CFRelease(port);
                RESTARTING.store(false, Ordering::SeqCst);
                error_status(&shared, "Could not create event-tap runloop source".into());
                break;
            }
            let runloop = CFRunLoopGetCurrent();
            CFRunLoopAddSource(runloop, source, kCFRunLoopDefaultMode);
            CGEventTapEnable(port, true);
            READY.store(true, Ordering::SeqCst);
            RESTARTING.store(false, Ordering::SeqCst);
            last_error.clear();
            while !shared.shutdown.load(Ordering::SeqCst) && READY.load(Ordering::SeqCst) {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1, false);
            }
            // Disabled by macOS (the callback set RESTARTING already):
            // reinstall at once while PLATFORM holds detection.
            if !shared.shutdown.load(Ordering::SeqCst) {
                RESTARTING.store(true, Ordering::SeqCst);
            }
            READY.store(false, Ordering::SeqCst);
            CGEventTapEnable(port, false);
            CFRunLoopRemoveSource(runloop, source, kCFRunLoopDefaultMode);
            CFMachPortInvalidate(port);
            CFRelease(source);
            CFRelease(port);
        }
    });
    Ok(())
}
/// Balance only the recorded IR; never synthesize physical releases to stop recording.
pub fn finish_recording(shared: &Arc<Shared>) {
    shared.cancel.store(true, Ordering::SeqCst);
    let error = RECORD_ERROR
        .get()
        .and_then(|error| error.lock().ok().and_then(|mut error| error.take()));
    let mut recording = shared.recording.lock().unwrap();
    let Some(steps) = recording.as_mut() else {
        return;
    };
    if let Some(message) = error {
        *shared.last.lock().unwrap() = Some(ExecutionResult {
            id: "recording".into(),
            status: "failed".into(),
            message,
            completed_steps: steps.len(),
            side_effects: false,
        });
        *recording = None;
        return;
    }
    let mut held = BTreeMap::new();
    let mut buttons = BTreeSet::new();
    for step in steps.iter() {
        match &step.operation {
            Operation::KeyDown { key } => {
                held.insert(canonical_key(key), key.clone());
            }
            Operation::KeyUp { key } => {
                held.remove(&canonical_key(key));
            }
            Operation::MouseButton {
                button,
                state: ButtonState::Down,
            } => {
                buttons.insert(mouse_code(*button));
            }
            Operation::MouseButton {
                button,
                state: ButtonState::Up,
            } => {
                buttons.remove(&mouse_code(*button));
            }
            _ => {}
        }
    }
    if steps.len() + held.len() + buttons.len() > MAX_STEPS {
        *shared.last.lock().unwrap()=Some(ExecutionResult{id:"recording".into(),status:"failed".into(),message:"Recording reached limit before releases could be balanced; record a shorter sequence".into(),completed_steps:steps.len(),side_effects:false});
        *recording = None;
        return;
    }
    for (_, key) in held {
        steps.push(Step::new(Operation::KeyUp { key }, SourceSpan::default()));
    }
    for code in buttons {
        if let Some(button) = mouse_name(code) {
            steps.push(Step::new(
                Operation::MouseButton {
                    button,
                    state: ButtonState::Up,
                },
                SourceSpan::default(),
            ));
        }
    }
}
/// Ends the active recording and takes it: [`finish_recording`] (releases
/// what is still held, or drops a recording that hit an error) followed by
/// `stop_recording`. A dropped recording fails with its own reason, not
/// "Recording is not active".
pub fn finish_and_take(shared: &Arc<Shared>) -> Result<ActionPlan, String> {
    finish_recording(shared);
    crate::stop_recording(shared).map_err(|error| {
        match shared.last.lock().ok().and_then(|last| last.clone()) {
            Some(last) if last.id == "recording" && last.status == "failed" => last.message,
            _ => error,
        }
    })
}
/// Whether the user holds the key. A key-up the tap never saw (Secure Input
/// hides key events from it; the tap may be reinstalled mid-press) would
/// leave the key held for good and refuse every later run, so an ordinary
/// key is also checked against the hardware state and dropped once it is up.
fn physical(code: u16) -> bool {
    let Some(set) = PHYSICAL.get() else {
        return true;
    };
    if !set.lock().unwrap().contains(&code) {
        return false;
    }
    if code < 256 && !unsafe { CGEventSourceKeyState(1, code) } {
        set.lock().unwrap().remove(&code);
        return false;
    }
    true
}
/// Whether the user holds the mouse button, checked against the hardware
/// state like [`physical`].
fn physical_button(code: u32) -> bool {
    let Some(set) = BUTTONS.get() else {
        return true;
    };
    if !set.lock().unwrap().contains(&code) {
        return false;
    }
    if !unsafe { CGEventSourceButtonState(1, code) } {
        set.lock().unwrap().remove(&code);
        return false;
    }
    true
}
fn modifier_flag(code: u16) -> u64 {
    match code {
        56 | 60 => SHIFT,
        59 | 62 => CTRL,
        58 | 61 => ALT,
        55 | 54 => CMD,
        _ => 0,
    }
}
fn key_name(code: u16) -> Option<&'static str> {
    KEYS.iter().find(|(_, c)| *c == code).map(|(name, _)| *name)
}
const KEYS: &[(&str, u16)] = &[
    ("A", 0),
    ("S", 1),
    ("D", 2),
    ("F", 3),
    ("H", 4),
    ("G", 5),
    ("Z", 6),
    ("X", 7),
    ("C", 8),
    ("V", 9),
    ("B", 11),
    ("Q", 12),
    ("W", 13),
    ("E", 14),
    ("R", 15),
    ("Y", 16),
    ("T", 17),
    ("D1", 18),
    ("D2", 19),
    ("D3", 20),
    ("D4", 21),
    ("D6", 22),
    ("D5", 23),
    ("OEMPLUS", 24),
    ("D9", 25),
    ("D7", 26),
    ("OEMMINUS", 27),
    ("D8", 28),
    ("D0", 29),
    ("OEMCLOSEBRACKETS", 30),
    ("O", 31),
    ("U", 32),
    ("OEMOPENBRACKETS", 33),
    ("I", 34),
    ("P", 35),
    ("ENTER", 36),
    ("L", 37),
    ("J", 38),
    ("OEMQUOTES", 39),
    ("K", 40),
    ("OEMSEMICOLON", 41),
    ("OEMPIPE", 42),
    ("OEMCOMMA", 43),
    ("OEMQUESTION", 44),
    ("N", 45),
    ("M", 46),
    ("OEMPERIOD", 47),
    ("TAB", 48),
    ("SPACE", 49),
    ("OEMTILDE", 50),
    ("BACKSPACE", 51),
    ("ESC", 53),
    ("CMD", 54),
    ("CMD", 55),
    ("SHIFT", 56),
    ("CAPSLOCK", 57),
    ("ALT", 58),
    ("CTRL", 59),
    ("RSHIFTKEY", 60),
    ("RMENU", 61),
    ("RCONTROLKEY", 62),
    ("F17", 64),
    ("DECIMAL", 65),
    ("MULTIPLY", 67),
    ("ADD", 69),
    ("CLEAR", 71),
    ("DIVIDE", 75),
    ("SUBTRACT", 78),
    ("F18", 79),
    ("F19", 80),
    ("NUMPADEQUAL", 81),
    ("NUMPAD0", 82),
    ("NUMPAD1", 83),
    ("NUMPAD2", 84),
    ("NUMPAD3", 85),
    ("NUMPAD4", 86),
    ("NUMPAD5", 87),
    ("NUMPAD6", 88),
    ("NUMPAD7", 89),
    ("F20", 90),
    ("NUMPAD8", 91),
    ("NUMPAD9", 92),
    ("F5", 96),
    ("F6", 97),
    ("F7", 98),
    ("F3", 99),
    ("F8", 100),
    ("F9", 101),
    ("F11", 103),
    ("F13", 105),
    ("F16", 106),
    ("F14", 107),
    ("F10", 109),
    ("F12", 111),
    ("F15", 113),
    ("HELP", 114),
    ("HOME", 115),
    ("PGUP", 116),
    ("DELETE", 117),
    ("F4", 118),
    ("END", 119),
    ("F2", 120),
    ("PGDN", 121),
    ("F1", 122),
    ("LEFT", 123),
    ("RIGHT", 124),
    ("DOWN", 125),
    ("UP", 126),
];
fn keycode(key: &Key) -> Result<u16, String> {
    let name = match key {
        Key::PlatformPrimary => return Ok(55),
        Key::WindowsCode { code } => match *code {
            0x41..=0x5a => char::from_u32(*code as u32).unwrap().to_string(),
            0x30..=0x39 => format!("D{}", code - 0x30),
            0x70..=0x83 => format!("F{}", code - 0x70 + 1),
            8 => "BACKSPACE".into(),
            9 => "TAB".into(),
            13 => "ENTER".into(),
            16 => "SHIFT".into(),
            17 => "CTRL".into(),
            18 => "ALT".into(),
            20 => "CAPSLOCK".into(),
            27 => "ESC".into(),
            32 => "SPACE".into(),
            33 => "PGUP".into(),
            34 => "PGDN".into(),
            35 => "END".into(),
            36 => "HOME".into(),
            37 => "LEFT".into(),
            38 => "UP".into(),
            39 => "RIGHT".into(),
            40 => "DOWN".into(),
            46 => "DELETE".into(),
            0x60..=0x69 => format!("NUMPAD{}", code - 0x60),
            0x6a => "MULTIPLY".into(),
            0x6b => "ADD".into(),
            0x6d => "SUBTRACT".into(),
            0x6e => "DECIMAL".into(),
            0x6f => "DIVIDE".into(),
            0xa0 => "SHIFT".into(),
            0xa1 => "RSHIFTKEY".into(),
            0xa2 => "CTRL".into(),
            0xa3 => "RCONTROLKEY".into(),
            0xa4 => "ALT".into(),
            0xa5 => "RMENU".into(),
            0xad => "VOLUMEMUTE".into(),
            0xae => "VOLUMEDOWN".into(),
            0xaf => "VOLUMEUP".into(),
            0xb0 => "MEDIANEXTTRACK".into(),
            0xb1 => "MEDIAPREVIOUSTRACK".into(),
            0xb3 => "MEDIAPLAYPAUSE".into(),
            0xba => "OEMSEMICOLON".into(),
            0xbb => "OEMPLUS".into(),
            0xbc => "OEMCOMMA".into(),
            0xbd => "OEMMINUS".into(),
            0xbe => "OEMPERIOD".into(),
            0xbf => "OEMQUESTION".into(),
            0xc0 => "OEMTILDE".into(),
            0xdb => "OEMOPENBRACKETS".into(),
            0xdc => "OEMPIPE".into(),
            0xdd => "OEMCLOSEBRACKETS".into(),
            0xde => "OEMQUOTES".into(),
            _ => {
                return Err(format!(
                "Windows key #{code} has no verified macOS mapping; choose a Mac key explicitly"
            ))
            }
        },
        Key::Named { name } => name.to_ascii_uppercase(),
    };
    let normalized = match name.as_str() {
        "CONTROL" | "CONTROLKEY" | "LCONTROLKEY" => "CTRL",
        "SHIFTKEY" | "LSHIFTKEY" => "SHIFT",
        "MENU" | "LMENU" => "ALT",
        "COMMAND" | "META" => "CMD",
        "RETURN" => "ENTER",
        "ESCAPE" => "ESC",
        "BACK" | "BS" => "BACKSPACE",
        "DEL" => "DELETE",
        "PRIOR" => "PGUP",
        "NEXT" => "PGDN",
        "CAPITAL" => "CAPSLOCK",
        n => n,
    };
    if let Some(code) = match normalized {
        "VOLUMEUP" => Some(0),
        "VOLUMEDOWN" => Some(1),
        "BRIGHTNESSUP" => Some(2),
        "BRIGHTNESSDOWN" => Some(3),
        "VOLUMEMUTE" => Some(7),
        "MEDIAPLAYPAUSE" => Some(16),
        "MEDIANEXTTRACK" => Some(17),
        "MEDIAPREVIOUSTRACK" => Some(18),
        _ => None,
    } {
        return Ok(256 + code);
    }
    KEYS.iter().find(|(n,_)|*n==normalized).map(|(_,code)|*code).ok_or_else(||format!("Key {name} is not supported on macOS; Ctrl is preserved and Windows keys are not silently converted to Cmd"))
}
fn system_key_name(code: u16) -> Option<&'static str> {
    match code {
        0 => Some("VOLUMEUP"),
        1 => Some("VOLUMEDOWN"),
        7 => Some("VOLUMEMUTE"),
        16 => Some("MEDIAPLAYPAUSE"),
        17 => Some("MEDIANEXTTRACK"),
        18 => Some("MEDIAPREVIOUSTRACK"),
        _ => None,
    }
}
fn mouse_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::X1 => 3,
        MouseButton::X2 => 4,
    }
}
fn mouse_name(code: u32) -> Option<MouseButton> {
    match code {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Right),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::X1),
        4 => Some(MouseButton::X2),
        _ => None,
    }
}
unsafe fn post(event: Event) -> Result<(), String> {
    if event.is_null() {
        return Err("CGEvent allocation failed".into());
    }
    CGEventSetIntegerValueField(event, 42, MARK);
    CGEventPost(0, event);
    CFRelease(event);
    Ok(())
}
fn send_key(code: u16, down: bool, held: &BTreeSet<u16>) -> Result<(), String> {
    native_ready()?;
    if code >= 256 {
        return system_key(code - 256, down);
    }
    unsafe {
        let event = CGEventCreateKeyboardEvent(ptr::null(), code, down);
        if event.is_null() {
            return Err("CGEvent allocation failed".into());
        }
        CGEventSetFlags(event, held.iter().fold(0, |f, key| f | modifier_flag(*key)));
        post(event)
    }
}
// Apple IOHIDSystem NX_KEYTYPE constants; NSEvent owns the borrowed CGEvent.
fn system_key(code: u16, down: bool) -> Result<(), String> {
    unsafe {
        let _pool = Pool::new();
        let process = msg!(class("NSProcessInfo"), "processInfo" => Id);
        let time = msg!(process, "systemUptime" => f64);
        let data = ((code as isize) << 16) | ((if down { 0x0a } else { 0x0b }) << 8);
        let event = msg!(class("NSEvent"), "otherEventWithType:location:modifierFlags:timestamp:windowNumber:context:subtype:data1:data2:" => Id,
            14usize => usize, Point::default() => Point, 0u64 => u64, time => f64, 0isize => isize, ptr::null_mut() => Id, 8i16 => i16, data => isize, -1isize => isize);
        if event.is_null() {
            return Err("macOS could not construct media/system-key event".into());
        }
        let quartz = msg!(event, "CGEvent" => Event);
        if quartz.is_null() {
            return Err("NSEvent has no CGEvent representation".into());
        }
        CFRetain(quartz);
        post(quartz)
    }
}
fn press(code: u16, held: &BTreeSet<u16>) -> Result<(), String> {
    if physical(code) {
        return Err("Physical key conflicts with requested press".into());
    }
    let mut next = held.clone();
    next.insert(code);
    send_key(code, true, &next)?;
    next.remove(&code);
    send_key(code, false, &next)
}
fn send_terminator(term: &str) -> Result<(), String> {
    let code = match term {
        "space" => 49,
        "tab" => 48,
        "enter" => 36,
        "semicolon" => 41,
        _ => return Err("Unknown terminator".into()),
    };
    // The held-back terminator is often still down when a short action
    // ends: wait for it to come up instead of losing it.
    crate::macos_rules::released_within(MODIFIER_RELEASE_WAIT, || physical(code));
    press(code, &BTreeSet::new())
}
fn hotkey(keys: &[Key], held: &BTreeSet<u16>) -> Result<(), String> {
    let codes = keys.iter().map(keycode).collect::<Result<Vec<_>, _>>()?;
    if codes.iter().any(|c| physical(*c) || held.contains(c)) {
        return Err("Physical or task-held key conflicts with hotkey".into());
    }
    let mut owned = held.clone();
    let mut sent = vec![];
    let mut result = Ok(());
    for code in codes {
        owned.insert(code);
        if let Err(e) = send_key(code, true, &owned) {
            result = Err(e);
            break;
        }
        sent.push(code);
    }
    for code in sent.into_iter().rev() {
        owned.remove(&code);
        if !physical(code) {
            if let Err(e) = send_key(code, false, &owned) {
                result = Err(e)
            }
        } else {
            result = Err("Physical key conflicts with hotkey cleanup; release the held key".into())
        }
    }
    result
}
fn common_keys(name: &str) -> Result<Vec<Key>, String> {
    let keys = match name {
        "copy" => vec!["CMD", "C"],
        "paste" => vec!["CMD", "V"],
        "cut" => vec!["CMD", "X"],
        "undo" => vec!["CMD", "Z"],
        "redo" => vec!["CMD", "SHIFT", "Z"],
        "select_all" => vec!["CMD", "A"],
        "browser_back" => vec!["CMD", "OEMOPENBRACKETS"],
        "browser_forward" => vec!["CMD", "OEMCLOSEBRACKETS"],
        "volume_up" => vec!["VOLUMEUP"],
        "volume_down" => vec!["VOLUMEDOWN"],
        "volume_mute" => vec!["VOLUMEMUTE"],
        "media_play_pause" => vec!["MEDIAPLAYPAUSE"],
        "media_next" => vec!["MEDIANEXTTRACK"],
        "media_previous" => vec!["MEDIAPREVIOUSTRACK"],
        "brightness_up" => vec!["BRIGHTNESSUP"],
        "brightness_down" => vec!["BRIGHTNESSDOWN"],
        _ => {
            return Err(format!(
                "Common function {name} has no macOS adapter; choose a supported function"
            ))
        }
    };
    Ok(keys.into_iter().map(Key::named).collect())
}
type Payload = Vec<Vec<(String, Vec<u8>)>>;
/// The pasteboard's plain text for `{cliptext}` (`crate::read_clipboard_text`).
/// A string that does not convert is an error, not an empty `{cliptext}`.
pub(crate) fn read_clipboard_text() -> Result<String, crate::ClipboardTextError> {
    unsafe {
        let _pool = Pool::new();
        let board = msg!(class("NSPasteboard"),"generalPasteboard"=>Id);
        let kind = StringRef::new("public.utf8-plain-text");
        let value = msg!(board,"stringForType:"=>CF,kind.0 as Id=>Id);
        if value.is_null() {
            return Err(crate::ClipboardTextError::NoText);
        }
        let text = string(value);
        if text.is_empty() && CFStringGetLength(value) > 0 {
            Err(crate::ClipboardTextError::Unreadable)
        } else {
            Ok(text)
        }
    }
}
fn clipboard_count() -> isize {
    unsafe {
        let _pool = Pool::new();
        let board = msg!(class("NSPasteboard"),"generalPasteboard"=>Id);
        msg!(board,"changeCount"=>isize)
    }
}
fn snapshot() -> Result<(isize, Payload), String> {
    unsafe {
        let _pool = Pool::new();
        let board = msg!(class("NSPasteboard"),"generalPasteboard"=>Id);
        let before = msg!(board,"changeCount"=>isize);
        let items = msg!(board,"pasteboardItems"=>Id);
        let mut snapshot = vec![];
        let count = if items.is_null() {
            0
        } else {
            msg!(items,"count"=>usize)
        };
        let mut total = 0;
        let policy = crate::clipboard_options::lease_options().policy;
        let (mut had_types, mut skipped) = (false, Vec::new());
        if count > 256 {
            return Err("Clipboard contains too many objects to safely preserve".into());
        }
        for index in 0..count {
            let item = msg!(items,"objectAtIndex:"=>Id,index=>usize);
            let types = msg!(item,"types"=>Id);
            let mut formats = vec![];
            for index in 0..msg!(types,"count"=>usize) {
                let kind = msg!(types,"objectAtIndex:"=>Id,index=>usize);
                let name = string(kind);
                let data = msg!(item,"dataForType:"=>Id,kind=>Id);
                had_types = true;
                // A promised or private type gives no data: the restore
                // policy decides (B-08), as on Windows.
                match crate::macos_rules::backup(policy, &name, !data.is_null()) {
                    Backup::Copy => {}
                    Backup::Skip => {
                        skipped.push(name);
                        continue;
                    }
                    Backup::Ignore => continue,
                    Backup::Refuse => return Err("A promised/private clipboard format could not be preserved; paste refused before erasure".into()),
                }
                let length = msg!(data,"length"=>usize);
                total += length;
                if total > 64_000_000 {
                    return Err(
                        "Clipboard snapshot exceeds 64 MB; paste refused before erasure".into(),
                    );
                }
                let bytes = msg!(data,"bytes"=>*const u8);
                if bytes.is_null() && length > 0 {
                    return Err("Clipboard data is unreadable".into());
                }
                formats.push((
                    name,
                    if length == 0 {
                        vec![]
                    } else {
                        std::slice::from_raw_parts(bytes, length).to_vec()
                    },
                ));
            }
            if !formats.is_empty() {
                snapshot.push(formats);
            }
        }
        if crate::macos_rules::backup_refused(policy, had_types, !snapshot.is_empty()) {
            return Err(
                "No clipboard format could be preserved; paste refused before erasure".into(),
            );
        }
        if !skipped.is_empty() {
            tracing::info!(event = "clipboard.skipped", formats = ?skipped, ?policy);
        }
        if before != msg!(board,"changeCount"=>isize) {
            return Err("Clipboard changed during backup; nothing was overwritten".into());
        }
        Ok((before, snapshot))
    }
}
fn write_clipboard(
    payload: &Payload,
    expected: isize,
    claimed: &mut Option<isize>,
) -> Result<isize, String> {
    unsafe {
        let _pool = Pool::new();
        let board = msg!(class("NSPasteboard"),"generalPasteboard"=>Id);
        let objects = msg!(class("NSMutableArray"),"array"=>Id);
        let token = format!(
            "{}-{}",
            std::process::id(),
            CLIPBOARD_TOKEN.fetch_add(1, Ordering::SeqCst)
        );
        let marker = StringRef::new("com.quicktext.clipboard-lease");
        for item in payload {
            let object = msg!(msg!(class("NSPasteboardItem"),"alloc"=>Id),"init"=>Id);
            if object.is_null() {
                return Err("Could not allocate pasteboard item".into());
            }
            for (kind, bytes) in item {
                if kind == "com.quicktext.clipboard-lease" {
                    continue;
                }
                let data = msg!(class("NSData"),"dataWithBytes:length:"=>Id,bytes.as_ptr() as *const c_void=>*const c_void,bytes.len()=>usize);
                let kind = StringRef::new(kind);
                let ok = msg!(object,"setData:forType:"=>bool,data=>Id,kind.0 as Id=>Id);
                if !ok {
                    msg!(object,"release"=>());
                    return Err("Could not prepare native pasteboard format".into());
                }
            }
            let marker_data = msg!(class("NSData"),"dataWithBytes:length:"=>Id,token.as_ptr() as *const c_void=>*const c_void,token.len()=>usize);
            if !msg!(object,"setData:forType:"=>bool,marker_data=>Id,marker.0 as Id=>Id) {
                msg!(object,"release"=>());
                return Err("Could not prepare clipboard ownership marker".into());
            }
            msg!(objects,"addObject:"=>(),object=>Id);
            msg!(object,"release"=>());
        }
        if msg!(board,"changeCount"=>isize) != expected {
            return Err("Clipboard changed concurrently; new user content preserved".into());
        }
        let cleared = msg!(board,"clearContents"=>isize);
        *claimed = Some(cleared);
        // NSPasteboard has no compare-and-swap: check ownership again before writing.
        if msg!(board,"changeCount"=>isize) != cleared {
            return Err("Clipboard changed after clear; write stopped".into());
        }
        if !payload.is_empty() && !msg!(board,"writeObjects:"=>bool,objects=>Id) {
            return Err("NSPasteboard rejected native formats".into());
        }
        let generation = msg!(board,"changeCount"=>isize);
        // Never claim a concurrent owner's generation as ours. NSPasteboard may
        // keep the clear generation or increment once for the write, but an
        // unexpected counter or payload marker makes this lease unowned.
        if generation != cleared && generation != cleared.saturating_add(1) {
            return Err("Clipboard changed during native write; restoration suppressed".into());
        }
        if !payload.is_empty() {
            let items = msg!(board,"pasteboardItems"=>Id);
            if items.is_null() || msg!(items,"count"=>usize) != payload.len() {
                return Err("Clipboard ownership changed during write".into());
            }
            let first = msg!(items,"objectAtIndex:"=>Id,0usize=>usize);
            let data = msg!(first,"dataForType:"=>Id,marker.0 as Id=>Id);
            if data.is_null() || msg!(data,"length"=>usize) != token.len() {
                return Err("Clipboard ownership marker was replaced".into());
            }
            let bytes = msg!(data,"bytes"=>*const u8);
            if bytes.is_null() || std::slice::from_raw_parts(bytes, token.len()) != token.as_bytes()
            {
                return Err("Clipboard ownership marker does not belong to this write".into());
            }
        }
        if msg!(board,"changeCount"=>isize) != generation {
            return Err("Clipboard changed while verifying write ownership".into());
        }
        *claimed = Some(generation);
        Ok(generation)
    }
}
struct ClipboardLease {
    original: Payload,
    generation: isize,
    written: bool,
}
impl ClipboardLease {
    fn new() -> Result<Self, String> {
        let (generation, original) = snapshot()?;
        Ok(Self {
            original,
            generation,
            written: false,
        })
    }
    fn write(&mut self, payload: &Payload) -> Result<(), String> {
        let mut claimed = None;
        let result = write_clipboard(payload, self.generation, &mut claimed);
        if let Some(generation) = claimed {
            self.generation = generation;
            self.written = true;
        }
        result.map(|_| ())
    }
    fn restore(&mut self) -> Result<(), String> {
        if self.written && clipboard_count() == self.generation {
            let mut claimed = None;
            let result = write_clipboard(&self.original, self.generation, &mut claimed);
            if let Some(generation) = claimed {
                self.generation = generation;
            }
            result?;
        }
        self.written = false;
        Ok(())
    }
}
impl Drop for ClipboardLease {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
fn payload(operation: &Operation) -> Result<Option<Payload>, String> {
    let simple = |kind: &str, bytes: Vec<u8>| Some(vec![vec![(kind.into(), bytes)]]);
    Ok(match operation {
        Operation::Text {
            text,
            method: OutputMethod::Paste,
        } => simple("public.utf8-plain-text", text.as_bytes().to_vec()),
        Operation::Html { html, plain_text } => Some(vec![vec![
            ("public.html".into(), html.as_bytes().to_vec()),
            (
                "public.utf8-plain-text".into(),
                plain_text.as_bytes().to_vec(),
            ),
        ]]),
        Operation::PasteFiles { paths } => {
            let mut items = vec![];
            for path in paths {
                let canonical =
                    std::fs::canonicalize(path).map_err(|_| "Clipboard file is missing")?;
                unsafe {
                    let _pool = Pool::new();
                    let name = StringRef::new(&canonical.to_string_lossy());
                    let url = msg!(class("NSURL"),"fileURLWithPath:"=>Id,name.0 as Id=>Id);
                    if url.is_null() {
                        return Err("Invalid file URL".into());
                    }
                    let value = string(msg!(url,"absoluteString"=>CF));
                    items.push(vec![("public.file-url".into(), value.into_bytes())]);
                }
            }
            Some(items)
        }
        Operation::PasteImage { path } => {
            let reader = image::ImageReader::open(path)
                .map_err(|e| e.to_string())?
                .with_guessed_format()
                .map_err(|e| e.to_string())?;
            let (w, h) = reader.into_dimensions().map_err(|e| e.to_string())?;
            if w as u64 * h as u64 > 16_000_000 {
                return Err("Image exceeds 16 million pixels".into());
            }
            let image = image::open(path).map_err(|e| e.to_string())?;
            let mut bytes = std::io::Cursor::new(Vec::new());
            image
                .write_to(&mut bytes, image::ImageFormat::Png)
                .map_err(|e| e.to_string())?;
            simple("public.png", bytes.into_inner())
        }
        _ => None,
    })
}
fn preflight_open(target: &str, args: &[String], directory: &Option<String>) -> Result<(), String> {
    if target.starts_with("http://") || target.starts_with("https://") {
        if !args.is_empty() || directory.is_some() {
            return Err("URL launch cannot have arguments or a working directory".into());
        }
    } else {
        let path = std::path::Path::new(target);
        if !path.is_absolute() || !path.exists() {
            return Err("Open target must be an existing absolute path or HTTP(S) URL".into());
        }
        if !args.is_empty() && path.extension().is_none_or(|s| s != "app") {
            use std::os::unix::fs::PermissionsExt;
            if !path.is_file()
                || std::fs::metadata(path)
                    .map_err(|e| e.to_string())?
                    .permissions()
                    .mode()
                    & 0o111
                    == 0
            {
                return Err("Arguments require a .app bundle or executable file".into());
            }
        }
        if path.extension().is_some_and(|s| s == "app") && directory.is_some() {
            return Err("macOS open cannot guarantee a .app working directory".into());
        }
    }
    if directory
        .as_ref()
        .is_some_and(|p| !std::path::Path::new(p).is_dir())
    {
        return Err("Working directory does not exist".into());
    }
    Ok(())
}
pub fn preflight(plan: &ActionPlan) -> Result<(), String> {
    let errors = validate_plan(plan);
    if !errors.is_empty() {
        return Err(errors
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
            .join("; "));
    }
    for step in &plan.steps {
        match &step.operation {
            Operation::KeyDown { key } | Operation::KeyUp { key } | Operation::KeyPress { key } => {
                keycode(key)?;
            }
            Operation::Hotkey { keys } => {
                if keys.len() > 1
                    && keys
                        .iter()
                        .any(|key| keycode(key).is_ok_and(|code| code >= 256))
                {
                    return Err(
                        "macOS media/system keys cannot be combined with ordinary modifiers".into(),
                    );
                }
                for key in keys {
                    keycode(key)?;
                }
            }
            Operation::Common { function } => {
                common_keys(function)?;
            }
            Operation::Open {
                target,
                args,
                working_directory,
            } => preflight_open(target, args, working_directory)?,
            Operation::Reusable { .. } => {
                return Err("Reusable actions must be expanded before execution".into())
            }
            Operation::PasteFiles { paths } => {
                for p in paths {
                    if !std::path::Path::new(p).exists() {
                        return Err("Clipboard file is missing".into());
                    }
                }
            }
            Operation::PasteImage { path } => {
                let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
                if size > 32_000_000 {
                    return Err("Image exceeds 32 MB".into());
                }
                payload(&step.operation)?;
            }
            _ => {}
        }
    }
    Ok(())
}
fn guarded(s: &Shared, target: &Foreground, generation: u64, revision: u64) -> Result<(), String> {
    if s.cancel.load(Ordering::SeqCst) || s.shutdown.load(Ordering::SeqCst) {
        return Err("Cancelled".into());
    }
    native_ready()?;
    if s.generation.load(Ordering::SeqCst) != generation {
        return Err("Physical input or focus changed; execution stopped".into());
    }
    if s.config.lock().unwrap().revision != revision {
        return Err("Active revision changed; execution stopped".into());
    }
    let now = foreground();
    if target.id == 0 || now.id != target.id {
        return Err("Focused application/window/control changed or cannot be verified".into());
    }
    if !layout_safe() {
        return Err(
            "IME/composition cannot be safely observed; choose a keyboard-layout input source"
                .into(),
        );
    }
    // The trigger's last character may be typed with Shift still held: give
    // the modifiers a moment to come up before refusing.
    if !crate::macos_rules::released_within(MODIFIER_RELEASE_WAIT, || {
        [54, 55, 56, 58, 59, 60, 61, 62].into_iter().any(physical)
    }) {
        return Err("A physical modifier is held; execution stopped".into());
    }
    Ok(())
}
fn mouse_event(button: MouseButton, down: bool, click: i64) -> Result<(), String> {
    native_ready()?;
    unsafe {
        let code = mouse_code(button);
        let kind = match (code, down) {
            (0, true) => 1,
            (0, false) => 2,
            (1, true) => 3,
            (1, false) => 4,
            (_, true) => 25,
            (_, false) => 26,
        };
        let event = CGEventCreateMouseEvent(ptr::null(), kind, location()?, code);
        if !event.is_null() {
            CGEventSetIntegerValueField(event, 1, click);
        }
        post(event)
    }
}
fn open(target: &str, args: &[String], directory: &Option<String>) -> Result<(), String> {
    preflight_open(target, args, directory)?;
    let path = std::path::Path::new(target);
    let mut command = if path.is_file() && !args.is_empty() {
        let mut command = Command::new(target);
        command.args(args);
        command
    } else {
        let mut command = Command::new("/usr/bin/open");
        command.arg(target);
        if !args.is_empty() {
            command.arg("--args").args(args);
        }
        command
    };
    if let Some(dir) = directory {
        command.current_dir(dir);
    }
    command.spawn().map_err(|e| format!("Launch failed: {e}"))?;
    Ok(())
}
pub fn execute(
    s: Arc<Shared>,
    plan: ActionPlan,
    mut target: Foreground,
    generation: u64,
    erase: usize,
    terminator: Option<String>,
    speed: f64,
) -> Result<ExecutionResult, String> {
    // Action::Replay was scaled during domain expansion; do not divide twice.
    if !speed.is_finite() || speed <= 0.0 {
        return Err("Replay speed must be positive and finite".into());
    }
    preflight(&plan)?;
    let revision = s.config.lock().unwrap().revision;
    guarded(&s, &target, generation, revision)?;
    let mut payloads = BTreeMap::new();
    for (index, step) in plan.steps.iter().enumerate() {
        if let Some(payload) = payload(&step.operation)? {
            payloads.insert(index, payload);
        }
    }
    let mut clipboard = if payloads.is_empty() {
        None
    } else {
        Some(ClipboardLease::new()?)
    };
    // The first paste is written before the trigger is erased only when
    // every earlier step just types text (Windows `written_before_erase`):
    // a key, chord or click before it may make the target copy or cut.
    let early = payloads
        .first_key_value()
        .map(|(index, _)| *index)
        .filter(|index| crate::macos_rules::written_before_erase(&plan, *index));
    if let Some(index) = early {
        clipboard.as_mut().unwrap().write(&payloads[&index])?;
    }
    guarded(&s, &target, generation, revision)?;
    let mut completed = 0;
    let mut effects = false;
    let mut failure = None;
    let mut held = BTreeSet::new();
    let mut buttons = BTreeSet::new();
    let mut wheel = crate::macos_rules::WheelSteps::default();
    let started = Instant::now();
    for _ in 0..erase {
        if started.elapsed() > Duration::from_secs(120) {
            failure = Some("Execution timeout".into());
            break;
        }
        match guarded(&s, &target, generation, revision).and_then(|_| press(51, &held)) {
            Ok(()) => effects = true,
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    if failure.is_none() {
        for (index, step) in plan.steps.iter().enumerate() {
            if started.elapsed() > Duration::from_secs(120) {
                failure = Some("Execution timeout".into());
                break;
            }
            if let TargetPolicy::Declared { process } = &step.target {
                let next = foreground();
                if next.id == 0 || next.process != *process {
                    failure =
                        Some("Declared bundle id is not the current foreground application".into());
                    break;
                }
                target = next;
            }
            if let Err(e) = guarded(&s, &target, generation, revision) {
                failure = Some(e);
                break;
            }
            let mut step_effects = false;
            let result = (|| -> Result<(), String> {
                match &step.operation {
                    Operation::Text {
                        text,
                        method: OutputMethod::Keys,
                    } => {
                        let mut chars = text.chars().peekable();
                        while let Some(ch) = chars.next() {
                            if started.elapsed() > Duration::from_secs(120) {
                                return Err("Execution timeout".into());
                            }
                            guarded(&s, &target, generation, revision)?;
                            // Line breaks and tabs are typed as Return and Tab, as on
                            // Windows: `\r\n` is one Return, and forms submit or move on.
                            let key = match ch {
                                '\r' => {
                                    chars.next_if_eq(&'\n');
                                    Some(36)
                                }
                                '\n' => Some(36),
                                '\t' => Some(48),
                                _ => None,
                            };
                            if let Some(code) = key {
                                step_effects = true;
                                send_key(code, true, &held)?;
                                send_key(code, false, &held)?;
                                continue;
                            }
                            let utf16 = ch.to_string().encode_utf16().collect::<Vec<_>>();
                            unsafe {
                                let event = CGEventCreateKeyboardEvent(ptr::null(), 0, true);
                                if event.is_null() {
                                    return Err("CGEvent allocation failed".into());
                                }
                                CGEventKeyboardSetUnicodeString(event, utf16.len(), utf16.as_ptr());
                                CGEventSetFlags(
                                    event,
                                    held.iter().fold(0, |f, c| f | modifier_flag(*c)),
                                );
                                post(event)?;
                                step_effects = true;
                                let event = CGEventCreateKeyboardEvent(ptr::null(), 0, false);
                                if event.is_null() {
                                    return Err("CGEvent allocation failed".into());
                                }
                                CGEventKeyboardSetUnicodeString(event, utf16.len(), utf16.as_ptr());
                                post(event)?;
                            }
                        }
                    }
                    Operation::Text {
                        method: OutputMethod::Paste,
                        ..
                    }
                    | Operation::Html { .. }
                    | Operation::PasteFiles { .. }
                    | Operation::PasteImage { .. } => {
                        let lease = clipboard.as_mut().unwrap();
                        if Some(index) != early {
                            lease.write(&payloads[&index])?;
                        }
                        if clipboard_count() != lease.generation {
                            return Err(
                                "Clipboard changed before paste; user's new content preserved"
                                    .into(),
                            );
                        }
                        guarded(&s, &target, generation, revision)?;
                        step_effects = true;
                        hotkey(&[Key::PlatformPrimary, Key::named("V")], &held)?;
                        // The user's restore delay (B-08), as on Windows.
                        std::thread::sleep(crate::clipboard_options::lease_options().restore_delay);
                    }
                    Operation::KeyDown { key } => {
                        let code = keycode(key)?;
                        if physical(code) || held.contains(&code) {
                            return Err("Requested key is already held".into());
                        }
                        held.insert(code);
                        step_effects = true;
                        send_key(code, true, &held)?;
                    }
                    Operation::KeyUp { key } => {
                        let code = keycode(key)?;
                        if !held.contains(&code) || physical(code) {
                            return Err("Cannot release an unowned or physically held key".into());
                        }
                        let mut remaining = held.clone();
                        remaining.remove(&code);
                        step_effects = true;
                        send_key(code, false, &remaining)?;
                        held.remove(&code);
                    }
                    Operation::KeyPress { key } => {
                        step_effects = true;
                        press(keycode(key)?, &held)?;
                    }
                    Operation::Hotkey { keys } => {
                        step_effects = true;
                        hotkey(keys, &held)?;
                    }
                    Operation::Delay { ms } => {
                        let deadline = Instant::now() + Duration::from_millis(*ms);
                        while Instant::now() < deadline {
                            guarded(&s, &target, generation, revision)?;
                            if started.elapsed() > Duration::from_secs(120) {
                                return Err("Execution timeout".into());
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    Operation::MouseMove {
                        x,
                        y,
                        relative,
                        percent,
                    } => {
                        let mut p = Point { x: *x, y: *y };
                        if *relative {
                            let at = location()?;
                            p.x += at.x;
                            p.y += at.y;
                        } else if *percent {
                            unsafe {
                                let r = CGDisplayBounds(CGMainDisplayID());
                                p.x = r.origin.x + x * r.size.width / 100.;
                                p.y = r.origin.y + y * r.size.height / 100.;
                            }
                        }
                        if !p.x.is_finite() || !p.y.is_finite() {
                            return Err("Mouse coordinate is not finite".into());
                        }
                        // A drag while the task holds a button (Quartz needs
                        // the drag type for selections and moves).
                        let (kind, button) = crate::macos_rules::move_event(&buttons);
                        step_effects = true;
                        unsafe {
                            post(CGEventCreateMouseEvent(ptr::null(), kind, p, button))?;
                        }
                    }
                    Operation::MouseButton { button, state } => {
                        let code = mouse_code(*button);
                        if physical_button(code) {
                            return Err("Physical mouse button conflicts with action".into());
                        }
                        match state {
                            ButtonState::Down => {
                                if !buttons.insert(code) {
                                    return Err("Mouse button already task-held".into());
                                }
                                step_effects = true;
                                mouse_event(*button, true, 1)?;
                            }
                            ButtonState::Up => {
                                if !buttons.contains(&code) {
                                    return Err("Cannot release an unowned mouse button".into());
                                }
                                step_effects = true;
                                mouse_event(*button, false, 1)?;
                                buttons.remove(&code);
                            }
                            ButtonState::Click | ButtonState::DoubleClick => {
                                if buttons.contains(&code) {
                                    return Err("Mouse button already task-held".into());
                                }
                                for click in 1..=if *state == ButtonState::DoubleClick {
                                    2
                                } else {
                                    1
                                } {
                                    buttons.insert(code);
                                    step_effects = true;
                                    mouse_event(*button, true, click)?;
                                    mouse_event(*button, false, click)?;
                                    buttons.remove(&code);
                                }
                            }
                        }
                    }
                    Operation::Wheel { axis, amount, unit } => {
                        // Windows deltas (1/120 notch) become line steps; a
                        // remainder carries over to the next wheel step.
                        let value = wheel.lines(*axis, *amount, *unit);
                        if value == 0 {
                            return Ok(());
                        }
                        step_effects = true;
                        unsafe {
                            post(CGEventCreateScrollWheelEvent(
                                ptr::null(),
                                1,
                                2,
                                if *axis == WheelAxis::Vertical {
                                    value
                                } else {
                                    0
                                },
                                if *axis == WheelAxis::Horizontal {
                                    value
                                } else {
                                    0
                                },
                            ))?;
                        }
                    }
                    Operation::Open {
                        target,
                        args,
                        working_directory,
                    } => {
                        step_effects = true;
                        open(target, args, working_directory)?;
                    }
                    Operation::Common { function } => {
                        step_effects = true;
                        hotkey(&common_keys(function)?, &held)?;
                    }
                    Operation::Reusable { .. } => return Err("Unexpanded reusable action".into()),
                }
                Ok(())
            })();
            effects |= step_effects;
            match result {
                Ok(()) => completed += 1,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
    }
    for code in held.clone() {
        if !physical(code) {
            held.remove(&code);
            if let Err(e) = send_key(code, false, &held) {
                failure.get_or_insert(e);
            }
        } else {
            failure.get_or_insert(
                "Physical key conflicts with cleanup; release the key manually".into(),
            );
        }
    }
    for code in buttons {
        if !physical_button(code) {
            if let Some(button) = mouse_name(code) {
                if let Err(e) = mouse_event(button, false, 1) {
                    failure.get_or_insert(e);
                }
            }
        } else {
            failure.get_or_insert(
                "Physical mouse button conflicts with cleanup; release it manually".into(),
            );
        }
    }
    if failure.is_none() {
        if let Some(term) = terminator {
            if let Err(e) =
                guarded(&s, &target, generation, revision).and_then(|_| send_terminator(&term))
            {
                failure = Some(e)
            } else {
                effects = true
            }
        }
    }
    if let Some(lease) = clipboard.as_mut() {
        if let Err(e) = lease.restore() {
            failure = Some(format!("Clipboard restoration failed: {e}"));
        }
    }
    let result = ExecutionResult {
        id: format!("run-{generation}"),
        status: if failure.as_deref() == Some("Cancelled") {
            "cancelled"
        } else if failure.is_some() {
            "failed"
        } else if !effects {
            "succeeded"
        } else {
            "unknown"
        }
        .into(),
        message: failure.unwrap_or_else(|| {
            if effects {
                "CGEvent sent; target consumption cannot be confirmed".into()
            } else {
                "No external action was required".into()
            }
        }),
        completed_steps: completed,
        side_effects: effects,
    };
    *s.last.lock().unwrap() = Some(result.clone());
    Ok(result)
}
