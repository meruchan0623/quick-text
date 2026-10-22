//! Configuration store: draft, active and previous configurations plus their
//! files in the data directory (§7.1 「数据目录」).
mod atomic;
mod fallback;
mod load;
mod meta;
mod metadata;
mod prefs;
mod revisions;
#[cfg(test)]
mod startup_tests;

pub use fallback::{retry_locked, RecoverySource, StoreEnv};
pub use meta::clear_run_marker_on_session_end;
pub use metadata::merge_import_metadata;

use crate::{
    error::{CommandError, IoOp, SafeModeReason},
    startup::{ProblemKind, StartupProblem},
};
use atomic::atomic_json;
use meta::MetaHandle;
use metadata::metadata_for_config;
use quicker_domain::*;
use quicker_platform::Shared;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct Store {
    dir: PathBuf,
    pub draft: Config,
    pub active: Config,
    previous: Option<Config>,
    disk_hash: Option<Vec<u8>>,
    pub engine: Arc<Shared>,
    pub unresolved: Vec<MigrationItem>,
    active_unresolved: Vec<MigrationItem>,
    previous_unresolved: Vec<MigrationItem>,
    pub pending_import: Option<(Config, Vec<MigrationItem>)>,
    safe_mode: Option<SafeModeReason>,
    startup_problems: Vec<StartupProblem>,
    meta: MetaHandle,
    /// In safe mode: the draft was committed from an import and may be
    /// published, which ends safe mode.
    safe_import: bool,
}

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
    pub fn add_problem(&mut self, kind: ProblemKind) {
        // P5: push the `startup-problem` event here.
        self.startup_problems.push(kind.into());
    }
    /// The safe-mode gate for write commands (plan §7.2): everything that
    /// writes data or settings, or would resume the engine, is refused.
    pub fn ensure_writable(&self) -> Result<(), CommandError> {
        match self.safe_mode {
            Some(reason) => Err(CommandError::SafeMode { reason }),
            None => Ok(()),
        }
    }
    /// The only place safe mode changes. The engine is never resumed here:
    /// it starts paused and stays paused through safe mode.
    // P1a: set/clear `PauseReasons::SAFE_MODE` here; P5: emit the change.
    fn set_safe_mode(&mut self, reason: Option<SafeModeReason>) {
        if self.safe_mode == reason {
            return;
        }
        match reason {
            Some(reason) => {
                self.engine
                    .paused
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                tracing::warn!(event = "store.safe_mode", ?reason, "safe mode active");
            }
            None => {
                self.startup_problems
                    .retain(|p| !matches!(p.kind, ProblemKind::SafeMode { .. }));
                tracing::info!(event = "store.safe_mode_exit", "safe mode ended");
            }
        }
        self.safe_mode = reason;
        self.safe_import = false;
    }
    /// Records `revision` as last-known-good in `store-meta.json`; it is the
    /// first candidate after `active.json` in `load_or_fallback`.
    ///
    /// Calling convention (plan §7.2 LKG): the supervisor (P1a) calls this
    /// once the engine has been Healthy for 60 seconds and at least one
    /// expansion has succeeded on `revision`. There is no timer here. When
    /// `revision` is the active one and has no `revisions/<n>.json` yet, that
    /// snapshot is written as well so that the candidate exists.
    #[allow(dead_code)] // Called by the supervisor (P1a).
    pub fn mark_last_good(&self, revision: u64) -> Result<(), CommandError> {
        self.ensure_writable()?;
        let revisions = self.dir.join("revisions");
        let snapshot = revisions.join(format!("{revision}.json"));
        if revision == self.active.revision && !snapshot.exists() {
            fs::create_dir_all(&revisions)
                .map_err(|e| CommandError::io(IoOp::CreateDir, Some(&revisions), &e))?;
            atomic_json(&snapshot, &self.active)?;
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
    /// Writes the draft. A configuration with error-level diagnostics is
    /// rejected with `Validation` and nothing is written (B-42). In safe mode
    /// only committing a previewed import is allowed (see
    /// [`import_leaves_safe_mode`]).
    pub fn save(&mut self, config: Config) -> Result<(), CommandError> {
        if let Some(reason) = self.safe_mode {
            let importing = self
                .pending_import
                .as_ref()
                .is_some_and(|(preview, _)| preview == &config);
            if !(importing && import_leaves_safe_mode(reason)) {
                return Err(CommandError::SafeMode { reason });
            }
        }
        if let Err(diagnostics) = compile_config(&config) {
            if diagnostics.iter().any(|d| d.severity == Severity::Error) {
                return Err(CommandError::validation(diagnostics));
            }
        }
        let metadata = self
            .pending_import
            .as_ref()
            .filter(|(preview, _)| preview == &config)
            .map(|(_, metadata)| metadata.clone())
            .unwrap_or_else(|| metadata_for_config(&config, &self.unresolved));
        atomic_json(&self.dir.join("draft.json"), &config)?;
        atomic_json(&self.dir.join("migration-unresolved.json"), &metadata)?;
        self.unresolved = metadata;
        self.pending_import = None;
        self.draft = config;
        self.safe_import = self.safe_mode.is_some();
        Ok(())
    }
    pub fn discard(&mut self) -> Result<(), CommandError> {
        self.ensure_writable()?;
        atomic_json(&self.dir.join("draft.json"), &self.active)?;
        atomic_json(
            &self.dir.join("migration-unresolved.json"),
            &self.active_unresolved,
        )?;
        self.draft = self.active.clone();
        self.unresolved = self.active_unresolved.clone();
        self.pending_import = None;
        Ok(())
    }
}

/// Synthetic rules and migration metadata shared by the store tests.
#[cfg(test)]
mod test_support {
    use quicker_domain::*;

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
    use std::fs;
    #[test]
    fn discard_import_restores_published_originals_after_reload() {
        let dir =
            std::env::temp_dir().join(format!("quicker-metadata-test-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone());
        let a = metadata_config(&["shared"]);
        store.pending_import = Some((a.clone(), vec![original("shared", "A")]));
        store.save(a).unwrap();
        store.publish(store.draft.clone()).unwrap();
        let mut b = metadata_config(&["shared"]);
        b.rules[0].description = "synthetic replacement".into();
        store.pending_import = Some((b.clone(), vec![original("shared", "B")]));
        store.save(b.clone()).unwrap();
        drop(store);
        let mut store = Store::load(dir.clone());
        assert_eq!(store.unresolved[0].original["synthetic_source"], "B");
        store.pending_import = Some((b, vec![original("shared", "pending")]));
        store.discard().unwrap();
        assert!(store.pending_import.is_none());
        assert_eq!(store.unresolved[0].original["synthetic_source"], "A");
        assert_eq!(store.draft, store.active);
        drop(store);
        let store = Store::load(dir.clone());
        assert_eq!(store.unresolved[0].original["synthetic_source"], "A");
        fs::remove_dir_all(dir).unwrap();
    }
    /// B-42: `save_draft` validates first; an invalid draft is not written.
    #[test]
    fn save_rejects_drafts_with_error_diagnostics() {
        let dir = std::env::temp_dir().join(format!("quicker-text-test-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone());
        let valid = metadata_config(&["kept"]);
        store.save(valid.clone()).unwrap();
        let mut invalid = metadata_config(&["kept", "broken"]);
        invalid.rules[1].match_spec.value.clear();
        invalid.globals.terminators.push(String::new());
        store.pending_import = Some((invalid.clone(), vec![original("broken", "pending")]));
        let Err(CommandError::Validation { diagnostics }) = store.save(invalid) else {
            panic!("an invalid draft must be rejected with Validation");
        };
        assert!(diagnostics.iter().any(|d| d.code == "match_empty"));
        assert!(diagnostics.iter().any(|d| d.path == "globals.terminators"));
        assert!(diagnostics.iter().all(|d| d.severity == Severity::Error));
        assert_eq!(store.draft, valid);
        assert!(store.pending_import.is_some());
        drop(store);
        assert_eq!(Store::load(dir.clone()).draft, valid);
        fs::remove_dir_all(dir).unwrap();
    }
}
