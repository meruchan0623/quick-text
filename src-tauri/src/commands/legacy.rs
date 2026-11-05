//! Configuration confirmation, legacy rollback, and import/export commands.
use crate::{
    app_state::{AppState, Managed},
    commands::import_export::{dialog, CONFIG_FILTERS, IMPORT_FILTERS},
    error::{CommandError, IoOp},
    store::merge_import_metadata,
};
use quicker_domain::*;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
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
            .set_file_name("quicktext-config.zip")
            .save_file()
    });
    let Some(target) = target else {
        return Ok(None);
    };
    // Reading and packing the images happens outside the store lock; only the protected atomic
    // write holds it, as for Excel.
    let (snapshot, dir, active) = {
        let s = store.lock();
        (
            s.export_snapshot()?,
            s.dir().to_path_buf(),
            s.active.clone(),
        )
    };
    let bytes = crate::package_zip::build(&dir, &active, &snapshot)?;
    store.lock().export_bytes(&target, &bytes)?;
    Ok(Some(target.to_string_lossy().into_owned()))
}
/// Without `path`, asks for a package (ZIP or JSON) or Excel; `Ok(None)` when the user cancels.
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
    import_from(&path, &strategy, &store).map(Some)
}

/// What an import file turned out to be, by its content: a ZIP holds a package or is a workbook
/// (the extension only decides for a file that is neither ZIP nor JSON).
struct ImportSource {
    report: ImportReport,
    excel: bool,
    full_excel: bool,
    rules_only: bool,
    /// Verified images of a ZIP package, by name.
    assets: BTreeMap<String, Vec<u8>>,
    label: &'static str,
}

fn read_import_source(path: &Path) -> Result<ImportSource, CommandError> {
    let read_error = |e: std::io::Error| CommandError::io(IoOp::Read, Some(path), &e);
    let mut head = [0u8; 2];
    let mut file = fs::File::open(path).map_err(read_error)?;
    let zip = match file.read_exact(&mut head) {
        Ok(()) => head == *b"PK",
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => false,
        Err(e) => return Err(read_error(e)),
    };
    file.seek(SeekFrom::Start(0)).map_err(read_error)?;
    let named_xlsx = path
        .extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("xlsx"));
    let workbook = if zip {
        crate::package_zip::classify(&mut file).map_err(CommandError::validation)?
            == crate::package_zip::ZipKind::Workbook
    } else {
        named_xlsx
    };
    if workbook {
        let (report, full_excel) =
            crate::excel::read_config(path).map_err(CommandError::validation)?;
        return Ok(ImportSource {
            report,
            excel: true,
            full_excel,
            rules_only: false,
            assets: BTreeMap::new(),
            label: "Excel",
        });
    }
    file.seek(SeekFrom::Start(0)).map_err(read_error)?;
    let (source, assets, label) = if zip {
        let package = crate::package_zip::read(std::io::BufReader::new(file))
            .map_err(CommandError::validation)?;
        (package.json, package.assets, "配置包")
    } else {
        let mut source = String::new();
        file.take(16_777_217)
            .read_to_string(&mut source)
            .map_err(read_error)?;
        (source, BTreeMap::new(), "JSON")
    };
    let imported =
        crate::config_package::import_source(&source).map_err(CommandError::validation)?;
    Ok(ImportSource {
        report: imported.report,
        excel: false,
        full_excel: false,
        rules_only: imported.rules_only,
        assets,
        label,
    })
}

/// docs/image-and-zip-package.md §3: a rule whose image is neither among `packaged` nor in the
/// library still imports, disabled, with a warning in the report.
fn disable_rules_missing_assets(
    report: &mut ImportReport,
    packaged: &BTreeMap<String, Vec<u8>>,
    data_dir: &Path,
) {
    for rule in &mut report.config.rules {
        let Action::Image { asset, .. } = &rule.action else {
            continue;
        };
        if !is_asset_name(asset)
            || packaged.contains_key(asset)
            || crate::assets::contains(data_dir, asset)
        {
            continue;
        }
        let mut warning = Diagnostic::error(
            ASSET_MISSING,
            "图片不在配置包和资源库中，已停用；补齐图片后可手动启用",
            SourceSpan::default(),
        );
        warning.severity = Severity::Warning;
        let original = serde_json::to_value(&*rule).unwrap_or_default();
        rule.enabled = false;
        match report.items.iter_mut().find(|item| item.rule_id == rule.id) {
            Some(item) => item.diagnostics.push(warning),
            None => report.items.push(MigrationItem {
                rule_id: rule.id.clone(),
                status: MigrationStatus::Compatible,
                diagnostics: vec![warning],
                original,
            }),
        }
    }
}

