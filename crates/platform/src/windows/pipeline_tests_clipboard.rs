//! L2 clipboard cases (B-08): the production paste pipeline and clipboard
//! lease over `MockClipboard` holding handle formats, oversized formats, a
//! busy clipboard, a hung owner, delay-rendered data that is slow or never
//! arrives, unreadable data and external changes. The pipeline leases with
//! `LeaseOptions::default()` (BestEffort); the other policies are exercised
//! on the lease directly until preferences select one.
use super::*;
use crate::health::{HealthState, ReasonCode};
use crate::traits::{LeaseOptions, RestorePolicy, DEFAULT_RESTORE_DELAY};
use crate::windows::clipboard::{
    clipboard_formats, ClipboardLease, MAX_FORMAT_BYTES, OPEN_ATTEMPTS, OPEN_RETRY_INTERVAL,
};

type Formats = Vec<(u32, Vec<u8>)>;

/// An external clipboard change a test scripts into a lock hook.
type Change = fn(&MockClipboard);

/// A paste rig whose clipboard starts with `formats`, owned by owner 7.
fn rig_with(formats: Formats) -> Rig {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    rig.clipboard = MockClipboard::new(7, formats);
    rig
}
/// Types the trigger and runs the paste plan.
fn paste(rig: &mut Rig) -> ExecutionResult {
    let candidate = rig.type_str("btw").unwrap();
    rig.execute(&candidate).unwrap()
}
/// What a lease under `policy` records as skipped, or why it refuses.
fn lease_result(clipboard: &MockClipboard, policy: RestorePolicy) -> Result<Vec<u32>, ReasonCode> {
    let options = LeaseOptions {
        policy,
        ..LeaseOptions::default()
    };
    let skipped = ClipboardLease::open(clipboard.clone(), options)
        .map(|lease| lease.skipped_formats().to_vec())
        .map_err(|error| error.reason);
    assert_eq!(clipboard.clears(), 0, "a backup never writes");
    skipped
}
fn skipped(formats: &[(u32, Vec<u8>)], policy: RestorePolicy) -> Result<Vec<u32>, ReasonCode> {
    lease_result(&MockClipboard::new(7, formats.to_vec()), policy)
}
/// A packed DIB: `header` is the BITMAPINFOHEADER/BITMAPV5HEADER size.
fn dib(header: u32) -> Vec<u8> {
    let mut bytes = header.to_le_bytes().to_vec();
    bytes.extend([0x10, 0x20, 0x30, 0x40, 0x50]);
    bytes
}
/// Formats a rich-text editor puts on the clipboard; the system adds
/// CF_METAFILEPICT for the CF_ENHMETAFILE.
fn rich_text() -> Formats {
    vec![
        (0xC0A1, b"{\\rtf1\\ansi rich}".to_vec()),
        (0xC0A2, b"Version:0.9\r\n<b>rich</b>".to_vec()),
        (13, utf16("rich")),
        (1, b"rich\0".to_vec()),
        (16, 0x0409u32.to_le_bytes().to_vec()),
        (7, b"rich\0".to_vec()),
        (14, vec![1, 0, 0, 0, 0x6C, 0, 0, 0, 0xEF]),
        (3, vec![0x3F]),
    ]
}

#[test]
fn screenshot_is_restored_byte_identical_without_its_bitmap() {
    let (dibv5, dib) = ((17, dib(124)), (8, dib(40)));
    let screenshot = vec![dibv5.clone(), (2, vec![0xB1]), dib.clone()];
    for policy in [RestorePolicy::Full, RestorePolicy::BestEffort] {
        assert_eq!(
            skipped(&screenshot, policy),
            Ok(vec![]),
            "CF_BITMAP is not recorded"
        );
    }
    let mut rig = rig_with(screenshot);
    let result = paste(&mut rig);
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.clipboard.content(), vec![dibv5, dib]);
    assert_eq!(rig.clipboard.clears(), 2);
}

#[test]
fn rich_text_with_enhanced_metafile_is_restored() {
    let rich = rich_text();
    for policy in [RestorePolicy::Full, RestorePolicy::BestEffort] {
        assert_eq!(skipped(&rich, policy), Ok(vec![]), "{policy:?}");
    }
    let mut rig = rig_with(rich.clone());
    let result = paste(&mut rig);
    assert_eq!(result.status, "unknown", "{}", result.message);
    // CF_METAFILEPICT is re-synthesized from the restored CF_ENHMETAFILE.
    assert_eq!(rig.clipboard.content(), rich[..7]);
}

