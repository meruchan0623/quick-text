//! Configuration store: draft, active and previous configurations plus their
//! files in the data directory (§7.1 「数据目录」).
mod atomic;
mod load;
mod metadata;
mod revisions;

pub use metadata::merge_import_metadata;

use atomic::atomic_json;
use metadata::metadata_for_config;
use quicker_domain::*;
use quicker_platform::Shared;
use std::{path::PathBuf, sync::Arc};

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
}
impl Store {
    pub fn save(&mut self, config: Config) -> Result<(), String> {
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
        Ok(())
    }
    pub fn discard(&mut self) -> Result<(), String> {
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
        let mut store = Store::load(dir.clone()).unwrap();
        let a = metadata_config(&["shared"]);
        store.pending_import = Some((a.clone(), vec![original("shared", "A")]));
        store.save(a).unwrap();
        store.publish(store.draft.clone()).unwrap();
        let mut b = metadata_config(&["shared"]);
        b.rules[0].description = "synthetic replacement".into();
        store.pending_import = Some((b.clone(), vec![original("shared", "B")]));
        store.save(b.clone()).unwrap();
        drop(store);
        let mut store = Store::load(dir.clone()).unwrap();
        assert_eq!(store.unresolved[0].original["synthetic_source"], "B");
        store.pending_import = Some((b, vec![original("shared", "pending")]));
        store.discard().unwrap();
        assert!(store.pending_import.is_none());
        assert_eq!(store.unresolved[0].original["synthetic_source"], "A");
        assert_eq!(store.draft, store.active);
        drop(store);
        let store = Store::load(dir.clone()).unwrap();
        assert_eq!(store.unresolved[0].original["synthetic_source"], "A");
        fs::remove_dir_all(dir).unwrap();
    }
}
