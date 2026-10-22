//! Focus epochs shared by the RawInput detector, hooks and transaction guard.
//! Text-body observation is no longer an input or output dependency.
use super::*;
pub(super) static FOCUS_EPOCH: AtomicU64 = AtomicU64::new(0);
pub(super) static CARET_EPOCH: AtomicU64 = AtomicU64::new(0);
pub(super) fn status() -> serde_json::Value {
    serde_json::json!({"provider":"optional_uia_protection_only", "available":false,
        "composition":"unknown", "blocked_reason":"Text readback is not used by the Espanso input backend"})
}
