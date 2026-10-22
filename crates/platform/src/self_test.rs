//! Self-test (S-09, B-10): four checks that the input engine can work, run
//! once Health is up at startup (before listening on its own) and on demand
//! by the `self_test` command (contract §5).
//!
//! The runner is platform-neutral. Each check runs on its own thread and is
//! bounded by its timeout, so a probe that blocks (a hung hook thread, a
//! clipboard another program keeps open) fails its check instead of holding
//! up the caller; such a thread is left to finish on its own. While the
//! checks run, `PauseReasons::SELF_TEST` holds the engine paused, and runs
//! are serialised so that the end of one never clears the pause under
//! another. No lock of the caller's is needed or taken.
//!
//! The probes live with the platform adapters (`windows/self_test_probes.rs`);
//! everywhere else every check fails with `unsupported`.
use crate::health::{HealthState, PauseReasons, ReasonCode};
use crate::runtime::Runtime;
use serde::Serialize;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long startup waits for Healthy before it runs the checks anyway
/// (B-10).
pub const STARTUP_HEALTHY_WAIT: Duration = Duration::from_secs(30);
const HEALTH_POLL: Duration = Duration::from_millis(100);

/// The checks, in report order (contract §5 `name`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckName {
    /// RawInput is registered to our detector window.
    RawInput,
    /// Both low-level hooks are installed and their thread answers.
    HookHeartbeat,
    /// The foreground window can be identified.
    Foreground,
    /// The clipboard can be opened.
    Clipboard,
}

impl CheckName {
    pub const ALL: [Self; 4] = [
        Self::RawInput,
        Self::HookHeartbeat,
        Self::Foreground,
        Self::Clipboard,
    ];
    /// The serde name, for logs and `self_test_failed { checks }`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RawInput => "raw_input",
            Self::HookHeartbeat => "hook_heartbeat",
            Self::Foreground => "foreground",
            Self::Clipboard => "clipboard",
        }
    }
    /// The reason of a check whose probe did not answer within its timeout.
    pub const fn timeout_reason(self) -> ReasonCode {
        match self {
            Self::RawInput => ReasonCode::RawInputMissing,
            Self::HookHeartbeat => ReasonCode::HookMissing,
            Self::Foreground => ReasonCode::Internal,
            Self::Clipboard => ReasonCode::ClipboardBusy,
        }
    }
}

/// One probe's verdict: passed, or why not.
pub type ProbeResult = Result<(), ReasonCode>;

/// The platform side of the self-test.
pub trait SelfTestProbes: Send + Sync + 'static {
    /// Runs one check. May block: the runner stops waiting after
    /// [`SelfTestProbes::timeout`] and reports the check failed. A panic
    /// fails the check with `internal`.
    fn probe(&self, check: CheckName) -> ProbeResult;
    /// How long the runner waits for `check`.
    fn timeout(&self, check: CheckName) -> Duration;
}

/// One check's result (contract §5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SelfTestCheck {
    pub name: CheckName,
    pub passed: bool,
    /// `None` exactly when the check passed.
    pub reason: Option<ReasonCode>,
    /// How long the probe took; its timeout when it did not answer in time.
    pub elapsed_ms: u64,
}

/// `self_test`'s return value (contract §5): every check, in
/// [`CheckName::ALL`] order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SelfTestReport {
    pub passed: bool,
    pub checks: Vec<SelfTestCheck>,
}

impl SelfTestReport {
    /// The checks that did not pass, in report order.
    pub fn failed(&self) -> Vec<CheckName> {
        self.checks
            .iter()
            .filter(|check| !check.passed)
            .map(|check| check.name)
            .collect()
    }
}

/// Every check fails with `unsupported`: platforms without Windows probes.
#[cfg_attr(windows, allow(dead_code))]
struct Unsupported;
impl SelfTestProbes for Unsupported {
    fn probe(&self, _check: CheckName) -> ProbeResult {
        Err(ReasonCode::Unsupported)
    }
    fn timeout(&self, _check: CheckName) -> Duration {
        Duration::from_secs(1)
    }
}

/// This platform's probes.
pub fn platform_probes() -> Arc<dyn SelfTestProbes> {
    #[cfg(windows)]
    {
        Arc::new(crate::windows::WinSelfTest)
    }
    #[cfg(not(windows))]
    {
        Arc::new(Unsupported)
    }
}

/// [`run`] with this platform's probes.
pub fn run_platform(runtime: &Runtime) -> SelfTestReport {
    run(runtime, platform_probes())
}

/// One run at a time: SELF_TEST is a single bit.
static RUNS: Mutex<()> = Mutex::new(());

/// Holds `PauseReasons::SELF_TEST` for its lifetime, also through a panic.
struct Pausing<'a>(&'a Runtime);
impl<'a> Pausing<'a> {
    fn hold(runtime: &'a Runtime) -> Self {
        runtime.set_pause(PauseReasons::SELF_TEST, true);
        Self(runtime)
    }
}
impl Drop for Pausing<'_> {
    fn drop(&mut self) {
        self.0.set_pause(PauseReasons::SELF_TEST, false);
    }
}

