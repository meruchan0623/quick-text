//! Application settings (0.5 adds `set_settings`, `get_prefs`, `set_prefs`).
use crate::autostart;

#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<bool, String> {
    autostart::set_enabled(enabled)
}
