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
        assert_eq!(
            report.items[0].status,
            if matches!(key, 9 | 13) {
                MigrationStatus::NeedsRewrite
            } else {
                MigrationStatus::Compatible
            }
        );
        assert_eq!(report.config.rules[0].trigger_mode, TriggerMode::Terminator);
        assert_eq!(
            report.config.rules[0].terminator.as_ref().unwrap().keys,
            [name]
        );
    }
}

#[test]
fn native_hotkey_encoding_preserves_windows_keys_and_requires_review() {
    let originals = vec![
        command("ctrl-a", 1, "2|65"),
        command("all-modifiers", 1, "15|112"),
    ];
    let report = import(originals.clone());
    assert!(
        matches!(&report.config.rules[0].action, Action::Hotkey { keys } if keys == &vec![Key::named("CTRL"), Key::WindowsCode { code: 65 }])
    );
    assert!(matches!(&report.config.rules[1].action, Action::Hotkey { keys } if keys.len() == 5));
    for (index, item) in report.items.iter().enumerate() {
        assert_eq!(item.status, MigrationStatus::Compatible);
        assert!(!report.config.rules[index].enabled);
        assert_eq!(item.original, originals[index]);
        assert!(item
            .diagnostics
            .iter()
            .any(|d| d.code == "import_confirmation"));
        assert!(item
            .diagnostics
            .iter()
            .any(|d| d.code == "quicker_windows_keycode" && d.severity == Severity::Warning));
    }
    for data in ["Ctrl+A", "16|65", "2|0", "2|256", "2|65|66", "-1|65"] {
        let report = import(vec![command("invalid", 1, data)]);
        assert_eq!(report.items[0].status, MigrationStatus::NeedsRewrite);
        assert!(!report.config.rules[0].enabled);
        assert!(report.items[0]
            .diagnostics
            .iter()
            .any(|d| d.code == "quicker_hotkey"));
    }
}

#[test]
fn common_operations_use_verified_exact_keys_only() {
    for (data, function) in [
        ("windows_volume_up", "volume_up"),
        ("windows_volume_down", "volume_down"),
        ("windows_volume_mute", "volume_mute"),
        ("windows_media_next_track", "media_next"),
        ("windows_media_prev_track", "media_previous"),
        ("windows_media_play_pause", "media_play_pause"),
        ("windows_browser_back", "browser_back"),
        ("windows_browser_forward", "browser_forward"),
        ("operation_copy", "copy"),
        ("operation_paste", "paste"),
    ] {
        let report = import(vec![command("common", 11, data)]);
        assert!(
            matches!(&report.config.rules[0].action, Action::Common { function: actual } if actual == function)
        );
        assert_eq!(report.items[0].status, MigrationStatus::Compatible);
        assert!(!report.config.rules[0].enabled);
    }
    for data in [
        "windows_media_stop",
        "windows_browser_refresh",
        "windows_window_close_tab",
    ] {
        let report = import(vec![command("hotkey", 11, data)]);
        assert!(matches!(
            &report.config.rules[0].action,
            Action::Hotkey { .. }
        ));
        assert!(!report.config.rules[0].enabled);
        assert_eq!(report.items[0].status, MigrationStatus::Compatible);
        assert!(report.items[0]
            .diagnostics
            .iter()
            .any(|d| d.code == "quicker_windows_operation"));
    }
    for (data, reason) in [
        ("quicker_show_search", "私有运行时"),
        ("windows_ime_zh", "输入法"),
        ("monitor_brightness_increase", "显示器亮度"),
        ("windows_window_max", "窗口"),
        ("operation_copy_or_paste", "分支状态"),
        ("copy", "未在已核实"),
    ] {
        let report = import(vec![command("unmapped", 11, data)]);
        assert_eq!(report.items[0].status, MigrationStatus::NeedsRewrite);
        assert!(report.items[0]
            .diagnostics
            .iter()
            .any(|d| d.code == "quicker_common" && d.message.contains(reason)));
    }
}

