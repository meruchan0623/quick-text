//! Configuration confirmation, legacy rollback, and import/export commands.
use crate::{
    app_state::{AppState, Managed},
    commands::import_export::{dialog, CONFIG_FILTERS, IMPORT_FILTERS},
    error::{CommandError, IoOp},
    store::merge_import_metadata,
};
use quicker_domain::*;
use std::{fs, io::Read, path::PathBuf};
use tauri::{Manager, State};

#[tauri::command]
pub fn save_config(
    config: Config,
    import_token: Option<String>,
    store: State<Managed>,
) -> Result<AppState, CommandError> {
    let mut s = store.lock();
    s.save_config(config, import_token)?;
    Ok(s.state())
}
#[tauri::command]
pub fn rollback(store: State<Managed>) -> Result<AppState, CommandError> {
    let mut s = store.lock();
    s.rollback_previous()?;
    Ok(s.state())
}
#[tauri::command]
pub async fn get_foreground(
    app: tauri::AppHandle,
) -> Result<quicker_platform::Foreground, CommandError> {
    if let Some(w) = app.get_webview_window("main") {
        w.hide().map_err(CommandError::internal)?;
    }
    let result = tauri::async_runtime::spawn_blocking(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        quicker_platform::foreground()
    })
    .await
    .map_err(CommandError::task_failed)?;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
    Ok(result)
}
/// Returns `Ok(None)` when the user cancels the save dialog (B-42). Refused
/// in safe mode other than CrashLoop, and for targets inside the data
/// directory (see `Store::export_active`).
#[tauri::command]
pub fn export_config(
    path: Option<String>,
    store: State<Managed>,
) -> Result<Option<String>, CommandError> {
    // Pick the target before taking the store lock: the dialog is modal and
    // must not block other commands and the tray while it is open.
    let target = path.map(PathBuf::from).or_else(|| {
        dialog(CONFIG_FILTERS)
            .set_file_name("quicktext-package.json")
            .save_file()
    });
    let Some(target) = target else {
        return Ok(None);
    };
    store.lock().export_active(&target)?;
    Ok(Some(target.to_string_lossy().into_owned()))
}
/// Without `path`, asks for JSON or Excel; `Ok(None)` when the user cancels.
#[tauri::command]
pub fn import_config(
    path: Option<String>,
    strategy: String,
    store: State<Managed>,
) -> Result<Option<serde_json::Value>, CommandError> {
    let path = match path {
        Some(path) => PathBuf::from(path),
        None => match dialog(IMPORT_FILTERS).pick_file() {
            Some(path) => path,
            None => return Ok(None),
        },
    };
    let excel = path
        .extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("xlsx"));
    let (mut imported, full_excel) = if excel {
        crate::excel::read_config(&path).map_err(CommandError::validation)?
    } else {
        let mut source = String::new();
        fs::File::open(&path)
            .and_then(|file| file.take(16_777_217).read_to_string(&mut source))
            .map_err(|e| CommandError::io(IoOp::Read, Some(&path), &e))?;
        (
            crate::config_package::import(&source).map_err(CommandError::validation)?,
            false,
        )
    };
    let mut incoming = imported.config;
    let mut s = store.lock();
    let mut candidate = s.active.clone();
    let conflicts: Vec<_> = incoming
        .rules
        .iter()
        .filter(|r| candidate.rules.iter().any(|x| x.id == r.id))
        .map(|r| r.id.clone())
        .collect();
    if excel && !full_excel {
        incoming = prepare_excel(&candidate, incoming.rules, &strategy)?;
    }
    let rules_by_id: std::collections::BTreeMap<_, _> = incoming
        .rules
        .iter()
        .map(|rule| (rule.id.as_str(), rule))
        .collect();
    let mut report: Vec<String> = imported
        .items
        .iter()
        .map(|item| import_item_summary(item, rules_by_id.get(item.rule_id.as_str()).copied()))
        .collect();
    report.push(format!(
        "{} 导入 {} 条指令；确认后保存并立即生效",
        if excel { "Excel" } else { "JSON" },
        incoming.rules.len()
    ));
    if !excel || full_excel {
        report.push(if strategy == "replace" {
            "完整替换将恢复导出时的全局设置，并保留未知字段及未识别指令".into()
        } else {
            "此策略保留当前全局设置；导入指令及其未知字段、未识别指令".into()
        });
    }
    for r in &mut incoming.rules {
        if disable_imported_action(r, excel) {
            report.push(format!("{}：含副作用的动作已禁用，请审阅后手动启用", r.id));
        }
    }
    report.push(format!(
        "重复编号 {} 个：{}；处理策略：{}",
        conflicts.len(),
        conflicts.join(", "),
        strategy
    ));
    if strategy == "keep_both" {
        if !excel || full_excel {
            let renamed = rename_import_conflicts(&candidate, &mut incoming);
            for item in &mut imported.unresolved {
                if let Some(id) = renamed.get(&item.rule_id) {
                    item.rule_id = id.clone();
                }
            }
        }
        candidate = merge_import(
            &candidate,
            &ImportReport {
                config: incoming.clone(),
                items: vec![],
                unresolved: vec![],
            },
            ConflictStrategy::KeepBoth,
        )
        .map_err(CommandError::validation)?;
        merge_unreadable_rules(&s.active, &mut candidate, &incoming, &strategy, &mut report);
    } else if strategy == "replace" {
        incoming.revision = s.active.revision;
        candidate = incoming.clone();
    } else {
        merge_readable_rules(&mut candidate, &incoming, &strategy, &mut report);
        merge_unreadable_rules(&s.active, &mut candidate, &incoming, &strategy, &mut report);
    }
    if excel {
        compile_config(&candidate).map_err(CommandError::validation)?;
    }
    let unresolved = merge_import_metadata(
        &strategy,
        &s.active,
        &incoming,
        &candidate,
        &s.active_unresolved,
        &imported.unresolved,
    );
    candidate.revision = s.active.revision;
    let token = s.preview_import(candidate.clone(), unresolved);
    Ok(Some(
        serde_json::json!({"config":candidate,"report":report,"token":token}),
    ))
}

