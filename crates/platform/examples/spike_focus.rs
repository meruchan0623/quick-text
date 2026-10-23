//! S-29 spike, item 1: getting focus for a popup and giving it back (B-31).
//!
//! RESEARCH CODE. It is compiled so that the techniques are written down
//! precisely, but it must NOT be run casually. With the confirmation flag it
//! creates visible windows, injects a synthetic Alt key press, moves the
//! foreground window and can make taskbar buttons flash. Without
//! `--i-understand-this-steals-focus` it only prints an explanation and exits
//! (exit code 2); `cargo build --examples` and CI never run it.
//!
//! The three approaches compared (plus a plain control):
//!
//! * `alt`        a marked synthetic Alt tap, then `SetForegroundWindow`.
//! * `attach`     `AttachThreadInput` to the thread that owns input, then
//!   `SetForegroundWindow` + `SetFocus`, then detach.
//! * `noactivate` never take focus: a `WS_EX_NOACTIVATE` overlay shown with
//!   `SW_SHOWNOACTIVATE`. Fallback only (cannot receive typing).
//! * `plain`      `SetForegroundWindow` alone. Control group.
//!
//! Output names only the process image (never a window title).
//! Analysis, preconditions and the checklist: docs/spikes/s-29.md.

const CONFIRM_FLAG: &str = "--i-understand-this-steals-focus";

