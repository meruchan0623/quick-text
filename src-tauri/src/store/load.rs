//! Startup load of the data directory on top of `load_or_fallback` (B-04).
use super::{
    atomic::atomic_json,
    fallback::{load_or_fallback, LoadReport, StoreEnv},
    meta::{MetaHandle, StoreMeta},
    metadata::{metadata_for_config, read_metadata},
    Store,
};
use crate::{
    error::{file_label, CommandError, IoOp},
    faults,
    startup::ProblemKind,
};
use quicker_domain::*;
use quicker_platform::{fault::FaultPoint, Shared};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

impl Store {
    /// Loads the data directory. Never fails: a broken directory starts
    /// recovered or in safe mode (see `load_or_fallback`).
    pub fn load(dir: PathBuf) -> Self {
        if faults::take(FaultPoint::ConfigCorrupt).is_some() {
            inject_config_corrupt(&dir);
        }
        Self::load_with(dir, &StoreEnv::system())
    }
    pub fn load_with(dir: PathBuf, env: &StoreEnv) -> Self {
        if let Err(e) = fs::create_dir_all(&dir) {
            tracing::warn!(event = "store.create_dir_failed", error = %e, "data directory not created");
        }
        let meta = MetaHandle::new(&dir, StoreMeta::default(), false);
        let report = load_or_fallback(&dir, env);
        let mut store = Self {
            dir,
            draft: Config::default(),
            active: Config::default(),
            previous: None,
            disk_hash: None,
            engine: Shared::new(),
            unresolved: vec![],
            active_unresolved: vec![],
            previous_unresolved: vec![],
            pending_import: None,
            safe_mode: None,
            startup_problems: vec![],
            meta,
            safe_import: false,
            writable_hooks: vec![],
            writable_done: vec![],
        };
        store.install(report);
        store
    }
    /// Takes over the result of a successful SafeMode(Locked) retry. The
    /// engine stays paused; leaving safe mode does not resume it.
    pub fn apply_retry(&mut self, report: LoadReport) {
        tracing::info!(
            event = "store.locked_recovered",
            outcome = ?report.outcome,
            "data directory readable again"
        );
        self.engine.cancel.store(true, Ordering::SeqCst);
        self.install(report);
    }
    fn install(&mut self, report: LoadReport) {
        let safe_mode = report.safe_mode();
        *self.engine.config.lock().unwrap() = report.active.clone();
        *self.engine.compiled.lock().unwrap() = report.compiled;
        self.engine.generation.fetch_add(1, Ordering::SeqCst);
        self.meta.replace(report.meta, report.meta_writable);
        self.disk_hash = report.disk_hash;
        self.active = report.active;
        self.draft = report.draft;
        self.previous = report.previous;
        self.pending_import = None;
        self.load_metadata(safe_mode.is_none());
        // Problems found outside the load (autostart) survive a reload.
        let kept: Vec<_> = self
            .startup_problems
            .drain(..)
            .filter(|p| matches!(p.kind, ProblemKind::AutostartRepaired {}))
            .collect();
        self.startup_problems = report.problems;
        self.startup_problems.extend(kept);
        self.set_safe_mode(safe_mode);
    }
    /// Migration metadata of the draft, active and previous configurations.
    /// Outside safe mode the active one is also snapshotted.
    fn load_metadata(&mut self, write_snapshot: bool) {
        let dir = &self.dir;
        let unresolved = read_metadata(&dir.join("migration-unresolved.json"));
        let active_metadata_path = dir
            .join("revisions")
            .join(format!("{}.metadata.json", self.active.revision));
        let active_unresolved = if active_metadata_path.exists() {
            read_metadata(&active_metadata_path)
        } else if dir.join("active-migration-unresolved.json").exists() {
            read_metadata(&dir.join("active-migration-unresolved.json"))
        } else {
            metadata_for_config(&self.active, &unresolved)
        };
        if write_snapshot {
            // Persist the legacy fallback before a later draft import can replace it.
            let revisions = dir.join("revisions");
            let written = fs::create_dir_all(&revisions)
                .map_err(|e| CommandError::io(IoOp::CreateDir, Some(&revisions), &e))
                .and_then(|()| atomic_json(&active_metadata_path, &active_unresolved));
            if let Err(e) = written {
                tracing::warn!(
                    event = "store.metadata_snapshot_failed",
                    error = %e,
                    "active metadata not snapshotted"
                );
            }
        }
        let previous_unresolved = self
            .previous
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
        self.unresolved = unresolved;
        self.active_unresolved = active_unresolved;
        self.previous_unresolved = previous_unresolved;
    }
}
/// `config_corrupt` fault: damages `active.json` before the load, as an
/// interrupted non-atomic write would.
fn inject_config_corrupt(dir: &Path) {
    let path = dir.join("active.json");
    let bytes = fs::read(&path).unwrap_or_default();
    let half = &bytes[..bytes.len() / 2];
    let damaged: &[u8] = if half.is_empty() { b"{" } else { half };
    match fs::write(&path, damaged) {
        Ok(()) => tracing::warn!(
            event = "fault.config_corrupt",
            "active.json damaged by fault injection"
        ),
        Err(e) => tracing::warn!(
            event = "fault.config_corrupt",
            error = %e,
            "fault injection could not write"
        ),
    }
}
/// Lenient read (B-43). Unreadable files are `Io`; content that cannot be
/// read at all is `Validation` with the reader's diagnostic at the file name.
pub(super) fn read_config(path: &Path) -> Result<Config, CommandError> {
    let bytes = fs::read(path).map_err(|e| CommandError::io(IoOp::Read, Some(path), &e))?;
    read_stored_config(&bytes)
        .map(|stored| stored.config)
        .map_err(|e| {
            CommandError::validation(vec![e
                .diagnostic()
                .at(file_label(path).unwrap_or_default())])
        })
}
pub(super) fn hash_file(path: &Path) -> Result<Option<Vec<u8>>, CommandError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|e| CommandError::io(IoOp::Read, Some(path), &e))?;
    Ok(Some(Sha256::digest(bytes).to_vec()))
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
        let mut store = Store::load(dir.clone());
        store.pending_import = Some((a.clone(), vec![original("shared", "B")]));
        store.save(a).unwrap();
        drop(store);
        let mut store = Store::load(dir.clone());
        store.discard().unwrap();
        assert_eq!(store.unresolved[0].original["synthetic_source"], "legacy-A");
        fs::remove_dir_all(dir).unwrap();
    }
}
