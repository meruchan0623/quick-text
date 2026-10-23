//! The data directory of the app before it was renamed Quicktext
//! (identifier `com.quickertext.desktop`), moved once to the new one.
use std::path::{Path, PathBuf};

/// The identifier the data directory was named after before the rename.
const LEGACY_IDENTIFIER: &str = "com.quickertext.desktop";

/// What [`migrate_data_dir`] did, logged once logging is up (it runs first).
#[derive(Debug, PartialEq)]
pub enum Migration {
    /// No legacy directory, or the new one already exists.
    None,
    /// The legacy directory was renamed to the new one.
    Moved,
    /// The rename failed (e.g. the old version still runs): the legacy
    /// directory is used as it is for this run and the move is retried at
    /// the next start.
    KeptLegacy(String),
}

/// Returns the data directory to use: `dir` (the new identifier's), after
/// renaming the legacy sibling to it when only the legacy one exists.
pub fn migrate_data_dir(dir: PathBuf) -> (PathBuf, Migration) {
    let Some(legacy) = dir.parent().map(|parent| parent.join(LEGACY_IDENTIFIER)) else {
        return (dir, Migration::None);
    };
    migrate_from(&legacy, dir)
}

fn migrate_from(legacy: &Path, dir: PathBuf) -> (PathBuf, Migration) {
    if dir.exists() || !legacy.is_dir() {
        return (dir, Migration::None);
    }
    match std::fs::rename(legacy, &dir) {
        Ok(()) => (dir, Migration::Moved),
        Err(e) => (legacy.to_path_buf(), Migration::KeptLegacy(e.to_string())),
    }
}

/// Logs the outcome of [`migrate_data_dir`].
pub fn log_migration(migration: &Migration) {
    match migration {
        Migration::None => {}
        Migration::Moved => tracing::info!(
            event = "store.legacy_dir_moved",
            "data directory moved from the legacy identifier"
        ),
        Migration::KeptLegacy(error) => tracing::warn!(
            event = "store.legacy_dir_kept",
            error = %error,
            "legacy data directory could not be moved; using it for this run"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("quicktext-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn moves_the_legacy_directory_only_when_the_new_one_is_missing() {
        let root = temp_root();
        let legacy = root.join(LEGACY_IDENTIFIER);
        let dir = root.join("com.quicktext.desktop");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("active.json"), b"{}").unwrap();

        assert_eq!(
            migrate_data_dir(dir.clone()),
            (dir.clone(), Migration::Moved)
        );
        assert!(dir.join("active.json").exists());
        assert!(!legacy.exists());

        // Both exist: the new one wins and the legacy one is left alone.
        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(
            migrate_data_dir(dir.clone()),
            (dir.clone(), Migration::None)
        );
        assert!(legacy.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nothing_to_move_without_a_legacy_directory() {
        let root = temp_root();
        let dir = root.join("com.quicktext.desktop");
        assert_eq!(
            migrate_data_dir(dir.clone()),
            (dir.clone(), Migration::None)
        );
        assert!(!dir.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
