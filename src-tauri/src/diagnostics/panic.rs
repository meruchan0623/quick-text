//! Panic hook (§7.1 「崩溃记录」): every panic leaves
//! `crash-<UTC yyyymmdd-hhmmss>.txt` in the data directory and an `app.panic`
//! log event, then the previous hook runs as before.
//!
//! The record has the version, thread, location, the panic message with
//! quoted spans redacted (std quotes the string it was slicing, which may be
//! a phrase body), user names in paths replaced and the result truncated,
//! the executable's load address and a
//! backtrace. Release builds keep line tables and PDBs are archived, so
//! frames can be symbolized offline. Writing the record never panics.
use super::redact::{redact_user_paths, sanitize_quoted, truncate_utf8, user_homes};
use crate::store::prefs::utc_stamp;
use std::{
    backtrace::Backtrace,
    cell::Cell,
    fmt::Write as _,
    fs,
    io::{self, Write as _},
    panic::PanicHookInfo,
    path::{Path, PathBuf},
    sync::{Once, OnceLock},
    time::SystemTime,
};

/// Panic messages longer than this are cut (after redaction).
pub const MAX_MESSAGE_BYTES: usize = 2048;

static CRASH_DIR: OnceLock<PathBuf> = OnceLock::new();

thread_local! {
    /// Set while this thread is inside the hook, so a panic while recording
    /// does not record again.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// Chains the crash recorder in front of the current panic hook. Call first
/// thing in `main`; later calls do nothing.
pub fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !IN_HOOK.with(|flag| flag.replace(true)) {
                record(info);
                IN_HOOK.with(|flag| flag.set(false));
            }
            previous(info);
        }));
    });
}

/// Where crash records go: the data directory, known once setup resolved
/// it. Panics before that only reach the previous hook.
pub fn set_crash_dir(dir: PathBuf) {
    let _ = CRASH_DIR.set(dir);
}

/// What one crash record contains.
pub struct CrashReport<'a> {
    pub version: &'a str,
    pub debug_build: bool,
    pub time_utc: String,
    pub thread: &'a str,
    pub location: Option<String>,
    /// Already redacted and truncated.
    pub message: String,
    pub exe_base: Option<usize>,
    pub backtrace: String,
}

fn record(info: &PanicHookInfo<'_>) {
    let now = SystemTime::now();
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("<unnamed>");
    let payload = info.payload();
    let raw = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>");
    let report = CrashReport {
        version: env!("CARGO_PKG_VERSION"),
        debug_build: cfg!(debug_assertions),
        time_utc: utc_stamp(now),
        thread,
        location: info
            .location()
            .map(|at| format!("{}:{}:{}", at.file(), at.line(), at.column())),
        message: clean_message(raw, &user_homes()),
        exe_base: exe_base(),
        backtrace: format!("{:#}", Backtrace::force_capture()),
    };
    let written = CRASH_DIR
        .get()
        .map(|dir| write_report(dir, now, &render_report(&report)));
    let crash_file = match &written {
        Some(Ok(path)) => crate::error::file_label(path),
        _ => None,
    };
    let write_error = match &written {
        Some(Err(e)) => Some(e.to_string()),
        _ => None,
    };
    tracing::error!(
        event = "app.panic",
        thread = report.thread,
        location = report.location.as_deref(),
        message = %report.message,
        crash_file = crash_file.as_deref(),
        write_error = write_error.as_deref(),
        "panic"
    );
}

/// The panic message as recorded: quoted spans redacted, user names in
/// paths replaced, then truncated.
pub fn clean_message(raw: &str, homes: &[String]) -> String {
    let clean = redact_user_paths(&sanitize_quoted(raw), homes);
    let cut = truncate_utf8(&clean, MAX_MESSAGE_BYTES);
    if cut.len() < clean.len() {
        format!("{cut}… <truncated, {} bytes total>", clean.len())
    } else {
        clean
    }
}

pub fn render_report(report: &CrashReport<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Quicker Text crash report");
    let _ = writeln!(out, "version: {}", report.version);
    let _ = writeln!(
        out,
        "build: {}",
        if report.debug_build {
            "debug"
        } else {
            "release"
        }
    );
    let _ = writeln!(out, "time_utc: {}", report.time_utc);
    let _ = writeln!(out, "thread: {}", report.thread);
    let _ = writeln!(
        out,
        "location: {}",
        report.location.as_deref().unwrap_or("<unknown>")
    );
    let _ = writeln!(out, "message: {}", report.message);
    match report.exe_base {
        Some(base) => {
            let _ = writeln!(out, "exe_base: {base:#x}");
        }
        None => {
            let _ = writeln!(out, "exe_base: <unknown>");
        }
    }
    let _ = writeln!(out, "backtrace:");
    let _ = writeln!(out, "{}", report.backtrace);
    out
}

