use quicker_domain::*;
use serde_json::{json, Value};

fn command(id: &str, action: i32, data: &str) -> Value {
    json!({
        "Id": id, "CmdText": "synthetic-trigger", "UseRegex": false,
        "BindingProcessName": "", "Title": "Synthetic command", "ActionType": action,
        "Data": data, "ParamData": "", "Message": null, "IsDisabled": false,
        "LastUpdateTimeUtc": "2026-10-14T09:00:00Z", "UseBackspaceWhenImeOpen": false,
        "Group": "Synthetic", "TriggerKey": 0, "ExtractFirstMatchGroup": false,
        "IgnoreCase": false
    })
}
fn import(entries: Vec<Value>) -> ImportReport {
    import_package(&serde_json::to_string(&entries).unwrap()).unwrap()
}

#[test]
fn native_action_values_and_review_policy() {
    let report = import(vec![
        command("none", 0, ""),
        command("paste", 3, "sample🙂"),
        command("keys", 6, "sample"),
        command("sendkeys", 2, "^a"),
        command("script", 7, "input:sample"),
    ]);
    assert_eq!(report.config.rules.len(), 5);
    assert_eq!(report.items.len(), 5);
    assert!(report
        .items
        .iter()
        .all(|i| i.status == MigrationStatus::Compatible));
    assert!(matches!(report.config.rules[0].action, Action::None));
    assert!(matches!(
        report.config.rules[1].action,
        Action::Text {
            method: OutputMethod::Paste,
            ..
        }
    ));
    assert!(matches!(
        report.config.rules[2].action,
        Action::Text {
            method: OutputMethod::Keys,
            ..
        }
    ));
    assert!(matches!(
        report.config.rules[3].action,
        Action::SendKeys { .. }
    ));
    assert!(
        matches!(report.config.rules[4].action, Action::Steps { ref plan } if plan.dialect == Dialect::Inputscript)
    );
    assert!(report.config.rules[..3].iter().all(|r| r.enabled));
    assert!(report.config.rules[3..].iter().all(|r| !r.enabled));
    assert!(report.items[3..].iter().all(|i| i
        .diagnostics
        .iter()
        .any(|d| d.code == "import_confirmation")));
    compile_config(&report.config).unwrap();
}

#[test]
fn native_fields_and_trigger_semantics() {
    let mut entry = command("fields", 3, "sample");
    entry["TriggerKey"] = Value::Null;
    entry["CmdText"] = json!("synthetic (.*)");
    entry["UseRegex"] = json!(true);
    entry["IgnoreCase"] = json!(true);
    entry["BindingProcessName"] = json!(" test.exe ; other.exe ; ");
    entry["UseBackspaceWhenImeOpen"] = json!(true);
    entry["ExtractFirstMatchGroup"] = json!(true);
    entry["Message"] = json!("Synthetic notification");
    let report = import(vec![entry]);
    let rule = &report.config.rules[0];
    assert!(rule.enabled && rule.ignore_case);
    assert_eq!(rule.trigger_mode, TriggerMode::Terminator);
    assert!(rule.terminator.is_none());
    assert_eq!(rule.match_spec.kind, MatchKind::Regex);
    assert_eq!(rule.parameter, ParameterSource::FirstCapture);
    assert_eq!(rule.ime_erasure, Some(ImeErasure::Backspace));
    assert_eq!(rule.description, "Synthetic command");
    assert_eq!(rule.group, "Synthetic");
    assert!(rule.notification.enabled);
    assert_eq!(rule.notification.message, "Synthetic notification");
    let immediate = import(vec![command("immediate", 3, "sample")]);
    assert_eq!(
        immediate.config.rules[0].trigger_mode,
        TriggerMode::Immediate
    );
}

#[test]
fn unsafe_and_malformed_entries_are_preserved_disabled() {
    let mut unknown = command("unknown", 3, "sample");
    unknown["FutureOption"] = json!({"keep": true});
    let mut malformed = command("malformed", 3, "sample");
    malformed["IgnoreCase"] = json!("false");
    let mut key = command("key", 3, "sample");
    key["TriggerKey"] = json!(9876);
    let mut regex = command("regex", 3, "sample");
    regex["TriggerKey"] = Value::Null;
    regex["UseRegex"] = json!(true);
    regex["CmdText"] = json!("(?<=x)y");
    let originals = vec![
        unknown,
        malformed,
        key,
        regex,
        command("type", 12, "private"),
        command("file", 3, "FILE:synthetic.txt"),
        json!(42),
    ];
    let report = import(originals.clone());
    assert!(report.config.rules.iter().all(|r| !r.enabled));
    assert!(report
        .items
        .iter()
        .all(|i| i.status == MigrationStatus::NeedsRewrite));
    assert_eq!(
        report
            .items
            .iter()
            .map(|i| i.original.clone())
            .collect::<Vec<_>>(),
        originals
    );
    assert!(report.items[3]
        .diagnostics
        .iter()
        .any(|d| d.code == "regex_unsupported"));
    compile_config(&report.config).unwrap();
}

#[test]
fn supported_terminator_keys_and_disabled_source_flag() {
    for (key, name) in [(9, "tab"), (13, "enter"), (32, "space"), (186, "semicolon")] {
        let mut entry = command("key", 3, "sample");
        entry["TriggerKey"] = json!(key);
        entry["IsDisabled"] = json!(true);
        let report = import(vec![entry]);
        assert!(!report.config.rules[0].enabled);
        assert_eq!(report.items[0].status, MigrationStatus::Compatible);
        assert_eq!(report.config.rules[0].trigger_mode, TriggerMode::Terminator);
        assert_eq!(
            report.config.rules[0].terminator.as_ref().unwrap().keys,
            [name]
        );
    }
}

