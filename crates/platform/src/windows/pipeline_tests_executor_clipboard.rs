//! L2 tests of the executor's clipboard path (P5-B) on the [`ExecRig`]: the
//! clipboard component in Health (probe, A2), and the lease's reason and
//! skipped formats reaching the history entry as raised (B-08, B-35).
use super::*;
use crate::windows::clipboard::OPEN_ATTEMPTS;

const CF_METAFILEPICT: u32 = 3;
const CF_PALETTE: u32 = 9;

/// The Clipboard status `probe` reports right now.
fn clipboard_status(rig: &ExecRig) -> Status {
    rig.core
        .probe(rig.clock.now())
        .into_iter()
        .find(|(component, _)| *component == Component::Clipboard)
        .map(|(_, status)| status)
        .expect("probe reports the clipboard")
}

fn clipboard_health(state: HealthState, reason: Option<ReasonCode>) -> ComponentHealth {
    ComponentHealth {
        component: Component::Clipboard,
        state,
        reason,
        since_ms: 0,
    }
}

/// The clipboard's own health (what its leases report, `clipboard_health`
/// in production) reaches Health through the executor's probe. Clipboard is
/// optional: Degraded only degrades Health, never pauses the engine, never
/// puts it into Recovering and never rebuilds the executor; the next lease
/// that keeps every section in time makes it Healthy again.
#[test]
fn a_busy_clipboard_only_degrades_health_and_is_never_rebuilt() {
    let mut rig = ExecRig::new(vec![text_rule("pst", OutputMethod::Paste, BODY)]);
    assert_eq!(
        rig.component(Component::Clipboard),
        Some((HealthState::Healthy, None))
    );
    assert!(capture::install());
    let mark = capture::mark();

    // Every OpenClipboard attempt of the backup finds the clipboard busy.
    rig.clipboard.set_busy(OPEN_ATTEMPTS);
    let sent = rig.injector.calls().len();
    assert_eq!(rig.run("pst").status, "failed");
    assert_eq!(
        rig.injector.calls().len(),
        sent,
        "no erase, no typing fallback"
    );
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason),
        (Outcome::Failed, ReasonCode::ClipboardBusy)
    );
    assert!(!entry.side_effects);

    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Degraded);
    assert_eq!(
        rig.component(Component::Clipboard),
        Some((HealthState::Degraded, Some(ReasonCode::ClipboardBusy)))
    );
    assert_eq!(
        rig.component(Component::Executor),
        Some((HealthState::Healthy, None))
    );
    assert!(rig.runtime.pause_reasons().is_empty(), "never paused");
    for backoff in BACKOFF {
        rig.clock.advance(backoff);
        rig.supervisor.tick();
        assert_eq!(rig.state(), HealthState::Degraded, "never Recovering");
    }
    assert!(
        !capture::since(mark)
            .iter()
            .any(|event| event.event == "executor.rebuilt"),
        "no rebuild for an optional component"
    );

    assert_eq!(rig.run("pst").status, "unknown");
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
    assert_eq!(
        rig.component(Component::Clipboard),
        Some((HealthState::Healthy, None))
    );
}

/// The probe never waits for the clipboard's health (A2): while it cannot
/// be read (a lease is updating it) the status last read is returned. A
/// clipboard reported Failed is still published Degraded (optional) and
/// sends the engine into neither Recovering nor a rebuild.
#[test]
fn the_probe_keeps_the_last_clipboard_status_while_it_cannot_read_it() {
    let rig = ExecRig::new(vec![]);
    let reported = Arc::new(Mutex::new(Some(clipboard_health(
        HealthState::Degraded,
        Some(ReasonCode::ClipboardBusy),
    ))));
    let source = Arc::clone(&reported);
    rig.core.set_clipboard_probe(move || {
        source
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    });
    let set = |health: Option<ComponentHealth>| {
        *reported.lock().unwrap_or_else(PoisonError::into_inner) = health;
    };
    assert_eq!(
        clipboard_status(&rig),
        Status::Degraded(ReasonCode::ClipboardBusy)
    );
    set(None);
    assert!(capture::install());
    let mark = capture::mark();
    assert_eq!(
        clipboard_status(&rig),
        Status::Degraded(ReasonCode::ClipboardBusy),
        "the last status read"
    );
    assert!(capture::since(mark)
        .iter()
        .any(|event| event.event == "executor.clipboard_probe_cached"));

    set(Some(clipboard_health(
        HealthState::Failed,
        Some(ReasonCode::ClipboardBusy),
    )));
    assert_eq!(
        clipboard_status(&rig),
        Status::Down(ReasonCode::ClipboardBusy)
    );
    let mark = capture::mark();
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Degraded);
    assert_eq!(
        rig.component(Component::Clipboard),
        Some((HealthState::Degraded, Some(ReasonCode::ClipboardBusy)))
    );
    for backoff in BACKOFF {
        rig.clock.advance(backoff);
        rig.supervisor.tick();
        assert_eq!(rig.state(), HealthState::Degraded, "never Recovering");
    }
    assert!(!capture::since(mark)
        .iter()
        .any(|event| event.event == "executor.rebuilt"));
    // Cached while unreadable, as Down with the reason it cannot keep.
    set(None);
    assert_eq!(clipboard_status(&rig), Status::Down(ReasonCode::Internal));

    set(Some(clipboard_health(HealthState::Healthy, None)));
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
}

