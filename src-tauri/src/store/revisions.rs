//! Publishing a configuration as a new revision, and rolling back to the previous one.
use super::{
    atomic::atomic_json,
    load::{hash_file, read_config},
    metadata::metadata_for_config,
    Store,
};
use crate::error::{CommandError, EntityKind, IoOp};
use quicker_domain::*;
use sha2::{Digest, Sha256};
use std::{fs, sync::atomic::Ordering};

impl Store {
    pub fn publish(&mut self, config: Config) -> Result<(), CommandError> {
        let metadata = metadata_for_config(&config, &self.unresolved);
        self.publish_with_metadata(config, metadata)
    }
    fn publish_with_metadata(
        &mut self,
        mut config: Config,
        metadata: Vec<MigrationItem>,
    ) -> Result<(), CommandError> {
        config.revision = self.active.revision.checked_add(1).ok_or_else(|| {
            CommandError::internal(
                "Revision counter exhausted; preserve data and start a new profile",
            )
        })?;
        let compiled = compile_config(&config).map_err(CommandError::validation)?;
        let active_path = self.dir.join("active.json");
        if hash_file(&active_path)? != self.disk_hash {
            // Edited externally: report the revision now on disk so the caller can reload.
            let current = read_config(&active_path)
                .map(|c| c.revision)
                .unwrap_or(self.active.revision);
            return Err(CommandError::RevisionConflict { current });
        }
        let was_paused = self.engine.paused.swap(true, Ordering::SeqCst);
        self.engine.cancel.store(true, Ordering::SeqCst);
        self.engine.generation.fetch_add(1, Ordering::SeqCst);
        if self.engine.busy.load(Ordering::SeqCst) {
            self.engine.paused.store(was_paused, Ordering::SeqCst);
            return Err(CommandError::Busy);
        }
        config.revision = match self.active.revision.checked_add(1) {
            Some(revision) => revision,
            None => {
                self.engine.paused.store(was_paused, Ordering::SeqCst);
                return Err(CommandError::internal(
                    "Revision counter exhausted; preserve data and start a new profile",
                ));
            }
        };
        let old = self.active.clone();
        let outcome = (|| {
            let revisions = self.dir.join("revisions");
            fs::create_dir_all(&revisions)
                .map_err(|e| CommandError::io(IoOp::CreateDir, Some(&revisions), &e))?;
            let backup = revisions.join(format!("{}.json", old.revision));
            if backup.exists() {
                if read_config(&backup)? != old {
                    return Err(CommandError::internal(
                        "Historical revision conflict; existing snapshot preserved",
                    ));
                }
            } else {
                atomic_json(&backup, &old)?;
            }
            atomic_json(
                &revisions.join(format!("{}.metadata.json", old.revision)),
                &self.active_unresolved,
            )?;
            atomic_json(&self.dir.join("draft.json"), &config)?;
            atomic_json(
                &revisions.join(format!("{}.metadata.json", config.revision)),
                &metadata,
            )?;
            atomic_json(&self.dir.join("migration-unresolved.json"), &metadata)?;
            atomic_json(
                &self.dir.join("active-migration-unresolved.json"),
                &metadata,
            )?;
            atomic_json(&self.dir.join("active.json"), &config)?;
            Ok::<_, CommandError>(())
        })();
        if let Err(e) = outcome {
            self.engine.paused.store(was_paused, Ordering::SeqCst);
            return Err(e);
        }
        *self.engine.compiled.lock().unwrap() = compiled;
        *self.engine.config.lock().unwrap() = config.clone();
        self.active = config.clone();
        self.previous = Some(old);
        self.previous_unresolved = self.active_unresolved.clone();
        self.active_unresolved = metadata.clone();
        self.unresolved = metadata;
        self.pending_import = None;
        self.draft = config;
        self.disk_hash = Some(
            Sha256::digest(
                serde_json::to_vec_pretty(&self.active).expect("validated config serialization"),
            )
            .to_vec(),
        );
        self.engine.paused.store(was_paused, Ordering::SeqCst);
        Ok(())
    }
    pub fn rollback_previous(&mut self) -> Result<(), CommandError> {
        let config = self
            .previous
            .clone()
            .ok_or_else(|| CommandError::NotFound {
                kind: EntityKind::Revision,
                id: "previous".into(),
            })?;
        self.publish_with_metadata(config, self.previous_unresolved.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    #[test]
    fn rollback_restores_revision_originals_after_reload() {
        let dir =
            std::env::temp_dir().join(format!("quicker-metadata-test-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone()).unwrap();
        for marker in ["A", "B"] {
            let mut config = metadata_config(&["shared"]);
            config.rules[0].description = marker.into();
            store.pending_import = Some((config.clone(), vec![original("shared", marker)]));
            store.save(config).unwrap();
            store.publish(store.draft.clone()).unwrap();
        }
        drop(store);
        let mut store = Store::load(dir.clone()).unwrap();
        store.rollback_previous().unwrap();
        assert_eq!(store.active.revision, 3);
        assert_eq!(store.active.rules[0].description, "A");
        assert_eq!(store.unresolved[0].original["synthetic_source"], "A");
        drop(store);
        let store = Store::load(dir.clone()).unwrap();
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "A");
        assert_eq!(
            store.previous_unresolved[0].original["synthetic_source"],
            "B"
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn persistent_revision_conflict() {
        let dir = std::env::temp_dir().join(format!("quicker-text-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone()).unwrap();
        s.publish(Config::default()).unwrap();
        assert_eq!(s.active.revision, 1);
        let mut altered = s.active.clone();
        altered.enabled = false;
        atomic_json(&dir.join("active.json"), &altered).unwrap();
        assert!(s.publish(Config::default()).is_err());
        assert_eq!(s.active.revision, 1);
        fs::remove_dir_all(dir).unwrap();
    }
    /// No-regression list #8 (§5.5): a failed publish keeps the active config and pause state.
    #[test]
    fn failed_publish_preserves_active_and_pause() {
        let dir = std::env::temp_dir().join(format!("quicker-text-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone()).unwrap();
        s.publish(Config::default()).unwrap();
        let old = s.active.clone();
        s.engine.paused.store(false, Ordering::SeqCst);
        fs::remove_file(dir.join("draft.json")).unwrap();
        fs::create_dir(dir.join("draft.json")).unwrap();
        let mut next = old.clone();
        next.enabled = false;
        assert!(s.publish(next).is_err());
        assert_eq!(s.active, old);
        assert_eq!(read_config(&dir.join("active.json")).unwrap(), old);
        assert!(!s.engine.paused.load(Ordering::SeqCst));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rollback_revision_and_reload() {
        let dir = std::env::temp_dir().join(format!("quicker-text-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone()).unwrap();
        s.publish(Config::default()).unwrap();
        let mut next = s.active.clone();
        next.enabled = false;
        s.publish(next).unwrap();
        let previous = s.previous.clone().unwrap();
        s.publish(previous).unwrap();
        assert_eq!(s.active.revision, 3);
        assert!(s.active.enabled);
        drop(s);
        let s = Store::load(dir.clone()).unwrap();
        assert_eq!(s.active.revision, 3);
        assert!(s.engine.paused.load(Ordering::SeqCst));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn revision_overflow_is_error() {
        let dir = std::env::temp_dir().join(format!("quicker-text-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone()).unwrap();
        s.active.revision = u64::MAX;
        assert!(s.publish(Config::default()).is_err());
        assert_eq!(s.active.revision, u64::MAX);
        assert!(s.engine.paused.load(Ordering::SeqCst));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn publish_and_rollback_failures_use_specific_error_codes() {
        let dir = std::env::temp_dir().join(format!("quicker-text-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone()).unwrap();
        assert!(matches!(
            s.rollback_previous(),
            Err(CommandError::NotFound {
                kind: EntityKind::Revision,
                ..
            })
        ));
        s.engine.busy.store(true, Ordering::SeqCst);
        assert!(matches!(
            s.publish(Config::default()),
            Err(CommandError::Busy)
        ));
        s.engine.busy.store(false, Ordering::SeqCst);
        s.publish(Config::default()).unwrap();
        let mut altered = s.active.clone();
        altered.revision = 5;
        atomic_json(&dir.join("active.json"), &altered).unwrap();
        assert!(matches!(
            s.publish(Config::default()),
            Err(CommandError::RevisionConflict { current: 5 })
        ));
        fs::remove_dir_all(dir).unwrap();
    }
}
