//! Application settings (0.5 adds `set_settings`, `get_prefs`, `set_prefs`).
use crate::{autostart, error::CommandError};

#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<bool, CommandError> {
    autostart::set_enabled(enabled)
}
