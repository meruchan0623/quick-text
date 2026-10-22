//! Fault injection points (B-52, §11). Compiled to no-ops unless the
//! `fault-injection` feature is enabled; release builds must never enable it
//! (CI checks `cargo tree -e features` and the archived PDB for
//! `debug_inject_fault`).
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultPoint {
    ExecutorPanic,
    ExecutorHang,
    HookPanic,
    HookUnhook,
    RawinputClose,
    RawinputUnregister,
    ClipboardBusy,
    StoreWriteFail,
    ConfigCorrupt,
    StoreLocked,
    AutostartStale,
    SessionEnd,
}

impl FaultPoint {
    pub const ALL: [FaultPoint; 12] = [
        Self::ExecutorPanic,
        Self::ExecutorHang,
        Self::HookPanic,
        Self::HookUnhook,
        Self::RawinputClose,
        Self::RawinputUnregister,
        Self::ClipboardBusy,
        Self::StoreWriteFail,
        Self::ConfigCorrupt,
        Self::StoreLocked,
        Self::AutostartStale,
        Self::SessionEnd,
    ];
    const fn index(self) -> usize {
        self as usize
    }
}

#[cfg(feature = "fault-injection")]
mod armed {
    use super::FaultPoint;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 0 = disarmed; otherwise `param + 1` (param is e.g. a duration in ms).
    static ARMED: [AtomicU64; FaultPoint::ALL.len()] =
        [const { AtomicU64::new(0) }; FaultPoint::ALL.len()];

    pub fn arm(point: FaultPoint, param: u64) {
        ARMED[point.index()].store(param.saturating_add(1), Ordering::SeqCst);
    }
    pub fn disarm(point: FaultPoint) {
        ARMED[point.index()].store(0, Ordering::SeqCst);
    }
    pub fn take(point: FaultPoint) -> Option<u64> {
        match ARMED[point.index()].swap(0, Ordering::SeqCst) {
            0 => None,
            armed => Some(armed - 1),
        }
    }
    pub fn peek(point: FaultPoint) -> Option<u64> {
        match ARMED[point.index()].load(Ordering::SeqCst) {
            0 => None,
            armed => Some(armed - 1),
        }
    }
}

/// Arm a one-shot fault. `param` is point-specific (milliseconds for
/// `ExecutorHang` and `ClipboardBusy`, ignored elsewhere).
#[cfg(feature = "fault-injection")]
pub fn debug_inject_fault(point: FaultPoint, param: u64) {
    armed::arm(point, param);
}

#[cfg(feature = "fault-injection")]
pub fn disarm(point: FaultPoint) {
    armed::disarm(point);
}

/// Consume an armed fault at an injection point. Always `None` without the
/// `fault-injection` feature, so the call sites compile away.
#[inline(always)]
pub fn take(point: FaultPoint) -> Option<u64> {
    #[cfg(feature = "fault-injection")]
    {
        armed::take(point)
    }
    #[cfg(not(feature = "fault-injection"))]
    {
        let _ = point.index();
        None
    }
}

/// Observe without consuming (for faults that must stay active, e.g. a lock).
#[inline(always)]
pub fn peek(point: FaultPoint) -> Option<u64> {
    #[cfg(feature = "fault-injection")]
    {
        armed::peek(point)
    }
    #[cfg(not(feature = "fault-injection"))]
    {
        let _ = point.index();
        None
    }
}

#[cfg(all(test, feature = "fault-injection"))]
mod tests {
    use super::*;

    #[test]
    fn one_shot_with_param() {
        debug_inject_fault(FaultPoint::ClipboardBusy, 250);
        assert_eq!(peek(FaultPoint::ClipboardBusy), Some(250));
        assert_eq!(take(FaultPoint::ClipboardBusy), Some(250));
        assert_eq!(take(FaultPoint::ClipboardBusy), None);
    }
}