#[test]
fn replay_uses_documented_text_and_default_speed_without_execution() {
    let original = command("replay", 13, "0;KP;#65;\n10;MV;None,100,200,0;\n0;DL;;");
    let report = import(vec![original.clone()]);
    assert!(
        matches!(&report.config.rules[0].action, Action::Replay { plan, speed } if *speed == 1.0 && plan.dialect == Dialect::Replay && !plan.steps.is_empty())
    );
    assert_eq!(report.items[0].status, MigrationStatus::Compatible);
    assert!(!report.config.rules[0].enabled);
    assert_eq!(report.items[0].original, original);
    let invalid = import(vec![command("invalid", 13, "0;UNKNOWN;data;")]);
    assert_eq!(invalid.items[0].status, MigrationStatus::NeedsRewrite);
    assert!(invalid.items[0]
        .diagnostics
        .iter()
        .any(|d| d.code == "replay" && d.message.contains("键鼠回放")));
}

#[test]
fn file_sources_and_html_are_reconstructed_but_require_explicit_completion() {
    for action_type in [2, 3, 6] {
        let original = command(
            "file",
            action_type,
            "file:C:\\missing-private-synthetic.txt",
        );
        let report = import(vec![original.clone()]);
        let source = match &report.config.rules[0].action {
            Action::Text { source, .. } | Action::SendKeys { source, .. } => source,
            _ => panic!("file source action was lost"),
        };
        assert!(
            matches!(source, ContentSource::File { path, encoding: Encoding::Utf8 } if path == "C:\\missing-private-synthetic.txt")
        );
        assert_eq!(report.items[0].status, MigrationStatus::NeedsRewrite);
        assert!(!report.config.rules[0].enabled);
        assert!(report.items[0]
            .diagnostics
            .iter()
            .any(|d| d.code == "quicker_file_source" && d.message.contains("编码")));
        assert_eq!(report.items[0].original, original);
    }
    let report = import(vec![command("html", 4, "<b>sample &amp; content</b>")]);
    assert!(
        matches!(&report.config.rules[0].action, Action::Html { html, plain_text } if html == "<b>sample &amp; content</b>" && plain_text.is_empty())
    );
    assert_eq!(report.items[0].status, MigrationStatus::NeedsRewrite);
    assert!(!report.config.rules[0].enabled);
    assert!(report.items[0]
        .diagnostics
        .iter()
        .any(|d| d.message.contains("纯文本回退")));
}

#[test]
fn open_maps_static_targets_but_never_guesses_arguments_or_runs_scripts() {
    for target in [
        "https://example.com/synthetic",
        "https://example.com/search?q=%E4%BD%A0",
        "C:\\100% synthetic.txt",
        "C:\\Program Files\\Synthetic\\app.exe",
        "C:\\synthetic document.txt",
        "C:\\synthetic\\",
    ] {
        let report = import(vec![command("open", 5, target)]);
        assert!(
            matches!(&report.config.rules[0].action, Action::Open { target: actual, args, working_directory: None } if actual == target && args.is_empty())
        );
        assert_eq!(report.items[0].status, MigrationStatus::Compatible);
        assert!(!report.config.rules[0].enabled);
    }
    for (data, code) in [
        (
            "C:\\app.exe\n--flag \"two words\"",
            "quicker_open_arguments",
        ),
        ("%LOCALAPPDATA%\\app.exe", "quicker_open_dynamic"),
        ("https://example.com/{cliptext}", "quicker_open_dynamic"),
        ("C:\\run.ps1", "quicker_open_target"),
        ("C:\\Windows\\cmd.exe", "quicker_open_target"),
        ("C:\\something.lnk", "quicker_open_target"),
        ("javascript:alert(1)", "quicker_open_target"),
        ("https://", "quicker_open_target"),
        ("app.exe", "quicker_open_target"),
    ] {
        let original = command("unmapped", 5, data);
        let report = import(vec![original.clone()]);
        assert_eq!(report.items[0].status, MigrationStatus::NeedsRewrite);
        assert!(!report.config.rules[0].enabled);
        assert!(matches!(report.config.rules[0].action, Action::None));
        assert!(report.items[0].diagnostics.iter().any(|d| d.code == code));
        assert_eq!(report.items[0].original, original);
    }
}

