//! B-43: tolerant reading of stored configuration and lossless write-back.
use proptest::prelude::*;
use quicker_domain::*;
use serde::Deserialize;
use serde_json::{json, Map, Value};

fn literal(id: &str, trigger: &str) -> Rule {
    Rule {
        id: id.into(),
        match_spec: MatchSpec {
            kind: MatchKind::Literal,
            value: trigger.into(),
        },
        ..Rule::default()
    }
}

fn read(value: &Value) -> StoredConfig {
    read_stored_config(value.to_string().as_bytes()).unwrap()
}

fn codes(diagnostics: &[Diagnostic]) -> Vec<(&str, &str)> {
    diagnostics
        .iter()
        .map(|d| (d.code.as_str(), d.path.as_str()))
        .collect()
}

#[test]
fn wrongly_typed_fields_fall_back_to_their_defaults() {
    let stored = read(&json!({
        "version": 1,
        "revision": "seven",
        "enabled": 3,
        "globals": {
            "terminators": "space",
            "word_separators": 5,
            "blacklist": ["game.exe"],
            "ime_erasure": "teleport",
            "x_future": {"kept": true}
        },
        "rules": [{"id": "r", "match": {"kind": "literal", "value": "abc"}}]
    }));
    let defaults = GlobalSettings::default();
    let config = &stored.config;
    assert_eq!(config.revision, 0);
    assert!(config.enabled);
    assert_eq!(config.globals.terminators, defaults.terminators);
    assert_eq!(config.globals.word_separators, defaults.word_separators);
    assert_eq!(config.globals.ime_erasure, defaults.ime_erasure);
    assert_eq!(config.globals.blacklist, ["game.exe"]);
    assert_eq!(config.globals.extra["x_future"], json!({"kept": true}));
    assert_eq!(config.rules.len(), 1);
    assert_eq!(
        codes(&stored.diagnostics),
        [
            (diagnostic_code::FIELD_RESET, "revision"),
            (diagnostic_code::FIELD_RESET, "enabled"),
            (diagnostic_code::FIELD_RESET, "globals.ime_erasure"),
            (diagnostic_code::FIELD_RESET, "globals.terminators"),
            (diagnostic_code::FIELD_RESET, "globals.word_separators"),
        ]
    );
    assert!(stored
        .diagnostics
        .iter()
        .all(|d| d.severity == Severity::Warning && !d.message.contains("teleport")));
    // Strict deserialization (IPC) still rejects the same fields.
    assert!(serde_json::from_str::<Config>(r#"{"revision":"seven"}"#).is_err());
    assert!(serde_json::from_str::<Config>(r#"{"globals":{"terminators":"space"}}"#).is_err());
}

#[test]
fn globals_of_the_wrong_type_reset_as_a_whole() {
    let stored = read(&json!({"globals": 5, "rules": []}));
    assert_eq!(stored.config.globals, GlobalSettings::default());
    assert_eq!(
        codes(&stored.diagnostics),
        [(diagnostic_code::FIELD_RESET, "globals")]
    );
}

#[test]
fn unreadable_rules_are_kept_verbatim_and_written_back_in_place() {
    let source = json!({
        "version": 1,
        "revision": 2,
        "enabled": true,
        "globals": GlobalSettings::default(),
        "rules": [
            literal("a", "aa"),
            {"id": "future", "match": {"kind": "literal", "value": "secret trigger"},
             "action": {"type": "teleport", "where": "secret place"}},
            literal("b", "bb"),
            {"id": "typed", "enabled": "secret"},
            7
        ]
    });
    let stored = read(&source);
    let config = &stored.config;
    assert_eq!(
        config
            .rules
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    let unreadable: Vec<_> = config
        .unreadable_rules
        .iter()
        .map(|u| (u.index, u.id()))
        .collect();
    assert_eq!(
        unreadable,
        [(1, Some("future")), (3, Some("typed")), (4, None)]
    );
    assert_eq!(config.unreadable_rules[0].raw, source["rules"][1]);
    assert_eq!(config.unreadable_rules[0].unreadable_fields(), ["action"]);
    assert_eq!(config.unreadable_rules[1].unreadable_fields(), ["enabled"]);
    assert_eq!(
        codes(&stored.diagnostics),
        [
            (diagnostic_code::RULE_UNREADABLE, "rules[1]"),
            (diagnostic_code::RULE_UNREADABLE, "rules[3]"),
            (diagnostic_code::RULE_UNREADABLE, "rules[4]"),
        ]
    );
    // Messages name fields only, never rule content.
    for d in &stored.diagnostics {
        assert_eq!(d.severity, Severity::Warning);
        assert!(!d.message.contains("secret") && !d.message.contains("teleport"));
    }
    assert!(stored.diagnostics[0].message.contains("action"));
    assert_eq!(serde_json::to_value(config).unwrap(), source);
    // The IPC path (strict deserialization) keeps them as well.
    let ipc: Config = serde_json::from_value(source.clone()).unwrap();
    assert_eq!(&ipc, config);
    // Unreadable entries never compile; diagnostics point at the entry an editor shows.
    assert_eq!(compile_config(config).unwrap().rules.len(), 2);
    let mut broken = config.clone();
    broken.rules[1].id = "a".into();
    let errors = compile_config(&broken).unwrap_err();
    assert_eq!(errors[0].path, "rules[2].id");
}

#[test]
fn unreadable_rules_past_the_end_follow_the_remaining_rules() {
    let mut config = read(&json!({"rules": [
        {"id": "a", "match": {"kind": "literal", "value": "aa"}},
        {"id": "b", "match": {"kind": "literal", "value": "bb"}},
        {"id": "future", "action": {"type": "teleport"}}
    ]}))
    .config;
    config.rules.clear();
    let written = serde_json::to_value(&config).unwrap();
    assert_eq!(
        written["rules"],
        json!([{"id": "future", "action": {"type": "teleport"}}])
    );
}

#[test]
fn newer_schema_is_identified() {
    for (source, version) in [
        (r#"{"version":2}"#, 2),
        (
            r#"{"version":99999999999,"rules":"anything"}"#,
            99_999_999_999,
        ),
    ] {
        let error = read_stored_config(source.as_bytes()).unwrap_err();
        assert_eq!(error, StoredConfigError::NewerSchema { version });
        assert_eq!(error.code(), diagnostic_code::SCHEMA_NEWER);
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.code, diagnostic_code::SCHEMA_NEWER);
        assert_eq!(diagnostic.path, "version");
        assert_eq!(diagnostic.severity, Severity::Error);
    }
}

#[test]
fn unsupported_versions_are_rejected() {
    for source in [
        r#"{"version":0}"#,
        r#"{"version":-1}"#,
        r#"{"version":1.5}"#,
        r#"{"version":"1"}"#,
        r#"{"version":null}"#,
    ] {
        let error = read_stored_config(source.as_bytes()).unwrap_err();
        assert_eq!(error, StoredConfigError::UnsupportedVersion, "{source}");
        assert_eq!(error.code(), diagnostic_code::SCHEMA_VERSION);
    }
    // A missing version is the first schema, as in 0.3.1.
    let stored = read_stored_config(br#"{"rules":[]}"#).unwrap();
    assert_eq!(stored.config.version, 1);
}

#[test]
fn malformed_files_are_reported_without_content() {
    let cases: [&[u8]; 6] = [
        b"{\"version\":1,\n\"rules\":[",
        b"\xff\xfe{}",
        b"[]",
        b"\"secret\"",
        br#"{"rules":{"id":"secret"}}"#,
        b"",
    ];
    for bytes in cases {
        let error = read_stored_config(bytes).unwrap_err();
        assert!(
            matches!(error, StoredConfigError::Malformed { .. }),
            "{error:?}"
        );
        assert_eq!(error.code(), diagnostic_code::CONFIG_MALFORMED);
        assert!(!error.to_string().contains("secret"));
    }
    let StoredConfigError::Malformed { span, .. } =
        read_stored_config(b"{\"version\":1,\n\"rules\":[").unwrap_err()
    else {
        unreachable!()
    };
    assert_eq!(span.line, 2);
}

#[test]
fn byte_order_mark_is_skipped() {
    let mut bytes = b"\xEF\xBB\xBF".to_vec();
    bytes.extend(serde_json::to_vec_pretty(&Config::default()).unwrap());
    let stored = read_stored_config(&bytes).unwrap();
    assert_eq!(stored.config, Config::default());
    assert!(stored.diagnostics.is_empty());
}

#[test]
fn diagnostics_come_in_reading_order() {
    let stored = read(&json!({
        "enabled": "yes",
        "globals": {"terminators": ["space", "tab"]},
        "rules": [{"id": "x", "match": 1}]
    }));
    assert_eq!(
        codes(&stored.diagnostics),
        [
            (diagnostic_code::FIELD_RESET, "enabled"),
            (diagnostic_code::RULE_UNREADABLE, "rules[0]"),
            (
                diagnostic_code::TERMINATOR_UNSUPPORTED,
                "globals.terminators"
            ),
        ]
    );
}

#[test]
fn compile_config_identifies_newer_schemas() {
    for (version, code) in [
        (2, diagnostic_code::SCHEMA_NEWER),
        (0, diagnostic_code::SCHEMA_VERSION),
    ] {
        let errors = compile_config(&Config {
            version,
            ..Config::default()
        })
        .unwrap_err();
        assert_eq!(errors[0].code, code);
        assert_eq!(errors[0].path, "version");
    }
    assert!(compile_config(&Config {
        version: SUPPORTED_SCHEMA_VERSION,
        ..Config::default()
    })
    .is_ok());
}

// ---- property tests ----

fn extra_key() -> impl Strategy<Value = String> {
    // Never a known field name, so the generated value is what a newer release would add.
    "x_[a-z]{1,6}"
}

/// JSON without floats that do not survive a text round trip.
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        prop_oneof![Just(0.5), Just(-2.25)].prop_map(Value::from),
        ".{0,6}".prop_map(Value::from),
    ];
    let key = prop_oneof![
        3 => "[a-z_]{1,6}",
        1 => prop::sample::select(vec![
            "id", "enabled", "group", "match", "kind", "value", "trigger_mode", "terminator",
            "keys", "consume", "ime_erasure", "action", "type", "method", "source", "plan",
            "steps", "operation", "parameter", "notification", "scope",
        ])
        .prop_map(str::to_string),
    ];
    leaf.prop_recursive(4, 48, 6, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
            prop::collection::btree_map(key.clone(), inner, 0..5)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

fn extras() -> impl Strategy<Value = Map<String, Value>> {
    prop::collection::btree_map(extra_key(), json_value(), 0..3)
        .prop_map(|m| m.into_iter().collect())
}

fn readable_rule() -> impl Strategy<Value = Rule> {
    (
        "[a-z]{1,6}",
        "[a-z;]{1,6}",
        any::<bool>(),
        any::<bool>(),
        extras(),
    )
        .prop_map(|(id, trigger, enabled, regex, extra)| Rule {
            enabled,
            match_spec: MatchSpec {
                kind: if regex {
                    MatchKind::Regex
                } else {
                    MatchKind::Literal
                },
                value: trigger,
            },
            extra,
            ..literal(&id, "")
        })
}

fn unreadable_entry() -> impl Strategy<Value = Value> {
    prop_oneof![
        json_value(),
        ("[a-z]{1,6}", "[a-z]{1,6}").prop_map(|(id, kind)| json!({
            "id": id, "action": {"type": format!("x_{kind}")}
        })),
        ("[a-z]{1,6}", json_value())
            .prop_map(|(id, enabled)| json!({"id": id, "enabled": enabled})),
    ]
    .prop_filter("must not read as a rule", |raw| {
        Rule::deserialize(raw).is_err()
    })
}

fn stored_config() -> impl Strategy<Value = Config> {
    (
        prop::collection::vec(
            prop_oneof![
                readable_rule().prop_map(Ok),
                unreadable_entry().prop_map(Err)
            ],
            0..8,
        ),
        extras(),
        extras(),
        any::<u64>(),
        any::<bool>(),
    )
        .prop_map(|(entries, extra, globals_extra, revision, enabled)| {
            let mut config = Config {
                revision,
                enabled,
                extra,
                ..Config::default()
            };
            config.globals.extra = globals_extra;
            for (index, entry) in entries.into_iter().enumerate() {
                match entry {
                    Ok(rule) => config.rules.push(rule),
                    Err(raw) => config.unreadable_rules.push(UnreadableRule { index, raw }),
                }
            }
            config
        })
}

/// A readable rule with one field replaced or removed, possibly one level down.
fn damaged_rule() -> impl Strategy<Value = Value> {
    let base = prop::sample::select(vec![
        serde_json::to_value(literal("a", "abc")).unwrap(),
        serde_json::to_value(Rule {
            match_spec: MatchSpec {
                kind: MatchKind::Regex,
                value: r"(\d+)kg".into(),
            },
            terminator: Some(Terminator::default()),
            parameter: ParameterSource::CaptureTemplate {
                template: "{$1}".into(),
            },
            action: Action::Text {
                method: OutputMethod::Paste,
                source: ContentSource::File {
                    path: "a.txt".into(),
                    encoding: Encoding::Gbk,
                },
            },
            ..literal("b", "")
        })
        .unwrap(),
        serde_json::to_value(Rule {
            action: Action::Steps {
                plan: parse_sendkeys("^c{LEFT 2}").unwrap(),
            },
            ..literal("c", "x")
        })
        .unwrap(),
    ]);
    let field = prop::sample::select(vec![
        "id",
        "enabled",
        "group",
        "description",
        "ignore_case",
        "match",
        "trigger_mode",
        "terminator",
        "ime_erasure",
        "action",
        "parameter",
        "notification",
        "scope",
        "x_new",
    ]);
    let nested = prop::option::of(prop::sample::select(vec![
        "type", "kind", "value", "keys", "consume", "method", "source", "plan", "template",
        "enabled", "message",
    ]));
    (base, field, nested, prop::option::of(json_value())).prop_map(
        |(mut rule, field, nested, replacement)| {
            let target = match nested {
                Some(nested) => rule
                    .get_mut(field)
                    .and_then(Value::as_object_mut)
                    .map(|object| (object, nested)),
                None => rule.as_object_mut().map(|object| (object, field)),
            };
            if let Some((object, key)) = target {
                match replacement {
                    Some(value) => {
                        object.insert(key.into(), value);
                    }
                    None => {
                        object.remove(key);
                    }
                }
            }
            rule
        },
    )
}

proptest! {
    #[test]
    fn roundtrip_keeps_unknown_fields_and_unreadable_rules(config in stored_config()) {
        let text = serde_json::to_string_pretty(&config).unwrap();
        let stored = read_stored_config(text.as_bytes()).unwrap();
        prop_assert_eq!(&stored.config, &config);
        let expected_codes: Vec<_> = config
            .unreadable_rules
            .iter()
            .map(|_| diagnostic_code::RULE_UNREADABLE)
            .collect();
        let actual_codes: Vec<_> = stored.diagnostics.iter().map(|d| d.code.as_str()).collect();
        prop_assert_eq!(actual_codes, expected_codes);
        prop_assert_eq!(serde_json::to_string_pretty(&stored.config).unwrap(), text.clone());
        // The IPC path keeps the same.
        let ipc: Config = serde_json::from_str(&text).unwrap();
        prop_assert_eq!(ipc, config);
    }

    #[test]
    fn lenient_reading_never_panics_on_damaged_rules(
        entries in prop::collection::vec(prop_oneof![damaged_rule(), json_value()], 0..6),
    ) {
        let source = json!({"version": 1, "rules": entries.clone()});
        let stored = read(&source);
        let config = &stored.config;
        prop_assert_eq!(config.rules.len() + config.unreadable_rules.len(), entries.len());
        for entry in &config.unreadable_rules {
            prop_assert_eq!(&entry.raw, &entries[entry.index]);
        }
        let written = serde_json::to_value(config).unwrap();
        let rules = written["rules"].as_array().unwrap();
        prop_assert_eq!(rules.len(), entries.len());
        for entry in &config.unreadable_rules {
            prop_assert_eq!(&rules[entry.index], &entry.raw);
        }
        // Reading what was written changes nothing further.
        let again = read(&written);
        prop_assert_eq!(&again.config, config);
        prop_assert_eq!(serde_json::to_value(&again.config).unwrap(), written);
        let _ = compile_config(config);
    }

    #[test]
    fn lenient_reading_never_panics_on_damaged_files(
        root in json_value(),
        globals in json_value(),
        rules in json_value(),
        bytes in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let _ = read_stored_config(&bytes);
        let _ = read_stored_config(root.to_string().as_bytes());
        let source = json!({"version": 1, "globals": globals, "rules": rules});
        match read_stored_config(source.to_string().as_bytes()) {
            Ok(stored) => {
                let written = serde_json::to_vec(&stored.config).unwrap();
                let again = read_stored_config(&written).unwrap();
                prop_assert_eq!(again.config, stored.config);
            }
            Err(error) => {
                let malformed = matches!(error, StoredConfigError::Malformed { .. });
                prop_assert!(malformed, "{:?}", error);
                prop_assert!(!rules.is_array());
            }
        }
    }
}