fn import_item_summary(item: &MigrationItem, rule: Option<&Rule>) -> String {
    let name = rule
        .map(|rule| rule.match_spec.value.as_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(&item.rule_id);
    let status = match item.status {
        MigrationStatus::Compatible if rule.is_some_and(|rule| rule.enabled) => "已转换",
        MigrationStatus::Compatible => "已转换，当前停用",
        MigrationStatus::NeedsRewrite => "需调整，已停用",
        MigrationStatus::NotPortable => "无法直接兼容，已停用",
    };
    let reasons = item
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect::<Vec<_>>()
        .join("；");
    format!(
        "指令「{name}」：{status}{}",
        if reasons.is_empty() {
            String::new()
        } else {
            format!("；{reasons}")
        }
    )
}

fn merge_readable_rules(
    candidate: &mut Config,
    incoming: &Config,
    strategy: &str,
    report: &mut Vec<String>,
) {
    for r in incoming.rules.iter().cloned() {
        if candidate
            .unreadable_rules
            .iter()
            .any(|entry| entry.id() == Some(r.id.as_str()))
        {
            if strategy == "overwrite" {
                candidate
                    .unreadable_rules
                    .retain(|entry| entry.id() != Some(r.id.as_str()));
            } else {
                report.push(format!("{}：重复编号已跳过", r.id));
                continue;
            }
        }
        if let Some(index) = candidate.rules.iter().position(|rule| rule.id == r.id) {
            if strategy == "overwrite" {
                candidate.rules[index] = r;
            } else {
                report.push(format!("{}：重复编号已跳过", r.id));
            }
        } else {
            candidate.rules.push(r);
        }
    }
}

fn rename_import_conflicts(
    candidate: &Config,
    incoming: &mut Config,
) -> std::collections::BTreeMap<String, String> {
    use std::collections::{BTreeMap, BTreeSet};
    let ids = |config: &Config| {
        config
            .rules
            .iter()
            .map(|rule| rule.id.clone())
            .chain(
                config
                    .unreadable_rules
                    .iter()
                    .filter_map(|rule| rule.id().map(str::to_owned)),
            )
            .collect::<BTreeSet<_>>()
    };
    let existing = ids(candidate);
    let mut reserved = existing.clone();
    reserved.extend(ids(incoming));
    let mut renamed = BTreeMap::new();
    for old in ids(incoming) {
        if !existing.contains(&old) {
            continue;
        }
        let mut suffix = 2;
        let new = loop {
            let candidate = format!("{old}-{suffix}");
            if reserved.insert(candidate.clone()) {
                break candidate;
            }
            suffix += 1;
        };
        renamed.insert(old, new);
    }
    for rule in &mut incoming.rules {
        if let Some(id) = renamed.get(&rule.id) {
            rule.id = id.clone();
        }
        match &mut rule.action {
            Action::Reusable { action_id, .. } => {
                if let Some(id) = renamed.get(action_id) {
                    *action_id = id.clone();
                }
            }
            Action::Steps { plan } | Action::Replay { plan, .. } => {
                for step in &mut plan.steps {
                    if let Operation::Reusable { action_id, .. } = &mut step.operation {
                        if let Some(id) = renamed.get(action_id) {
                            *action_id = id.clone();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for entry in &mut incoming.unreadable_rules {
        if let Some(id) = entry.id().and_then(|id| renamed.get(id)).cloned() {
            entry.raw["id"] = serde_json::json!(id);
        }
    }
    renamed
}

fn merge_unreadable_rules(
    existing: &Config,
    candidate: &mut Config,
    incoming: &Config,
    strategy: &str,
    report: &mut Vec<String>,
) {
    use std::collections::BTreeSet;
    let existing_ids: BTreeSet<_> = existing
        .rules
        .iter()
        .map(|rule| rule.id.as_str())
        .chain(
            existing
                .unreadable_rules
                .iter()
                .filter_map(UnreadableRule::id),
        )
        .collect();
    let mut replaced = BTreeSet::new();
    for unreadable in &incoming.unreadable_rules {
        if let Some(id) = unreadable.id() {
            if existing_ids.contains(id) && strategy != "keep_both" {
                if strategy != "overwrite" {
                    report.push(format!("{id}：重复的未识别指令已跳过"));
                    continue;
                }
                if replaced.insert(id) {
                    if !incoming.rules.iter().any(|rule| rule.id == id) {
                        candidate.rules.retain(|rule| rule.id != id);
                    }
                    candidate
                        .unreadable_rules
                        .retain(|entry| entry.id() != Some(id));
                }
            }
        }
        let mut entry = unreadable.clone();
        entry.index = candidate.rules.len() + candidate.unreadable_rules.len();
        candidate.unreadable_rules.push(entry);
    }
}

/// Excel contributes rules only. Reserve the entire batch's IDs before rewriting
/// references, so a renamed rule cannot steal an unconflicted incoming ID.
fn prepare_excel(
    candidate: &Config,
    rules: Vec<Rule>,
    strategy: &str,
) -> Result<Config, CommandError> {
    let mut incoming = Config {
        rules,
        unreadable_rules: vec![],
        ..candidate.clone()
    };
    if strategy == "keep_both" {
        use std::collections::{BTreeMap, BTreeSet};
        let existing: BTreeSet<_> = candidate.rules.iter().map(|r| r.id.clone()).collect();
        let mut reserved = existing.clone();
        reserved.extend(incoming.rules.iter().map(|r| r.id.clone()));
        let mut renamed = BTreeMap::new();
        for rule in &mut incoming.rules {
            if existing.contains(&rule.id) {
                let old = rule.id.clone();
                let mut suffix = 2;
                let new = loop {
                    let candidate = format!("{old}-{suffix}");
                    if reserved.insert(candidate.clone()) {
                        break candidate;
                    }
                    suffix += 1;
                };
                renamed.insert(old, new.clone());
                rule.id = new;
            }
        }
        for rule in &mut incoming.rules {
            match &mut rule.action {
                Action::Reusable { action_id, .. } => {
                    if let Some(new) = renamed.get(action_id) {
                        *action_id = new.clone();
                    }
                }
                Action::Steps { plan } | Action::Replay { plan, .. } => {
                    for step in &mut plan.steps {
                        if let Operation::Reusable { action_id, .. } = &mut step.operation {
                            if let Some(new) = renamed.get(action_id) {
                                *action_id = new.clone();
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // Validate spreadsheet actions even when their requested enabled value is false.
    // Existing disabled legacy actions keep their original state.
    let mut validation = candidate.clone();
    if strategy == "replace" {
        validation.rules.clear();
        validation.unreadable_rules.clear();
    }
    validation
        .rules
        .retain(|r| !incoming.rules.iter().any(|x| x.id == r.id));
    validation
        .rules
        .extend(incoming.rules.iter().cloned().map(|mut r| {
            r.enabled = true;
            r
        }));
    compile_config(&validation).map_err(|mut diagnostics| {
        for d in &mut diagnostics {
            if let Some(index) = d
                .path
                .strip_prefix("rules[")
                .and_then(|p| p.split(']').next())
                .and_then(|p| p.parse::<usize>().ok())
            {
                if let Some(rule) = validation.rules.get(index) {
                    d.message = format!("指令「{}」：{}", rule.id, d.message);
                }
            }
        }
        CommandError::validation(diagnostics)
    })?;
    Ok(incoming)
}

fn disable_imported_action(rule: &mut Rule, excel: bool) -> bool {
    let review = matches!(
        rule.action,
        Action::Quicker { .. }
            | Action::Reusable { .. }
            | Action::Open { .. }
            | Action::Steps { .. }
            | Action::Replay { .. }
            | Action::SendKeys { .. }
    ) || (excel
        && matches!(rule.action, Action::Hotkey { .. } | Action::Common { .. }));
    if review {
        rule.enabled = false;
    }
    review
}

#[cfg(test)]
mod excel_preview_tests {
    use super::*;

    #[test]
    fn quicker_json_reaches_preview_with_preserved_original_and_chinese_reason() {
        let originals = serde_json::json!([
            {"Id":"hotkey", "CmdText":"hk", "ActionType":1, "Data":"2|65", "TriggerKey":0},
            {"Id":"private", "CmdText":"private-action", "ActionType":12, "Data":"original-action-id", "TriggerKey":0}
        ]);
        let imported = crate::config_package::import(&originals.to_string()).unwrap();
        assert_eq!(imported.config.rules.len(), 2);
        assert!(matches!(
            imported.config.rules[0].action,
            Action::Hotkey { .. }
        ));
        assert!(imported.config.rules.iter().all(|rule| !rule.enabled));
        assert_eq!(imported.unresolved[1].original, originals[1]);
        let summary = import_item_summary(&imported.items[1], imported.config.rules.get(1));
        assert!(summary.contains("private-action"));
        assert!(summary.contains("Quicker"));
        assert!(summary.contains("已停用"));
        assert!(summary.contains("运行时"));
    }

    #[test]
    fn import_summary_explains_conversion_without_claiming_activation() {
        let config = Config {
            rules: vec![Rule {
                id: "source-id".into(),
                enabled: false,
                match_spec: MatchSpec {
                    kind: MatchKind::Literal,
                    value: "hhhh".into(),
                },
                ..Rule::default()
            }],
            ..Config::default()
        };
        let mut item = MigrationItem {
            rule_id: "source-id".into(),
            status: MigrationStatus::Compatible,
            diagnostics: vec![],
            original: serde_json::Value::Null,
        };
        assert_eq!(
            import_item_summary(&item, config.rules.first()),
            "指令「hhhh」：已转换，当前停用"
        );
        item.status = MigrationStatus::NotPortable;
        item.diagnostics.push(Diagnostic::error(
            "quicker_action",
            "需要原版 Quicker 动作运行环境",
            SourceSpan::default(),
        ));
        assert_eq!(
            import_item_summary(&item, config.rules.first()),
            "指令「hhhh」：无法直接兼容，已停用；需要原版 Quicker 动作运行环境"
        );
        item.status = MigrationStatus::NeedsRewrite;
        assert!(import_item_summary(&item, config.rules.first()).contains("需调整，已停用"));
        assert!(import_item_summary(&item, None).contains("source-id"));
    }

    fn rule(id: &str, action: Action) -> Rule {
        Rule {
            id: id.into(),
            match_spec: MatchSpec {
                value: format!("trigger-{id}"),
                ..MatchSpec::default()
            },
            action,
            ..Rule::default()
        }
    }
    fn reusable(id: &str) -> Action {
        Action::Reusable {
            action_id: id.into(),
            parameters: Default::default(),
        }
    }

    #[test]
    fn keep_both_remaps_batch_refs_and_reserves_unconflicted_ids() {
        let candidate = Config {
            rules: vec![rule("A", Action::None), rule("B", Action::None)],
            ..Config::default()
        };
        let incoming = prepare_excel(
            &candidate,
            vec![
                rule("A", Action::None),
                rule("B", reusable("A")),
                rule("A-2", reusable("B")),
            ],
            "keep_both",
        )
        .unwrap();
        assert_eq!(
            incoming
                .rules
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["A-3", "B-2", "A-2"]
        );
        assert_eq!(incoming.rules[1].action, reusable("A-3"));
        assert_eq!(incoming.rules[2].action, reusable("B-2"));
        let merged = merge_import(
            &candidate,
            &ImportReport {
                config: incoming,
                items: vec![],
                unresolved: vec![],
            },
            ConflictStrategy::KeepBoth,
        )
        .unwrap();
        assert_eq!(merged.rules[0].id, "A");
        assert_eq!(merged.rules[3].action, reusable("A-3"));
    }

    #[test]
    fn keep_both_rewrites_steps_and_replay_but_preserves_external_refs() {
        let candidate = Config {
            rules: vec![rule("A", Action::None)],
            ..Config::default()
        };
        let plan = ActionPlan {
            steps: vec![
                Step::new(
                    Operation::Reusable {
                        action_id: "A".into(),
                        parameters: Default::default(),
                    },
                    SourceSpan::default(),
                ),
                Step::new(
                    Operation::Reusable {
                        action_id: "external-uuid".into(),
                        parameters: Default::default(),
                    },
                    SourceSpan::default(),
                ),
            ],
            ..ActionPlan::default()
        };
        let incoming = prepare_excel(
            &candidate,
            vec![
                rule("A", Action::None),
                rule("steps", Action::Steps { plan: plan.clone() }),
                rule("replay", Action::Replay { plan, speed: 1.0 }),
                rule("external", reusable("external-uuid")),
            ],
            "keep_both",
        )
        .unwrap();
        for rule in &incoming.rules[1..3] {
            let plan = match &rule.action {
                Action::Steps { plan } | Action::Replay { plan, .. } => plan,
                _ => panic!("expected plan"),
            };
            assert!(
                matches!(&plan.steps[0].operation, Operation::Reusable { action_id, .. } if action_id == "A-2")
            );
            assert!(
                matches!(&plan.steps[1].operation, Operation::Reusable { action_id, .. } if action_id == "external-uuid")
            );
        }
        assert_eq!(incoming.rules[3].action, reusable("external-uuid"));
    }

    #[test]
    fn unconflicted_batch_keeps_reusable_ids() {
        let incoming = prepare_excel(
            &Config::default(),
            vec![rule("A", Action::None), rule("B", reusable("A"))],
            "keep_both",
        )
        .unwrap();
        assert_eq!(incoming.rules[1].action, reusable("A"));
    }

    #[test]
    fn excel_reviews_hotkeys_and_common_without_changing_json_behavior() {
        for action in [
            Action::Hotkey { keys: vec![] },
            Action::Common {
                function: "copy".into(),
            },
        ] {
            let mut excel = rule("r", action.clone());
            assert!(disable_imported_action(&mut excel, true));
            assert!(!excel.enabled);
            let mut json = rule("r", action);
            assert!(!disable_imported_action(&mut json, false));
            assert!(json.enabled);
        }
    }

    #[test]
    fn replace_keeps_globals_clears_old_rules_and_validates_disabled_rows() {
        let mut candidate = Config::default();
        candidate.enabled = false;
        candidate.globals.word_separators = ",".into();
        candidate
            .rules
            .push(rule("old-invalid", Action::Hotkey { keys: vec![] }));
        let incoming =
            prepare_excel(&candidate, vec![rule("new", Action::None)], "replace").unwrap();
        assert_eq!(incoming.globals, candidate.globals);
        assert_eq!(incoming.enabled, candidate.enabled);
        assert_eq!(incoming.rules.len(), 1);
        assert!(incoming.unreadable_rules.is_empty());
        let mut invalid = rule("bad-disabled", Action::None);
        invalid.enabled = false;
        invalid.match_spec.value.clear();
        let error = prepare_excel(&candidate, vec![invalid], "replace").unwrap_err();
        assert!(format!("{error:?}").contains("bad-disabled"));
        candidate.rules[0].enabled = false;
        assert!(prepare_excel(&candidate, vec![rule("new", Action::None)], "merge").is_ok());
    }

    #[test]
    fn native_keep_both_reserves_unreadable_ids_and_preserves_duplicate_raw_entries() {
        let unreadable = |id: &str, value| UnreadableRule {
            index: 0,
            raw: serde_json::json!({"id":id,"action":{"type":"future","value":value}}),
        };
        let existing = Config {
            rules: vec![rule("known", Action::None)],
            unreadable_rules: vec![unreadable("raw", 0)],
            ..Config::default()
        };
        let mut incoming = Config {
            rules: vec![
                rule("raw-2", reusable("known")),
                rule("known", Action::None),
            ],
            unreadable_rules: vec![unreadable("raw", 1), unreadable("raw", 2)],
            ..Config::default()
        };
        let renamed = rename_import_conflicts(&existing, &mut incoming);
        assert_eq!(renamed["raw"], "raw-3");
        assert_eq!(renamed["known"], "known-2");
        assert_eq!(incoming.rules[0].action, reusable("known-2"));
        let mut merged = existing.clone();
        merged.rules.extend(incoming.rules.clone());
        merge_unreadable_rules(&existing, &mut merged, &incoming, "keep_both", &mut vec![]);
        assert_eq!(merged.unreadable_rules.len(), 3);
        assert_eq!(merged.unreadable_rules[1].raw["action"]["value"], 1);
        assert_eq!(merged.unreadable_rules[2].raw["action"]["value"], 2);
        let mut metadata = vec![MigrationItem {
            rule_id: "raw".into(),
            status: MigrationStatus::NeedsRewrite,
            diagnostics: vec![],
            original: serde_json::json!({"raw":"keep"}),
        }];
        metadata[0].rule_id = renamed[&metadata[0].rule_id].clone();
        let result =
            merge_import_metadata("keep_both", &existing, &incoming, &merged, &[], &metadata);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].rule_id, "raw-3");
        assert_eq!(result[0].original, metadata[0].original);
    }

    #[test]
    fn native_merge_and_overwrite_keep_all_accepted_unreadable_entries() {
        let raw = |value| UnreadableRule {
            index: 0,
            raw: serde_json::json!({"id":"shared","action":{"type":"future","n":value}}),
        };
        let existing = Config {
            rules: vec![rule("shared", Action::None)],
            ..Config::default()
        };
        let incoming = Config {
            unreadable_rules: vec![raw(1), raw(2)],
            ..Config::default()
        };
        let mut merged = existing.clone();
        merge_unreadable_rules(&existing, &mut merged, &incoming, "merge", &mut vec![]);
        assert_eq!(merged.rules.len(), 1);
        assert!(merged.unreadable_rules.is_empty());
        merge_unreadable_rules(&existing, &mut merged, &incoming, "overwrite", &mut vec![]);
        assert!(merged.rules.is_empty());
        assert_eq!(merged.unreadable_rules.len(), 2);
        let mut merged = Config::default();
        merge_unreadable_rules(
            &Config::default(),
            &mut merged,
            &incoming,
            "merge",
            &mut vec![],
        );
        assert_eq!(merged.unreadable_rules.len(), 2);
        let existing_raw = Config {
            unreadable_rules: vec![raw(1), raw(2)],
            ..Config::default()
        };
        let incoming_known = Config {
            rules: vec![rule("shared", Action::None)],
            ..Config::default()
        };
        let mut merged = existing_raw.clone();
        merge_readable_rules(&mut merged, &incoming_known, "merge", &mut vec![]);
        assert!(merged.rules.is_empty());
        assert_eq!(merged.unreadable_rules.len(), 2);
        merge_readable_rules(&mut merged, &incoming_known, "overwrite", &mut vec![]);
        assert_eq!(merged.rules.len(), 1);
        assert!(merged.unreadable_rules.is_empty());
    }
}