#[test]
fn copied_files_are_restored() {
    let operation = Operation::PasteFiles {
        paths: vec!["C:\\fixture.txt".into(), "C:\\其他\\b.txt".into()],
    };
    let mut files = clipboard_formats(&operation).unwrap().unwrap();
    assert_eq!(files[0].0, 15, "CF_HDROP");
    files.push((0xC0B0, vec![1, 0, 0, 0, 0x14, 0, 0, 0]));
    files.push((0xC0B1, 1u32.to_le_bytes().to_vec()));
    assert_eq!(skipped(&files, RestorePolicy::Full), Ok(vec![]));
    let mut rig = rig_with(files.clone());
    let result = paste(&mut rig);
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.clipboard.content(), files);
}

#[test]
fn palette_owner_display_private_and_gdi_formats_are_recorded() {
    let kept = vec![(8, dib(40)), (13, utf16("still here"))];
    let mut formats = kept.clone();
    let handles = [9, 0x80, 0x81, 0x8E, 0x8F, 0x200, 0x2FF, 0x300, 0x3FF];
    formats.extend(handles.map(|format| (format, vec![0xAB])));
    assert_eq!(
        skipped(&formats, RestorePolicy::BestEffort),
        Ok(handles.to_vec())
    );
    assert_eq!(
        skipped(&formats, RestorePolicy::Full),
        Err(ReasonCode::ClipboardFormatSkipped)
    );
    assert_eq!(skipped(&formats, RestorePolicy::TextOnly), Ok(vec![]));
    let mut rig = rig_with(formats);
    let result = paste(&mut rig);
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.clipboard.content(), kept);
}

#[test]
fn format_over_32_mib_follows_the_restore_policy() {
    let big = 0xC0C0;
    let formats = vec![(13, utf16("original")), (big, vec![7; 16])];
    let oversized = || {
        let clipboard = MockClipboard::new(7, formats.clone());
        clipboard.report_size(big, MAX_FORMAT_BYTES + 1);
        clipboard
    };
    let expected = [
        (RestorePolicy::Full, Err(ReasonCode::ClipboardTooLarge)),
        (RestorePolicy::BestEffort, Ok(vec![big])),
        (RestorePolicy::TextOnly, Ok(vec![])),
    ];
    for (policy, outcome) in expected {
        assert_eq!(lease_result(&oversized(), policy), outcome, "{policy:?}");
    }
    // TextOnly still records the loss of a text format.
    let clipboard = MockClipboard::new(7, clip("original"));
    clipboard.report_size(13, MAX_FORMAT_BYTES + 1);
    assert_eq!(
        lease_result(&clipboard, RestorePolicy::TextOnly),
        Ok(vec![13])
    );
    // The pipeline (BestEffort) pastes and restores everything else.
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    rig.clipboard = oversized();
    let result = paste(&mut rig);
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.clipboard.content(), clip("original"));
}

const DEGRADED: Option<(HealthState, Option<ReasonCode>)> =
    Some((HealthState::Degraded, Some(ReasonCode::ClipboardBusy)));

#[test]
fn hung_owner_refuses_before_erasure_under_every_policy() {
    let formats = vec![(13, utf16("original")), (8, dib(40)), (9, vec![0x9A])];
    let hung = || {
        let clipboard = MockClipboard::new(7, formats.clone());
        clipboard.set_owner_hung(true);
        clipboard
    };
    // The mock panics on any read while the hung owner holds the clipboard.
    for policy in [
        RestorePolicy::Full,
        RestorePolicy::BestEffort,
        RestorePolicy::TextOnly,
    ] {
        let clipboard = hung();
        assert_eq!(
            lease_result(&clipboard, policy),
            Err(ReasonCode::ClipboardBusy),
            "{policy:?}"
        );
        assert_eq!(clipboard.health(), DEGRADED, "{policy:?}");
    }
    let mut rig = rig_with(formats.clone());
    rig.clipboard.set_owner_hung(true);
    let candidate = rig.type_str("btw").unwrap();
    assert_eq!(
        rig.execute(&candidate).unwrap_err(),
        "Clipboard owner is not responding; trigger preserved"
    );
    assert!(rig.injector.calls().is_empty(), "trigger preserved");
    assert_eq!(rig.clipboard.content(), formats, "clipboard untouched");
    assert_eq!(rig.clipboard.clears(), 0);
    assert_eq!(rig.clipboard.reads(), 0);
    assert_eq!(rig.clipboard.health(), DEGRADED);
}