fn import_from(
    path: &Path,
    strategy: &str,
    store: &Managed,
) -> Result<serde_json::Value, CommandError> {
    let ImportSource {
        report: mut imported,
        excel,
        full_excel,
        rules_only,
        assets,
        label,
    } = read_import_source(path)?;
    let data_dir = store.lock().dir().to_path_buf();
    disable_rules_missing_assets(&mut imported, &assets, &data_dir);
    let strategy = strategy.to_string();
    let mut incoming = imported.config;
    let s = store.lock();
    if rules_only {
        // A table has no settings of its own; "replace" replaces the rules only.
        incoming = Config {
            rules: incoming.rules,
            unreadable_rules: incoming.unreadable_rules,
            ..s.active.clone()
        };
    }
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
        label,
        incoming.rules.len()
    ));
    if rules_only {
        report.push("话术表不含全局设置，导入后保留当前全局设置".into());
    } else if !excel || full_excel {
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
    drop(s);
    // Every check has passed: only now do the images reach the library. Without the store lock,
    // since a package can hold thousands of files to sync. A preview that is then cancelled leaves
    // unused files, which the library allows (nothing refers to them; §2).
    for rule in &incoming.rules {
        if let Action::Image { asset, .. } = &rule.action {
            if let Some(bytes) = assets.get(asset) {
                crate::assets::write(&data_dir, asset, bytes)
                    .map_err(|e| CommandError::io(IoOp::Write, Some(Path::new(asset)), &e))?;
            }
        }
    }
    let token = store.lock().preview_import(candidate.clone(), unresolved);
    Ok(serde_json::json!({"config":candidate,"report":report,"token":token}))
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
        let mut candidate = Config {
            enabled: false,
            ..Config::default()
        };
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

#[cfg(test)]
mod package_import_tests {
    use super::*;
    use crate::store::Store;
    use std::io::Write;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage";

    struct World {
        data: PathBuf,
        store: Managed,
        files: PathBuf,
    }
    impl World {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("quicker-import-{}", uuid::Uuid::new_v4()));
            let data = root.join("data");
            let files = root.join("files");
            fs::create_dir_all(&files).unwrap();
            Self {
                store: Managed::new(Store::load(data.clone())),
                data,
                files,
            }
        }
        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.files.join(name);
            fs::write(&path, bytes).unwrap();
            path
        }
    }
    impl Drop for World {
        fn drop(&mut self) {
            if let Some(root) = self.files.parent() {
                let _ = fs::remove_dir_all(root);
            }
        }
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn image_rule(id: &str, asset: &str) -> Rule {
        Rule {
            id: id.into(),
            match_spec: MatchSpec {
                kind: MatchKind::Literal,
                value: format!("t{id}"),
            },
            action: Action::Image {
                asset: asset.into(),
                name: "码.png".into(),
            },
            ..Rule::default()
        }
    }

    fn package_of(rules: Vec<Rule>) -> String {
        export_package(&Config {
            rules,
            ..Config::default()
        })
        .unwrap()
    }

    fn rule_enabled(preview: &serde_json::Value, id: &str) -> bool {
        preview["config"]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| rule["id"] == id)
            .unwrap()["enabled"]
            .as_bool()
            .unwrap()
    }

    /// §3: the images of a package reach the library, the rule stays on, the file on disk is told
    /// by its content whatever it is called.
    #[test]
    fn a_zip_imports_rules_and_images_whatever_its_extension() {
        for name in ["package.zip", "renamed.json", "renamed.xlsx"] {
            let world = World::new();
            let asset = asset_name_for(PNG).unwrap();
            let json = package_of(vec![image_rule("a", &asset)]);
            let path = world.file(
                name,
                &zip_of(&[
                    ("package.json", json.as_bytes()),
                    (&format!("assets/{asset}"), PNG),
                ]),
            );
            let preview = import_from(&path, "merge", &world.store).unwrap();
            assert!(rule_enabled(&preview, "a"), "{name}");
            assert!(crate::assets::contains(&world.data, &asset), "{name}");
            assert!(preview["report"]
                .to_string()
                .contains("配置包 导入 1 条指令"));
        }
    }

    /// Legacy JSON still imports, also named differently.
    #[test]
    fn bare_json_still_imports() {
        let world = World::new();
        let json = package_of(vec![]);
        let path = world.file("old.json", json.as_bytes());
        import_from(&path, "merge", &world.store).unwrap();
        let path = world.file("named.zip", json.as_bytes());
        import_from(&path, "merge", &world.store).unwrap();
    }

    /// §3: a rule whose image is in neither place imports disabled, with a warning; the same rule
    /// stays on when the library already has it, in every source format.
    #[test]
    fn a_rule_without_its_image_imports_disabled_with_a_warning() {
        let world = World::new();
        let asset = asset_name_for(PNG).unwrap();
        let json = package_of(vec![image_rule("a", &asset)]);
        let zip = world.file("missing.zip", &zip_of(&[("package.json", json.as_bytes())]));
        let bare = world.file("missing.json", json.as_bytes());
        for path in [&zip, &bare] {
            let preview = import_from(path, "merge", &world.store).unwrap();
            assert!(!rule_enabled(&preview, "a"));
            let report = preview["report"].to_string();
            assert!(report.contains("图片不在配置包和资源库中"), "{report}");
        }
        crate::assets::write(&world.data, &asset, PNG).unwrap();
        for path in [&zip, &bare] {
            let preview = import_from(path, "merge", &world.store).unwrap();
            assert!(rule_enabled(&preview, "a"));
        }
    }

    /// §3: refused means nothing written, also when other images in it were fine.
    #[test]
    fn a_refused_package_writes_nothing() {
        let world = World::new();
        let good = asset_name_for(PNG).unwrap();
        let other = b"\x89PNG\r\n\x1a\nother";
        let wrong = format!("{}.png", "0".repeat(64));
        let json = package_of(vec![image_rule("a", &good)]);
        let cases = [
            zip_of(&[
                ("package.json", json.as_bytes()),
                (&format!("assets/{good}"), PNG),
                (&format!("assets/{wrong}"), other),
            ]),
            // Rejected by the JSON after every image passed.
            zip_of(&[
                ("package.json", b"{\"format\":\"unknown\"}"),
                (&format!("assets/{good}"), PNG),
            ]),
        ];
        for bytes in &cases {
            let path = world.file("bad.zip", bytes);
            assert!(import_from(&path, "merge", &world.store).is_err());
            assert!(!crate::assets::dir(&world.data).exists());
        }
    }

    /// §3: a phrase table from TSim in a ZIP. Replacing keeps the settings it does not have.
    #[test]
    fn a_table_package_keeps_the_settings() {
        let world = World::new();
        {
            let mut s = world.store.lock();
            let mut config = s.active.clone();
            config.globals.word_separators = ",".into();
            s.save_config(config, None).unwrap();
        }
        let asset = asset_name_for(PNG).unwrap();
        let table = serde_json::json!({
            "format": "quicktext-cloud-phrases", "schema_version": 1, "revision": 3,
            "updated_at": null,
            "rules": [
                serde_json::to_value(image_rule("a", &asset)).unwrap(),
                { "id": "raw", "action": { "type": "future" } }
            ]
        });
        let path = world.file(
            "table.zip",
            &zip_of(&[
                ("package.json", table.to_string().as_bytes()),
                (&format!("assets/{asset}"), PNG),
            ]),
        );
        let preview = import_from(&path, "replace", &world.store).unwrap();
        assert_eq!(preview["config"]["globals"]["word_separators"], ",");
        assert!(rule_enabled(&preview, "a"));
        assert!(preview["config"]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| rule["id"] == "raw"));
        assert!(crate::assets::contains(&world.data, &asset));
    }

    /// An export and an import through the file, with the image, into a fresh data directory.
    #[test]
    fn an_exported_package_imports_elsewhere_with_its_image() {
        let from = World::new();
        let asset = asset_name_for(PNG).unwrap();
        crate::assets::write(&from.data, &asset, PNG).unwrap();
        {
            let mut s = from.store.lock();
            let mut config = s.active.clone();
            config.rules.push(image_rule("a", &asset));
            s.save_config(config, None).unwrap();
        }
        let path = from.files.join("quicktext-config.zip");
        from.store.lock().export_active(&path).unwrap();
        let to = World::new();
        let preview = import_from(&path, "replace", &to.store).unwrap();
        assert!(rule_enabled(&preview, "a"));
        assert_eq!(
            fs::read(crate::assets::dir(&to.data).join(&asset)).unwrap(),
            PNG
        );
    }
}
