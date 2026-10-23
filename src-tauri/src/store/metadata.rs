//! Per-rule migration metadata (the original Quicker entries kept for import).
use quicker_domain::*;
use std::{collections::BTreeSet, fs, path::Path};

pub(super) fn read_metadata(path: &Path) -> Vec<MigrationItem> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}
/// The ids of the rules `config` keeps: the readable ones and the unreadable
/// entries that have a string id. An unreadable rule is kept verbatim
/// (B-43), and so is the migration original attached to it (F4-2): it is
/// what the user needs to repair the rule.
pub(super) fn rule_ids(config: &Config) -> BTreeSet<&str> {
    config
        .rules
        .iter()
        .map(|rule| rule.id.as_str())
        .chain(
            config
                .unreadable_rules
                .iter()
                .filter_map(UnreadableRule::id),
        )
        .collect()
}
pub(super) fn metadata_for_config(
    config: &Config,
    metadata: &[MigrationItem],
) -> Vec<MigrationItem> {
    let ids = rule_ids(config);
    metadata
        .iter()
        .filter(|item| ids.contains(item.rule_id.as_str()))
        .cloned()
        .collect()
}

/// Keep each original attached to the rule actually accepted by the import.
/// Unreadable rules count as rules here too (see [`rule_ids`]).
pub fn merge_import_metadata(
    strategy: &str,
    existing: &Config,
    incoming: &Config,
    merged: &Config,
    old_metadata: &[MigrationItem],
    incoming_metadata: &[MigrationItem],
) -> Vec<MigrationItem> {
    use std::collections::BTreeMap;
    let existing_ids = rule_ids(existing);
    let incoming_ids = rule_ids(incoming);
    let final_ids = rule_ids(merged);
    let mut metadata = BTreeMap::new();
    if strategy != "replace" {
        for item in old_metadata {
            if existing_ids.contains(item.rule_id.as_str())
                && !(strategy == "overwrite" && incoming_ids.contains(item.rule_id.as_str()))
            {
                metadata.insert(item.rule_id.clone(), item.clone());
            }
        }
    }
    // KeepBoth appends every incoming rule in its original order. Using the
    // actual appended IDs also handles pre-existing and incoming suffix clashes.
    let appended_ids: BTreeMap<_, _> = if strategy == "keep_both" {
        incoming
            .rules
            .iter()
            .zip(merged.rules.iter().skip(existing.rules.len()))
            .map(|(original, appended)| (original.id.as_str(), appended.id.as_str()))
            .collect()
    } else {
        BTreeMap::new()
    };
    for item in incoming_metadata {
        if !incoming_ids.contains(item.rule_id.as_str()) {
            continue;
        }
        let mut item = item.clone();
        if strategy == "keep_both" {
            let Some(id) = appended_ids.get(item.rule_id.as_str()) else {
                continue;
            };
            item.rule_id = (*id).to_string();
        } else if strategy != "replace"
            && strategy != "overwrite"
            && existing_ids.contains(item.rule_id.as_str())
        {
            continue;
        }
        metadata.insert(item.rule_id.clone(), item);
    }
    metadata
        .into_values()
        .filter(|item| final_ids.contains(item.rule_id.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    #[test]
    fn import_metadata_replace_merge_and_overwrite_follow_accepted_rules() {
        let existing = metadata_config(&["shared", "old", "without-raw"]);
        let incoming = metadata_config(&["shared", "new", "without-raw"]);
        let old = vec![
            original("shared", "old-shared"),
            original("old", "old"),
            original("without-raw", "obsolete"),
            original("deleted", "stale"),
        ];
        let imported = vec![
            original("shared", "new-shared"),
            original("new", "new"),
            original("missing", "stale"),
        ];
        let merged = metadata_config(&["shared", "old", "without-raw", "new"]);
        let replace = metadata_sources(merge_import_metadata(
            "replace", &existing, &incoming, &incoming, &old, &imported,
        ));
        assert_eq!(
            replace,
            [
                ("shared".into(), "new-shared".into()),
                ("new".into(), "new".into())
            ]
            .into()
        );
        let merge = metadata_sources(merge_import_metadata(
            "merge", &existing, &incoming, &merged, &old, &imported,
        ));
        assert_eq!(
            merge,
            [
                ("shared".into(), "old-shared".into()),
                ("old".into(), "old".into()),
                ("new".into(), "new".into()),
                ("without-raw".into(), "obsolete".into())
            ]
            .into()
        );
        let overwrite = metadata_sources(merge_import_metadata(
            "overwrite",
            &existing,
            &incoming,
            &merged,
            &old,
            &imported,
        ));
        assert_eq!(
            overwrite,
            [
                ("shared".into(), "new-shared".into()),
                ("old".into(), "old".into()),
                ("new".into(), "new".into())
            ]
            .into()
        );
    }
    #[test]
    fn keep_both_metadata_uses_actual_appended_ids_with_suffix_collisions() {
        let existing = metadata_config(&["shared", "shared-2"]);
        let incoming = metadata_config(&["shared", "shared-3", "shared-2"]);
        let merged = merge_import(
            &existing,
            &ImportReport {
                config: incoming.clone(),
                items: vec![],
                unresolved: vec![],
            },
            ConflictStrategy::KeepBoth,
        )
        .unwrap();
        assert_eq!(
            merged
                .rules
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["shared", "shared-2", "shared-3", "shared-3-2", "shared-2-2"]
        );
        let old = vec![
            original("shared", "old-base"),
            original("shared-2", "old-suffix"),
        ];
        let imported = vec![
            original("shared", "new-base"),
            original("shared-3", "new-three"),
            original("shared-2", "new-two"),
        ];
        let metadata = metadata_sources(merge_import_metadata(
            "keep_both",
            &existing,
            &incoming,
            &merged,
            &old,
            &imported,
        ));
        assert_eq!(
            metadata,
            [
                ("shared".into(), "old-base".into()),
                ("shared-2".into(), "old-suffix".into()),
                ("shared-3".into(), "new-base".into()),
                ("shared-3-2".into(), "new-three".into()),
                ("shared-2-2".into(), "new-two".into())
            ]
            .into()
        );
    }

    /// `existing` with one more rule, `broken`, that this build cannot read.
    fn with_unreadable(mut config: Config) -> Config {
        config.unreadable_rules.push(UnreadableRule {
            index: config.rules.len(),
            raw: serde_json::json!({"id": "broken", "enabled": "not a bool"}),
        });
        config
    }

    /// F4-2: an import keeps the original of an unreadable rule that stays
    /// in the draft, and drops it only with the rule (replace).
    #[test]
    fn import_metadata_keeps_originals_of_unreadable_rules() {
        let existing = with_unreadable(metadata_config(&["old"]));
        let incoming = metadata_config(&["new"]);
        let old = vec![original("old", "old"), original("broken", "broken")];
        let imported = vec![original("new", "new")];
        let mut merged = existing.clone();
        merged.rules.push(incoming.rules[0].clone());
        let expected: std::collections::BTreeMap<String, String> = [
            ("old".into(), "old".into()),
            ("broken".into(), "broken".into()),
            ("new".into(), "new".into()),
        ]
        .into();
        for strategy in ["merge", "overwrite"] {
            let metadata = metadata_sources(merge_import_metadata(
                strategy, &existing, &incoming, &merged, &old, &imported,
            ));
            assert_eq!(metadata, expected, "{strategy}");
        }
        let kept_both = merge_import(
            &existing,
            &ImportReport {
                config: incoming.clone(),
                items: vec![],
                unresolved: vec![],
            },
            ConflictStrategy::KeepBoth,
        )
        .unwrap();
        assert_eq!(kept_both.unreadable_rules, existing.unreadable_rules);
        let metadata = metadata_sources(merge_import_metadata(
            "keep_both",
            &existing,
            &incoming,
            &kept_both,
            &old,
            &imported,
        ));
        assert_eq!(metadata, expected);
        let replaced = metadata_sources(merge_import_metadata(
            "replace", &existing, &incoming, &incoming, &old, &imported,
        ));
        assert_eq!(replaced, [("new".into(), "new".into())].into());
    }

    /// F4-2 (Codex's case): an imported rule that is not published yet
    /// becomes unreadable through an outside edit of `draft.json`. An
    /// unrelated save, a restart, an export and a publish all keep its
    /// Quicker original, the only copy there is.
    #[test]
    fn unreadable_imported_rule_keeps_its_original() {
        use super::super::Store;
        let dir =
            std::env::temp_dir().join(format!("quicker-metadata-test-{}", uuid::Uuid::new_v4()));
        let elsewhere =
            std::env::temp_dir().join(format!("quicker-metadata-export-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&elsewhere).unwrap();
        let expected: std::collections::BTreeMap<String, String> =
            [("kept".into(), "K".into()), ("imported".into(), "Q".into())].into();
        let on_disk = |name: &str| metadata_sources(read_metadata(&dir.join(name)));
        // A clean exit, so the restarts do not read as a crash loop.
        let exit = |store: Store| store.meta.clear_run_marker();

        let mut store = Store::load(dir.clone());
        let imported = metadata_config(&["kept", "imported"]);
        store.pending_import = Some((
            imported.clone(),
            vec![original("kept", "K"), original("imported", "Q")],
        ));
        store.save(imported).unwrap();
        exit(store);
        let draft_path = dir.join("draft.json");
        let mut draft: serde_json::Value =
            serde_json::from_slice(&fs::read(&draft_path).unwrap()).unwrap();
        draft["rules"][1]["enabled"] = serde_json::json!("not a bool");
        let raw = draft["rules"][1].clone();
        fs::write(&draft_path, serde_json::to_vec_pretty(&draft).unwrap()).unwrap();

        // An unrelated save.
        let mut store = Store::load(dir.clone());
        assert_eq!(store.safe_mode(), None);
        assert_eq!(store.draft.rules.len(), 1);
        assert_eq!(store.draft.unreadable_rules[0].id(), Some("imported"));
        let mut edited = store.draft.clone();
        edited.rules[0].description = "unrelated edit".into();
        store.save(edited).unwrap();
        assert_eq!(metadata_sources(store.unresolved.clone()), expected);
        assert_eq!(on_disk("migration-unresolved.json"), expected);
        exit(store);

        // A restart, then an export.
        let mut store = Store::load(dir.clone());
        assert_eq!(metadata_sources(store.unresolved.clone()), expected);
        assert_eq!(store.draft.unreadable_rules[0].raw, raw);
        let target = elsewhere.join("backup.json");
        store.export_draft(&target).unwrap();
        let package: serde_json::Value =
            serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
        let exported: Vec<MigrationItem> =
            serde_json::from_value(package["unresolved"].clone()).unwrap();
        assert_eq!(metadata_sources(exported), expected);
        assert_eq!(package["config"]["rules"][1], raw);

        // A publish keeps it in the active metadata, also after a restart.
        store.publish(store.draft.clone()).unwrap();
        let revision = store.active.revision;
        assert_eq!(metadata_sources(store.active_unresolved.clone()), expected);
        assert_eq!(on_disk("active-migration-unresolved.json"), expected);
        assert_eq!(
            on_disk(&format!("revisions/{revision}.metadata.json")),
            expected
        );
        exit(store);
        let mut store = Store::load(dir.clone());
        assert_eq!(store.safe_mode(), None);
        assert_eq!(metadata_sources(store.active_unresolved.clone()), expected);
        store.discard().unwrap();
        assert_eq!(metadata_sources(store.unresolved.clone()), expected);
        exit(store);
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(elsewhere).unwrap();
    }
}
