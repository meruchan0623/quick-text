//! Application settings (0.5 adds `set_settings`, `get_prefs`, `set_prefs`).
use crate::{app_state::Managed, autostart, error::CommandError};
use tauri::State;

#[tauri::command]
pub fn set_autostart(enabled: bool, store: State<Managed>) -> Result<bool, CommandError> {
    store.lock().ensure_writable()?;
    autostart::set_enabled(enabled)
}
