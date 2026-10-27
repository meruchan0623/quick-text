//! Configuration store: active and previous configurations plus their
//! files in the data directory (§7.1 「数据目录」).
mod atomic;
mod backup;
mod export;
mod fallback;
pub mod legacy;
mod load;
mod meta;
mod metadata;
pub mod prefs;
mod revisions;
#[cfg(test)]
mod startup_tests;

pub use atomic::replace_file;
pub use fallback::{retry_locked, RecoverySource, StoreEnv};
pub use meta::clear_run_marker_on_session_end;
pub use metadata::merge_import_metadata;

use crate::{
    error::{CommandError, EntityKind, IoOp, SafeModeReason},
    events::{ProblemSink, StoreSink, StoreState},
    startup::{ProblemKind, StartupProblem},
};
use meta::MetaHandle;
use quicker_domain::*;
use quicker_platform::{health::PauseReasons, runtime::Runtime, Shared};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

/// The diagnostic code of the `validation` error with which
/// [`Store::mark_last_good`] refuses a `revisions/<n>.json` that does not
/// hold the active revision; `lkg.rs` logs it as `snapshot_mismatch`.
pub const SNAPSHOT_MISMATCH: &str = "snapshot_mismatch";

pub struct Store {
    dir: PathBuf,
    pub active: Config,
    /// Whether `active` was read from a file or published by this store.
    /// False for the empty configuration that stands in when there is none
    /// (a new data directory, SafeMode(NoValidConfig), the read-only safe
    /// modes). Only a configuration that was on disk is backed up as
    /// `revisions/<n>.json`: a backup of the stand-in would be a valid
    /// fallback candidate, and an interrupted publish out of
    /// SafeMode(NoValidConfig) would then leave the next launch Recovered
    /// with an empty configuration instead of still protected (F4-1, R1).
    active_on_disk: bool,
    previous: Option<Config>,
    disk_hash: Option<Vec<u8>>,
    pub engine: Arc<Shared>,
    /// The runtime `engine` is attached to: the authority on pause reasons
    /// and health (P5 item 2). `engine.paused` is only its derived value;
    /// nothing in the shell writes it.
    pub runtime: Arc<Runtime>,
    pub active_unresolved: Vec<MigrationItem>,
    previous_unresolved: Vec<MigrationItem>,
    pub pending_import: Option<(Config, Vec<MigrationItem>)>,
    pending_import_token: Option<String>,
    safe_mode: Option<SafeModeReason>,
    startup_problems: Vec<StartupProblem>,
    /// Corrupt files the load left in place until the pre-0.4 backup holds
    /// them and they could be renamed aside (review I-B1, integration fix
    /// (d), `Store::quarantine_deferred`).
    deferred: Vec<fallback::DeferredQuarantine>,
    meta: MetaHandle,
    /// Work waiting for the store to become writable (see
    /// [`Store::when_writable`]).
    writable_hooks: Vec<(&'static str, WritableHook)>,
    /// Keys of hooks that already ran.
    writable_done: Vec<&'static str>,
    /// Where problems found after startup go (the EventBus, which sends
    /// `startup-problem`); `None` until the bus is up and in unit tests.
    problem_sink: Option<ProblemSink>,
    /// Where the revision and safe mode go after each change (the EventBus,
    /// which sends `store-state`); `None` until the bus is up and in most
    /// unit tests.
    store_sink: Option<StoreSink>,
    /// Successful [`Store::set_manual_pause`] calls so far, so startup can
    /// tell whether the user paused or resumed while it waited.
    manual_changes: AtomicU64,
}

/// Deferred work run with the store once it is writable.
pub type WritableHook = Box<dyn FnOnce(&mut Store) + Send>;

/// Makes the running store's run marker reachable from
/// [`clear_run_marker_on_session_end`].
pub fn register_session_meta(store: &Store) {
    meta::register_session_meta(store.meta.clone());
}

/// Importing is the way out of SafeMode(CrashLoop) and
/// SafeMode(NoValidConfig). In SafeMode(Locked) and SafeMode(NewerSchema)
/// the file that could not be read must not be replaced, so an import can be
/// previewed but not written.
fn import_leaves_safe_mode(reason: SafeModeReason) -> bool {
    matches!(
        reason,
        SafeModeReason::CrashLoop | SafeModeReason::NoValidConfig
    )
}

impl Store {
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn safe_mode(&self) -> Option<SafeModeReason> {
        self.safe_mode
    }
    pub fn startup_problems(&self) -> &[StartupProblem] {
        &self.startup_problems
    }
    /// Records a problem found after the load (autostart repaired, self-test
    /// failed, ...) and reports it as one `startup-problem` event.
    pub fn add_problem(&mut self, kind: ProblemKind) {
        let problem = StartupProblem::from(kind);
        self.report_problem(&problem);
        self.startup_problems.push(problem);
    }
    /// From now on every new problem is also given to `sink`. Problems
    /// already recorded are not: the frontend reads them with
    /// `startup_problems` / `load_state`.
    pub fn set_problem_sink(&mut self, sink: ProblemSink) {
        self.problem_sink = Some(sink);
    }
    fn report_problem(&self, problem: &StartupProblem) {
        if let Some(sink) = &self.problem_sink {
            sink(problem.clone());
        }
    }
    /// The active revision and safe mode (`store-state`,
    /// `get_runtime_snapshot`).
    pub fn store_state(&self) -> StoreState {
        StoreState {
            revision: self.active.revision,
            safe_mode: self.safe_mode,
        }
    }
    /// Review A9: the current revision and safe mode become the EventBus's
    /// baseline at once, on this thread, under this store lock (no `seq`,
    /// no event: the frontend reads initial values from `load_state` /
    /// `get_runtime_snapshot`); from now on every change is reported to
    /// `sink`. Called once, in setup, before the bus is managed.
    pub fn set_store_sink(&mut self, sink: StoreSink) {
        sink.baseline(self.store_state());
        self.store_sink = Some(sink);
    }
    /// Reports the current revision and safe mode, still under the store
    /// lock, so reports follow the order of the changes. The two places
    /// that change them call it once they are done: [`Store::install`]
    /// (startup and the SafeMode(Locked) retry: new revision and safe mode
    /// left, one report) and `publish_with_metadata` (save/apply, rollback,
    /// published import, which may also end safe mode). The EventBus drops
    /// a report equal to its baseline or the last one it sent.
    fn report_store_state(&self) {
        if let Some(sink) = &self.store_sink {
            sink.report(self.store_state());
        }
    }
    /// The safe-mode gate for write commands (plan §7.2): everything that
    /// writes data or settings, or would resume the engine, is refused.
    pub fn ensure_writable(&self) -> Result<(), CommandError> {
        match self.safe_mode {
            Some(reason) => Err(CommandError::SafeMode { reason }),
            None => Ok(()),
        }
    }
    /// `set_paused` and the tray toggle (contract §4): sets or clears
    /// `PauseReasons::MANUAL` and nothing else, so with another reason still
    /// set (recording, self-test, an engine that is not up) the engine stays
    /// paused after a successful resume.
    ///
    /// A resume is refused in safe mode (`safe_mode`, plan §7.2) and while
    /// the health does not accept it (`engine_unavailable` with that health,
    /// B-03); `ready` is the platform's own live readiness test, asked last.
    /// The refusal is feedback only: even a resume that raced a health
    /// change cannot expand, because the runtime derives the gates' flag
    /// from the health as well. Every change bumps the generation, and a
    /// pause cancels the running task.
    pub fn set_manual_pause(
        &self,
        paused: bool,
        ready: impl FnOnce() -> bool,
    ) -> Result<(), CommandError> {
        if !paused {
            self.ensure_writable()?;
            let health = self.runtime.health().state;
            if !health.accepts_resume() {
                return Err(CommandError::EngineUnavailable { health });
            }
            if !ready() {
                return Err(CommandError::engine_unavailable(
                    "This platform input adapter is not ready; engine remains paused",
                ));
            }
        }
        self.runtime.set_pause(PauseReasons::MANUAL, paused);
        self.manual_changes.fetch_add(1, Ordering::SeqCst);
        self.engine.generation.fetch_add(1, Ordering::SeqCst);
        if paused {
            self.engine.cancel.store(true, Ordering::SeqCst);
        }
        tracing::info!(
            event = "engine.manual_pause",
            paused,
            "manual pause changed"
        );
        Ok(())
    }
    /// How many times [`Store::set_manual_pause`] has succeeded.
    pub fn manual_changes(&self) -> u64 {
        self.manual_changes.load(Ordering::SeqCst)
    }
    /// Runs `hook` once per session, the first time the store is writable:
    /// right away outside safe mode, otherwise when safe mode ends (the
    /// SafeMode(Locked) retry succeeds, or a published import leaves
    /// CrashLoop or NoValidConfig). A `key` that is already waiting or has
    /// run is ignored, so registering twice changes nothing.
    pub fn when_writable(&mut self, key: &'static str, hook: WritableHook) {
        if self.writable_done.contains(&key) || self.writable_hooks.iter().any(|(k, _)| *k == key) {
            return;
        }
        if self.safe_mode.is_none() {
            self.writable_done.push(key);
            hook(self);
        } else {
            tracing::info!(
                event = "store.writable_hook_deferred",
                hook = key,
                "deferred until safe mode ends"
            );
            self.writable_hooks.push((key, hook));
        }
    }
    /// The only place safe mode changes, and with it
    /// `PauseReasons::SAFE_MODE`. The engine is never resumed here: entering
    /// also sets MANUAL (as the 0.3 flag did), and leaving clears only
    /// SAFE_MODE, so the engine stays paused until the user resumes it.
    /// Resuming is refused while in safe mode ([`Store::set_manual_pause`]).
    fn set_safe_mode(&mut self, reason: Option<SafeModeReason>) {
        if self.safe_mode == reason {
            return;
        }
        match reason {
            Some(reason) => {
                self.runtime
                    .set_pause(PauseReasons::SAFE_MODE.with(PauseReasons::MANUAL), true);
                tracing::warn!(event = "store.safe_mode", ?reason, "safe mode active");
            }
            None => {
                self.runtime.set_pause(PauseReasons::SAFE_MODE, false);
                self.startup_problems
                    .retain(|p| !matches!(p.kind, ProblemKind::SafeMode { .. }));
                tracing::info!(event = "store.safe_mode_exit", "safe mode ended");
            }
        }
        self.safe_mode = reason;
        if reason.is_none() {
            for (key, hook) in std::mem::take(&mut self.writable_hooks) {
                self.writable_done.push(key);
                hook(self);
            }
        }
    }
    /// Records `revision` as last-known-good in `store-meta.json`; it is the
    /// first candidate after `active.json` in `load_or_fallback`.
    ///
    /// Called by `lkg.rs` (plan §7.2 LKG, B-04: [`crate::lkg::mark_active`]),
    /// once the engine has been Healthy for 60 seconds and at least one
    /// expansion has succeeded on `revision`. There is no timer here.
    ///
    /// Like every write command it passes the backup gate first
    /// (`backup_before_write`, integration fix (c)): while the pre-0.4 backup
    /// fails it lists `backup_failed` and returns that `io` error with
    /// nothing written; once the backup succeeds it drops the problem and
    /// renames the corrupt files the load left in place (I-B1), so a corrupt
    /// `revisions/<n>.json` of the active revision is moved aside before
    /// anything else looks at it. While that rename fails (another program
    /// holds the file) the gate refuses the mark with the `io` error and
    /// keeps the file listed; the next mark tries the rename again
    /// (integration fix (d)).
    ///
    /// When `revision` is the active one, `revisions/<n>.json` must then hold
    /// it. A missing snapshot is written, so that the candidate exists. One
    /// that is there is read as the load reads a candidate
    /// (`read_stored_config`) and must equal the active configuration; if it
    /// does not parse or differs, nothing is recorded or overwritten and the
    /// mark is refused with a `validation` error whose diagnostic code is
    /// [`SNAPSHOT_MISMATCH`] (a fallback would load that file, not the
    /// revision that proved itself). One that cannot be read is an `io`
    /// error. The empty stand-in of a data directory without a configuration
    /// is never snapshotted (see `active_on_disk`): it is `NotFound`.
    pub fn mark_last_good(&mut self, revision: u64) -> Result<(), CommandError> {
        self.ensure_writable()?;
        self.backup_before_write()?;
        let revisions = self.dir.join("revisions");
        let snapshot = revisions.join(format!("{revision}.json"));
        if revision == self.active.revision {
            match fs::read(&snapshot) {
                Ok(bytes) => {
                    let holds_active =
                        read_stored_config(&bytes).is_ok_and(|stored| stored.config == self.active);
                    if !holds_active {
                        return Err(CommandError::invalid_file(
                            SNAPSHOT_MISMATCH,
                            "Snapshot of the active revision differs from it; not recorded",
                            &snapshot,
                        ));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if !self.active_on_disk {
                        return Err(CommandError::NotFound {
                            kind: EntityKind::Revision,
                            id: revision.to_string(),
                        });
                    }
                    fs::create_dir_all(&revisions)
                        .map_err(|e| CommandError::io(IoOp::CreateDir, Some(&revisions), &e))?;
                    self.write_data(&snapshot, &self.active)?;
                }
                Err(e) => return Err(CommandError::io(IoOp::Read, Some(&snapshot), &e)),
            }
        }
        self.meta
            .update(|meta| meta.last_good_revision = Some(revision))?;
        tracing::info!(
            event = "store.last_good",
            revision,
            "last-known-good revision recorded"
        );
        Ok(())
    }
    /// Validate and publish one configuration, using its revision as a compare-and-swap token.
    /// Import previews remain in memory until this succeeds.
    pub fn save_config(
        &mut self,
        config: Config,
        import_token: Option<String>,
    ) -> Result<(), CommandError> {
        if config.revision != self.active.revision {
            return Err(CommandError::RevisionConflict {
                current: self.active.revision,
            });
        }
        if let Some(token) = import_token {
            if self.pending_import_token.as_deref() != Some(token.as_str())
                || !self
                    .pending_import
                    .as_ref()
                    .is_some_and(|(preview, _)| preview == &config)
            {
                return Err(CommandError::TokenExpired);
            }
            self.publish(config)
        } else {
            self.ensure_writable()?;
            let metadata = metadata::metadata_for_config(&config, &self.active_unresolved);
            self.publish_with_metadata(config, metadata)
        }
    }

    /// One live preview per store: making another explicitly expires the previous token.
    pub fn preview_import(&mut self, config: Config, metadata: Vec<MigrationItem>) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.pending_import = Some((config, metadata));
        self.pending_import_token = Some(token.clone());
        token
    }
}

/// Synthetic rules and migration metadata shared by the store tests.
#[cfg(test)]
mod test_support {
    use quicker_domain::*;
    use quicker_platform::health::{HealthSnapshot, HealthState};