/// Runs every check with SELF_TEST set on `runtime` and returns the report.
/// Returns within the longest check timeout (after any run already in
/// progress has finished). Logs `self_test.check` per check and
/// `self_test.finished`.
pub fn run(runtime: &Runtime, probes: Arc<dyn SelfTestProbes>) -> SelfTestReport {
    let _one = RUNS.lock().unwrap_or_else(PoisonError::into_inner);
    let _pausing = Pausing::hold(runtime);
    let checks = run_checks(&probes);
    let report = SelfTestReport {
        passed: checks.iter().all(|check| check.passed),
        checks,
    };
    for check in &report.checks {
        tracing::info!(
            event = "self_test.check",
            check = check.name.as_str(),
            passed = check.passed,
            reason = ?check.reason,
            elapsed_ms = check.elapsed_ms
        );
    }
    let failed: Vec<_> = report.failed().into_iter().map(CheckName::as_str).collect();
    tracing::info!(
        event = "self_test.finished",
        passed = report.passed,
        failed = ?failed,
        "self-test finished"
    );
    report
}

/// Starts every probe on its own thread, then collects answers until each
/// check has answered or its deadline has passed. An answer that took
/// longer than the check's timeout counts as a timeout.
fn run_checks(probes: &Arc<dyn SelfTestProbes>) -> Vec<SelfTestCheck> {
    let started = Instant::now();
    let (tx, rx) = mpsc::channel();
    let mut answers: [Option<(ProbeResult, Duration)>; 4] = Default::default();
    for (slot, check) in CheckName::ALL.into_iter().enumerate() {
        let (tx, worker) = (tx.clone(), Arc::clone(probes));
        let spawned = std::thread::Builder::new()
            .name("qt-self-test".into())
            .spawn(move || {
                let begun = Instant::now();
                let result = catch_unwind(AssertUnwindSafe(|| worker.probe(check)))
                    .unwrap_or(Err(ReasonCode::Internal));
                let _ = tx.send((slot, result, begun.elapsed()));
            });
        if spawned.is_err() {
            answers[slot] = Some((Err(ReasonCode::Internal), Duration::ZERO));
        }
    }
    drop(tx);
    loop {
        let last = CheckName::ALL
            .into_iter()
            .zip(&answers)
            .filter(|(_, answer)| answer.is_none())
            .map(|(check, _)| started + probes.timeout(check))
            .max();
        let Some(last) = last else { break };
        let left = last.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((slot, result, took)) => {
                let in_time = CheckName::ALL
                    .get(slot)
                    .is_some_and(|check| took <= probes.timeout(*check));
                if let (true, Some(answer)) = (in_time, answers.get_mut(slot)) {
                    *answer = Some((result, took));
                }
            }
            // Every deadline has passed, or every probe has answered.
            Err(_) => break,
        }
    }
    CheckName::ALL
        .into_iter()
        .zip(answers)
        .map(|(name, answer)| {
            let (result, took) =
                answer.unwrap_or((Err(name.timeout_reason()), probes.timeout(name)));
            SelfTestCheck {
                name,
                passed: result.is_ok(),
                reason: result.err(),
                elapsed_ms: u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
            }
        })
        .collect()
}

