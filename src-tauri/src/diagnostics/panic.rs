//! Panic hook (§7.1 「崩溃记录」): every panic leaves
//! `crash-<UTC yyyymmdd-hhmmss>.txt` in the data directory and an `app.panic`
//! log event, then the previous hook runs as before.
//!
//! The record has the version, thread, location (relative to its crate),
//! the panic payload's class, length and fingerprint ([`PanicPayload`]; never
//! its text, which may quote a phrase body), the executable's load address
//! and a backtrace. Release builds keep line tables and PDBs are archived,
//! so frames can be symbolized offline. Writing the record never panics.
use super::redact::{redact_user_paths, user_homes, PanicPayload};
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

static CRASH_DIR: OnceLock<PathBuf> = OnceLock::new();

thread_local! {
    /// Set while this thread is inside the hook, so a panic while recording
    /// does not record again.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
thread_local! {
    /// Tests share one process and so one hook: a test sends the panics of
    /// its own thread to its own directory instead of [`CRASH_DIR`].
    static TEST_CRASH_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

fn crash_dir() -> Option<PathBuf> {
    #[cfg(test)]
    let test_dir = TEST_CRASH_DIR.with(|dir| dir.borrow().clone());
    #[cfg(not(test))]
    let test_dir = None;
    test_dir.or_else(|| CRASH_DIR.get().cloned())
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
    /// See [`crate_location`].
    pub location: Option<String>,
    pub payload: PanicPayload,
    pub exe_base: Option<usize>,
    pub backtrace: String,
}

fn record(info: &PanicHookInfo<'_>) {
    let now = SystemTime::now();
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("<unnamed>");
    let payload = info.payload();
    let text = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str));
    let report = CrashReport {
        version: env!("CARGO_PKG_VERSION"),
        debug_build: cfg!(debug_assertions),
        time_utc: utc_stamp(now),
        thread,
        location: info
            .location()
            .map(|at| crate_location(at.file(), at.line(), at.column(), &user_homes())),
        payload: PanicPayload::summarize(text),
        exe_base: exe_base(),
        backtrace: format!("{:#}", Backtrace::force_capture()),
    };
    let written = crash_dir().map(|dir| write_report(&dir, now, &render_report(&report)));
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
        panic_class = report.payload.class.as_str(),
        payload_len = report.payload.len,
        payload_hash = %report.payload.hash,
        crash_file = crash_file.as_deref(),
        write_error = write_error.as_deref(),
        "panic"
    );
}

