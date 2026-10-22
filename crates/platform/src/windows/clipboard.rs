//! Clipboard lease over the [`ClipboardBackend`] port: backup, verified
//! writes and verified restore, plus the Windows adapter.
use super::inject::mark_external_effect;
use super::*;
use crate::traits::ClipboardBackend;
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

// no-regression #2: a paste whose clipboard lease fails never downgrades to typed keys.
pub(super) fn acquire_clipboard<T>(
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
fn html_clipboard(fragment: &str) -> String {
    let prefix = "<html><body><!--StartFragment-->";
    let suffix = "<!--EndFragment--></body></html>";
    let head="Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
    let start = head.len();
    let start_fragment = start + prefix.len();
    format!("Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n{}{}{}\0",start,start+prefix.len()+fragment.len()+suffix.len(),start_fragment,start_fragment+fragment.len(),prefix,fragment,suffix)
}
type ClipboardFormats = Vec<(u32, Vec<u8>)>;
pub(super) fn clipboard_formats(op: &Operation) -> Result<Option<ClipboardFormats>, String> {
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
/// Production [`ClipboardBackend`]: a message-only owner window per lease.
pub(super) struct WinClipboard {
    window: HWND,
}
impl WinClipboard {
    pub(super) fn new() -> Result<Self, String> {
        let window = unsafe {
            CreateWindowExW(
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
            )
        };
        if window.is_null() {
            return Err("Clipboard owner window creation failed".into());
        }
        Ok(Self { window })
    }
}
impl Drop for WinClipboard {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.window);
        }
    }
}
/// Global allocation not yet handed to the clipboard; freed on drop.
pub(super) struct WinStaged(HGLOBAL);
impl WinStaged {
    fn into_raw(mut self) -> HGLOBAL {
        std::mem::replace(&mut self.0, std::ptr::null_mut())
    }
}
impl Drop for WinStaged {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                GlobalFree(self.0);
            }
        }
    }
}
impl ClipboardBackend for WinClipboard {
    type Staged = WinStaged;
    fn identity(&self) -> u64 {
        self.window as u64
    }
    fn owner(&self) -> u64 {
        unsafe { GetClipboardOwner() as u64 }
    }
    fn sequence(&self) -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }
    fn locked<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        unsafe {
            if OpenClipboard(self.window) == 0 {
                return None;
            }
            let result = f();
            CloseClipboard();
            Some(result)
        }
    }
    fn formats(&self) -> Vec<u32> {
        let mut formats = Vec::new();
        let mut format = 0;
        loop {
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                break;
            }
            formats.push(format);
        }
        formats
    }
    fn read(&self, format: u32, limit: usize) -> Result<Option<Vec<u8>>, String> {
        unsafe {
            let handle = GetClipboardData(format);
            if handle.is_null() {
                return Ok(None);
            }
            let size = GlobalSize(handle);
            if size == 0 || size > limit {
                return Ok(None);
            }
            let pointer = GlobalLock(handle);
            if pointer.is_null() {
                return Err("Clipboard data lock failed".into());
            }
            let bytes = std::slice::from_raw_parts(pointer as *const u8, size).to_vec();
            GlobalUnlock(handle);
            Ok(Some(bytes))
        }
    }
    fn clear(&self) -> bool {
        unsafe { EmptyClipboard() != 0 }
    }
    fn stage(&self, bytes: &[u8]) -> Result<(WinStaged, Vec<u8>), String> {
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len());
            if handle.is_null() {
                return Err("Clipboard allocation failed".into());
            }
            let staged = WinStaged(handle);
            let pointer = GlobalLock(handle);
            if pointer.is_null() {
                return Err("Clipboard allocation lock failed".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
            let reported =
                std::slice::from_raw_parts(pointer as *const u8, GlobalSize(handle)).to_vec();
            GlobalUnlock(handle);
            Ok((staged, reported))
        }
    }
    fn set(&self, format: u32, staged: WinStaged) -> bool {
        unsafe {
            let handle = staged.into_raw();
            if SetClipboardData(format, handle).is_null() {
                GlobalFree(handle);
                return false;
            }
            true
        }
    }
}
/// Payloads allocated before trigger deletion; unused ones free on drop.
pub(super) struct PreparedClipboard<S> {
    handles: Vec<(u32, S)>,
    expected: ClipboardFormats,
}
// no-regression #1: restore only after sequence, owner and payload re-verify.
pub(super) struct ClipboardLease<C: ClipboardBackend> {
    backend: C,
    backup: ClipboardFormats,
    owned_payload: ClipboardFormats,
    written: Option<u32>,
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
impl<C: ClipboardBackend> ClipboardLease<C> {
    fn verified_sequence(
        &self,
        owner: u64,
        expected: &[(u32, Vec<u8>)],
        exact_formats: bool,
    ) -> Result<u32, String> {
        let backend = &self.backend;
        retry_clipboard_verification(
            || {
                let Some(verified) = backend.locked(|| -> Result<u32, String> {
                    if backend.owner() != owner {
                        return Err(
                            "Clipboard owner changed externally; clipboard preserved".into()
                        );
                    }
                    if exact_formats {
                        let formats = backend.formats();
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
                        match backend.read(*format, bytes.len()) {
                            Err(_) => return Err("Clipboard payload cannot be verified".into()),
                            Ok(Some(current)) if current == *bytes => {}
                            Ok(_) => {
                                return Err(
                                    "Clipboard payload changed externally; clipboard preserved"
                                        .into(),
                                )
                            }
                        }
                    }
                    Ok(backend.sequence())
                }) else {
                    return Ok(None);
                };
                let sequence = verified?;
                if backend.sequence() == sequence && backend.owner() == owner {
                    return Ok(Some(sequence));
                }
                Ok(None)
            },
            || std::thread::sleep(Duration::from_millis(5)),
        )
    }
    pub(super) fn verify_current(&self) -> Result<(), String> {
        if self
            .written
            .is_none_or(|seq| self.backend.sequence() != seq)
        {
            return Err("Clipboard changed externally before paste; no paste was sent".into());
        }
        Ok(())
    }
    /// Allocates `formats` up front so allocation cannot fail after deletion.
    pub(super) fn prepare(
        &self,
        formats: ClipboardFormats,
    ) -> Result<PreparedClipboard<C::Staged>, String> {
        let mut prepared = PreparedClipboard {
            handles: vec![],
            expected: vec![],
        };
        for (format, bytes) in formats {
            let (staged, reported) = self
                .backend
                .stage(&bytes)
                .map_err(|error| format!("{error} before deletion"))?;
            prepared.expected.push((format, reported));
            prepared.handles.push((format, staged));
        }
        Ok(prepared)
    }
    pub(super) fn write_prepared(
        &mut self,
        prepared: &mut PreparedClipboard<C::Staged>,
    ) -> Result<(), String> {
        let Self {
            backend,
            owned_payload,
            written,
            dirty,
            ..
        } = self;
        let result = backend.locked(|| -> Result<(), String> {
            if written.is_some_and(|seq| backend.sequence() != seq) {
                return Err("Clipboard changed externally; stopped without overwriting it".into());
            }
            if !backend.clear() {
                return Err("Clipboard clear failed".into());
            }
            *dirty = true;
            owned_payload.clear();
            mark_external_effect();
            while !prepared.handles.is_empty() {
                let (f, h) = prepared.handles.remove(0);
                if !backend.set(f, h) {
                    *written = Some(backend.sequence());
                    return Err("Clipboard write failed".into());
                }
                if let Some((_, bytes)) = prepared.expected.iter().find(|(format, _)| *format == f)
                {
                    owned_payload.push((f, bytes.clone()));
                }
            }
            *written = Some(backend.sequence());
            Ok(())
        });
        result.ok_or("Clipboard is busy")??;
        self.written =
            Some(self.verified_sequence(self.backend.identity(), &prepared.expected, false)?);
        Ok(())
    }
    pub(super) fn new(backend: C) -> Result<Self, String> {
        let Some(result) = backend.locked(|| -> Result<(u64, ClipboardFormats, u32), String> {
            let original_owner = backend.owner();
            let mut backup = vec![];
            for format in backend.formats() {
                if matches!(format, 2 | 3 | 9 | 14 | 0x80..=0x8f) {
                    return Err("Existing clipboard contains handle-based rich formats that cannot be safely restored; trigger preserved".into());
                }
                match backend.read(format, 32 * 1024 * 1024) {
                    Ok(Some(bytes)) => backup.push((format, bytes)),
                    Ok(None) => {
                        return Err(
                            "Clipboard format cannot be safely backed up; trigger preserved".into(),
                        )
                    }
                    Err(_) => return Err("Clipboard backup failed".into()),
                }
            }
            Ok((original_owner, backup, backend.sequence()))
        }) else {
            return Err("Clipboard is busy; trigger preserved".into());
        };
        let (original_owner, backup, sequence) = result?;
        let mut lease = Self {
            backend,
            backup,
            owned_payload: vec![],
            written: Some(sequence),
            dirty: false,
        };
        match lease.verified_sequence(original_owner, &lease.backup, true) {
            Ok(sequence) => {
                lease.written = Some(sequence);
                Ok(lease)
            }
            Err(reason) => {
                lease.written = None;
                Err(reason)
            }
        }
    }
    fn write(&mut self, formats: ClipboardFormats) -> Result<(), String> {
        let Self {
            backend,
            owned_payload,
            written,
            ..
        } = self;
        backend
            .locked(|| -> Result<(), String> {
                if let Some(seq) = *written {
                    if backend.sequence() != seq {
                        return Err(
                            "Clipboard changed externally; stopped without overwriting it".into(),
                        );
                    }
                }
                if !backend.clear() {
                    return Err("Clipboard clear failed".into());
                }
                mark_external_effect();
                owned_payload.clear();
                *written = Some(backend.sequence());
                for (format, bytes) in formats {
                    let (staged, expected) = backend.stage(&bytes)?;
                    if !backend.set(format, staged) {
                        return Err("Clipboard write failed".into());
                    }
                    owned_payload.push((format, expected));
                }
                *written = Some(backend.sequence());
                Ok(())
            })
            .unwrap_or_else(|| Err("Clipboard is busy".into()))
    }
    pub(super) fn restore(&mut self) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        if self.backend.owner() != self.backend.identity() {
            return Ok(());
        }
        // Closing the clipboard or a listener requesting a synthesized
        // format can advance the sequence after our writes. Re-establish
        // ownership from the exact payload under a lock; never adopt a new
        // sequence solely because the previous one is no longer current.
        self.written =
            Some(self.verified_sequence(self.backend.identity(), &self.owned_payload, false)?);
        let result = self.write(self.backup.clone());
        finish_clipboard_restore(&mut self.backup, result)?;
        self.written = None;
        self.dirty = false;
        self.owned_payload.clear();
        Ok(())
    }
}
impl<C: ClipboardBackend> Drop for ClipboardLease<C> {
    fn drop(&mut self) {
        // The backend (owner window) is released after this restore attempt.
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::inject::SENT_EFFECT;
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
}