/// I4a: the backup's verification never settles (every release of the
/// clipboard moves its sequence on). The lease gives up before anything was
/// written or erased: the run is Skipped with `ClipboardChanged`, the
/// lease's own reason, never `Internal` from its wording. Nothing was sent
/// (the abbreviation is still there) and the clipboard was never cleared.
#[test]
fn backup_verification_that_never_settles_is_skipped_with_the_trigger_kept() {
    let mut rig = ExecRig::new(vec![text_rule("pst", OutputMethod::Paste, BODY)]);
    let script = rig.clipboard.clone();
    // Lock 0 takes the backup, lock 1 verifies it.
    rig.clipboard.on_lock(move |index| {
        if index == 1 {
            script.churn_sequence(true);
        }
    });
    let sent = rig.injector.calls().len();
    let result = rig.run("pst");
    rig.clipboard.churn_sequence(false);
    rig.clipboard.on_lock(|_| {});
    assert_eq!(
        (
            result.status.as_str(),
            result.message.as_str(),
            result.side_effects
        ),
        (
            "failed",
            "Clipboard ownership could not be verified within the retry limit",
            false
        )
    );
    assert_eq!(
        rig.injector.calls().len(),
        sent,
        "no backspace, no paste, no typing fallback"
    );
    assert_eq!(rig.clipboard.clears(), 0, "the clipboard was never cleared");
    assert_eq!(rig.clipboard.content(), vec![(13, utf16("original"))]);
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason, entry.side_effects),
        (Outcome::Skipped, ReasonCode::ClipboardChanged, false)
    );
    assert!(entry.skipped_formats.is_empty());

    let result = rig.run("pst");
    assert_eq!(result.status, "unknown", "{}", result.message);
}

/// The history entry lists the formats the backup skipped (BestEffort,
/// restored without them) or, for a refused backup, the formats it would
/// have lost; a refusal is Skipped with the lease's reason and nothing sent.
#[test]
fn the_history_entry_lists_the_formats_the_backup_skipped() {
    let mut rig = ExecRig::new(vec![text_rule("pst", OutputMethod::Paste, BODY)]);
    rig.clipboard
        .take_over(9, vec![(13, utf16("original")), (CF_PALETTE, vec![0x9A])]);
    let result = rig.run("pst");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.clipboard.content(), vec![(13, utf16("original"))]);
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason),
        (Outcome::Succeeded, ReasonCode::Ok)
    );
    assert_eq!(entry.skipped_formats, vec![CF_PALETTE]);

    // Nothing re-synthesizes a lone CF_METAFILEPICT: the backup refuses.
    let lone = vec![(13, utf16("caption")), (CF_METAFILEPICT, vec![3])];
    rig.clipboard.take_over(9, lone.clone());
    let sent = rig.injector.calls().len();
    let result = rig.run("pst");
    assert_eq!(result.status, "failed");
    assert_eq!(rig.injector.calls().len(), sent, "no erase, no typing");
    assert_eq!(rig.clipboard.content(), lone);
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason, entry.side_effects),
        (Outcome::Skipped, ReasonCode::ClipboardFormatSkipped, false)
    );
    assert_eq!(entry.skipped_formats, vec![CF_METAFILEPICT]);

    rig.clipboard.take_over(9, vec![(13, utf16("original"))]);
    let result = rig.run("pst");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert!(rig.last_entry().skipped_formats.is_empty());
}

/// A manual run's entry gets the lease's reason and formats the same way
/// (`record_manual` reads the report, not the message).
#[test]
fn a_manual_paste_records_the_lease_reason_and_formats() {
    let rig = ExecRig::new(vec![]);
    rig.clipboard
        .take_over(9, vec![(13, utf16("caption")), (CF_METAFILEPICT, vec![3])]);
    super::actions::begin_trace();
    let manual = Serialized::new(&*rig.injector, Some(Arc::clone(&rig.core)));
    let clipboard = rig.clipboard.clone();
    let report = super::actions::run_plan(
        &Ports {
            injector: &manual,
            foreground: &rig.foreground,
            clock: &*rig.clock,
            clipboard: move || Ok(clipboard.clone()),
        },
        &rig.shared,
        ActionPlan {
            steps: vec![Step::new(
                Operation::Text {
                    text: BODY.into(),
                    method: OutputMethod::Paste,
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        },
        MockForeground::window(FOCUS, APP),
        rig.shared.generation.load(Ordering::SeqCst),
        None,
        None,
        1.0,
    );
    assert_eq!(report.reason, Some(ReasonCode::ClipboardFormatSkipped));
    assert_eq!(report.skipped_formats, vec![CF_METAFILEPICT]);
    let before = crate::runtime::history_seq();
    let process = "qt-manual-lease.exe";
    super::executor::record_manual(process, &report, Instant::now());
    assert!(report.result.is_err());
    assert!(rig.injector.calls().is_empty(), "nothing was sent");
    let entry = crate::runtime::list_history(Some(before))
        .into_iter()
        .rfind(|entry| entry.process.as_deref() == Some(process))
        .expect("the manual run was recorded");
    assert_eq!(
        (
            entry.trigger,
            entry.outcome,
            entry.reason,
            entry.side_effects
        ),
        (
            TriggerKind::Manual,
            Outcome::Skipped,
            ReasonCode::ClipboardFormatSkipped,
            false
        )
    );
    assert_eq!(entry.skipped_formats, vec![CF_METAFILEPICT]);
}
