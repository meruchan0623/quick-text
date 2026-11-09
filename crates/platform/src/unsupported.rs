use super::*;
pub fn foreground() -> Foreground {
    Foreground {
        id: 0,
        process: String::new(),
        title: String::new(),
        platform: std::env::consts::OS.into(),
        bundle_id: String::new(),
    }
}
pub fn engine_ready() -> bool {
    false
}
pub fn native_ready() -> Result<(), String> {
    Err("Native input backend is unsupported on this platform".into())
}
pub fn input_status() -> serde_json::Value {
    serde_json::json!({"platform":std::env::consts::OS,"reason":"Native input backend is unsupported"})
}
pub fn capture_selection() -> Result<String, String> {
    Err("Selection reading is unsupported on this platform".into())
}
pub fn cursor() -> serde_json::Value {
    serde_json::json!({"x":0,"y":0,"available":false})
}
pub fn start_engine(_: Arc<Shared>) -> Result<(), String> {
    Err("Native input backend is unavailable on this build; engine remains paused".into())
}
pub fn execute(
    _: Arc<Shared>,
    _: ActionPlan,
    _: Foreground,
    _: u64,
    _: usize,
    _: Option<String>,
    _: f64,
) -> Result<ExecutionResult, String> {
    Err("Native actions unavailable on this platform".into())
}
pub fn preflight(_: &ActionPlan) -> Result<(), String> {
    Err("Native actions unavailable on this platform".into())
}

/// The caret in screen pixels for the beginner hint overlay; unknown here, so the app places
/// the overlay by the pointer.
pub fn hint_anchor() -> Option<(i32, i32)> {
    None
}

/// Not used on this platform: the app shows its non-focusable window itself.
pub fn show_without_activation(_hwnd: isize, _x: i32, _y: i32, _w: i32, _h: i32) -> bool {
    false
}
