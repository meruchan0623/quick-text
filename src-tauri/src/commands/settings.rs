//! Application settings (0.5 adds `set_settings`, `get_prefs`, `set_prefs`).
use crate::{app_state::Managed, autostart, error::CommandError, store::prefs};
use tauri::State;

#[tauri::command]
pub fn set_autostart(enabled: bool, store: State<Managed>) -> Result<bool, CommandError> {
    store.lock().ensure_writable()?;
    autostart::set_enabled(enabled)
}

/// Whether to start listening on its own at the next start (B-10, contract
/// §5): writes `prefs.json` `app.auto_listen_on_start` and returns the value
/// now stored. Allowed in safe mode, unlike `set_autostart`: prefs live
/// apart from the configuration, revisions and exports that safe mode
/// protects, nothing changes before the next start, and the write is a
/// read-modify-write that never replaces a prefs file it could not read.
#[tauri::command(rename_all = "snake_case")]
pub fn set_auto_listen(enabled: bool, store: State<Managed>) -> Result<bool, CommandError> {
    write_auto_listen(&store, enabled)
}

fn write_auto_listen(store: &Managed, enabled: bool) -> Result<bool, CommandError> {
    // The store lock is held only to read the directory, not for the write.
    let dir = store.lock().dir().to_path_buf();
    let changed = prefs::update_prefs(&dir, |prefs| {
        let changed = prefs.app.auto_listen_on_start != enabled;
        prefs.app.auto_listen_on_start = enabled;
        changed
    })?;
    tracing::info!(
        event = "prefs.auto_listen",
        enabled,
        changed,
        "auto listen on start set"
    );
    Ok(enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{error::SafeModeReason, store::Store};

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("quicker-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn set_auto_listen_writes_prefs_and_returns_the_stored_value() {
        let dir = temp_dir();
        let store = Managed::new(Store::load(dir.clone()));
        // On by default.
        assert!(prefs::auto_listen_on_start(&dir));
        assert!(!write_auto_listen(&store, false).unwrap());
        assert!(!prefs::auto_listen_on_start(&dir));
        // Asking again for the stored value succeeds and changes nothing.
        assert!(!write_auto_listen(&store, false).unwrap());
        assert!(write_auto_listen(&store, true).unwrap());
        assert!(prefs::auto_listen_on_start(&dir));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Prefs are not the configuration safe mode protects.
    #[test]
    fn set_auto_listen_is_allowed_in_safe_mode() {
        let dir = temp_dir();
        std::fs::write(dir.join("active.json"), b"{").unwrap();
        let store = Managed::new(Store::load(dir.clone()));
        assert_eq!(
            store.lock().safe_mode(),
            Some(SafeModeReason::NoValidConfig)
        );
        assert!(!write_auto_listen(&store, false).unwrap());
        assert!(!prefs::auto_listen_on_start(&dir));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