/// Waits, on the runtime's clock, until Health is Healthy, `limit` has
/// passed or the engine is shutting down, and returns the last Health state
/// seen. Startup runs the checks afterwards whatever the outcome: a check
/// that depends on a component that is not up fails with its reason.
pub fn wait_for_healthy(runtime: &Runtime, limit: Duration) -> HealthState {
    let clock = Arc::clone(runtime.clock());
    let deadline = clock.now() + limit;
    loop {
        let state = runtime.health().state;
        let now = clock.now();
        if state == HealthState::Healthy || runtime.shutting_down() || now >= deadline {
            return state;
        }
        clock.sleep(HEALTH_POLL.min(deadline - now));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::health::HealthSnapshot;
    use crate::mock::FakeClock;
    use crate::traits::SystemClock;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Each check's behaviour; records whether SELF_TEST was set while a
    /// probe ran.
    struct Fake {
        runtime: Arc<Runtime>,
        timeout: Duration,
        verdicts: [Verdict; 4],
        paused_during: AtomicBool,
    }
    #[derive(Clone, Copy)]
    enum Verdict {
        Pass,
        Fail(ReasonCode),
        Panic,
        Hang(Duration),
    }
    impl SelfTestProbes for Fake {
        fn probe(&self, check: CheckName) -> ProbeResult {
            if self
                .runtime
                .pause_reasons()
                .contains(PauseReasons::SELF_TEST)
            {
                self.paused_during.store(true, Ordering::SeqCst);
            }
            let slot = CheckName::ALL.iter().position(|c| *c == check).unwrap();
            match self.verdicts[slot] {
                Verdict::Pass => Ok(()),
                Verdict::Fail(reason) => Err(reason),
                Verdict::Panic => panic!("probe panicked"),
                Verdict::Hang(by) => {
                    std::thread::sleep(by);
                    Ok(())
                }
            }
        }
        fn timeout(&self, _check: CheckName) -> Duration {
            self.timeout
        }
    }

    fn fake(verdicts: [Verdict; 4]) -> (Arc<Runtime>, Arc<Fake>) {
        let runtime = Arc::new(Runtime::new(Arc::new(SystemClock)));
        let probes = Arc::new(Fake {
            runtime: runtime.clone(),
            timeout: Duration::from_millis(200),
            verdicts,
            paused_during: AtomicBool::new(false),
        });
        (runtime, probes)
    }

    #[test]
    fn passing_checks_report_in_order_and_hold_self_test_only_while_running() {
        let (runtime, probes) = fake([Verdict::Pass; 4]);
        let report = run(&runtime, probes.clone());
        assert!(report.passed);
        assert_eq!(
            report.checks.iter().map(|c| c.name).collect::<Vec<_>>(),
            CheckName::ALL
        );
        assert!(report.checks.iter().all(|c| c.passed && c.reason.is_none()));
        assert!(report.failed().is_empty());
        assert!(probes.paused_during.load(Ordering::SeqCst));
        assert!(!runtime.pause_reasons().contains(PauseReasons::SELF_TEST));
    }

    /// A failing, a panicking and a hanging probe each fail their own check;
    /// the hung one does not hold up the run, and SELF_TEST is cleared.
    #[test]
    fn failures_panics_and_timeouts_fail_their_check_only() {
        let (runtime, probes) = fake([
            Verdict::Pass,
            Verdict::Hang(Duration::from_secs(2)),
            Verdict::Panic,
            Verdict::Fail(ReasonCode::ClipboardBusy),
        ]);
        runtime.set_pause(PauseReasons::MANUAL, true);
        let begun = Instant::now();
        let report = run(&runtime, probes);
        assert!(
            begun.elapsed() < Duration::from_secs(1),
            "{:?}",
            begun.elapsed()
        );
        assert!(!report.passed);
        assert_eq!(
            report.failed(),
            [
                CheckName::HookHeartbeat,
                CheckName::Foreground,
                CheckName::Clipboard
            ]
        );
        let reasons: Vec<_> = report.checks.iter().map(|c| c.reason).collect();
        assert_eq!(
            reasons,
            [
                None,
                Some(ReasonCode::HookMissing),
                Some(ReasonCode::Internal),
                Some(ReasonCode::ClipboardBusy)
            ]
        );
        assert_eq!(
            report.checks[1].elapsed_ms, 200,
            "a timeout reports the timeout"
        );
        // Other pause reasons are left alone.
        assert_eq!(runtime.pause_reasons(), PauseReasons::MANUAL);
    }

    #[test]
    fn other_platforms_fail_every_check_as_unsupported() {
        let runtime = Runtime::new(Arc::new(SystemClock));
        let report = run(&runtime, Arc::new(Unsupported));
        assert!(!report.passed);
        assert_eq!(report.failed(), CheckName::ALL);
        assert!(report
            .checks
            .iter()
            .all(|c| c.reason == Some(ReasonCode::Unsupported)));
    }

    /// docs/ipc-contract-0.4.md §5 `self_test`.
    #[test]
    fn report_serializes_as_the_contract_shape() {
        let report = SelfTestReport {
            passed: false,
            checks: vec![
                SelfTestCheck {
                    name: CheckName::RawInput,
                    passed: true,
                    reason: None,
                    elapsed_ms: 3,
                },
                SelfTestCheck {
                    name: CheckName::HookHeartbeat,
                    passed: false,
                    reason: Some(ReasonCode::HookMissing),
                    elapsed_ms: 1500,
                },
            ],
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({"passed": false, "checks": [
                {"name": "raw_input", "passed": true, "reason": null, "elapsed_ms": 3},
                {"name": "hook_heartbeat", "passed": false, "reason": "hook_missing", "elapsed_ms": 1500}
            ]})
        );
        for check in CheckName::ALL {
            assert_eq!(serde_json::to_value(check).unwrap(), check.as_str());
        }
    }

    #[test]
    fn startup_waits_for_healthy_at_most_the_limit() {
        let healthy = HealthSnapshot {
            state: HealthState::Healthy,
            components: vec![],
        };
        // Healthy after a second: the wait ends then.
        let clock = Arc::new(FakeClock::new());
        let runtime = Arc::new(Runtime::new(clock.clone()));
        let (publisher, at) = (runtime.clone(), clock.clone());
        clock.on_sleep(move |_| {
            if at.elapsed() >= Duration::from_secs(1) {
                publisher.publish_health(healthy.clone());
            }
        });
        assert_eq!(
            wait_for_healthy(&runtime, STARTUP_HEALTHY_WAIT),
            HealthState::Healthy
        );
        assert!(clock.elapsed() <= Duration::from_millis(1200));
        // Never Healthy: gives up after exactly the limit.
        let clock = Arc::new(FakeClock::new());
        let runtime = Runtime::new(clock.clone());
        assert_eq!(
            wait_for_healthy(&runtime, STARTUP_HEALTHY_WAIT),
            HealthState::Starting
        );
        assert_eq!(clock.elapsed(), STARTUP_HEALTHY_WAIT);
    }
}
