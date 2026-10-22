//! Platform-agnostic ports (§7.1, B-48). 0.4 keeps them in the platform
//! crate; 0.5 moves them to `crates/engine/src/ports.rs` (B-49).
use crate::health::EngineEvent;
use std::time::{Duration, Instant};

/// Engine → shell event channel. Implementations must not block: the
/// supervisor and executor call this from their own threads.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: EngineEvent);
}

/// Discards every event (default before the shell attaches its EventBus).
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: EngineEvent) {}
}

/// Monotonic time source, so backoff, timeouts and log-level TTLs can be
/// driven by a fake clock in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    /// Milliseconds since the Unix epoch (for timestamps shown to users).
    fn wall_ms(&self) -> i64;
    fn sleep(&self, duration: Duration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn wall_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration)
    }
}
