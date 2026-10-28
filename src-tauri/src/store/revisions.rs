//! Publishing a configuration as a new revision, and rolling back to the previous one.
use super::{
    fallback::{is_revision_stem, quarantined_stem},
    load::{read_config, read_existing},
    meta::{Fingerprint, PreviousRevision},
    metadata::metadata_for_config,
    Store,
};
use crate::error::{CommandError, EntityKind, IoOp};
use quicker_domain::*;
use quicker_platform::health::PauseReasons;
use sha2::{Digest, Sha256};
use std::{fs, io, path::Path, sync::atomic::Ordering};

impl Store {
    pub fn publish(&mut self, config: Config) -> Result<(), CommandError> {
        let metadata = self
            .pending_import
            .as_ref()
            .filter(|(preview, _)| preview == &config)
            .map(|(_, metadata)| metadata.clone())
            .unwrap_or_else(|| metadata_for_config(&config, &self.active_unresolved));
        self.publish_with_metadata(config, metadata)
    }
    /// In recoverable safe mode, only the exact previewed import may be
    /// confirmed; publishing it ends safe mode.
    pub(super) fn publish_with_metadata(
        &mut self,
        mut config: Config,
        metadata: Vec<MigrationItem>,
    ) -> Result<(), CommandError> {
        if let Some(reason) = self.safe_mode {
            if !(super::import_leaves_safe_mode(reason)
                && self
                    .pending_import
                    .as_ref()
                    .is_some_and(|(preview, _)| preview == &config))
            {
                return Err(CommandError::SafeMode { reason });
            }
        }
        config.revision = self.next_revision()?;
        let compiled = compile_config(&config).map_err(CommandError::validation)?;
        // R6: before anything is written or the engine held. Also before the
        // conflict check: a corrupt `active.json` the load left in place
        // (I-B1) is renamed by it, as the load would have done. One that
        // cannot be renamed yet, `revisions/<active>.json` included, refuses
        // the publish here (`io`) rather than at the snapshot check below,
        // and the next publish tries again (integration fix (d)).
        self.backup_before_write()?;
        let active_path = self.dir.join("active.json");
        let on_disk = read_existing(&active_path)?;
        if on_disk.as_ref().map(|bytes| Sha256::digest(bytes).to_vec()) != self.disk_hash {
            // Edited externally: report the revision now on disk so the caller can reload.
            let current = read_config(&active_path)
                .map(|c| c.revision)
                .unwrap_or(self.active.revision);
            return Err(CommandError::RevisionConflict { current });
        }
        let new_bytes = serde_json::to_vec_pretty(&config).expect("validated config serialization");
        // I-R1: the record that describes the `active.json` on disk now
        // (the one that matched at load, or that this session's last publish
        // wrote) is kept beside the new one, so that if this publish fails
        // after recording, the revision that stays active keeps its target.
        let current = on_disk.as_deref().map(Fingerprint::of);
        let committed = self.meta.read(|meta| {
            meta.previous_revisions
                .iter()
                .find(|record| {
                    record.active == self.active.revision
                        && Some(&record.fingerprint) == current.as_ref()
                })
                .cloned()
        });
        // Detection stops while the configuration is swapped: MANUAL is held
        // for the publish and put back as it was afterwards, whatever the
        // outcome (no-regression #8). Only MANUAL is touched, so a reason
        // set meanwhile by someone else (safe mode, recording, the engine
        // going down) is never cleared by the restore.
        let was_manual = self.runtime.pause_reasons().contains(PauseReasons::MANUAL);
        self.runtime.set_pause(PauseReasons::MANUAL, true);
        self.engine.cancel.store(true, Ordering::SeqCst);
        self.engine.generation.fetch_add(1, Ordering::SeqCst);
        let restore = |store: &Self| {
            store.runtime.set_pause(PauseReasons::MANUAL, was_manual);
        };
        if self.engine.busy.load(Ordering::SeqCst) {
            restore(self);
            return Err(CommandError::Busy);
        }
        let old = self.active.clone();
        // The empty stand-in for a missing configuration is not backed up
        // (see `active_on_disk`); there is nothing to roll back to either.
        let old_on_disk = self.active_on_disk;
        let outcome = (|| {
            let revisions = self.dir.join("revisions");
            fs::create_dir_all(&revisions)
                .map_err(|e| CommandError::io(IoOp::CreateDir, Some(&revisions), &e))?;
            if old_on_disk {
                let backup = revisions.join(format!("{}.json", old.revision));
                if backup.exists() {
                    if read_config(&backup)? != old {
                        return Err(CommandError::internal(
                            "Historical revision conflict; existing snapshot preserved",
                        ));
                    }
                } else {
                    self.write_data(&backup, &old)?;
                }
                self.write_data(
                    &revisions.join(format!("{}.metadata.json", old.revision)),
                    &self.active_unresolved,
                )?;
            }
            self.write_data(
                &revisions.join(format!("{}.metadata.json", config.revision)),
                &metadata,
            )?;
            // Revision metadata is staged first; active.json is the commit pointer.
            // Failed saves leave the previous revision's metadata authoritative.
            // R3: the predecessor is recorded before the new revision becomes
            // active, so that after a restart rollback returns where it would
            // now. If it is not written (store-meta.json read-only this
            // session, or the write fails), no entry on disk matches the new
            // `active.json` (I-R2), and the next load guesses as before.
            let previous = PreviousRevision {
                active: config.revision,
                revision: old_on_disk.then_some(old.revision),
                fingerprint: Fingerprint::of(&new_bytes),
            };
            let records: Vec<PreviousRevision> =
                std::iter::once(previous).chain(committed).collect();
            if let Err(e) = self.meta.update(|meta| meta.previous_revisions = records) {
                tracing::warn!(event = "store.meta_write_failed", error = %e, "store-meta.json not written");
            }
            self.write_data(&self.dir.join("active.json"), &config)?;
            Ok::<_, CommandError>(())
        })();
        if let Err(e) = outcome {
            restore(self);
            return Err(e);
        }
        *self.engine.compiled.lock().unwrap() = compiled;
        *self.engine.config.lock().unwrap() = config.clone();
        self.active = config.clone();
        self.active_on_disk = true;
        // A stand-in was not backed up: there is no revision to roll back to.
        if old_on_disk {
            self.previous = Some(old);
            self.previous_unresolved = self.active_unresolved.clone();
        } else {
            self.previous = None;
            self.previous_unresolved = vec![];
        }
        self.active_unresolved = metadata.clone();
        self.pending_import = None;
        self.pending_import_token = None;
        self.disk_hash = Some(Sha256::digest(&new_bytes).to_vec());
        restore(self);
        if self.safe_mode.is_some() {
            tracing::info!(event = "store.safe_mode_import", "import published");
            self.set_safe_mode(None);
        }
        self.report_store_state();
        tracing::info!(
            event = "store.published",
            revision = self.active.revision,
            enabled = self.active.enabled,
            rule_count = self.active.rules.len(),
            enabled_rule_count = self.active.rules.iter().filter(|rule| rule.enabled).count(),
            "configuration published to disk and engine"
        );
        Ok(())
    }
    /// One above the active revision and every number a file of this program
    /// in `revisions/` uses, so that after recovering from an older revision
    /// the newer snapshots are never overwritten or mistaken for a conflict,
    /// and a number is never given out twice (review R7): not after its
    /// snapshot was quarantined (`<n>.corrupt-*.json`) or never written (only
    /// `<n>.metadata.json`), where the publish would overwrite that metadata
    /// and a record for the old revision could match the new one. Other
    /// names do not count, so a stray file cannot use up the numbers (review
    /// R8-2). A `revisions/` that cannot be listed refuses the publish.
    fn next_revision(&self) -> Result<u64, CommandError> {
        let revisions = self.dir.join("revisions");
        occupied_revision_numbers(&revisions)
            .map_err(|e| CommandError::io(IoOp::Read, Some(&revisions), &e))?
            .into_iter()
            .fold(self.active.revision, u64::max)
            .checked_add(1)
            .ok_or_else(|| {
                CommandError::internal(
                    "Revision counter exhausted; preserve data and start a new profile",
                )
            })
    }
    pub fn rollback_previous(&mut self) -> Result<(), CommandError> {
        self.ensure_writable()?;
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

/// Every revision number that a file of this program in `revisions` uses
/// (review R7-2), see [`occupied_number`]. Only for [`Store::next_revision`];
/// the fallback candidates are the snapshots alone
/// (`fallback::revision_numbers`). No `revisions/` yet is no number.
fn occupied_revision_numbers(revisions: &Path) -> io::Result<Vec<u64>> {
    let entries = match fs::read_dir(revisions) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut numbers = vec![];
    for entry in entries {
        numbers.extend(occupied_number(&entry?.file_name().to_string_lossy()));
    }
    Ok(numbers)
}

/// `n` for the names this program gives files in `revisions/`; `None` for
/// any other name (review R8-2):
/// - `<n>.json` and `<n>.metadata.json`;
/// - their quarantined forms `<n>.corrupt-<stamp>.json` and
///   `<n>.metadata.corrupt-<stamp>.json` (`fallback::quarantine_target`);
/// - the temp files `atomic_json` writes them through, `<n>.json.tmp` and
///   `<n>.metadata.json.tmp`.
fn occupied_number(name: &str) -> Option<u64> {
    let stem = match name.strip_suffix(".json.tmp") {
        Some(stem) => stem,
        None => quarantined_stem(name).or_else(|| name.strip_suffix(".json"))?,
    };
    let number = stem.strip_suffix(".metadata").unwrap_or(stem);
    if !is_revision_stem(number) {
        return None;
    }
    number.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::super::{atomic::atomic_json, test_support::*};
    use super::*;
    fn assert_runtime_match(
        store: &Store,
        id: &str,
        trigger: &str,
        terminator: Option<&str>,
        mode: TriggerMode,
        body: &str,
    ) {
        let compiler = store.engine.compiled.lock().unwrap();
        assert_eq!(compiler.config, store.active);
        assert!(compiler.config.enabled);
        let context = ForegroundContext {
            process: "notepad.exe".into(),
            ..Default::default()
        };
        let matched = compiler.match_input(trigger, terminator, &context).unwrap();
        assert_eq!(matched.rule_id, id);
        assert_eq!(matched.revision, store.active.revision);
        let rule = compiler.rule(id).unwrap();
        assert!(rule.enabled);
        assert_eq!(rule.trigger_mode, mode);
        let plan = compiler
            .expand_reusable(&rule.action, &VariableContext::default())
            .unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(
            plan.steps[0].operation,
            Operation::Text {
                text: body.into(),
                method: OutputMethod::Keys
            }
        );
    }

    #[test]
    fn imported_and_manual_rules_keep_matching_after_edits_failed_save_and_reload() {
        let dir = std::env::temp_dir().join(format!("quicktext-matching-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone());
        let text_rule = |id: &str, trigger: &str, body: &str, mode| Rule {
            id: id.into(),
            match_spec: MatchSpec {
                kind: MatchKind::Literal,
                value: trigger.into(),
            },
            trigger_mode: mode,
            action: Action::Text {
                method: OutputMethod::Keys,
                source: ContentSource::inline(body),
            },
            ..Default::default()
        };
        // Take the same deserialize + preview/confirm path as a JSON import.
        let imported = Config {
            rules: vec![text_rule(
                "imported",
                "thx",
                "Thanks",
                TriggerMode::Immediate,
            )],
            ..Default::default()
        };
        let imported: Config =
            serde_json::from_slice(&serde_json::to_vec(&imported).unwrap()).unwrap();
        let token = store.preview_import(imported.clone(), vec![]);
        store.save_config(imported, Some(token)).unwrap();
        listening(&store);
        assert_runtime_match(
            &store,
            "imported",
            "thx",
            None,
            TriggerMode::Immediate,
            "Thanks",
        );

        let mut manual = store.active.clone();
        manual.rules.push(text_rule(
            "manual",
            "time",
            "Time body",
            TriggerMode::Terminator,
        ));
        manual.rules.push(Rule {
            enabled: false,
            ..text_rule("disabled", "off", "Disabled body", TriggerMode::Immediate)
        });
        store.save_config(manual, None).unwrap();
        assert_runtime_match(
            &store,
            "imported",
            "thx",
            None,
            TriggerMode::Immediate,
            "Thanks",
        );
        assert_runtime_match(
            &store,
            "manual",
            "time",
            Some("space"),
            TriggerMode::Terminator,
            "Time body",
        );
        let context = ForegroundContext::default();
        assert!(store
            .engine
            .compiled
            .lock()
            .unwrap()
            .match_input("time", None, &context)
            .is_none());
        assert!(store
            .engine
            .compiled
            .lock()
            .unwrap()
            .match_input("off", None, &context)
            .is_none());

        let mut edit = store.active.clone();
        edit.rules[1].match_spec.value = "whoami".into();
        edit.rules[1].trigger_mode = TriggerMode::Immediate;
        edit.rules[1].action = Action::Text {
            method: OutputMethod::Keys,
            source: ContentSource::inline("Edited body"),
        };
        store.save_config(edit, None).unwrap();
        assert!(store
            .engine
            .compiled
            .lock()
            .unwrap()
            .match_input("time", Some("space"), &context)
            .is_none());
        assert_runtime_match(
            &store,
            "manual",
            "whoami",
            None,
            TriggerMode::Immediate,
            "Edited body",
        );
        assert!(store.runtime.engine_state().listening);

        let committed = store.active.clone();
        let disk = fs::read(dir.join("active.json")).unwrap();
        fs::create_dir(dir.join("active.json.tmp")).unwrap();
        let mut rejected = committed.clone();
        rejected.rules[1].enabled = false;
        assert!(store.save_config(rejected, None).is_err());
        assert_eq!(store.active, committed);
        assert_eq!(fs::read(dir.join("active.json")).unwrap(), disk);
        assert!(store.runtime.engine_state().listening);
        assert_runtime_match(
            &store,
            "manual",
            "whoami",
            None,
            TriggerMode::Immediate,
            "Edited body",
        );
        fs::remove_dir(dir.join("active.json.tmp")).unwrap();
        store.meta.clear_run_marker();
        drop(store);

        let reloaded = Store::load(dir.clone());
        assert_eq!(reloaded.active, committed);
        assert_runtime_match(
            &reloaded,
            "imported",
            "thx",
            None,
            TriggerMode::Immediate,
            "Thanks",
        );
        assert_runtime_match(
            &reloaded,
            "manual",
            "whoami",
            None,
            TriggerMode::Immediate,
            "Edited body",
        );
        assert!(reloaded
            .engine
            .compiled
            .lock()
            .unwrap()
            .rule("disabled")
            .is_none());
        reloaded.meta.clear_run_marker();
        drop(reloaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_config_updates_engine_immediately_and_rejects_stale_revision() {
        let dir = std::env::temp_dir().join(format!("quicktext-direct-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone());
        let first = metadata_config(&["first"]);
        store.save_config(first.clone(), None).unwrap();
        assert_eq!(store.active.revision, 1);
        assert_eq!(*store.engine.config.lock().unwrap(), store.active);
        assert_eq!(store.active.rules, first.rules);
        let committed = fs::read(dir.join("active.json")).unwrap();
        assert!(matches!(
            store.save_config(first, None),
            Err(CommandError::RevisionConflict { current: 1 })
        ));
        assert_eq!(fs::read(dir.join("active.json")).unwrap(), committed);
        assert_eq!(*store.engine.config.lock().unwrap(), store.active);
        assert!(!dir.join("draft.json").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn preview_is_memory_only_and_failed_confirmation_keeps_active_originals() {
        let dir =
            std::env::temp_dir().join(format!("quicktext-direct-import-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone());
        let first = metadata_config(&["shared"]);
        let token = store.preview_import(first.clone(), vec![original("shared", "A")]);
        store.save_config(first, Some(token)).unwrap();
        let committed = fs::read(dir.join("active.json")).unwrap();
        let metadata = fs::read(dir.join("revisions/1.metadata.json")).unwrap();
        let mut preview = store.active.clone();
        preview.rules[0].description = "imported preview".into();
        let token = store.preview_import(preview.clone(), vec![original("shared", "B")]);
        assert_eq!(fs::read(dir.join("active.json")).unwrap(), committed);
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "A");
        let exported: serde_json::Value =
            serde_json::from_str(&store.export_snapshot().unwrap()).unwrap();
        assert_ne!(
            exported["config"]["rules"][0]["description"],
            "imported preview"
        );
        fs::create_dir(dir.join("active.json.tmp")).unwrap();
        assert!(store
            .save_config(preview.clone(), Some(token.clone()))
            .is_err());
        assert_eq!(fs::read(dir.join("active.json")).unwrap(), committed);
        assert_eq!(
            fs::read(dir.join("revisions/1.metadata.json")).unwrap(),
            metadata
        );
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "A");
        assert_eq!(*store.engine.config.lock().unwrap(), store.active);
        assert!(store.pending_import.is_some());
        fs::remove_dir(dir.join("active.json.tmp")).unwrap();
        store.meta.clear_run_marker();
        let reloaded = Store::load(dir.clone());
        assert_eq!(
            reloaded.active_unresolved[0].original["synthetic_source"],
            "A"
        );
        reloaded.meta.clear_run_marker();
        drop(reloaded);
        store.save_config(preview, Some(token)).unwrap();
        assert!(store.pending_import.is_none());
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "B");
        store.meta.clear_run_marker();
        drop(store);
        let reloaded = Store::load(dir.clone());
        assert_eq!(reloaded.active.rules[0].description, "imported preview");
        assert_eq!(
            reloaded.active_unresolved[0].original["synthetic_source"],
            "B"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn import_tokens_bind_the_exact_preview_and_expire_when_replaced() {
        let dir = std::env::temp_dir().join(format!("quicktext-tokens-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone());
        let config = metadata_config(&["shared"]);
        let a = store.preview_import(config.clone(), vec![original("shared", "A")]);
        let b = store.preview_import(config.clone(), vec![original("shared", "B")]);
        assert!(matches!(
            store.save_config(config.clone(), Some(a)),
            Err(CommandError::TokenExpired)
        ));
        assert!(!dir.join("active.json").exists());
        let mut changed = config.clone();
        changed.enabled = false;
        assert!(matches!(
            store.save_config(changed, Some(b.clone())),
            Err(CommandError::TokenExpired)
        ));
        store.save_config(config, Some(b)).unwrap();
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "B");
        let current = store.active.clone();
        let token = store.preview_import(current.clone(), vec![original("shared", "unconfirmed")]);
        store.save_config(current, None).unwrap();
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "B");
        let current = store.active.clone();
        assert!(matches!(
            store.save_config(current, Some(token)),
            Err(CommandError::TokenExpired)
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ordinary_save_cannot_claim_the_safe_mode_import_exemption() {
        let dir =
            std::env::temp_dir().join(format!("quicktext-safe-import-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("active.json"), b"{").unwrap();
        let mut store = Store::load(dir.clone());
        let config = metadata_config(&["recovery"]);
        let token = store.preview_import(config.clone(), vec![original("recovery", "R")]);
        assert!(matches!(
            store.save_config(config.clone(), None),
            Err(CommandError::SafeMode { .. })
        ));
        store.save_config(config, Some(token)).unwrap();
        assert_eq!(store.safe_mode(), None);
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "R");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_unconfirmed_content_is_archived_once_and_never_activated() {
        let dir = std::env::temp_dir().join(format!("quicktext-legacy-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let active = metadata_config(&["active"]);
        let unconfirmed = metadata_config(&["unconfirmed"]);
        atomic_json(&dir.join("active.json"), &active).unwrap();
        atomic_json(&dir.join("draft.json"), &unconfirmed).unwrap();
        atomic_json(
            &dir.join("migration-unresolved.json"),
            &vec![original("unconfirmed", "legacy")],
        )
        .unwrap();
        let legacy = fs::read(dir.join("draft.json")).unwrap();
        for _ in 0..2 {
            let store = Store::load(dir.clone());
            assert_eq!(store.active, active);
            assert_eq!(*store.engine.config.lock().unwrap(), active);
            assert_eq!(
                fs::read(dir.join("legacy-unconfirmed/draft.json")).unwrap(),
                legacy
            );
            assert!(!dir.join("draft.json").exists());
            assert!(!dir.join("migration-unresolved.json").exists());
            assert_eq!(
                fs::read_dir(dir.join("legacy-unconfirmed"))
                    .unwrap()
                    .count(),
                2
            );
            let state = serde_json::to_value(store.state()).unwrap();
            assert!(state.get("draft").is_none());
            assert!(state.get("dirty").is_none());
            store.meta.clear_run_marker();
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rollback_restores_revision_originals_after_reload() {
        let dir =
            std::env::temp_dir().join(format!("quicker-metadata-test-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone());
        for marker in ["A", "B"] {
            let mut config = metadata_config(&["shared"]);
            config.rules[0].description = marker.into();
            store.pending_import = Some((config.clone(), vec![original("shared", marker)]));
            store.publish(config).unwrap();
        }
        drop(store);
        let mut store = Store::load(dir.clone());
        store.rollback_previous().unwrap();
        assert_eq!(store.active.revision, 3);
        assert_eq!(store.active.rules[0].description, "A");
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "A");
        drop(store);
        let store = Store::load(dir.clone());
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "A");
        assert_eq!(
            store.previous_unresolved[0].original["synthetic_source"],
            "B"
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn persistent_revision_conflict() {
        let dir = std::env::temp_dir().join(format!("quicktext-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone());
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
        let dir = std::env::temp_dir().join(format!("quicktext-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone());
        s.publish(Config::default()).unwrap();
        let old = s.active.clone();
        listening(&s);
        assert!(!s.engine.paused.load(Ordering::SeqCst));
        fs::create_dir(dir.join("active.json.tmp")).unwrap();
        let mut next = old.clone();
        next.enabled = false;
        assert!(s.publish(next).is_err());
        assert_eq!(s.active, old);
        assert_eq!(read_config(&dir.join("active.json")).unwrap(), old);
        assert!(!s.engine.paused.load(Ordering::SeqCst));
        assert!(s.runtime.engine_state().listening);
        assert!(s.runtime.pause_reasons().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
    /// The publish hold restores MANUAL as it was: a paused engine stays
    /// paused and a listening one listens, on success and on failure, and
    /// a reason set by someone else is left alone.
    #[test]
    fn publish_restores_only_the_manual_reason() {
        let dir = std::env::temp_dir().join(format!("quicktext-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone());
        s.runtime.publish_health(healthy());
        assert_eq!(s.runtime.pause_reasons(), PauseReasons::MANUAL);
        s.publish(Config::default()).unwrap();
        assert_eq!(s.runtime.pause_reasons(), PauseReasons::MANUAL);
        listening(&s);
        s.publish(Config::default()).unwrap();
        assert!(s.runtime.engine_state().listening);
        s.runtime.set_pause(PauseReasons::RECORDING, true);
        s.engine.busy.store(true, Ordering::SeqCst);
        assert!(matches!(
            s.publish(Config::default()),
            Err(CommandError::Busy)
        ));
        assert_eq!(s.runtime.pause_reasons(), PauseReasons::RECORDING);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rollback_revision_and_reload() {
        let dir = std::env::temp_dir().join(format!("quicktext-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone());
        s.publish(Config::default()).unwrap();
        let mut next = s.active.clone();
        next.enabled = false;
        s.publish(next).unwrap();
        let previous = s.previous.clone().unwrap();
        s.publish(previous).unwrap();
        assert_eq!(s.active.revision, 3);
        assert!(s.active.enabled);
        drop(s);
        let s = Store::load(dir.clone());
        assert_eq!(s.active.revision, 3);
        assert!(s.engine.paused.load(Ordering::SeqCst));
        fs::remove_dir_all(dir).unwrap();
    }
    /// Review R8-2: only the names this program writes occupy a number, the
    /// ones `quarantine_target` and `atomic_json` produce included.
    #[test]
    fn every_name_of_this_program_occupies_its_revision() {
        let dir = std::env::temp_dir().join(format!("quicktext-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let quarantined = |name: &str| {
            let path = dir.join(name);
            fs::write(&path, b"").unwrap();
            let target = super::super::fallback::quarantine_target(&path, 0);
            fs::rename(&path, &target).unwrap();
            target.file_name().unwrap().to_string_lossy().into_owned()
        };
        let first = quarantined("7.json");
        let second = quarantined("7.json");
        let metadata = quarantined("8.metadata.json");
        assert_eq!(
            [&first, &second, &metadata],
            [
                "7.corrupt-19700101-000000.json",
                "7.corrupt-19700101-000000-1.json",
                "8.metadata.corrupt-19700101-000000.json",
            ]
        );
        // The temp name `atomic_json` uses.
        let temp = |name: &str| {
            let path = Path::new(name).with_extension("json.tmp");
            path.to_string_lossy().into_owned()
        };
        let (snapshot_temp, metadata_temp) = (temp("9.json"), temp("10.metadata.json"));
        for (name, number) in [
            ("3.json", 3),
            ("5.metadata.json", 5),
            (first.as_str(), 7),
            (second.as_str(), 7),
            (metadata.as_str(), 8),
            (snapshot_temp.as_str(), 9),
            (metadata_temp.as_str(), 10),
            ("18446744073709551615.json", u64::MAX),
        ] {
            assert_eq!(occupied_number(name), Some(number), "{name}");
        }
        for name in [
            "index.json",
            ".json",
            "a1.json",
            "1a.json",
            "-1.json",
            "12",
            "5.x",
            "18446744073709551615.x",
            "18446744073709551614.x",
            "3.json.bak",
            "3.metadata",
            "3.metadata.metadata.json",
            "3.backup.json",
            "7.corrupt-garbage.json",
            "7.corrupt-19700101-000000.json.tmp",
            "18446744073709551616.json",
        ] {
            assert_eq!(occupied_number(name), None, "{name}");
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn revision_overflow_is_error() {
        let dir = std::env::temp_dir().join(format!("quicktext-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone());
        s.active.revision = u64::MAX;
        assert!(s.publish(Config::default()).is_err());
        assert_eq!(s.active.revision, u64::MAX);
        assert!(s.engine.paused.load(Ordering::SeqCst));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn publish_and_rollback_failures_use_specific_error_codes() {
        let dir = std::env::temp_dir().join(format!("quicktext-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone());
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