#[test]
fn invalid_script_and_parameter_conflict_require_rewrite() {
    let mut parameter = command("parameter", 3, "sample");
    parameter["ExtractFirstMatchGroup"] = json!(true);
    let report = import(vec![command("script", 7, "unknown:sample"), parameter]);
    assert!(report.config.rules.iter().all(|r| !r.enabled));
    assert!(report
        .items
        .iter()
        .all(|i| i.status == MigrationStatus::NeedsRewrite));
}

#[test]
fn text_templates_validate_date_formats_without_capturing_dynamic_values() {
    for action_type in [3, 6] {
        let supported = command(
            "supported",
            action_type,
            "{cliptext} {selection} {0:yyyy-MM-dd}",
        );
        let unsupported = command("unsupported", action_type, "{cliptext} {0:MMMM}");
        let missing_parameter = command("parameter", action_type, "{param:missing} {0:yyyy}");
        let report = import(vec![
            supported.clone(),
            unsupported.clone(),
            missing_parameter,
        ]);
        assert!(report.config.rules[0].enabled);
        assert_eq!(report.items[0].status, MigrationStatus::Compatible);
        assert!(!report.config.rules[1].enabled);
        assert_eq!(report.items[1].status, MigrationStatus::NeedsRewrite);
        assert!(report.items[1]
            .diagnostics
            .iter()
            .any(|d| d.code == "date_format"));
        assert_eq!(report.items[1].original, unsupported);
        assert!(
            matches!(&report.config.rules[0].action, Action::Text { source: ContentSource::Inline { value }, .. } if value == supported["Data"].as_str().unwrap())
        );
        assert!(!report.config.rules[2].enabled);
        assert!(report.items[2]
            .diagnostics
            .iter()
            .any(|d| d.code == "parameter_unavailable"));
    }
}

#[test]
fn duplicate_ids_are_rejected_even_if_a_row_is_malformed() {
    let mut malformed = command("duplicate", 3, "sample");
    malformed["Data"] = json!(3);
    let source =
        serde_json::to_string(&vec![command("duplicate", 3, "sample"), malformed]).unwrap();
    assert_eq!(import_package(&source).unwrap_err()[0].code, "rule_id");
}

#[test]
fn bom_and_all_original_metadata_survive_native_package_roundtrip() {
    let originals = vec![
        command("enabled", 3, "sample"),
        command("automation", 2, "^a"),
    ];
    let source = format!("\u{feff}{}", serde_json::to_string(&originals).unwrap());
    let report = import_package(&source).unwrap();
    assert!(report.config.rules[0].enabled);
    assert_eq!(report.unresolved.len(), 2);
    assert_eq!(report.unresolved[0].status, MigrationStatus::Compatible);
    let exported = export_package_with_unresolved(&report.config, report.unresolved).unwrap();
    let restored = import_package(&exported).unwrap();
    assert_eq!(
        restored
            .unresolved
            .iter()
            .map(|i| i.original.clone())
            .collect::<Vec<_>>(),
        originals
    );
    assert!(restored.config.rules[0].enabled);
}

#[test]
#[ignore = "Requires an explicitly supplied local native Quicker export; no private fixture is checked in"]
fn externally_supplied_quicker_sample() {
    let path = std::env::var("QUICKER_IMPORT_SAMPLE").expect("set QUICKER_IMPORT_SAMPLE");
    let source = std::fs::read_to_string(path).unwrap();
    let report = import_package(&source).unwrap();
    assert_eq!(report.config.rules.len(), 53);
    assert_eq!(report.config.rules.iter().filter(|r| r.enabled).count(), 51);
    assert_eq!(
        report
            .config
            .rules
            .iter()
            .filter(|r| matches!(
                r.action,
                Action::Text {
                    method: OutputMethod::Paste,
                    ..
                }
            ))
            .count(),
        50
    );
    assert_eq!(
        report
            .config
            .rules
            .iter()
            .filter(|r| matches!(
                r.action,
                Action::Text {
                    method: OutputMethod::Keys,
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(report
        .config
        .rules
        .iter()
        .all(|r| r.trigger_mode == TriggerMode::Immediate));
    assert!(report
        .items
        .iter()
        .all(|i| i.status == MigrationStatus::Compatible));
    compile_config(&report.config).unwrap();
    let exported =
        export_package_with_unresolved(&report.config, report.unresolved.clone()).unwrap();
    let restored = import_package(&exported).unwrap();
    assert_eq!(
        serde_json::to_value(restored.unresolved).unwrap(),
        serde_json::to_value(report.unresolved).unwrap()
    );
}

#[test]
fn removed_quicker_process_binding_is_metadata_only() {
    for binding in ["not-running.exe; other.exe", " ; ; "] {
        let mut original = command("bound", 3, "sample");
        original["BindingProcessName"] = json!(binding);
        let report = import(vec![original.clone()]);
        assert!(report.config.rules[0].enabled);
        assert_eq!(report.items[0].status, MigrationStatus::Compatible);
        assert!(report.items[0].diagnostics.iter().any(|d| d.code == "quicker_process_binding_ignored" && d.severity == Severity::Warning));
        let package = export_package_with_unresolved(&report.config, report.unresolved).unwrap();
        let reimported = import_package(&package).unwrap();
        assert_eq!(reimported.unresolved[0].original, original);
        assert!(reimported.config.rules[0].enabled);
        let compiler = compile_config(&reimported.config).unwrap();
        for foreground in [
            ForegroundContext { process: "test.exe".into(), ..Default::default() },
            ForegroundContext { platform: Platform::Macos, bundle_id: "com.test".into(), ..Default::default() },
        ] {
            assert!(compiler.match_input("synthetic-trigger", None, &foreground).is_some());
        }
    }
}
