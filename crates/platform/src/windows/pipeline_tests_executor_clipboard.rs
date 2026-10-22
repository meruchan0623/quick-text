//! L2 tests of the executor's clipboard path (P5-B) on the [`ExecRig`]: the
//! clipboard component in Health (probe, A2).
use super::*;
use crate::windows::clipboard::OPEN_ATTEMPTS;

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
