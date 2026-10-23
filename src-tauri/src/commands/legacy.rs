//! Commands marked deprecated in §7.4. 0.5 replaces them (`save_rule(s)`,
//! save-is-publish, `undo_last_change` / `restore_revision`, `pick_window`,
//! `export`, `import_preview` / `import_commit`) and keeps these names as
//! aliases that call the new implementations; 0.6 removes them.
use crate::{
    app_state::{AppState, Managed},
    commands::import_export::{dialog, CONFIG_FILTERS, IMPORT_FILTERS},
    error::{CommandError, IoOp},
    store::merge_import_metadata,
};
use quicker_domain::*;
use std::{fs, path::PathBuf};
use tauri::{Manager, State};

#[tauri::command]
pub fn save_draft(config: Config, store: State<Managed>) -> Result<AppState, CommandError> {
    let mut s = store.lock();
    s.save(config)?;
    Ok(s.state())
}
#[tauri::command]
pub fn apply_settings(store: State<Managed>) -> Result<AppState, CommandError> {
    let mut s = store.lock();
    let c = s.draft.clone();
    s.publish(c)?;
    Ok(s.state())
}
#[tauri::command]
pub fn discard_draft(store: State<Managed>) -> Result<AppState, CommandError> {
    let mut s = store.lock();
    s.discard()?;
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
/// directory (see `Store::export_draft`).
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
    store.lock().export_draft(&target)?;
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
    let imported = if excel {
        ImportReport {
            config: Config {
                rules: crate::excel::read(&path).map_err(CommandError::validation)?,
                ..Config::default()
            },
            items: vec![],
            unresolved: vec![],
        }
    } else {
        let source =
            fs::read_to_string(&path).map_err(|e| CommandError::io(IoOp::Read, Some(&path), &e))?;
        import_package(&source).map_err(CommandError::validation)?
    };
    let mut incoming = imported.config;
    let mut s = store.lock();
    let mut draft = s.draft.clone();
    let conflicts: Vec<_> = incoming
        .rules
        .iter()
        .filter(|r| draft.rules.iter().any(|x| x.id == r.id))
        .map(|r| r.id.clone())
        .collect();
    if excel {
        incoming = prepare_excel(&draft, incoming.rules, &strategy)?;
    }
    let mut report: Vec<String> = imported
        .items
        .iter()
        .map(|x| {
            format!(
                "{}: {:?} {}",
                x.rule_id,
                x.status,
                x.diagnostics
                    .iter()
                    .map(|d| d.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })
        .collect();
    report.push(format!(
        "{} 导入 {} 条指令；确认后仅写入草稿，应用设置后生效",
        if excel { "Excel" } else { "JSON" },
        incoming.rules.len()
    ));
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
        draft = merge_import(
            &draft,
            &ImportReport {
                config: incoming.clone(),
                items: vec![],
                unresolved: vec![],
            },
            ConflictStrategy::KeepBoth,
        )
        .map_err(CommandError::validation)?;
    } else if strategy == "replace" {
        incoming.revision = s.active.revision;
        draft = incoming.clone();
    } else {
        for r in incoming.rules.iter().cloned() {
            if let Some(i) = draft.rules.iter().position(|x| x.id == r.id) {
                if strategy == "overwrite" {
                    draft.rules[i] = r
                } else {
                    report.push(format!("{}：重复编号已跳过", r.id));
                }
            } else {
                draft.rules.push(r)
            }
        }
    }
    if excel {
        compile_config(&draft).map_err(CommandError::validation)?;
    }
    let unresolved = merge_import_metadata(
        &strategy,
        &s.draft,
        &incoming,
        &draft,
        &s.unresolved,
        &imported.unresolved,
    );
    s.pending_import = Some((draft.clone(), unresolved));
    Ok(Some(serde_json::json!({"config":draft,"report":report})))
}

/// Excel contributes rules only. Reserve the entire batch's IDs before rewriting
/// references, so a renamed rule cannot steal an unconflicted incoming ID.
fn prepare_excel(draft: &Config, rules: Vec<Rule>, strategy: &str) -> Result<Config, CommandError> {
    let mut incoming = Config {
        rules,
        unreadable_rules: vec![],
        ..draft.clone()
    };
    if strategy == "keep_both" {
        use std::collections::{BTreeMap, BTreeSet};
        let existing: BTreeSet<_> = draft.rules.iter().map(|r| r.id.clone()).collect();
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
    let mut validation = draft.clone();
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
        let draft = Config {
            rules: vec![rule("A", Action::None), rule("B", Action::None)],
            ..Config::default()
        };
        let incoming = prepare_excel(
            &draft,
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
            &draft,
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
        let draft = Config {
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
            &draft,
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
        let mut draft = Config::default();
        draft.enabled = false;
        draft.globals.word_separators = ",".into();
        draft
            .rules
            .push(rule("old-invalid", Action::Hotkey { keys: vec![] }));
        let incoming = prepare_excel(&draft, vec![rule("new", Action::None)], "replace").unwrap();
        assert_eq!(incoming.globals, draft.globals);
        assert_eq!(incoming.enabled, draft.enabled);
        assert_eq!(incoming.rules.len(), 1);
        assert!(incoming.unreadable_rules.is_empty());
        let mut invalid = rule("bad-disabled", Action::None);
        invalid.enabled = false;
        invalid.match_spec.value.clear();
        let error = prepare_excel(&draft, vec![invalid], "replace").unwrap_err();
        assert!(format!("{error:?}").contains("bad-disabled"));
        draft.rules[0].enabled = false;
        assert!(prepare_excel(&draft, vec![rule("new", Action::None)], "merge").is_ok());
    }
}