#[test]
fn delay_rendered_data_that_never_arrives_preserves_trigger_and_clipboard() {
    let mut rig = rig_with(rich_text());
    let candidate = rig.type_str("btw").unwrap();
    let sequence = rig.clipboard.sequence_now();
    // The owner never renders the first format the backup reads.
    let release = rig.clipboard.block_next_read();
    assert_eq!(
        rig.execute(&candidate).unwrap_err(),
        "Clipboard data did not arrive in time; trigger preserved"
    );
    assert!(rig.injector.calls().is_empty(), "trigger preserved");
    assert_eq!(rig.clipboard.health(), DEGRADED);
    // The owner answers late: the abandoned read returns, nothing follows.
    drop(release);
    assert!(rig.clipboard.await_closed(Duration::from_secs(5)));
    assert_eq!(rig.clipboard.reads(), 1);
    assert_eq!(rig.clipboard.content(), rich_text());
    assert_eq!(rig.clipboard.sequence_now(), sequence);
    assert_eq!(rig.clipboard.clears(), 0);
}

#[test]
fn slow_delay_rendering_past_the_hold_budget_refuses() {
    let mut rig = rig_with(rich_text());
    let candidate = rig.type_str("btw").unwrap();
    // 100 ms per format: the third read crosses the 250 ms budget.
    rig.clipboard.set_read_delay(Duration::from_millis(100));
    assert_eq!(
        rig.execute(&candidate).unwrap_err(),
        "Clipboard data took too long to read; trigger preserved"
    );
    assert_eq!(
        rig.clipboard.reads(),
        3,
        "stopped at the read that crossed it"
    );
    assert!(rig.injector.calls().is_empty(), "trigger preserved");
    assert_eq!(rig.clipboard.health(), DEGRADED);
    assert_eq!(rig.clipboard.content(), rich_text());
    assert_eq!(rig.clipboard.clears(), 0);
}

#[test]
fn unreadable_format_refuses_before_erasure() {
    let mut rig = rig_with(rich_text());
    let candidate = rig.type_str("btw").unwrap();
    rig.clipboard.fail_read(0xC0A2);
    assert_eq!(
        rig.execute(&candidate).unwrap_err(),
        "Clipboard format 0xc0a2 could not be read; trigger preserved"
    );
    assert!(rig.injector.calls().is_empty(), "trigger preserved");
    assert_eq!(rig.clipboard.content(), rich_text());
    assert_eq!(rig.clipboard.clears(), 0);
}

#[test]
fn external_change_during_backup_preserves_trigger_and_clipboard() {
    let changes: [(&str, Change, Formats); 3] = [
        ("owner", |c| c.take_over(9, clip("theirs")), clip("theirs")),
        ("content", |c| c.tamper(clip("edited")), clip("edited")),
        ("sequence", |c| c.churn_sequence(true), rich_text()),
    ];
    for (name, change, after) in changes {
        let mut rig = rig_with(rich_text());
        let candidate = rig.type_str("btw").unwrap();
        let script = rig.clipboard.clone();
        // Lock 0 takes the backup, lock 1 verifies it.
        rig.clipboard.on_lock(move |index| {
            if index == 1 {
                change(&script);
            }
        });
        assert!(rig.execute(&candidate).is_err(), "{name}");
        assert!(rig.injector.calls().is_empty(), "{name}: trigger preserved");
        assert_eq!(rig.clipboard.clears(), 0, "{name}");
        assert_eq!(rig.clipboard.content(), after, "{name}");
    }
}