fn explanation() -> String {
    format!(
        "spike_focus (S-29 item 1, B-31) - research code for the focus-return spike.\n\
         \n\
         With {CONFIRM_FLAG} this program:\n\
         \x20 - creates small topmost windows on your desktop,\n\
         \x20 - sends a synthetic Alt key press (some apps react: Word shows key tips,\n\
         \x20   Notepad highlights its menu bar),\n\
         \x20 - tries to take the foreground window away from whatever you are using and\n\
         \x20   to hand it back, which can flash taskbar buttons or leave focus elsewhere.\n\
         \n\
         Run it only on a disposable test desktop or VM, with no unsaved work open.\n\
         It reads no window titles and injects no text.\n\
         \n\
         Usage (after confirming):\n\
         \x20 spike_focus {CONFIRM_FLAG} [--approach plain|alt|attach|noactivate|all]\n\
         \x20   [--wait-secs N]   seconds to focus the target app before each run (default 5)\n\
         \x20   [--hold-ms N]     how long the popup keeps focus (default 800)\n\
         \x20   [--settle-ms N]   how long to wait for the foreground to change (default 200, B-31)\n\
         \x20   [--hide-first]    hide the popup before the return step instead of after\n\
         \x20   [--no-alt-mask]   send a bare Alt tap instead of Alt + unassigned key\n\
         \n\
         Nothing was done. Re-run with the flag if you really mean it.\n"
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The gate comes first and is not conditional on anything else.
    if !args.iter().any(|a| a == CONFIRM_FLAG) {
        print!("{}", explanation());
        std::process::exit(2);
    }
    run_confirmed(&args);
}

#[cfg(not(windows))]
fn run_confirmed(_args: &[String]) {
    eprintln!("spike_focus requires Windows.");
}

#[cfg(windows)]
fn run_confirmed(args: &[String]) {
    match imp::parse_options(args) {
        Ok(options) => imp::run(&options),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::mem::{size_of, zeroed};
    use std::ptr::{null, null_mut};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HWND, LPARAM, LRESULT, WPARAM,
    };
    use windows_sys::Win32::Graphics::Gdi::COLOR_HIGHLIGHT;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::{
        AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, SetFocus, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
        VK_MENU,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GetForegroundWindow, GetGUIThreadInfo, GetWindowLongW, GetWindowThreadProcessId,
        LoadCursorW, PeekMessageW, RegisterClassW, SetForegroundWindow, ShowWindow,
        TranslateMessage, GUITHREADINFO, GWL_EXSTYLE, IDC_ARROW, MA_NOACTIVATE, MSG, PM_REMOVE,
        SW_HIDE, SW_SHOWNOACTIVATE, WM_MOUSEACTIVATE, WNDCLASSW, WS_BORDER, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    /// Marks self-injected input so the engine's hook ignores it (B-07).
    const MARK: usize = 0x51545854;
    /// 0xE8 is "Unassigned" in the Virtual-Key Codes table. Tapped between Alt down and
    /// Alt up it turns the gesture into "Alt + key", which should not activate menu mode.
    const VK_UNASSIGNED: u16 = 0xE8;

    // ---------------------------------------------------------------------
    // Options
    // ---------------------------------------------------------------------

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Approach {
        Plain,
        AltThenForeground,
        AttachThenForeground,
        NoActivate,
    }

    impl Approach {
        fn name(self) -> &'static str {
            match self {
                Approach::Plain => "plain SetForegroundWindow (control)",
                Approach::AltThenForeground => "synthetic Alt, then SetForegroundWindow",
                Approach::AttachThenForeground => {
                    "AttachThreadInput, then SetForegroundWindow + SetFocus"
                }
                Approach::NoActivate => "WS_EX_NOACTIVATE overlay (never takes focus)",
            }
        }
    }

    pub struct Options {
        approaches: Vec<Approach>,
        wait: Duration,
        hold: Duration,
        settle: Duration,
        hide_first: bool,
        mask_alt: bool,
    }

    pub fn parse_options(args: &[String]) -> Result<Options, String> {
        let mut options = Options {
            approaches: vec![
                Approach::Plain,
                Approach::AltThenForeground,
                Approach::AttachThenForeground,
                Approach::NoActivate,
            ],
            wait: Duration::from_secs(5),
            hold: Duration::from_millis(800),
            settle: Duration::from_millis(200),
            hide_first: false,
            mask_alt: true,
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut number = |name: &str| -> Result<u64, String> {
                it.next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| format!("{name} needs a non-negative integer"))
            };
            match arg.as_str() {
                "--i-understand-this-steals-focus" => {}
                "--hide-first" => options.hide_first = true,
                "--no-alt-mask" => options.mask_alt = false,
                "--wait-secs" => options.wait = Duration::from_secs(number("--wait-secs")?),
                "--hold-ms" => options.hold = Duration::from_millis(number("--hold-ms")?),
                "--settle-ms" => options.settle = Duration::from_millis(number("--settle-ms")?),
                "--approach" => {
                    let value = it.next().ok_or("--approach needs a value")?;
                    options.approaches = match value.as_str() {
                        "plain" => vec![Approach::Plain],
                        "alt" => vec![Approach::AltThenForeground],
                        "attach" => vec![Approach::AttachThenForeground],
                        "noactivate" => vec![Approach::NoActivate],
                        "all" => options.approaches,
                        other => return Err(format!("unknown approach {other}")),
                    };
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(options)
    }

    // ---------------------------------------------------------------------
    // Small helpers (queries and message pumping)
    // ---------------------------------------------------------------------

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// Process image name only. Never a window title (titles can carry customer data).
    unsafe fn exe_name(pid: u32) -> String {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return "<unreadable>".into();
        }
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(process);
        if ok == 0 {
            return "<unreadable>".into();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().unwrap_or("").to_owned()
    }

    unsafe fn describe(hwnd: HWND) -> String {
        if hwnd.is_null() {
            return "none".into();
        }
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(hwnd, &mut pid);
        format!(
            "hwnd={:#x} pid={pid} tid={tid} exe={}",
            hwnd as usize,
            exe_name(pid)
        )
    }

    /// The control that has keyboard focus inside `thread` (the "FocusSnapshot" of B-31).
    unsafe fn focus_of(thread: u32) -> HWND {
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(thread, &mut info) == 0 {
            return null_mut();
        }
        info.hwndFocus
    }

    /// Dispatches this thread's messages for `span` so our own windows stay alive.
    unsafe fn pump(span: Duration) {
        let deadline = Instant::now() + span;
        while Instant::now() < deadline {
            let mut msg: MSG = zeroed();
            if PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    /// Polls until `want` is the foreground window. Returns the elapsed milliseconds.
    unsafe fn wait_foreground(want: HWND, limit: Duration) -> Option<u128> {
        let start = Instant::now();
        loop {
            if GetForegroundWindow() == want {
                return Some(start.elapsed().as_millis());
            }
            if start.elapsed() >= limit {
                return None;
            }
            pump(Duration::from_millis(5));
        }
    }

    // ---------------------------------------------------------------------
    // The popup (stand-in for the quick-search window / overlay)
    // ---------------------------------------------------------------------

    unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        // A click on a no-activate overlay must not activate it either.
        if msg == WM_MOUSEACTIVATE
            && (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_NOACTIVATE) != 0
        {
            return MA_NOACTIVATE as LRESULT;
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }

    const CLASS_NAME: &str = "QuicktextS29FocusSpike";

    unsafe fn register_class() -> bool {
        let class = wide(CLASS_NAME);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(popup_proc),
            hInstance: GetModuleHandleW(null()),
            lpszClassName: class.as_ptr(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_HIGHLIGHT as usize + 1) as _,
            ..zeroed()
        };
        RegisterClassW(&wc) != 0
    }

    /// A 320x96 bordered rectangle. `no_activate` adds `WS_EX_NOACTIVATE` (tao calls this
    /// `focusable(false)`); both variants are topmost tool windows (no taskbar button).
    unsafe fn create_popup(no_activate: bool) -> HWND {
        let class = wide(CLASS_NAME);
        let title = wide("S-29 focus spike popup");
        let mut ex_style = WS_EX_TOPMOST | WS_EX_TOOLWINDOW;
        if no_activate {
            ex_style |= WS_EX_NOACTIVATE;
        }
        CreateWindowExW(
            ex_style,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_BORDER,
            200,
            200,
            320,
            96,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        )
    }

    // ---------------------------------------------------------------------
    // The approaches
    // ---------------------------------------------------------------------

    struct Step {
        /// Return value of `SetForegroundWindow` (success does NOT prove the foreground changed).
        api_ok: bool,
        last_error: u32,
        detail: String,
    }

    fn key_event(vk: u16, up: bool) -> INPUT {
        // SAFETY: INPUT is plain data; all-zero is a valid value.
        let mut input: INPUT = unsafe { zeroed() };
        input.r#type = INPUT_KEYBOARD;
        input.Anonymous.ki = KEYBDINPUT {
            wVk: vk,
            wScan: 0,
            dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
            time: 0,
            dwExtraInfo: MARK,
        };
        input
    }

    /// Synthetic Alt tap. Returns how many events SendInput accepted. A short count means the
    /// injection failed; it would not say whether UIPI was the cause (the documentation says
    /// neither the return value nor `GetLastError` identifies UIPI), which is irrelevant here:
    /// the target is only a window, and this example does not compare integrity levels.
    unsafe fn alt_tap(mask: bool) -> u32 {
        let mut events = vec![key_event(VK_MENU, false)];
        if mask {
            events.push(key_event(VK_UNASSIGNED, false));
            events.push(key_event(VK_UNASSIGNED, true));
        }
        events.push(key_event(VK_MENU, true));
        SendInput(
            events.len() as u32,
            events.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    }

    /// Control: what a background process gets without any trick.
    unsafe fn plain(dest: HWND) -> Step {
        let api_ok = SetForegroundWindow(dest) != 0;
        Step {
            api_ok,
            last_error: GetLastError(),
            detail: String::new(),
        }
    }

    /// Approach 1: LockSetForegroundWindow documents that "the system automatically enables
    /// calls to SetForegroundWindow if the user presses the ALT key". Whether a SendInput
    /// Alt counts as "the user pressing ALT" is not documented (open, docs/spikes/s-29.md).
    /// tao's Window::set_focus does this on failure with an unmarked, unmasked Alt (so the
    /// engine's own hook may see it; B-31 asks for MARK).
    unsafe fn alt_then_foreground(dest: HWND, mask: bool) -> Step {
        // Never fight a physically held Alt: our Alt-up would release the user's key.
        let held = GetAsyncKeyState(i32::from(VK_MENU)) < 0;
        let sent = if held { 0 } else { alt_tap(mask) };
        let api_ok = SetForegroundWindow(dest) != 0;
        Step {
            api_ok,
            last_error: GetLastError(),
            detail: format!("alt_held={held} events_accepted={sent} masked={mask}"),
        }
    }

    /// Approach 2: share the input state of the thread that owns the focus. Preconditions
    /// (AttachThreadInput documentation): both threads need a message queue (created on a
    /// thread's first USER/GDI call); it fails when a journal record hook is installed, for a
    /// system thread, or across desktops; key state (GetKeyState / GetKeyboardState) is reset,
    /// so a held modifier can appear released. The function page says nothing about integrity
    /// levels; that limit (attaching up needs UIAccess) comes from "Windows Integrity
    /// Mechanism Design" and is the open question C.2 #8. Always detach again.
    unsafe fn attach_then_foreground(dest: HWND, owner_thread: u32, focus_child: HWND) -> Step {
        let me = GetCurrentThreadId();
        let mut detail = String::new();
        let attached = owner_thread != 0 && owner_thread != me;
        let mut attach_error = 0;
        let mut did_attach = false;
        if attached {
            did_attach = AttachThreadInput(me, owner_thread, 1) != 0;
            if !did_attach {
                attach_error = GetLastError();
            }
        }
        let api_ok = SetForegroundWindow(dest) != 0;
        let last_error = GetLastError();
        BringWindowToTop(dest);
        if did_attach && !focus_child.is_null() {
            // Only valid while attached: SetFocus targets a window of another thread's queue.
            let previous = SetFocus(focus_child);
            detail.push_str(&format!("setfocus_prev_null={} ", previous.is_null()));
        }
        if did_attach {
            AttachThreadInput(me, owner_thread, 0);
        }
        detail.push_str(&format!(
            "attach_needed={attached} attached={did_attach} attach_error={attach_error}"
        ));
        Step {
            api_ok,
            last_error,
            detail,
        }
    }

    fn report(stage: &str, step: &Step, arrived: Option<u128>, settle: Duration) {
        let outcome = match arrived {
            Some(ms) => format!("foreground reached after {ms} ms"),
            None => format!("foreground NOT reached within {} ms", settle.as_millis()),
        };
        println!(
            "  [{stage}] SetForegroundWindow={} last_error={} {} -> {outcome}",
            step.api_ok, step.last_error, step.detail
        );
    }

    // ---------------------------------------------------------------------
    // The scenario
    // ---------------------------------------------------------------------

    unsafe fn run_step(
        approach: Approach,
        dest: HWND,
        owner_thread: u32,
        focus_child: HWND,
        options: &Options,
    ) -> Step {
        match approach {
            Approach::Plain => plain(dest),
            Approach::AltThenForeground => alt_then_foreground(dest, options.mask_alt),
            Approach::AttachThenForeground => {
                attach_then_foreground(dest, owner_thread, focus_child)
            }
            Approach::NoActivate => Step {
                api_ok: false,
                last_error: 0,
                detail: "not applicable".into(),
            },
        }
    }

    unsafe fn scenario(approach: Approach, options: &Options) {
        println!("=== {} ===", approach.name());
        println!(
            "  focus the TARGET application now ({} s)",
            options.wait.as_secs()
        );
        std::thread::sleep(options.wait);

        let target = GetForegroundWindow();
        if target.is_null() {
            println!("  no foreground window; skipped");
            return;
        }
        let target_thread = GetWindowThreadProcessId(target, null_mut());
        let focus_before = focus_of(target_thread);
        println!("  target: {}", describe(target));
        println!("  target focus control: {:#x}", focus_before as usize);

        let popup = create_popup(approach == Approach::NoActivate);
        if popup.is_null() {
            println!("  CreateWindowExW failed (error {})", GetLastError());
            return;
        }
        ShowWindow(popup, SW_SHOWNOACTIVATE);

        if approach == Approach::NoActivate {
            // Nothing to acquire and nothing to return: the question is only whether the
            // target stays foreground while the overlay is up, including after a click.
            println!(
                "  overlay shown; click it during the next {} ms to test WM_MOUSEACTIVATE",
                options.hold.as_millis()
            );
            pump(options.hold);
            let unchanged = GetForegroundWindow() == target;
            println!("  [overlay] target still foreground: {unchanged}");
            DestroyWindow(popup);
            return;
        }

        let step = run_step(approach, popup, target_thread, null_mut(), options);
        let arrived = wait_foreground(popup, options.settle);
        report("acquire", &step, arrived, options.settle);
        pump(options.hold);

        if options.hide_first {
            ShowWindow(popup, SW_HIDE);
        }
        let step = run_step(approach, target, target_thread, focus_before, options);
        let arrived = wait_foreground(target, options.settle);
        report("return ", &step, arrived, options.settle);
        println!(
            "  [return ] target focus control restored: {}",
            focus_of(target_thread) == focus_before
        );
        DestroyWindow(popup);
    }

    pub fn run(options: &Options) {
        // SAFETY: plain Win32 calls on this thread; nothing is shared across threads.
        unsafe {
            if !register_class() {
                eprintln!("RegisterClassW failed (error {})", GetLastError());
                return;
            }
            println!(
                "spike_focus: confirmed run, {} approach(es)",
                options.approaches.len()
            );
            for approach in &options.approaches {
                scenario(*approach, options);
            }
        }
    }
}