/// `file:line:col` with the file cut to its crate: from the directory above
/// the last `src` segment (`tokio-1.40.0/src/runtime/mod.rs` rather than the
/// registry path in the user's profile, `core/src/str/mod.rs` for std), or
/// the bare file name without a `src` segment; `/` separated, user names in
/// what is left replaced.
pub fn crate_location(file: &str, line: u32, column: u32, homes: &[String]) -> String {
    let file = file.replace('\\', "/");
    let segments: Vec<&str> = file.split('/').filter(|s| !s.is_empty()).collect();
    let start = match segments.iter().rposition(|s| *s == "src") {
        Some(src) => src.saturating_sub(1),
        None => segments.len().saturating_sub(1),
    };
    let file = redact_user_paths(&segments[start..].join("/"), homes);
    format!("{file}:{line}:{column}")
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
    let _ = writeln!(out, "panic_class: {}", report.payload.class.as_str());
    let _ = writeln!(out, "payload_len: {}", report.payload.len);
    let _ = writeln!(out, "payload_hash: {}", report.payload.hash);
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
    use crate::diagnostics::redact::{fingerprint, PanicClass};
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
        let payload = r"byte index 2 is not a char boundary of `SENTINEL-7f3a` in D:\Home\bob\x";
        let text = render_report(&CrashReport {
            version: "0.4.0",
            debug_build: false,
            time_utc: "20261022-102133".into(),
            thread: "quicker-executor",
            location: Some(crate_location(
                r"D:\Home\bob\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\tokio-1.40.0\src\runtime\mod.rs",
                10,
                5,
                &[r"D:\Home\bob".to_string()],
            )),
            payload: PanicPayload::summarize(Some(payload)),
            exe_base: Some(0x7ff6_1234_0000),
            backtrace: "   0: std::backtrace".into(),
        });
        for expected in [
            "version: 0.4.0",
            "build: release",
            "time_utc: 20261022-102133",
            "thread: quicker-executor",
            "location: tokio-1.40.0/src/runtime/mod.rs:10:5",
            "panic_class: byte_index",
            &format!("payload_len: {}", payload.len()),
            &format!("payload_hash: {}", fingerprint(payload)),
            "exe_base: 0x7ff612340000",
            "backtrace:\n   0: std::backtrace",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(!text.contains("SENTINEL"), "{text}");
        assert!(!text.contains("bob"), "{text}");

        for (file, expected) in [
            (
                r"src-tauri\src\diagnostics\panic.rs",
                "src-tauri/src/diagnostics/panic.rs",
            ),
            (
                "crates/platform/src/windows/hook.rs",
                "platform/src/windows/hook.rs",
            ),
            (
                "/rustc/90b35a6/library/core/src/str/mod.rs",
                "core/src/str/mod.rs",
            ),
            ("src/main.rs", "src/main.rs"),
            (r"C:\Users\alice\build\gen.rs", "gen.rs"),
        ] {
            assert_eq!(crate_location(file, 1, 2, &[]), format!("{expected}:1:2"));
        }
    }

    /// Long payloads cost nothing: only their length is recorded.
    #[test]
    fn long_messages_are_truncated_on_a_char_boundary() {
        let long = "中".repeat(1000);
        let payload = PanicPayload::summarize(Some(&long));
        assert_eq!((payload.class, payload.len), (PanicClass::Other, 3000));
        let text = render_report(&CrashReport {
            version: "0.4.0",
            debug_build: true,
            time_utc: "20261022-102133".into(),
            thread: "main",
            location: None,
            payload,
            exe_base: None,
            backtrace: String::new(),
        });
        assert!(text.len() < 300, "{text}");
        assert!(!text.contains('中'), "{text}");
        assert!(text.contains("location: <unknown>"), "{text}");
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
        // "cannot slice `SENTINEL-crash-phrase`": not std's wording.
        assert!(record.contains("panic_class: other"), "{record}");
        assert!(record.contains("payload_len: 36"), "{record}");
        assert!(
            record.contains("location: src-tauri/src/diagnostics/panic.rs:"),
            "{record}"
        );
        assert!(record.contains("backtrace:"), "{record}");
        assert!(!record.contains("SENTINEL"), "{record}");
        if crash_dir == dir {
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// S-08 privacy test, panic half: real panics whose payloads quote a
    /// phrase in ways a quote-matching redactor gets wrong (a slice of text
    /// with backticks in it, `Debug` escapes, no quotes at all) leave the
    /// phrase in neither the crash records, the `app.panic` log nor a
    /// Summary bundle.
    #[test]
    fn panic_payload_text_never_reaches_the_crash_record_the_log_or_the_bundle() {
        use crate::diagnostics::{
            bundle::test_support::summary_entries,
            logging::{
                log_dir,
                test_support::{read_logs, scoped},
            },
        };
        use std::panic::{catch_unwind, AssertUnwindSafe};
        use tracing::level_filters::LevelFilter;

        const S: &str = "SENTINEL-payload-5b0e";
        let dir = temp_dir();
        install();
        TEST_CRASH_DIR.with(|crash_dir| *crash_dir.borrow_mut() = Some(dir.clone()));
        let backticks = std::hint::black_box(format!("é`{S}`é"));
        let quoted = format!("a\"{S}\"b");
        let failed: Result<(), String> = std::hint::black_box(Err(format!("x\"{S}\"y")));
        let panics: [Box<dyn Fn()>; 4] = [
            // end byte index 1 is not a char boundary; ... of `é`SENTINEL`é`
            Box::new(|| {
                std::hint::black_box(&backticks[..1]);
            }),
            // called `Result::unwrap()` on an `Err` value: "x\"SENTINEL\"y"
            Box::new(|| failed.clone().unwrap()),
            Box::new(|| panic!("could not expand {S} at all")),
            // "a\"SENTINEL\"b"
            Box::new(|| panic!("{quoted:?}")),
        ];
        let built = scoped(&log_dir(&dir), LevelFilter::TRACE);
        tracing::subscriber::with_default(built.subscriber, || {
            for panic in &panics {
                assert!(catch_unwind(AssertUnwindSafe(panic)).is_err());
            }
        });
        drop(built.guard);
        TEST_CRASH_DIR.with(|crash_dir| crash_dir.borrow_mut().take());

        let records: Vec<String> = crash_files(&dir)
            .into_iter()
            .map(|path| fs::read_to_string(path).unwrap())
            .collect();
        assert_eq!(records.len(), 4);
        let classes: Vec<_> = records
            .iter()
            .filter_map(|record| record.lines().find(|l| l.starts_with("panic_class: ")))
            .collect();
        for class in ["byte_index", "result_unwrap", "other"] {
            assert!(
                classes.contains(&format!("panic_class: {class}").as_str()),
                "{class} missing from {classes:?}"
            );
        }
        for record in &records {
            assert!(!record.contains(S), "the crash record leaks:\n{record}");
        }

        let log = read_logs(&log_dir(&dir));
        assert_eq!(log.matches("app.panic").count(), 4, "{log}");
        assert!(log.contains("panic_class=\"byte_index\""), "{log}");
        assert!(!log.contains(S), "the log leaks:\n{log}");

        let files = summary_entries(&dir);
        assert_eq!(
            files.keys().filter(|n| n.starts_with("crashes/")).count(),
            4
        );
        assert!(files.keys().any(|name| name.starts_with("logs/")));
        for (name, text) in &files {
            assert!(!text.contains(S), "{name} leaks:\n{text}");
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
