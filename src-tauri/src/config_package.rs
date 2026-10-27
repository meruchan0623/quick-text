//! Native packages use the stored model's lossless reader. Quicker command
//! arrays continue through the domain migration importer.
use quicker_domain::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn invalid(code: &str, message: &str) -> Vec<Diagnostic> {
    vec![Diagnostic::error(code, message, SourceSpan::default())]
}

pub(crate) fn import(source: &str) -> Result<ImportReport, Vec<Diagnostic>> {
    if source.len() > 16_777_216 {
        return Err(invalid("package_size", "配置包超过16MiB限制"));
    }
    let value: Value = serde_json::from_str(source.strip_prefix('\u{feff}').unwrap_or(source))
        .map_err(|_| invalid("package_json", "配置包JSON格式错误"))?;
    if value.is_array() {
        return import_package(source);
    }
    let (raw, unresolved) = if value.get("format").is_some() {
        if value.get("format").and_then(Value::as_str) != Some("quicker-text")
            || value.get("version").and_then(Value::as_u64) != Some(SCHEMA_VERSION as u64)
        {
            return Err(invalid("package_version", "配置包格式或版本不受支持"));
        }
        if value.as_object().is_some_and(|fields| {
            fields
                .keys()
                .any(|key| !["format", "version", "config", "unresolved"].contains(&key.as_str()))
        }) {
            return Err(invalid(
                "package_schema",
                "无法识别配置包元数据，未导入任何内容",
            ));
        }
        (
            value
                .get("config")
                .cloned()
                .ok_or_else(|| invalid("package_schema", "缺少配置"))?,
            serde_json::from_value(
                value
                    .get("unresolved")
                    .cloned()
                    .unwrap_or(serde_json::json!([])),
            )
            .map_err(|_| invalid("package_schema", "迁移原始数据格式错误"))?,
        )
    } else {
        (value, vec![])
    };
    let mut config: Config = serde_json::from_value(raw.clone())
        .map_err(|_| invalid("config_schema", "配置字段类型不兼容，未导入任何内容"))?;
    if config.version != SCHEMA_VERSION {
        return Err(invalid("schema_version", "配置版本不受支持"));
    }
    let mut ids = BTreeSet::new();
    for rule in &config.rules {
        if rule.id.trim().is_empty() || !ids.insert(rule.id.clone()) {
            return Err(invalid("rule_id", "导入指令编号不能为空或重复"));
        }
    }
    let mut items = vec![];
    // Validate against the whole graph, retaining disabled legacy values. As
    // with the migration importer, invalid imported actions remain disabled.
    let mut candidate = config.clone();
    candidate
        .rules
        .iter_mut()
        .for_each(|rule| rule.enabled = true);
    let validation = compile_config_lenient(&candidate)?;
    let skipped: BTreeSet<_> = validation.skipped_rule_ids.into_iter().collect();
    let mut diagnostics_by_id: BTreeMap<String, Vec<Diagnostic>> = BTreeMap::new();
    for diagnostic in validation.diagnostics {
        if let Some(id) = diagnostic
            .path
            .strip_prefix("rules[")
            .and_then(|path| path.split(']').next())
            .and_then(|index| index.parse::<usize>().ok())
            .and_then(|index| raw.get("rules")?.get(index)?.get("id")?.as_str())
        {
            diagnostics_by_id
                .entry(id.into())
                .or_default()
                .push(diagnostic);
        }
    }
    for rule in &mut config.rules {
        let review = matches!(
            rule.action,
            Action::SendKeys { .. }
                | Action::Steps { .. }
                | Action::Replay { .. }
                | Action::Open { .. }
                | Action::Reusable { .. }
                | Action::Common { .. }
                | Action::Quicker { .. }
        );
        if skipped.contains(&rule.id) || review {
            let original = serde_json::to_value(&*rule)
                .map_err(|_| invalid("config_schema", "无法读取指令"))?;
            rule.enabled = false;
            let mut diagnostics = diagnostics_by_id.remove(&rule.id).unwrap_or_default();
            if review {
                let mut warning = Diagnostic::error(
                    "import_confirmation",
                    "导入的自动化动作已禁用，请审阅后手动启用",
                    SourceSpan::default(),
                );
                warning.severity = Severity::Warning;
                diagnostics.push(warning);
            }
            items.push(MigrationItem {
                rule_id: rule.id.clone(),
                status: if matches!(rule.action, Action::Quicker { .. }) {
                    MigrationStatus::NotPortable
                } else if skipped.contains(&rule.id) {
                    MigrationStatus::NeedsRewrite
                } else {
                    MigrationStatus::Compatible
                },
                diagnostics,
                original,
            });
        }
    }
    for entry in &config.unreadable_rules {
        items.push(MigrationItem {
            rule_id: entry.id().unwrap_or("未识别指令").into(),
            status: MigrationStatus::NeedsRewrite,
            diagnostics: invalid("rule_schema", "无法识别的指令已原样保留，不会运行"),
            original: entry.raw.clone(),
        });
    }
    Ok(ImportReport {
        config,
        items,
        unresolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_exports_import_with_unknown_fields_and_unreadable_rules() {
        let mut config = Config::default();
        config.enabled = false;
        config.globals.word_separators = "中文,\n".into();
        config
            .extra
            .insert("future".into(), serde_json::json!({"a":[1,true]}));
        config
            .globals
            .extra
            .insert("future_global".into(), serde_json::json!("保留"));
        let mut rule = Rule {
            id: "known".into(),
            ..Rule::default()
        };
        rule.match_spec.value = "001".into();
        rule.extra
            .insert("future_rule".into(), serde_json::json!({"n":4}));
        config.rules.push(rule);
        config.unreadable_rules.push(UnreadableRule {
            index: 0,
            raw: serde_json::json!({"id":"future","action":{"type":"future"}}),
        });
        let report = import(&export_package(&config).unwrap()).unwrap();
        assert_eq!(report.config, config);
    }

    #[test]
    fn native_import_rejects_newer_schema_bad_metadata_and_known_types() {
        for value in [
            serde_json::json!({"version":999,"rules":[]}),
            serde_json::json!({"enabled":"yes","rules":[]}),
            serde_json::json!({"format":"quicker-text","version":1,"config":{},"unknown":true}),
            serde_json::json!({"rules":[{"id":"x"},{"id":"x"}]}),
        ] {
            assert!(import(&value.to_string()).is_err());
        }
    }

    #[test]
    fn native_import_preserves_raw_ids_and_disables_automation_without_altering_original() {
        let mut config = Config::default();
        let mut rule = Rule {
            id: "automation".into(),
            action: Action::Common {
                function: "copy".into(),
            },
            ..Rule::default()
        };
        rule.match_spec.value = "copy".into();
        config.rules.push(rule);
        for raw in [
            serde_json::json!({"id":"","action":{"type":"future"}}),
            serde_json::json!({"id":"same","action":{"type":"future"}}),
            serde_json::json!({"id":"same","action":{"type":"future2"}}),
        ] {
            config
                .unreadable_rules
                .push(UnreadableRule { index: 0, raw });
        }
        let report = import(&export_package(&config).unwrap()).unwrap();
        assert!(!report.config.rules[0].enabled);
        assert_eq!(report.config.rules[0].action, config.rules[0].action);
        assert_eq!(report.items[0].original["enabled"], true);
        assert_eq!(
            serde_json::to_value(&report.config).unwrap()["rules"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
    }
}
