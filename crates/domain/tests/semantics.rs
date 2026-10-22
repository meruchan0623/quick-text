use quicker_domain::*;
use std::collections::BTreeMap;
fn literal(id: &str, trigger: &str) -> Rule {
    Rule {
        id: id.into(),
        match_spec: MatchSpec {
            kind: MatchKind::Literal,
            value: trigger.into(),
        },
        action: Action::Text {
            method: OutputMethod::Keys,
            source: ContentSource::inline("合成 output🙂"),
        },
        ..Rule::default()
    }
}
fn config(rules: Vec<Rule>) -> Config {
    Config {
        rules,
        ..Config::default()
    }
}
fn context() -> ForegroundContext {
    ForegroundContext {
        process: "test.exe".into(),
        ..ForegroundContext::default()
    }
}
fn stamp() -> EnvironmentStamp {
    EnvironmentStamp {
        revision: 1,
        buffer_generation: 2,
        focus_id: "window-1".into(),
        process: "test.exe".into(),
        ime_state: ImeState::FullyCommitted,
        injection_safe: true,
    }
}
fn vars() -> VariableContext {
    VariableContext {
        now: chrono::DateTime::parse_from_rfc3339("2026-10-14T09:08:07.1234567+08:00").unwrap(),
        ..VariableContext::default()
    }
}
#[test]
fn default_model_roundtrip() {
    let c = config(vec![literal("r1", ";test")]);
    let value = serde_json::to_value(&c).unwrap();
    assert_eq!(value["rules"][0]["match"]["kind"], "literal");
    let c2: Config = serde_json::from_value(value).unwrap();
    assert_eq!(c, c2);
}
#[test]
fn unknown_fields_rejected() {
    assert!(serde_json::from_str::<Config>(r#"{"garbage":true}"#).is_err());
}
#[test]
fn immediate_regex_rejected() {
    let mut r = literal("r", "x");
    r.trigger_mode = TriggerMode::Immediate;
    r.match_spec.kind = MatchKind::Regex;
    assert!(compile_config(&config(vec![r]))
        .unwrap_err()
        .iter()
        .any(|e| e.code == "immediate_regex"));
}
#[test]
fn first_capture_requires_group() {
    let mut r = literal("r", "x");
    r.parameter = ParameterSource::FirstCapture;
    assert!(compile_config(&config(vec![r])).is_err());
}
#[test]
fn regex_captures_and_complete_parameter() {
    let mut r = literal("r", r"hello(\d+)");
    r.match_spec.kind = MatchKind::Regex;
    let c = compile_config(&config(vec![r])).unwrap();
    let m = c.match_input("hello42", Some("space"), &context()).unwrap();
    assert_eq!(m.parameter, Some("hello42".into()));
    assert_eq!(m.captures[1], Some("42".into()));
}
#[test]
fn capture_template_encodes_once() {
    let p = ParameterSource::CaptureTemplate {
        template: "a={$1}&constant=x%20y".into(),
    };
    let out = resolve_parameter(
        &p,
        "ignored",
        &[Some("input".into()), Some("a+b %20汉".into())],
    )
    .unwrap()
    .unwrap();
    assert_eq!(out, "a=a%2Bb%20%2520%E6%B1%89&constant=x%20y");
}
#[test]
fn null_differs_from_empty_fixed() {
    assert_eq!(
        resolve_parameter(&ParameterSource::Null, "x", &[]).unwrap(),
        None
    );
    assert_eq!(
        resolve_parameter(
            &ParameterSource::Fixed {
                value: String::new()
            },
            "x",
            &[]
        )
        .unwrap(),
        Some(String::new())
    );
}
#[test]
fn capture_template_missing_compile_fails() {
    let mut r = literal("r", "x");
    r.parameter = ParameterSource::CaptureTemplate {
        template: "{$1}".into(),
    };
    assert!(compile_config(&config(vec![r])).is_err());
}
#[test]
fn optional_first_capture_cannot_silently_be_empty() {
    assert!(resolve_parameter(
        &ParameterSource::FirstCapture,
        "x",
        &[Some("x".into()), None]
    )
    .is_err());
}
#[test]
fn dotnet_regex_unsupported() {
    let mut r = literal("r", r"a(?=b)");
    r.match_spec.kind = MatchKind::Regex;
    assert_eq!(
        compile_config(&config(vec![r])).unwrap_err()[0].code,
        "regex_unsupported"
    );
}
#[test]
fn ignore_case_only_match() {
    let mut r = literal("r", "HELLO");
    r.ignore_case = true;
    let c = compile_config(&config(vec![r.clone()])).unwrap();
    assert!(c.match_input("hello", Some("space"), &context()).is_some());
    assert_eq!(c.rule("r").unwrap().action, r.action);
}
#[test]
fn literal_word_boundary() {
    let c = compile_config(&config(vec![literal("r", "abc")])).unwrap();
    assert!(c.match_input("xabc", Some("space"), &context()).is_none());
    assert!(c.match_input("x abc", Some("space"), &context()).is_some());
}
#[test]
fn longest_match_and_stable_tie() {
    let mut a = literal("short", "ab");
    a.trigger_mode = TriggerMode::Immediate;
    let mut b = literal("long", "cab");
    b.trigger_mode = TriggerMode::Immediate;
    let mut d = b.clone();
    d.id = "tie".into();
    let c = compile_config(&config(vec![a, b, d])).unwrap();
    assert_eq!(
        c.match_input("cab", None, &context()).unwrap().rule_id,
        "long"
    );
}
#[test]
fn terminator_inheritance_override_and_consume() {
    let mut r = literal("r", "a");
    r.terminator = Some(Terminator {
        keys: vec!["enter".into()],
        consume: false,
    });
    let c = compile_config(&config(vec![r])).unwrap();
    assert!(c.match_input("a", Some("space"), &context()).is_none());
    assert!(
        !c.match_input("a", Some("enter"), &context())
            .unwrap()
            .consume_terminator
    );
}
#[test]
fn global_and_disabled() {
    let mut c = config(vec![literal("r", "a")]);
    c.enabled = false;
    assert!(compile_config(&c)
        .unwrap()
        .match_input("a", Some("space"), &context())
        .is_none());
    c.enabled = true;
    c.rules[0].enabled = false;
    assert!(compile_config(&c)
        .unwrap()
        .match_input("a", Some("space"), &context())
        .is_none());
}
#[test]
fn legacy_program_restrictions_are_ignored_after_import_save_and_reload() {
    let mut raw = serde_json::to_value(config(vec![literal("r", "a")])).unwrap();
    raw["rules"][0]["scope"] = serde_json::json!({
        "windows_processes": ["definitely-other.exe"],
        "mac_bundle_ids": ["com.other.app"]
    });
    let directly_loaded: Config = serde_json::from_value(raw.clone()).unwrap();
    let report = import_package(&raw.to_string()).unwrap();
    assert_eq!(report.items[0].status, MigrationStatus::Compatible);
    assert!(report.config.rules[0].enabled);
    let saved = serde_json::to_string(&directly_loaded).unwrap();
    assert!(!saved.contains("scope"));
    let reloaded: Config = serde_json::from_str(&saved).unwrap();
    let package = export_package(&report.config).unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&package).unwrap()["config"]["rules"][0]
            .get("scope")
            .is_none()
    );
    let package_reloaded = import_package(&package).unwrap().config;
    let mac = ForegroundContext {
        platform: Platform::Macos,
        bundle_id: "com.test".into(),
        ..Default::default()
    };
    for mut config in [directly_loaded, reloaded, package_reloaded] {
        let compiler = compile_config(&config).unwrap();
        for foreground in [&context(), &mac] {
            assert!(compiler
                .match_input("a", Some("space"), foreground)
                .is_some());
        }
        config.globals.blacklist = vec!["TEST.EXE".into(), "COM.TEST".into()];
        let compiler = compile_config(&config).unwrap();
        for foreground in [&context(), &mac] {
            assert!(compiler
                .match_input("a", Some("space"), foreground)
                .is_none());
        }
    }
}
#[test]
fn ignoring_legacy_scope_keeps_unknown_rule_fields_strict() {
    let mut raw = serde_json::to_value(literal("r", "a")).unwrap();
    raw["scope"] = serde_json::json!({"windows_processes": ["other.exe"]});
    raw["future_unknown"] = serde_json::json!(true);
    assert!(serde_json::from_value::<Rule>(raw).is_err());
}
#[test]
fn duplicate_ids_fail() {
    assert!(compile_config(&config(vec![literal("r", "a"), literal("r", "b")])).is_err());
}
#[test]
fn empty_regex_rejected() {
    let mut r = literal("r", "a*");
    r.match_spec.kind = MatchKind::Regex;
    assert!(compile_config(&config(vec![r])).is_err());
}
#[test]
fn sendkeys_group_modifiers() {
    let p = parse_sendkeys("^(ab)+{LEFT 2}~").unwrap();
    assert_eq!(p.steps.len(), 5);
    assert!(matches!(&p.steps[0].operation,Operation::Hotkey{keys} if keys[0]==Key::named("Ctrl")));
    assert!(validate_plan(&p).is_empty());
}
#[test]
fn sendkeys_punctuation_escape() {
    let source = escape_sendkeys_literal("+^%~()[]{}你好");
    let p = parse_sendkeys(&source).unwrap();
    assert!(matches!(&p.steps[0].operation,Operation::Text{text,..} if text=="+^%~()[]{}你好"));
}
#[test]
fn sendkeys_f16_supported_f17_not() {
    assert!(parse_sendkeys("{F16}").is_ok());
    for v in ["{F17}", "{Win}", "{VOLUMEUP}", "{#175}"] {
        assert!(parse_sendkeys(v).is_err());
    }
}
#[test]
fn sendkeys_malformed() {
    for v in [
        "^",
        "(",
        ")",
        "{LEFT",
        "{LEFT 0}",
        "{LEFT 10001}",
        "{unknown}",
        "[",
        "^^a",
    ] {
        assert!(parse_sendkeys(v).is_err(), "{v}");
    }
}
#[test]
fn sendkeys_depth_limited() {
    let source = format!("{}x{}", "(".repeat(33), ")".repeat(33));
    assert_eq!(parse_sendkeys(&source).unwrap_err()[0].code, "group_depth");
}
#[test]
fn inputscript_all_21_commands() {
    let script="input:hello\ninput2:A\\tB\\r\\nC\nsendkeys:^a\ndelay:1\npaste:hello\nkeydown:Ctrl\nkeyup:Control\nkeypress:#175\nhotkey:Ctrl+Alt+D1\nmoveto:50%,50%\nmove:-20,10\nclick:left\ndbclick:right\ndown:middle\nup:middle\nwheel:1\nwheeldelta:120\nhwheel:-1\nhwheeldelta:-120\npastefile:C:\\synthetic.txt;D:\\synthetic.txt\npasteimage:C:\\synthetic.png";
    let plan = parse_inputscript(script).unwrap();
    assert_eq!(plan.steps.len(), 21);
    assert!(validate_plan(&plan).is_empty());
}
#[test]
fn inputscript_first_colon_retains_drive_and_text() {
    let p = parse_inputscript("input:https://example.invalid/a:b;;pastefile:C:\\file.txt").unwrap();
    assert!(
        matches!(&p.steps[0].operation,Operation::Text{text,..} if text=="https://example.invalid/a:b")
    );
    assert!(
        matches!(&p.steps[1].operation,Operation::PasteFiles{paths} if paths[0]=="C:\\file.txt")
    );
}
#[test]
fn inputscript_comments_and_diagnostics() {
    let p = parse_inputscript(" // synthetic\n input: hi\nunknown:no").unwrap_err();
    assert_eq!(p[0].span.line, 3);
    assert!(parse_inputscript("input:a;;literal").is_err());
}
#[test]
fn input2_unknown_escapes_rejected() {
    for s in [r"input2:\q", r"input2:\n", r"input2:\r", r"input2:\\"] {
        assert!(parse_inputscript(s).is_err());
    }
}
#[test]
fn numeric_hotkey_requires_d_prefix() {
    assert!(parse_inputscript("hotkey:Ctrl+1").is_err());
    assert!(parse_inputscript("hotkey:Ctrl+D1").is_ok());
}
#[test]
fn wheel_units_independent() {
    let p = parse_inputscript("wheel:1;;wheeldelta:120;;hwheel:-1;;hwheeldelta:-120").unwrap();
    assert!(matches!(
        p.steps[0].operation,
        Operation::Wheel {
            unit: WheelUnit::Clicks,
            ..
        }
    ));
    assert!(matches!(
        p.steps[1].operation,
        Operation::Wheel {
            unit: WheelUnit::Delta,
            ..
        }
    ));
    assert!(matches!(
        p.steps[2].operation,
        Operation::Wheel {
            axis: WheelAxis::Horizontal,
            ..
        }
    ));
}
#[test]
fn replay_all_11_opcodes() {
    let s="1;MV;None,-20,10,0;\n1;MD;Left,,,-0;\n1;MC;Right,-99999,-99999,1;\n1;MU;Left,,,0;\n1;MH;None,,,1;\n1;MW;None,,,-1;\n1;DL;\n1;KD;Ctrl;\n1;KU;Control;\n1;KP;D1;\n1;MVD;None,-10,0,0;";
    let plan = parse_replay(s).unwrap();
    assert!(validate_plan(&plan).is_empty());
    assert_eq!(plan.dialect, Dialect::Replay);
}
#[test]
fn replay_tab_and_semicolon_tab() {
    for s in ["10\tKP\tSpace", "10;\tKP;\tSpace;"] {
        assert!(parse_replay(s).is_ok());
    }
}
#[test]
fn dialects_never_cross_parse() {
    assert!(parse_sendkeys("input:hello").is_ok()); // SendKeys has no commands, so the same string is literal text.
    let p = parse_sendkeys("input:hello").unwrap();
    assert!(matches!(&p.steps[0].operation,Operation::Text{text,..} if text=="input:hello"));
    assert!(parse_inputscript("10;KP;Space;").is_err());
    assert!(parse_replay("keypress:Space").is_err());
}
#[test]
fn replay_speed_finite_positive() {
    let p = parse_replay("100;KP;Space;").unwrap();
    for speed in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(scale_replay(&p, speed).is_err());
    }
    assert!(matches!(
        scale_replay(&p, 2.0).unwrap().steps[0].operation,
        Operation::Delay { ms: 50 }
    ));
    assert!(matches!(
        scale_replay(&p, 0.5).unwrap().steps[0].operation,
        Operation::Delay { ms: 200 }
    ));
}
#[test]
fn replay_preserves_negative_coordinates() {
    let p = parse_replay("0;MV;None,-1920,0,0").unwrap();
    assert!(matches!(
        p.steps[1].operation,
        Operation::MouseMove {
            x: -1920.0,
            y: 0.0,
            relative: false,
            ..
        }
    ));
}
#[test]
fn replay_unknown_and_partial_coordinate_fail() {
    for s in ["0;XX;X", "0;MV;None,,2,0", "0;MV;None,20%,20%,0"] {
        assert!(parse_replay(s).is_err());
    }
}
#[test]
fn key_alias_pair_validation() {
    let p = parse_inputscript("keydown:Ctrl;;keyup:controlkey").unwrap();
    assert!(validate_plan(&p).is_empty());
    let p = parse_inputscript("keydown:Ctrl;;keydown:Control;;keyup:Ctrl;;keyup:Control").unwrap();
    assert!(!validate_plan(&p).is_empty());
    let p = parse_inputscript("hotkey:Ctrl+Control").unwrap();
    assert!(!validate_plan(&p).is_empty());
}
#[test]
fn unbalanced_keys_buttons_rejected() {
    for source in ["keydown:Ctrl", "keyup:Ctrl", "down:left", "up:left"] {
        assert!(!validate_plan(&parse_inputscript(source).unwrap()).is_empty());
    }
}
#[test]
fn date_dotnet_numeric() {
    assert_eq!(
        format_dotnet_date("yyyy-MM-dd HH:mm:ss.fff zzz", &vars().now).unwrap(),
        "2026-10-14 09:08:07.123 +08:00"
    );
    assert!(format_dotnet_date("MMMM", &vars().now).is_err());
}
#[test]
fn variable_unavailable_fails_before_delete() {
    assert!(preflight_action(
        &Action::Text {
            method: OutputMethod::Keys,
            source: ContentSource::inline("{selection}")
        },
        &vars()
    )
    .is_err());
}
#[test]
fn variables_never_reparsed_as_sendkeys() {
    let mut v = vars();
    v.clipboard = Some("^a{ENTER}".into());
    let p = preflight_action(
        &Action::SendKeys {
            syntax_variables: vec![],
            source: ContentSource::inline("{cliptext}"),
        },
        &v,
    )
    .unwrap();
    assert!(matches!(&p.steps[0].operation,Operation::Text{text,..} if text=="^a{ENTER}"));
}
#[test]
fn variable_values_never_interpolated_twice() {
    let mut v = vars();
    v.clipboard = Some("{selection}".into());
    v.selection = Some("secret synthetic".into());
    let p = preflight_action(
        &Action::Text {
            method: OutputMethod::Keys,
            source: ContentSource::inline("{cliptext}"),
        },
        &v,
    )
    .unwrap();
    assert!(matches!(&p.steps[0].operation,Operation::Text{text,..} if text=="{selection}"));
}
#[test]
fn syntax_variable_explicit_and_validated() {
    let mut v = vars();
    v.clipboard = Some("^a".into());
    v.syntax_variables.insert("cliptext".into());
    let p = preflight_action(
        &Action::SendKeys {
            syntax_variables: vec![],
            source: ContentSource::inline("{cliptext}"),
        },
        &v,
    )
    .unwrap();
    assert!(matches!(&p.steps[0].operation, Operation::Hotkey { .. }));
    v.clipboard = Some("{BOGUS}".into());
    assert!(preflight_action(
        &Action::SendKeys {
            syntax_variables: vec![],
            source: ContentSource::inline("{cliptext}")
        },
        &v
    )
    .is_err());
}
#[test]
fn missing_file_preflight_preserves_transaction() {
    let action = Action::Text {
        method: OutputMethod::Paste,
        source: ContentSource::File {
            path: "__synthetic_missing__".into(),
            encoding: Encoding::Utf8,
        },
    };
    assert!(preflight_action(&action, &vars()).is_err());
    let t = Transaction::new("r", stamp(), 100);
    assert!(!t.report.trigger_erased);
}
#[test]
fn file_encoding_and_literal_source() {
    let dir = std::env::temp_dir().join(format!("quicker-domain-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("encoding.bin");
    std::fs::write(&path, [0xff, 0xfe, 0x41, 0]).unwrap();
    assert_eq!(
        read_content_source(&ContentSource::File {
            path: path.to_string_lossy().into(),
            encoding: Encoding::Utf16Le
        })
        .unwrap(),
        "A"
    );
    assert!(read_content_source(&ContentSource::File {
        path: path.to_string_lossy().into(),
        encoding: Encoding::Utf8
    })
    .is_err());
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
#[test]
fn transaction_erasure_exactly_once_and_stale_checks() {
    let mut t = Transaction::new("r", stamp(), 100);
    t.mark_ready().unwrap();
    let mut changed = stamp();
    changed.buffer_generation += 1;
    assert_eq!(
        t.check_before_erase(&changed, ImeErasure::Escape, 0),
        Err("stale_buffer")
    );
    changed = stamp();
    changed.focus_id = "different".into();
    assert_eq!(
        t.check_before_erase(&changed, ImeErasure::Escape, 0),
        Err("focus_changed")
    );
    t.mark_erased(&stamp(), ImeErasure::Escape, 0).unwrap();
    assert_eq!(
        t.mark_erased(&stamp(), ImeErasure::Escape, 0),
        Err("invalid_phase")
    );
}
#[test]
fn transaction_unsafe_ime_cancel_timeout() {
    let mut t = Transaction::new("r", stamp(), 100);
    t.mark_ready().unwrap();
    let mut current = stamp();
    current.ime_state = ImeState::PartiallyCommitted;
    assert_eq!(
        t.check_before_erase(&current, ImeErasure::Backspace, 0),
        Err("unsafe_ime")
    );
    assert_eq!(
        t.check_before_erase(&stamp(), ImeErasure::Escape, 100),
        Err("timeout")
    );
    t.cancel();
    assert!(t
        .check_before_erase(&stamp(), ImeErasure::Escape, 0)
        .is_err());
}
#[test]
fn transaction_focus_transition_declared() {
    let mut t = Transaction::new("r", stamp(), 100);
    t.mark_ready().unwrap();
    t.mark_erased(&stamp(), ImeErasure::Escape, 0).unwrap();
    let mut other = stamp();
    other.focus_id = "new".into();
    other.process = "other.exe".into();
    assert_eq!(
        t.check_step(&other, &TargetPolicy::Locked, 1),
        Err("focus_changed")
    );
    assert!(t
        .check_step(
            &other,
            &TargetPolicy::Declared {
                process: "other.exe".into()
            },
            1
        )
        .is_ok());
}
#[test]
fn consumption_unknown_not_success() {
    let mut t = Transaction::new("r", stamp(), 100);
    t.mark_ready().unwrap();
    t.mark_erased(&stamp(), ImeErasure::Escape, 0).unwrap();
    t.step_sent(
        &Operation::KeyPress {
            key: Key::named("A"),
        },
        false,
    );
    t.finish(false).unwrap();
    assert_eq!(t.report.state, TransactionState::Unknown);
    assert_eq!(t.report.side_effects.len(), 2);
}
#[test]
fn cleanup_respects_physical_keys() {
    let mut t = Transaction::new("r", stamp(), 100);
    t.step_sent(
        &Operation::KeyDown {
            key: Key::named("Ctrl"),
        },
        false,
    );
    let mut physical = BTreeMap::new();
    physical.insert(Key::named("Ctrl"), Some(true));
    assert!(t.cleanup(&physical, &BTreeMap::new()).keys.is_empty());
    physical.insert(Key::named("Ctrl"), Some(false));
    assert_eq!(t.cleanup(&physical, &BTreeMap::new()).keys.len(), 1);
    assert!(
        t.cleanup(&BTreeMap::new(), &BTreeMap::new())
            .blocked_by_physical_state
    );
}
#[test]
fn clipboard_generation_preserves_new_copy() {
    let c = ClipboardLease {
        written_generation: 5,
    };
    assert!(c.can_restore(5));
    assert!(!c.can_restore(6));
}
#[test]
fn draft_publish_failure_keeps_active_and_draft() {
    let mut store = DraftStore::new(config(vec![literal("r", "a")]));
    store.draft.rules[0].description = "pending".into();
    let original = store.base_serialized.clone();
    assert!(store.apply(&original, |_| Err("simulated".into())).is_err());
    assert_eq!(store.active.revision, 0);
    assert!(store.is_dirty());
    assert_eq!(
        store.apply(&original, |c| Ok(c.config.revision)).unwrap(),
        1
    );
    assert!(!store.is_dirty());
}
#[test]
fn draft_external_edit_and_ack_mismatch() {
    let mut store = DraftStore::new(Config::default());
    assert!(store.apply("changed", |_| Ok(1)).is_err());
    let baseline = store.base_serialized.clone();
    assert!(store.apply(&baseline, |_| Ok(99)).is_err());
    assert_eq!(store.active.revision, 0);
}
#[test]
fn package_roundtrip_full_fields() {
    let original = config(vec![literal("r", ";hello")]);
    let text = export_package(&original).unwrap();
    let report = import_package(&text).unwrap();
    assert_eq!(report.config, original);
    assert_eq!(report.items[0].status, MigrationStatus::Compatible);
}
#[test]
fn import_unsupported_regex_preserves_disabled_original() {
    let mut rule = literal("r", "a(?=b)");
    rule.match_spec.kind = MatchKind::Regex;
    let raw = serde_json::to_string(&config(vec![rule])).unwrap();
    let imported = import_package(&raw).unwrap();
    assert!(!imported.config.rules[0].enabled);
    assert_eq!(imported.config.rules[0].match_spec.value, "a(?=b)");
    assert_eq!(imported.items[0].status, MigrationStatus::NeedsRewrite);
    assert_eq!(imported.items[0].original["enabled"], true);
}
#[test]
fn import_conflict_preview_strategies() {
    let original = config(vec![literal("r", "a")]);
    let report = import_package(&export_package(&original).unwrap()).unwrap();
    assert!(merge_import(&original, &report, ConflictStrategy::Reject).is_err());
    assert_eq!(
        merge_import(&original, &report, ConflictStrategy::Replace)
            .unwrap()
            .rules
            .len(),
        1
    );
    let both = merge_import(&original, &report, ConflictStrategy::KeepBoth).unwrap();
    assert_eq!(both.rules[1].id, "r-2");
}
#[test]
fn legacy_parameter_semantics() {
    assert_eq!(
        migrate_parameter(false, false, None, None).unwrap(),
        ParameterSource::DefaultMatch
    );
    assert_eq!(
        migrate_parameter(true, false, Some("*NULL*"), None).unwrap(),
        ParameterSource::Null
    );
    assert!(matches!(
        migrate_parameter(true, true, Some("a={$1}"), None).unwrap(),
        ParameterSource::CaptureTemplate { .. }
    ));
    assert!(migrate_parameter(true, true, Some("fixed"), None).is_err());
}
#[test]
fn package_assets_cannot_escape() {
    for p in ["../a", "..\\a", "C:\\file", "/absolute", "a/../../b", ""] {
        assert!(validate_asset_path(p).is_err());
    }
    assert!(validate_asset_path("assets/synthetic.txt").is_ok());
}
#[test]
fn synthetic_fixture_compiles() {
    let s = include_str!("fixtures/portable.json");
    let report = import_package(s).unwrap();
    assert!(compile_config(&report.config).is_ok());
}

#[test]
fn reusable_typed_parameters_and_literal_b() {
    let rule = Rule {
        id: "action-1".into(),
        action: Action::SendKeys {
            syntax_variables: vec![],
            source: ContentSource::inline("{param:text}"),
        },
        ..literal("action-1", "a")
    };
    let mut parameters = BTreeMap::new();
    parameters.insert("text".into(), serde_json::json!("^a"));
    let plan = expand_reusable(
        &Action::Reusable {
            action_id: "action-1".into(),
            parameters,
        },
        &config(vec![rule]),
        &vars(),
    )
    .unwrap();
    assert!(matches!(&plan.steps[0].operation,Operation::Text{text,..} if text=="^a"));
}
#[test]
fn reusable_cycle_unknown_disabled_and_nonscalar() {
    let rule = Rule {
        action: Action::Reusable {
            action_id: "r".into(),
            parameters: BTreeMap::new(),
        },
        ..literal("r", "a")
    };
    assert_eq!(
        expand_reusable(&rule.action, &config(vec![rule.clone()]), &vars()).unwrap_err()[0].code,
        "action_cycle"
    );
    assert!(expand_reusable(&rule.action, &Config::default(), &vars()).is_err());
    let mut disabled = literal("r", "a");
    disabled.enabled = false;
    assert!(expand_reusable(&rule.action, &config(vec![disabled]), &vars()).is_err());
    let mut parameters = BTreeMap::new();
    parameters.insert("a".into(), serde_json::json!([1]));
    assert!(expand_reusable(
        &Action::Reusable {
            action_id: "r".into(),
            parameters
        },
        &config(vec![literal("r", "a")]),
        &vars()
    )
    .is_err());
}
#[test]
fn unknown_import_entry_keeps_raw_and_visible_row() {
    let raw = r#"{"version":1,"rules":[{"id":"old","match":{"kind":"literal","value":"x"},"action":{"type":"private_plugin","script":"synthetic"},"extra":"keep"}]}"#;
    let report = import_package(raw).unwrap();
    assert_eq!(report.unresolved.len(), 1);
    assert_eq!(report.unresolved[0].original["extra"], "keep");
    assert!(!report.config.rules[0].enabled);
    assert_eq!(report.config.rules[0].id, "old");
    let exported =
        export_package_with_unresolved(&report.config, report.unresolved.clone()).unwrap();
    let restored = import_package(&exported).unwrap();
    assert_eq!(
        restored.unresolved[0].original,
        report.unresolved[0].original
    );
}
#[test]
fn imported_automation_disabled_until_review() {
    let mut rule = literal("script", "s");
    rule.action = Action::SendKeys {
        syntax_variables: vec![],
        source: ContentSource::inline("^a"),
    };
    let report = import_package(&export_package(&config(vec![rule])).unwrap()).unwrap();
    assert!(!report.config.rules[0].enabled);
    assert_eq!(report.items[0].status, MigrationStatus::Compatible);
    assert!(report.items[0]
        .diagnostics
        .iter()
        .any(|d| d.code == "import_confirmation"));
}
#[test]
fn duplicate_import_ids_rejected() {
    let c = config(vec![literal("r", "a"), literal("r", "b")]);
    assert!(import_package(&serde_json::to_string(&c).unwrap()).is_err());
}
#[test]
fn fixture_full_parser_sources() {
    assert!(parse_sendkeys(include_str!("fixtures/sendkeys-b.txt")).is_ok());
    assert!(parse_inputscript(include_str!("fixtures/inputscript.txt")).is_ok());
    assert!(parse_replay(include_str!("fixtures/replay.txt")).is_ok());
}
#[test]
fn reusable_parameter_template_compiles_before_values_available() {
    let rule = Rule {
        action: Action::SendKeys {
            syntax_variables: vec![],
            source: ContentSource::inline("{param:text}"),
        },
        ..literal("r", "x")
    };
    assert!(compile_config(&config(vec![rule])).is_ok());
}
