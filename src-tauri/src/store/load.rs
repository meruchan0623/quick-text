//! Startup load of the data directory on top of `load_or_fallback` (B-04).
use super::{
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
use quicker_platform::{fault::FaultPoint, runtime::Runtime, Shared};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
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
        // Attached before `install`, so safe mode lands in the runtime's
        // reasons; the engine starts paused (adopted as MANUAL).
        let engine = Shared::new();
        let runtime = engine_runtime();
        runtime.attach(&engine);
        let mut store = Self {
            dir,
            active: Config::default(),
            active_on_disk: false,
            previous: None,
            disk_hash: None,
            engine,
            runtime,
            active_unresolved: vec![],
            previous_unresolved: vec![],
            pending_import: None,
            pending_import_token: None,
            safe_mode: None,
            startup_problems: vec![],
            deferred: vec![],
            meta,
            writable_hooks: vec![],
            writable_done: vec![],
            problem_sink: None,
            store_sink: None,
            manual_changes: Default::default(),
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
        self.active_on_disk = report.active_on_disk;
        self.previous = report.previous;
        self.pending_import = None;
        self.pending_import_token = None;
        self.deferred = report.deferred;
        let mut problems = report.problems;
        // R6: the metadata snapshot is the first write into a directory
        // 0.3.1 left, so the pre-0.4 backup comes first, and without it the
        // snapshot is not written. Safe mode writes nothing. I-B1: the
        // corrupt files the load left in place are renamed only once the
        // backup holds them; while it fails they stay, and the next write or
        // launch tries again.
        let backed_up = safe_mode.is_none().then(|| self.backup_pre_0_4().is_ok());
        if backed_up == Some(true) {
            // A file that stays in place refuses the writes that would
            // replace it (logged as `store.quarantine_failed`).
            let _ = self.quarantine_deferred(&mut problems);
        }
        self.load_metadata(backed_up == Some(true));
        if backed_up == Some(true) {
            self.preserve_legacy_unconfirmed();
        }
        // Problems found outside the load (autostart, the startup self-test)
        // survive a reload.
        let before = std::mem::take(&mut self.startup_problems);
        // A reload (Locked retry) reports what it found that was not
        // already listed; at the first load nobody listens yet.
        for problem in problems.iter().filter(|p| !before.contains(p)) {
            self.report_problem(problem);
        }
        let kept = before.into_iter().filter(|p| {
            matches!(
                p.kind,
                ProblemKind::AutostartRepaired {}
                    | ProblemKind::SelfTestFailed { .. }
                    | ProblemKind::LogLevelElevated { .. }
                    | ProblemKind::BackupFailed { .. }
            )
        });
        self.startup_problems = problems;
        self.startup_problems.extend(kept);
        if let Some(ok) = backed_up {
            self.note_backup(ok);
        }
        self.set_safe_mode(safe_mode);
        // The new revision and the safe mode it left (or entered) together,
        // in one report, under the lock the caller holds.
        self.report_store_state();
    }
    /// Move obsolete unconfirmed files aside once after the legacy backup gate.
    /// Never replace an earlier archive, even when an older app recreated a file.
    pub(super) fn preserve_legacy_unconfirmed(&self) {
        for name in ["draft.json", "migration-unresolved.json"] {
            let source = self.dir.join(name);
            if !source.is_file() {
                continue;
            }
            let folder = self.dir.join("legacy-unconfirmed");
            let mut target = folder.join(name);
            if target.exists() {
                target = folder.join(format!("{}-{name}", uuid::Uuid::new_v4()));
            }
            let result = fs::create_dir_all(&folder).and_then(|()| fs::rename(&source, &target));
            if let Err(error) = result {
                tracing::warn!(event = "store.legacy_preserve_failed", %error);
            }
        }
    }
    /// Migration metadata of the active and previous configurations.
    /// Outside safe mode the active one is also snapshotted.
    fn load_metadata(&mut self, write_snapshot: bool) {
        let dir = &self.dir;
        let legacy_metadata = dir.join("migration-unresolved.json");
        let unresolved = read_metadata(&if legacy_metadata.exists() {
            legacy_metadata
        } else {
            dir.join("legacy-unconfirmed/migration-unresolved.json")
        });
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
        self.active_unresolved = active_unresolved;
        self.previous_unresolved = previous_unresolved;
        if write_snapshot {
            self.snapshot_active_metadata();
        }
    }
    /// Writes the active metadata as `revisions/<n>.metadata.json`: the
    /// legacy fallback is persisted before legacy metadata is archived. Also called once the pre-0.4 backup
    /// succeeds after the load could not write this (`Store::note_backup`).
    pub(super) fn snapshot_active_metadata(&self) {
        let revisions = self.dir.join("revisions");
        let path = revisions.join(format!("{}.metadata.json", self.active.revision));
        let written = self
            .backup_pre_0_4()
            .and_then(|()| {
                fs::create_dir_all(&revisions)
                    .map_err(|e| CommandError::io(IoOp::CreateDir, Some(&revisions), &e))
            })
            .and_then(|()| self.write_data(&path, &self.active_unresolved));
        if let Err(e) = written {
            tracing::warn!(
                event = "store.metadata_snapshot_failed",
                error = %e,
                "active metadata not snapshotted"
            );
        }
    }
}
/// The runtime a new store's engine reports to: the process-wide one in the
/// app (the supervisor, the EventBus and the commands use it too). Each
/// unit-test store gets its own, so parallel tests never share pause
/// reasons or health.
fn engine_runtime() -> Arc<Runtime> {
    if cfg!(test) {
        Arc::new(Runtime::new(Arc::new(
            quicker_platform::traits::SystemClock,
        )))
    } else {
        quicker_platform::runtime::current()
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
/// The content of `path`; `None` when it does not exist.
pub(super) fn read_existing(path: &Path) -> Result<Option<Vec<u8>>, CommandError> {
    if !path.exists() {
        return Ok(None);
    }
    fs::read(path)
        .map(Some)
        .map_err(|e| CommandError::io(IoOp::Read, Some(path), &e))
}

#[cfg(test)]
mod tests {
    use super::super::{atomic::atomic_json, test_support::*};
    use super::*;
    #[test]
    fn legacy_metadata_is_snapshotted_before_direct_import() {
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
        store.publish(a).unwrap();
        drop(store);
        let mut store = Store::load(dir.clone());
        store.rollback_previous().unwrap();
        assert_eq!(
            store.active_unresolved[0].original["synthetic_source"],
            "legacy-A"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