#[test]
fn external_change_after_paste_is_never_overwritten() {
    let changes: [(&str, Change, &str); 3] = [
        ("owner", |c| c.take_over(9, clip("theirs")), "theirs"),
        ("content", |c| c.tamper(clip("edited")), "edited"),
        ("sequence", |c| c.churn_sequence(true), EXPANSION),
    ];
    for (name, change, left) in changes {
        let mut rig = rig_with(rich_text());
        rig.on_paste(change);
        paste(&mut rig);
        let expected = if left == EXPANSION {
            pasted(&rig.clipboard, EXPANSION)
        } else {
            clip(left)
        };
        assert_eq!(rig.clipboard.content(), expected, "{name}");
        assert_eq!(rig.clipboard.clears(), 1, "{name}: only the paste write");
    }
    // Another application adds a format without emptying the clipboard: the
    // owner and the paste stay, the format set does not.
    let mut rig = rig_with(rich_text());
    rig.on_paste(|c| c.add_format(0xC0B1, vec![1, 2, 3]));
    let result = paste(&mut rig);
    assert_eq!(result.status, "failed");
    assert_eq!(
        result.message,
        "Clipboard restore failed after input: Clipboard formats changed externally; clipboard preserved"
    );
    let mut expected = pasted(&rig.clipboard, EXPANSION);
    expected.push((0xC0B1, vec![1, 2, 3]));
    assert_eq!(rig.clipboard.content(), expected, "added");
    assert_eq!(rig.clipboard.clears(), 1, "added: only the paste write");
}

#[test]
fn open_retry_is_bounded_and_exhaustion_degrades_the_clipboard() {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    rig.clipboard.set_busy(OPEN_ATTEMPTS - 1);
    let result = paste(&mut rig);
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(
        rig.clipboard.pauses(),
        vec![OPEN_RETRY_INTERVAL; OPEN_ATTEMPTS - 1]
    );
    assert_eq!(rig.clipboard.health(), Some((HealthState::Healthy, None)));
    assert_eq!(rig.clipboard.content(), clip("original"));

    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    rig.clipboard.set_busy(OPEN_ATTEMPTS);
    assert_eq!(
        rig.execute(&candidate).unwrap_err(),
        "Clipboard is busy; trigger preserved"
    );
    assert!(rig.injector.calls().is_empty());
    assert_eq!(
        rig.clipboard.health(),
        Some((HealthState::Degraded, Some(ReasonCode::ClipboardBusy)))
    );
    rig.clipboard.set_busy(OPEN_ATTEMPTS);
    assert_eq!(
        lease_result(&rig.clipboard, RestorePolicy::BestEffort),
        Err(ReasonCode::ClipboardBusy)
    );
}

#[test]
fn paste_write_sets_the_three_exclusion_formats() {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let seen = Arc::new(std::sync::Mutex::new(None));
    let record = seen.clone();
    rig.on_paste(move |clipboard| *record.lock().unwrap() = Some(clipboard.content()));
    let result = paste(&mut rig);
    assert_eq!(result.status, "unknown", "{}", result.message);
    let written = seen.lock().unwrap().take().expect("paste chord sent");
    for name in [
        "ExcludeClipboardContentFromMonitorProcessing",
        "CanIncludeInClipboardHistory",
        "CanUploadToCloudClipboard",
    ] {
        let id = rig.clipboard.registered(name).expect("registered");
        assert!(written.contains(&(id, vec![0, 0, 0, 0])), "{name}");
    }
    assert_eq!(written, pasted(&rig.clipboard, EXPANSION));
    assert_eq!(
        rig.clipboard.content(),
        clip("original"),
        "restore adds none"
    );
}

#[test]
fn nothing_waits_or_injects_while_the_clipboard_is_open() {
    let mut rig = Rig::new(rule(None, OutputMethod::Paste));
    let candidate = rig.type_str("btw").unwrap();
    // Busy attempts make the lease pause (MockClipboard asserts it is closed).
    rig.clipboard.set_busy(2);
    let clipboard = rig.clipboard.clone();
    rig.injector.on_call(move |call| {
        assert!(
            !clipboard.is_open(),
            "{call:?} sent with the clipboard open"
        );
    });
    let clipboard = rig.clipboard.clone();
    let slept = Arc::new(std::sync::Mutex::new(vec![]));
    let record = slept.clone();
    rig.clock.on_sleep(move |duration| {
        assert!(!clipboard.is_open(), "slept with the clipboard open");
        record.lock().unwrap().push((duration, clipboard.content()));
    });
    let result = rig.execute(&candidate).unwrap();
    assert_eq!(result.status, "unknown", "{}", result.message);
    // The restore waits the lease's restore delay after the paste chord.
    let slept = slept.lock().unwrap().clone();
    let (_, during) = slept
        .iter()
        .find(|(duration, _)| *duration == DEFAULT_RESTORE_DELAY)
        .expect("restore delay");
    assert_eq!(*during, pasted(&rig.clipboard, EXPANSION));
    assert_eq!(rig.clipboard.content(), clip("original"));
}