/// Writes `crash-<UTC yyyymmdd-hhmmss>.txt` (`-<n>` added if that second
/// already has one) into `dir` and returns its path. Never overwrites.
pub fn write_report(dir: &Path, now: SystemTime, text: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let stamp = utc_stamp(now);
    for n in 0..1000 {
        let name = if n == 0 {
            format!("crash-{stamp}.txt")
        } else {
            format!("crash-{stamp}-{n}.txt")
        };
        let path = dir.join(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many crash records in one second",
    ))
}

/// Load address of the executable, for symbolizing raw frame addresses.
#[cfg(windows)]
fn exe_base() -> Option<usize> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut std::ffi::c_void;
    }
    // SAFETY: a null name asks for the executable's own module handle, which
    // is its load address; nothing is dereferenced.
    let base = unsafe { GetModuleHandleW(std::ptr::null()) };
    (!base.is_null()).then_some(base as usize)
}

#[cfg(not(windows))]
fn exe_base() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("quicker-crash-test-{}", uuid::Uuid::new_v4()))
    }

    fn crash_files(dir: &Path) -> Vec<PathBuf> {
        let mut files: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.starts_with("crash-") && name.ends_with(".txt")
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn report_has_every_field_and_no_quoted_content() {
        let text = render_report(&CrashReport {
            version: "0.4.0",
            debug_build: false,
            time_utc: "20261022-102133".into(),
            thread: "quicker-executor",
            location: Some("src\\engine.rs:10:5".into()),
            message: clean_message(
                r"byte index 2 is not a char boundary of `SENTINEL-7f3a` in D:\Home\bob\x",
                &[r"D:\Home\bob".to_string()],
            ),
            exe_base: Some(0x7ff6_1234_0000),
            backtrace: "   0: std::backtrace".into(),
        });
        for expected in [
            "version: 0.4.0",
            "build: release",
            "time_utc: 20261022-102133",
            "thread: quicker-executor",
            "location: src\\engine.rs:10:5",
            r"message: byte index 2 is not a char boundary of `<redacted len=13>` in D:\Home\%USERNAME%\x",
            "exe_base: 0x7ff612340000",
            "backtrace:\n   0: std::backtrace",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(!text.contains("SENTINEL"));
    }

    #[test]
    fn long_messages_are_truncated_on_a_char_boundary() {
        let message = clean_message(&"中".repeat(1000), &[]);
        assert!(message.len() < MAX_MESSAGE_BYTES + 64);
        assert!(message.ends_with("<truncated, 3000 bytes total>"));
        assert_eq!(clean_message("short", &[]), "short");
    }

    #[test]
    fn records_never_overwrite_and_write_errors_are_returned() {
        let dir = temp_dir();
        let at = UNIX_EPOCH + Duration::from_secs(1_792_664_493);
        let first = write_report(&dir, at, "one").unwrap();
        let second = write_report(&dir, at, "two").unwrap();
        assert_eq!(first.file_name().unwrap(), "crash-20261022-102133.txt");
        assert_eq!(second.file_name().unwrap(), "crash-20261022-102133-1.txt");
        assert_eq!(fs::read_to_string(&first).unwrap(), "one");
        // A data "directory" that is a file: an error, not a second panic.
        let blocked = dir.join("not-a-dir");
        fs::write(&blocked, b"").unwrap();
        assert!(write_report(&blocked, at, "three").is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    /// End to end through the installed hook: a panicking thread leaves a
    /// crash record with its name and no quoted content.
    #[test]
    fn a_panicking_thread_leaves_a_crash_record() {
        let dir = temp_dir();
        install();
        set_crash_dir(dir.clone());
        let Some(crash_dir) = CRASH_DIR.get().cloned() else {
            unreachable!("set above");
        };
        let joined = std::thread::Builder::new()
            .name("crash-record-test".into())
            .spawn(|| {
                let phrase = String::from("SENTINEL-crash-phrase");
                panic!("cannot slice `{phrase}`");
            })
            .unwrap()
            .join();
        assert!(joined.is_err());
        let record = crash_files(&crash_dir)
            .into_iter()
            .map(|path| fs::read_to_string(path).unwrap())
            .find(|text| text.contains("thread: crash-record-test"))
            .expect("crash record written");
        assert!(
            record.contains("message: cannot slice `<redacted len=21>`"),
            "{record}"
        );
        assert!(record.contains("panic.rs"), "{record}");
        assert!(record.contains("backtrace:"), "{record}");
        assert!(!record.contains("SENTINEL"), "{record}");
        if crash_dir == dir {
            fs::remove_dir_all(dir).unwrap();
        }
    }
}
