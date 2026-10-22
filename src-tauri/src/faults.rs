//! Host-side fault injection points (B-52). Without the `fault-injection`
//! feature every call is `None` and compiles away.
use quicker_platform::fault::{self, FaultPoint};

/// Consumes a one-shot fault armed with `debug_inject_fault`.
pub fn take(point: FaultPoint) -> Option<u64> {
    if !scope::active() {
        return None;
    }
    fault::take(point)
}

/// Observes a fault that stays armed until disarmed (e.g. a lock).
pub fn peek(point: FaultPoint) -> Option<u64> {
    if !scope::active() {
        return None;
    }
    fault::peek(point)
}

/// Fault points are process-wide statics while unit tests run in parallel:
/// in test builds a fault is visible only on a thread that holds a
/// [`scope::FaultScope`], so other tests never consume it.
#[cfg(not(test))]
mod scope {
    #[inline(always)]
    pub fn active() -> bool {
        true
    }
}
#[cfg(test)]
pub mod scope {
    use std::cell::Cell;

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
    }
    pub fn active() -> bool {
        ACTIVE.with(Cell::get)
    }

    /// Serializes fault tests and enables faults on the current thread;
    /// dropping it disarms every point.
    #[cfg(feature = "fault-injection")]
    pub struct FaultScope {
        _serial: parking_lot::MutexGuard<'static, ()>,
    }
    #[cfg(feature = "fault-injection")]
    impl FaultScope {
        pub fn new() -> Self {
            static SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
            let serial = SERIAL.lock();
            disarm_all();
            ACTIVE.with(|a| a.set(true));
            Self { _serial: serial }
        }
    }
    #[cfg(feature = "fault-injection")]
    impl Drop for FaultScope {
        fn drop(&mut self) {
            ACTIVE.with(|a| a.set(false));
            disarm_all();
        }
    }
    #[cfg(feature = "fault-injection")]
    fn disarm_all() {
        for point in quicker_platform::fault::FaultPoint::ALL {
            quicker_platform::fault::disarm(point);
        }
    }
}