    pub fn healthy() -> HealthSnapshot {
        HealthSnapshot {
            state: HealthState::Healthy,
            components: vec![],
        }
    }
    /// Brings a test store's engine to listening the way the app does: the
    /// supervisor reports Healthy, then the user resumes (`set_paused`).
    pub fn listening(store: &super::Store) {
        store.runtime.publish_health(healthy());
        store.set_manual_pause(false, || true).unwrap();
        assert!(store.runtime.engine_state().listening);
    }

    pub fn metadata_config(ids: &[&str]) -> Config {
        Config {
            rules: ids
                .iter()
                .map(|id| Rule {
                    id: (*id).into(),
                    match_spec: MatchSpec {
                        kind: MatchKind::Literal,
                        value: format!("trigger-{id}"),
                    },
                    ..Rule::default()
                })
                .collect(),
            ..Config::default()
        }
    }
    pub fn original(id: &str, marker: &str) -> MigrationItem {
        MigrationItem {
            rule_id: id.into(),
            status: MigrationStatus::Compatible,
            diagnostics: vec![],
            original: serde_json::json!({"synthetic_source": marker, "timestamp": 123}),
        }
    }
    pub fn metadata_sources(
        items: Vec<MigrationItem>,
    ) -> std::collections::BTreeMap<String, String> {
        items
            .into_iter()
            .map(|item| {
                assert_eq!(item.status, MigrationStatus::Compatible);
                assert_eq!(item.original["timestamp"], 123);
                (
                    item.rule_id,
                    item.original["synthetic_source"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use quicker_platform::health::{HealthSnapshot, HealthState};
    use std::fs;
    /// Contract §4: `set_paused` only sets or clears MANUAL. A resume is
    /// refused while the engine is not up (with that health) or not ready;
    /// with another reason still set it succeeds and stays paused.
    #[test]
    fn manual_pause_is_the_only_reason_set_paused_touches() {
        let dir = std::env::temp_dir().join(format!("quicktext-test-{}", uuid::Uuid::new_v4()));
        let store = Store::load(dir.clone());
        assert_eq!(store.runtime.pause_reasons(), PauseReasons::MANUAL);
        let generation = || store.engine.generation.load(Ordering::SeqCst);
        // Starting: refused with the health that refused it, nothing changes.
        let before = generation();
        assert!(matches!(
            store.set_manual_pause(false, || true),
            Err(CommandError::EngineUnavailable {
                health: HealthState::Starting
            })
        ));
        assert_eq!(store.runtime.pause_reasons(), PauseReasons::MANUAL);
        assert_eq!(generation(), before);
        // Up but the platform adapter is not ready.
        store.runtime.publish_health(healthy());
        assert!(matches!(
            store.set_manual_pause(false, || false),
            Err(CommandError::EngineUnavailable {
                health: HealthState::Failed
            })
        ));
        assert!(store.engine.paused.load(Ordering::SeqCst));
        // Resume while recording: MANUAL clears, the engine stays paused.
        store.runtime.set_pause(PauseReasons::RECORDING, true);
        store.set_manual_pause(false, || true).unwrap();
        assert_eq!(store.runtime.pause_reasons(), PauseReasons::RECORDING);
        assert!(!store.runtime.engine_state().listening);
        assert!(store.engine.paused.load(Ordering::SeqCst));
        assert_eq!(generation(), before + 1);
        // Recording ends: listening.
        store.runtime.set_pause(PauseReasons::RECORDING, false);
        assert!(store.runtime.engine_state().listening);
        assert!(!store.engine.paused.load(Ordering::SeqCst));
        // A pause cancels the running task and leaves the other reasons alone.
        store.engine.cancel.store(false, Ordering::SeqCst);
        store.runtime.set_pause(PauseReasons::SELF_TEST, true);
        store.set_manual_pause(true, || unreachable!()).unwrap();
        assert!(store.engine.cancel.load(Ordering::SeqCst));
        assert_eq!(
            store.runtime.pause_reasons(),
            PauseReasons::MANUAL.with(PauseReasons::SELF_TEST)
        );
        // The engine going down holds the flag without touching MANUAL.
        store.runtime.set_pause(PauseReasons::SELF_TEST, false);
        store.set_manual_pause(false, || true).unwrap();
        store.runtime.publish_health(HealthSnapshot {
            state: HealthState::Recovering,
            components: vec![],
        });
        assert!(store.engine.paused.load(Ordering::SeqCst));
        assert!(!store
            .runtime
            .pause_reasons()
            .intersects(PauseReasons::USER_VISIBLE));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rollback_import_restores_published_originals_after_reload() {
        let dir =
            std::env::temp_dir().join(format!("quicker-metadata-test-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone());
        let a = metadata_config(&["shared"]);
        store.pending_import = Some((a.clone(), vec![original("shared", "A")]));
        store.publish(a).unwrap();
        let mut b = metadata_config(&["shared"]);
        b.rules[0].description = "synthetic replacement".into();
        store.pending_import = Some((b.clone(), vec![original("shared", "B")]));
        store.publish(b.clone()).unwrap();
        drop(store);
        let mut store = Store::load(dir.clone());
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "B");
        store.pending_import = Some((b, vec![original("shared", "pending")]));
        store.rollback_previous().unwrap();
        assert!(store.pending_import.is_none());
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "A");
        assert_eq!(store.active, store.active);
        drop(store);
        let store = Store::load(dir.clone());
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "A");
        fs::remove_dir_all(dir).unwrap();
    }
    /// Invalid configuration is rejected before publishing.
    #[test]
    fn save_rejects_config_with_error_diagnostics() {
        let dir = std::env::temp_dir().join(format!("quicktext-test-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone());
        let valid = metadata_config(&["kept"]);
        store.publish(valid.clone()).unwrap();
        let mut invalid = metadata_config(&["kept", "broken"]);
        invalid.rules[1].match_spec.value.clear();
        invalid.globals.terminators.push(String::new());
        store.pending_import = Some((invalid.clone(), vec![original("broken", "pending")]));
        let Err(CommandError::Validation { diagnostics }) = store.publish(invalid) else {
            panic!("an invalid configuration must be rejected with Validation");
        };
        assert!(diagnostics.iter().any(|d| d.code == "match_empty"));
        assert!(diagnostics.iter().any(|d| d.path == "globals.terminators"));
        assert!(diagnostics.iter().all(|d| d.severity == Severity::Error));
        assert_eq!(store.active.rules, valid.rules);
        assert!(store.pending_import.is_some());
        drop(store);
        assert_eq!(Store::load(dir.clone()).active.rules, valid.rules);
        fs::remove_dir_all(dir).unwrap();
    }
}
