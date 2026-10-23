#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("The native input fixture requires Windows.");
}

#[cfg(windows)]
mod fixture {
    use std::{ffi::c_void, path::PathBuf, ptr::null_mut};
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::{GetModuleHandleW, LoadLibraryW},
        UI::{Input::KeyboardAndMouse::SetFocus, WindowsAndMessaging::*},
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    struct Field {
        name: &'static str,
        handle: HWND,
        selection: bool,
    }

    struct State {
        path: Option<PathBuf>,
        fields: Vec<Field>,
        last_json: String,
    }

    impl State {
        unsafe fn save(&mut self) {
            let Some(path) = &self.path else { return };
            let mut controls = serde_json::Map::new();
            let mut selections = serde_json::Map::new();
            for field in &self.fields {
                // Read only child controls created by this application.
                let length = GetWindowTextLengthW(field.handle).max(0) as usize;
                let mut buffer = vec![0_u16; length + 1];
                let copied = GetWindowTextW(field.handle, buffer.as_mut_ptr(), buffer.len() as i32);
                let value = String::from_utf16_lossy(&buffer[..copied.max(0) as usize]);
                controls.insert(
                    field.name.into(),
                    if field.name == "password" {
                        serde_json::json!({ "length": copied.max(0) })
                    } else {
                        serde_json::json!(value)
                    },
                );
                if field.selection {
                    let (mut start, mut end) = (0_u32, 0_u32);
                    SendMessageW(
                        field.handle,
                        0x00B0, /* EM_GETSEL */
                        &mut start as *mut u32 as WPARAM,
                        &mut end as *mut u32 as LPARAM,
                    );
                    selections.insert(
                        field.name.into(),
                        serde_json::json!({"start": start, "end": end}),
                    );
                }
            }
            let json = serde_json::to_string_pretty(&serde_json::json!({
                "fixture": "Quicktext Native Input Fixture",
                "controls": controls,
                "selections": selections
            }))
            .unwrap();
            if json != self.last_json && std::fs::write(path, &json).is_ok() {
                self.last_json = json;
            }
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        w: WPARAM,
        l: LPARAM,
    ) -> LRESULT {
        match message {
            WM_TIMER => {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
                if let Some(state) = state.as_mut() {
                    state.save();
                }
                0
            }
            WM_DESTROY => {
                KillTimer(hwnd, 1);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, message, w, l),
        }
    }

    pub unsafe fn run() {
        let path = std::env::args_os().nth(1).map(PathBuf::from);
        if let Some(parent) = path
            .as_ref()
            .and_then(|p| p.parent())
            .filter(|p| !p.as_os_str().is_empty())
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let instance = GetModuleHandleW(std::ptr::null());
        assert!(
            !LoadLibraryW(wide("Msftedit.dll").as_ptr()).is_null(),
            "Load Msftedit.dll"
        );
        let class = wide("QuicktextInputFixtureClass");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (16_usize) as _,
            ..std::mem::zeroed()
        };
        assert_ne!(RegisterClassW(&wc), 0, "Register fixture class");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("Quicktext Native Input Fixture").as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            800,
            730,
            null_mut(),
            null_mut(),
            instance,
            std::ptr::null::<c_void>(),
        );
        assert!(!hwnd.is_null(), "Create fixture window");
        let specs = [
            (
                "singleLine",
                "1. Single-line TextBox",
                "EDIT",
                20,
                30,
                ES_AUTOHSCROLL as u32,
                true,
            ),
            (
                "multiLine",
                "2. Multi-line TextBox",
                "EDIT",
                70,
                140,
                (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32 | WS_VSCROLL,
                true,
            ),
            (
                "richText",
                "3. RichTextBox",
                "RICHEDIT50W",
                230,
                140,
                (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32 | WS_VSCROLL,
                true,
            ),
            (
                "editableCombo",
                "4. Editable ComboBox",
                "COMBOBOX",
                390,
                140,
                CBS_DROPDOWN as u32 | CBS_AUTOHSCROLL as u32,
                false,
            ),
            (
                "password",
                "5. Password TextBox",
                "EDIT",
                440,
                30,
                (ES_PASSWORD | ES_AUTOHSCROLL) as u32,
                true,
            ),
            (
                "readOnly",
                "6. Read-only TextBox",
                "EDIT",
                490,
                30,
                (ES_READONLY | ES_AUTOHSCROLL) as u32,
                true,
            ),
            (
                "disabled",
                "7. Disabled TextBox",
                "EDIT",
                540,
                30,
                ES_AUTOHSCROLL as u32 | WS_DISABLED,
                true,
            ),
        ];
        let mut state = Box::new(State {
            path,
            fields: Vec::new(),
            last_json: String::new(),
        });
        for (index, (name, caption, kind, y, height, style, selection)) in
            specs.into_iter().enumerate()
        {
            CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide(caption).as_ptr(),
                WS_CHILD | WS_VISIBLE,
                18,
                y + 5,
                210,
                25,
                hwnd,
                null_mut(),
                instance,
                std::ptr::null(),
            );
            let initial = if name == "readOnly" {
                "Synthetic read-only fixture text."
            } else {
                ""
            };
            let handle = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                wide(kind).as_ptr(),
                wide(initial).as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | style,
                240,
                y,
                520,
                height,
                hwnd,
                (index + 100) as _,
                instance,
                std::ptr::null(),
            );
            assert!(!handle.is_null(), "Create {name}");
            state.fields.push(Field {
                name,
                handle,
                selection,
            });
        }
        CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide("Evidence captures only this window's controls. Password: length only.").as_ptr(),
            WS_CHILD | WS_VISIBLE,
            18,
            600,
            740,
            45,
            hwnd,
            null_mut(),
            instance,
            std::ptr::null(),
        );
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *state as *mut State) as isize);
        state.save();
        SetTimer(hwnd, 1, 100, None);
        ShowWindow(hwnd, SW_SHOW);
        SetFocus(state.fields[0].handle);
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

#[cfg(windows)]
fn main() {
    unsafe {
        fixture::run();
    }
}
