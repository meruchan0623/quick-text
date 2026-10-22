//! Windows self-test probes (S-09, B-10; runner in `crate::self_test`).
//!
//! Every probe is read-only: nothing is installed, registered, injected,
//! written to the clipboard or logged about the foreground window. Each
//! gives up on its own after [`PROBE_WAIT`], before the runner's
//! [`CHECK_TIMEOUT`], so its thread is not left behind in the common cases.
//! 待实测: all four probes on a real desktop (cold start at login, a locked
//! workstation, a clipboard held open by another program).
use super::input::Registration;
use super::*;
use crate::health::{Component, ComponentHealth, HealthState, ReasonCode};
use crate::self_test::{CheckName, ProbeResult, SelfTestProbes};

/// How long a probe keeps retrying before it fails its check.
const PROBE_WAIT: Duration = Duration::from_millis(1500);
const PROBE_POLL: Duration = Duration::from_millis(25);
/// How long the runner waits for a probe.
const CHECK_TIMEOUT: Duration = Duration::from_millis(2500);

/// The production probes.
pub(crate) struct WinSelfTest;

impl SelfTestProbes for WinSelfTest {
    fn probe(&self, check: CheckName) -> ProbeResult {
        match check {
            CheckName::RawInput => raw_input(),
            CheckName::HookHeartbeat => hook_heartbeat(),
            CheckName::Foreground => foreground_window(),
            CheckName::Clipboard => clipboard_opens(),
        }
    }
    fn timeout(&self, _check: CheckName) -> Duration {
        CHECK_TIMEOUT
    }
}

/// Retries `attempt` every [`PROBE_POLL`] until it gives `Some` or
/// [`PROBE_WAIT`] has passed.
fn retry<T>(mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + PROBE_WAIT;
    loop {
        if let Some(found) = attempt() {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(PROBE_POLL);
    }
}

/// The input sources' health table, without blocking on a re-attach that
/// holds it.
fn input_table() -> Option<Vec<ComponentHealth>> {
    retry(input::try_input_health)
}

/// `component` is Healthy in `table`; otherwise its reason, or `missing`.
fn healthy(table: &[ComponentHealth], component: Component, missing: ReasonCode) -> ProbeResult {
    match table.iter().find(|entry| entry.component == component) {
        Some(entry) if entry.state == HealthState::Healthy => Ok(()),
        Some(entry) => Err(entry.reason.unwrap_or(missing)),
        None => Err(missing),
    }
}

/// RawInput is Healthy, its detector window exists and the process's
/// keyboard registration points at it (or cannot be read, which the input
/// sources also accept).
fn raw_input() -> ProbeResult {
    let missing = ReasonCode::RawInputMissing;
    let table = input_table().ok_or(missing)?;
    healthy(&table, Component::RawInput, missing)?;
    if !rawinput::alive() {
        return Err(missing);
    }
    match rawinput::registration() {
        Registration::Ours | Registration::Unknown => Ok(()),
        Registration::Missing | Registration::Elsewhere => Err(missing),
    }
}

/// Both low-level hooks are Healthy and the hook thread answers a message.
fn hook_heartbeat() -> ProbeResult {
    let missing = ReasonCode::HookMissing;
    let table = input_table().ok_or(missing)?;
    healthy(&table, Component::KeyboardHook, missing)?;
    healthy(&table, Component::MouseHook, missing)?;
    if hook::ping(PROBE_WAIT) {
        Ok(())
    } else {
        Err(missing)
    }
}

/// A foreground window exists and belongs to a process we can name by id.
/// Reads no title. Fails with `focus_changed` (no window has focus: the
/// secure desktop, a locked session, a switch in progress).
fn foreground_window() -> ProbeResult {
    let found = retry(|| {
        // SAFETY: these calls accept any window handle, including null.
        unsafe {
            let window = GetForegroundWindow();
            if window.is_null() {
                return None;
            }
            let mut pid = 0;
            GetWindowThreadProcessId(window, &mut pid);
            let focused = foreground::focus_handle(window);
            (pid != 0 && !focused.is_null()).then_some(())
        }
    });
    found.ok_or(ReasonCode::FocusChanged)
}

/// The clipboard can be opened, and is closed again at once. Nothing is
/// read or written, so the sequence number and owner the clipboard lease
/// checks stay unchanged. Fails with `clipboard_busy` while another
/// program keeps it open.
fn clipboard_opens() -> ProbeResult {
    let opened = retry(|| {
        // SAFETY: a null owner is allowed; the clipboard is closed on the
        // same thread right after it was opened.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }
            CloseClipboard();
        }
        Some(())
    });
    opened.ok_or(ReasonCode::ClipboardBusy)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn entry(
        component: Component,
        state: HealthState,
        reason: Option<ReasonCode>,
    ) -> ComponentHealth {
        ComponentHealth {
            component,
            state,
            reason,
            since_ms: 0,
        }
    }

    /// The table half of the RawInput and hook probes (no system calls).
    #[test]
    fn a_component_passes_only_when_healthy() {
        let table = [
            entry(Component::KeyboardHook, HealthState::Healthy, None),
            entry(
                Component::MouseHook,
                HealthState::Recovering,
                Some(ReasonCode::HookInstallFailed),
            ),
            entry(Component::RawInput, HealthState::Starting, None),
        ];
        let missing = ReasonCode::HookMissing;
        assert_eq!(healthy(&table, Component::KeyboardHook, missing), Ok(()));
        assert_eq!(
            healthy(&table, Component::MouseHook, missing),
            Err(ReasonCode::HookInstallFailed)
        );
        assert_eq!(
            healthy(&table, Component::RawInput, ReasonCode::RawInputMissing),
            Err(ReasonCode::RawInputMissing)
        );
        assert_eq!(
            healthy(&table, Component::FocusEvents, missing),
            Err(missing)
        );
    }

    /// The probes give up before the runner stops waiting for them.
    #[test]
    fn probes_give_up_before_the_runner_does() {
        assert!(PROBE_WAIT + PROBE_POLL * 4 < CHECK_TIMEOUT);
        for check in CheckName::ALL {
            assert_eq!(WinSelfTest.timeout(check), CHECK_TIMEOUT);
        }
    }

    #[test]
    fn retry_stops_at_the_first_answer() {
        let mut calls = 0;
        assert_eq!(
            retry(|| {
                calls += 1;
                (calls == 3).then_some(calls)
            }),
            Some(3)
        );
    }
}
