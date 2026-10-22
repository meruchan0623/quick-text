//! Per-rule migration metadata (the original Quicker entries kept for import).
use quicker_domain::*;
use std::{fs, path::Path};

pub(super) fn read_metadata(path: &Path) -> Vec<MigrationItem> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}
pub(super) fn metadata_for_config(
    config: &Config,
    metadata: &[MigrationItem],
) -> Vec<MigrationItem> {
    metadata
        .iter()
        .filter(|item| config.rules.iter().any(|rule| rule.id == item.rule_id))
        .cloned()
        .collect()
}

/// Keep each original attached to the rule actually accepted by the import.
pub fn merge_import_metadata(
    strategy: &str,
    existing: &Config,
    incoming: &Config,
    merged: &Config,
    old_metadata: &[MigrationItem],
    incoming_metadata: &[MigrationItem],
) -> Vec<MigrationItem> {
    use std::collections::{BTreeMap, BTreeSet};
    let existing_ids: BTreeSet<_> = existing.rules.iter().map(|r| r.id.as_str()).collect();
    let incoming_ids: BTreeSet<_> = incoming.rules.iter().map(|r| r.id.as_str()).collect();
    let final_ids: BTreeSet<_> = merged.rules.iter().map(|r| r.id.as_str()).collect();
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
}
