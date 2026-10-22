//! IME open-state probe.
use super::*;
pub(super) fn ime_active() -> bool {
    unsafe {
        let h = GetForegroundWindow();
        let ctx = ImmGetContext(h);
        if ctx.is_null() {
            return false;
        }
        let active = ImmGetOpenStatus(ctx) != 0;
        ImmReleaseContext(h, ctx);
        active
    }
}