#[test]
fn private_and_unverified_types_have_distinct_chinese_reasons_and_roundtrip() {
    let types = [
        (12, "私有运行时"),
        (21, "轮盘"),
        (22, "手势"),
        (23, "面板"),
        (24, "截图"),
        (25, "窗口"),
        (100, "全局"),
        (999, "999"),
    ];
    let originals: Vec<_> = types
        .iter()
        .map(|(action, _)| command(&format!("type-{action}"), *action, "synthetic-native-data"))
        .collect();
    let report = import(originals.clone());
    for (index, (_, reason)) in types.iter().enumerate() {
        assert_eq!(report.items[index].status, MigrationStatus::NeedsRewrite);
        assert!(!report.config.rules[index].enabled);
        assert!(report.items[index]
            .diagnostics
            .iter()
            .any(|d| d.message.contains(reason)));
    }
    let exported = export_package_with_unresolved(&report.config, report.unresolved).unwrap();
    let restored = import_package(&exported).unwrap();
    assert_eq!(
        restored
            .unresolved
            .iter()
            .map(|item| item.original.clone())
            .collect::<Vec<_>>(),
        originals
    );
}

#[test]
fn dynamic_inputscript_is_not_silently_treated_as_literal_steps() {
    for data in [
        "input:{cliptext}",
        "input:{selection}",
        "input:{0:yyyy}",
        "FILE:C:\\synthetic.txt",
    ] {
        let report = import(vec![command("dynamic", 7, data)]);
        assert!(!report.config.rules[0].enabled);
        assert_eq!(report.items[0].status, MigrationStatus::NeedsRewrite);
        assert!(report.items[0]
            .diagnostics
            .iter()
            .any(|d| d.code == "quicker_script_dynamic"));
    }
}

#[test]
fn dynamic_sendkeys_reports_original_syntax_expansion_difference() {
    for data in ["{cliptext}", "{selection}", "{0:yyyy-MM-dd}"] {
        let report = import(vec![command("dynamic", 2, data)]);
        assert!(
            matches!(&report.config.rules[0].action, Action::SendKeys { syntax_variables, .. } if syntax_variables.is_empty())
        );
        assert!(!report.config.rules[0].enabled);
        assert_eq!(report.items[0].status, MigrationStatus::NeedsRewrite);
        assert!(report.items[0]
            .diagnostics
            .iter()
            .any(|d| d.code == "quicker_sendkeys_dynamic" && d.message.contains("字面文字")));
    }
}

#[test]
fn native_validation_and_schema_diagnostics_are_localized_without_values() {
    let mut malformed = command("malformed", 3, "private-data-must-not-appear");
    malformed["IgnoreCase"] = json!("private-invalid-value");
    let mut regex = command("regex", 3, "sample");
    regex["UseRegex"] = json!(true);
    regex["CmdText"] = json!("(?<=sample)x");
    let report = import(vec![
        malformed,
        regex,
        command("sendkeys", 2, "^a"),
        command("bad-step", 7, "unknown:x"),
    ]);
    for item in &report.items {
        for diagnostic in &item.diagnostics {
            assert!(
                diagnostic
                    .message
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "diagnostic code {} must explain in Chinese",
                diagnostic.code
            );
            assert!(!diagnostic.message.contains("private-"));
        }
    }
    assert!(report.items[0]
        .diagnostics
        .iter()
        .any(|d| d.code == "quicker_schema" && d.message.contains("IgnoreCase")));
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
        assert!(report.items[0].diagnostics.iter().any(|d| d.code
            == "quicker_process_binding_ignored"
            && d.severity == Severity::Warning));
        let package = export_package_with_unresolved(&report.config, report.unresolved).unwrap();
        let reimported = import_package(&package).unwrap();
        assert_eq!(reimported.unresolved[0].original, original);
        assert!(reimported.config.rules[0].enabled);
        let compiler = compile_config(&reimported.config).unwrap();
        for foreground in [
            ForegroundContext {
                process: "test.exe".into(),
                ..Default::default()
            },
            ForegroundContext {
                platform: Platform::Macos,
                bundle_id: "com.test".into(),
                ..Default::default()
            },
        ] {
            assert!(compiler
                .match_input("synthetic-trigger", None, &foreground)
                .is_some());
        }
    }
}
