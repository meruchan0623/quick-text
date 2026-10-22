//! Startup load of the data directory (B-04 replaces this with `load_or_fallback`).
use super::{
    atomic::atomic_json,
    metadata::{metadata_for_config, read_metadata},
    Store,
};
use crate::error::diagnostics;
use quicker_domain::*;
use quicker_platform::Shared;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

impl Store {
    pub fn load(dir: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join("active.json");
        let active = if path.exists() {
            read_config(&path)?
        } else {
            Config::default()
        };
        compile_config(&active).map_err(diagnostics)?;
        let draft = if dir.join("draft.json").exists() {
            read_config(&dir.join("draft.json"))?
        } else {
            active.clone()
        };
        let historical = dir
            .join("revisions")
            .join(format!("{}.json", active.revision.saturating_sub(1)));
        let previous = if active.revision > 0 && historical.exists() {
            Some(read_config(&historical)?)
        } else if dir.join("previous.json").exists() {
            Some(read_config(&dir.join("previous.json"))?)
        } else {
            None
        };
        let engine = Shared::new();
        *engine.config.lock().unwrap() = active.clone();
        *engine.compiled.lock().unwrap() = compile_config(&active).map_err(diagnostics)?;
        let disk_hash = hash_file(&path)?;
        let unresolved = read_metadata(&dir.join("migration-unresolved.json"));
        let active_metadata_path = dir
            .join("revisions")
            .join(format!("{}.metadata.json", active.revision));
        let active_unresolved = if active_metadata_path.exists() {
            read_metadata(&active_metadata_path)
        } else if dir.join("active-migration-unresolved.json").exists() {
            read_metadata(&dir.join("active-migration-unresolved.json"))
        } else {
            metadata_for_config(&active, &unresolved)
        };
        // Persist the legacy fallback before a later draft import can replace it.
        fs::create_dir_all(dir.join("revisions")).map_err(|e| e.to_string())?;
        atomic_json(&active_metadata_path, &active_unresolved)?;
        let previous_unresolved = previous
            .as_ref()
            .map(|config| {
                let path = dir
                    .join("revisions")
                    .join(format!("{}.metadata.json", config.revision));
                if path.exists() {
                    read_metadata(&path)
                } else {
                    metadata_for_config(config, &unresolved)
                }
            })
            .unwrap_or_default();
        Ok(Self {
            dir,
            draft,
            active,
            previous,
            disk_hash,
            engine,
            unresolved,
            active_unresolved,
            previous_unresolved,
            pending_import: None,
        })
    }
}
pub(super) fn read_config(path: &Path) -> Result<Config, String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Configuration read failed: {e}"))
}
pub(super) fn hash_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(
        Sha256::digest(fs::read(path).map_err(|e| e.to_string())?).to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    #[test]
    fn legacy_metadata_is_snapshotted_before_replacing_draft() {
        let dir =
            std::env::temp_dir().join(format!("quicker-metadata-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let a = metadata_config(&["shared"]);
        atomic_json(&dir.join("active.json"), &a).unwrap();
        atomic_json(
            &dir.join("migration-unresolved.json"),
            &vec![original("shared", "legacy-A")],
        )
        .unwrap();
        let mut store = Store::load(dir.clone()).unwrap();
        store.pending_import = Some((a.clone(), vec![original("shared", "B")]));
        store.save(a).unwrap();
        drop(store);
        let mut store = Store::load(dir.clone()).unwrap();
        store.discard().unwrap();
        assert_eq!(store.unresolved[0].original["synthetic_source"], "legacy-A");
        fs::remove_dir_all(dir).unwrap();
    }
}
