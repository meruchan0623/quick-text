//! B-09: types that carry abbreviations, phrase bodies, typed text, clipboard or selection
//! contents never print them through `Debug`, only `<redacted len=N>`; serde is unchanged.
use quicker_domain::*;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Every sensitive string the fixtures below use. None may appear in any `Debug` output.
const SECRETS: [&str; 16] = [
    "zqabbr",
    "phrase-body",
    "param-secret",
    "template-secret",
    "html-secret",
    "plain-secret",
    "quicker-secret",
    "script-secret",
    "step-secret",
    "extra-secret",
    "global-secret",
    "unreadable-secret",
    "clip-secret",
    "selection-secret",
    "open-secret",
    "arg-secret",
];

fn assert_redacted(shown: &str) {
    for secret in SECRETS {
        assert!(!shown.contains(secret), "{secret} shown in {shown}");
    }
    assert!(
        shown.contains("<redacted len="),
        "nothing redacted in {shown}"
    );
}

fn extra(key: &str, value: &str) -> Map<String, Value> {
    Map::from_iter([(key.to_string(), Value::String(value.to_string()))])
}

fn rule() -> Rule {
    Rule {
        id: "rule-1".into(),
        match_spec: MatchSpec {
            kind: MatchKind::Literal,
            value: "zqabbr".into(),
        },
        trigger_mode: TriggerMode::Immediate,
        action: Action::Text {
            method: OutputMethod::Paste,
            source: ContentSource::inline("phrase-body"),
        },
        parameter: ParameterSource::Fixed {
            value: "param-secret".into(),
        },
        extra: extra("future_field", "extra-secret"),
        ..Rule::default()
    }
}

fn config() -> Config {
    Config {
        revision: 3,
        globals: GlobalSettings {
            extra: extra("future_global", "global-secret"),
            ..GlobalSettings::default()
        },
        rules: vec![rule()],
        unreadable_rules: vec![UnreadableRule {
            index: 1,
            raw: json!({"id": "unreadable", "match": {"value": "unreadable-secret"}}),
        }],
        extra: extra("future_top", "extra-secret"),
        ..Config::default()
    }
}

fn plan() -> ActionPlan {
    ActionPlan {
        dialect: Dialect::Inputscript,
        source: Some("script-secret".into()),
        steps: vec![
            Step::new(
                Operation::Text {
                    text: "step-secret".into(),
                    method: OutputMethod::Keys,
                },
                SourceSpan::default(),
            ),
            Step::new(
                Operation::KeyPress {
                    key: Key::named("enter"),
                },
                SourceSpan::default(),
            ),
        ],
        ..ActionPlan::default()
    }
}

fn parameters() -> BTreeMap<String, Value> {
    BTreeMap::from([("name".to_string(), json!("param-secret"))])
}

fn observed(text: &str, at: u64) -> TextObservation {
    TextObservation {
        revision: 7,
        focus_id: "window:1".into(),
        process: "synthetic.exe".into(),
        control_id: "editor:3".into(),
        caret_anchor: "navigation:1".into(),
        selection_collapsed: true,
        committed_suffix: text.into(),
        composition: CompositionEvidence::Inactive,
        observed_at_ms: at,
        self_injected: false,
    }
}

#[test]
fn helpers_print_only_the_length() {
    assert_eq!(format!("{:?}", Redacted("zqabbr")), "<redacted len=6>");
    // Bytes, not characters.
    assert_eq!(format!("{:?}", Redacted("中文")), "<redacted len=6>");
    assert_eq!(format!("{:?}", RedactedLen(42)), "<redacted len=42>");
    assert_eq!(
        format!("{:?}", RedactedValue(&json!("zqabbr"))),
        "<redacted len=6>"
    );
    assert_eq!(
        format!("{:?}", RedactedValue(&json!({"a": "zqabbr"}))),
        "<redacted len=14>"
    );
    assert_eq!(
        format!("{:?}", RedactedValues(&parameters())),
        r#"{"name": <redacted len=12>}"#
    );
    let optional: Option<String> = Some("zqabbr".into());
    assert_eq!(
        format!("{:?}", optional.as_deref().map(Redacted)),
        "Some(<redacted len=6>)"
    );
}

#[test]
fn configuration_types_redact_abbreviations_phrases_and_unknown_values() {
    let config = config();
    let shown = format!("{config:?}");
    assert_redacted(&shown);
    // Identifiers, kinds and field names stay readable.
    for visible in [
        "rule-1",
        "Literal",
        "Immediate",
        "Paste",
        "future_field",
        "future_global",
        "future_top",
        "revision: 3",
    ] {
        assert!(shown.contains(visible), "{visible} missing from {shown}");
    }
    assert!(shown.contains("MatchSpec { kind: Literal, value: <redacted len=6> }"));
    assert!(shown.contains("Inline { value: <redacted len=11> }"));
    assert!(shown.contains("Fixed { value: <redacted len=12> }"));
    let rule = rule();
    for shown in [
        format!("{rule:?}"),
        format!("{rule:#?}"),
        format!("{:?}", rule.match_spec),
        format!("{:?}", rule.action),
        format!("{:?}", rule.parameter),
        format!("{:?}", config.globals),
        format!("{:?}", config.unreadable_rules),
        format!(
            "{:?}",
            ParameterSource::CaptureTemplate {
                template: "template-secret {$1}".into()
            }
        ),
    ] {
        assert_redacted(&shown);
    }
    let file = ContentSource::File {
        path: "phrases/a.txt".into(),
        encoding: Encoding::Utf8,
    };
    assert_eq!(
        format!("{file:?}"),
        r#"File { path: "phrases/a.txt", encoding: Utf8 }"#
    );
}

#[test]
fn actions_operations_and_plans_redact_their_text() {
    let actions = [
        Action::Html {
            html: "<b>html-secret</b>".into(),
            plain_text: "plain-secret".into(),
        },
        Action::SendKeys {
            source: ContentSource::inline("phrase-body{enter}"),
            syntax_variables: vec!["clipboard".into()],
        },
        Action::Steps { plan: plan() },
        Action::Replay {
            plan: plan(),
            speed: 1.5,
        },
        Action::Reusable {
            action_id: "reusable-1".into(),
            parameters: parameters(),
        },
        Action::Quicker {
            action_id: "quicker-1".into(),
            parameter: "quicker-secret".into(),
        },
        Action::Open {
            target: r"C:\Clients\open-secret.xlsx".into(),
            args: vec!["--user=arg-secret".into()],
            working_directory: Some(r"C:\Work".into()),
        },
    ];
    for action in &actions {
        assert_redacted(&format!("{action:?}"));
        assert_redacted(&format!("{action:#?}"));
    }
    // What an Open opens is redacted, as a list for its arguments; the working directory
    // stays visible (as a file source's path does).
    assert_eq!(
        format!("{:?}", actions[6]),
        r#"Open { target: <redacted len=27>, args: [<redacted len=17>], working_directory: Some("C:\\Work") }"#
    );
    let plan = plan();
    let shown = format!("{plan:?}");
    assert_redacted(&shown);
    // A plan shows its step count only: recorded steps spell out what was typed.
    assert_eq!(
        shown,
        "ActionPlan { version: 1, dialect: Inputscript, source: Some(<redacted len=13>), \
         steps: <redacted len=2> }"
    );
    for operation in [
        Operation::Text {
            text: "step-secret".into(),
            method: OutputMethod::Keys,
        },
        Operation::Html {
            html: "html-secret".into(),
            plain_text: "plain-secret".into(),
        },
        Operation::Reusable {
            action_id: "reusable-1".into(),
            parameters: parameters(),
        },
        Operation::Open {
            target: "https://example.com/?q=open-secret".into(),
            args: vec!["arg-secret".into()],
            working_directory: None,
        },
    ] {
        assert_redacted(&format!("{operation:?}"));
        assert_redacted(&format!("{operation:#?}"));
    }
    assert_redacted(&format!("{:?}", plan.steps));
    // A key operation hides its key (see `recorded_key_sequences_never_show_their_keys`).
    assert_eq!(
        format!(
            "{:?}",
            Operation::KeyPress {
                key: Key::named("enter")
            }
        ),
        "KeyPress { key: <redacted> }"
    );
}

#[test]
fn compiled_rules_and_match_results_redact_the_pattern_and_typed_text() {
    let compiled = compile_config(&config()).unwrap();
    let shown = format!("{compiled:?}");
    assert_redacted(&shown);
    assert!(shown.contains("regex: None"), "{shown}");
    let matched = compiled
        .match_input("zqabbr", None, &ForegroundContext::default())
        .unwrap();
    assert_eq!(matched.matched, "zqabbr");
    let shown = format!("{matched:?}");
    assert_redacted(&shown);
    assert!(shown.contains(r#"rule_id: "rule-1""#), "{shown}");
    let with_captures = MatchResult {
        captures: vec![Some("zqabbr".into()), None],
        ..matched
    };
    let shown = format!("{with_captures:?}");
    assert_redacted(&shown);
    assert!(
        shown.contains("captures: [Some(<redacted len=6>), None]"),
        "{shown}"
    );
}

#[test]
fn variable_context_redacts_clipboard_selection_and_parameter_values() {
    let context = VariableContext {
        clipboard: Some("clip-secret".into()),
        selection: Some("selection-secret".into()),
        parameters: parameters(),
        ..VariableContext::default()
    };
    let shown = format!("{context:?}");
    assert_redacted(&shown);
    assert!(
        shown.contains("clipboard: Some(<redacted len=11>)"),
        "{shown}"
    );
    assert!(shown.contains(r#""name": <redacted len=12>"#), "{shown}");
}

#[test]
fn input_session_types_redact_the_detection_buffer() {
    let mut session = InputSession::new(1024, 250);
    session.observe(observed("existing ", 1), 1);
    let delta = session.observe(observed("existing zqabbr", 2), 2);
    assert_eq!(delta, ObservationDelta::CommittedAppend("zqabbr".into()));
    assert_eq!(session.matching_input(), Some("zqabbr"));
    let proof = session
        .erasure_proof("zqabbr", ImeErasure::Escape, 2)
        .unwrap();
    let composing = CompositionEvidence::Active {
        text: "zqabbr".into(),
        body_unchanged: true,
    };
    for shown in [
        format!("{session:?}"),
        format!("{delta:?}"),
        format!(
            "{:?}",
            ObservationDelta::CompositionUpdated("zqabbr".into())
        ),
        format!("{proof:?}"),
        format!("{composing:?}"),
        format!("{:?}", session.last_observation()),
    ] {
        assert_redacted(&shown);
        assert!(!shown.contains("existing"), "{shown}");
    }
    let shown = format!("{:?}", session.last_observation().unwrap());
    assert!(shown.contains(r#"process: "synthetic.exe""#), "{shown}");
    assert!(
        shown.contains("committed_suffix: <redacted len=15>"),
        "{shown}"
    );
}

#[test]
fn configuration_packages_and_exports_redact_serialized_content() {
    let store = ConfigStore::new(config());
    assert!(store.base_serialized.contains("zqabbr"));
    assert_redacted(&format!("{store:?}"));
    let export = export_espanso_subset(&config());
    assert!(export.yaml.contains("zqabbr") && export.yaml.contains("phrase-body"));
    let shown = format!("{export:?}");
    assert_redacted(&shown);
    let item = MigrationItem {
        rule_id: "imported".into(),
        status: MigrationStatus::NeedsRewrite,
        diagnostics: vec![],
        original: json!({"trigger": "zqabbr", "replace": "phrase-body"}),
    };
    let shown = format!("{item:?}");
    assert_redacted(&shown);
    assert!(shown.contains(r#"rule_id: "imported""#), "{shown}");
}

#[test]
fn serde_shapes_are_unchanged() {
    // Redaction is Debug only: serialization still carries every value.
    let config = config();
    let serialized = serde_json::to_string(&config).unwrap();
    for secret in [
        "zqabbr",
        "phrase-body",
        "param-secret",
        "extra-secret",
        "global-secret",
        "unreadable-secret",
    ] {
        assert!(serialized.contains(secret), "{secret} missing");
    }
    let back: Rule = serde_json::from_value(serde_json::to_value(rule()).unwrap()).unwrap();
    assert_eq!(back, rule());
    let plan_value = serde_json::to_value(plan()).unwrap();
    assert_eq!(plan_value["source"], json!("script-secret"));
    assert_eq!(
        plan_value["steps"][0]["operation"]["text"],
        json!("step-secret")
    );
    let matched = MatchResult {
        rule_id: "rule-1".into(),
        matched: "zqabbr".into(),
        captures: vec![],
        erase_chars: 6,
        parameter: None,
        consume_terminator: false,
        revision: 3,
        tied_rule_ids: vec![],
    };
    assert_eq!(
        serde_json::to_value(&matched).unwrap()["matched"],
        json!("zqabbr")
    );
}

/// A recorded macro is the typed key sequence, one key operation per key: here "SECRET" as
/// the Windows recorder stores it (virtual-key codes 83 69 67 82 69 84) and as the macOS one
/// does (key names). No single operation, step or list of steps shows a key.
#[test]
fn recorded_key_sequences_never_show_their_keys() {
    const CODES: [u16; 6] = [83, 69, 67, 82, 69, 84];
    let recorded = |key: fn(u16) -> Key| -> Vec<Step> {
        CODES
            .iter()
            .flat_map(|&code| {
                [
                    Operation::KeyDown { key: key(code) },
                    Operation::KeyUp { key: key(code) },
                ]
            })
            .chain([Operation::KeyPress { key: key(CODES[0]) }])
            .map(|operation| Step::new(operation, SourceSpan::default()))
            .collect()
    };
    let by_code = recorded(|code| Key::WindowsCode { code });
    let by_name = recorded(|code| Key::named(char::from(code as u8).to_string()));
    for steps in [&by_code, &by_name] {
        let mut shown = vec![format!("{steps:?}"), format!("{steps:#?}")];
        for step in steps {
            shown.extend([
                format!("{step:?}"),
                format!("{step:#?}"),
                format!("{:?}", step.operation),
                format!("{:#?}", step.operation),
            ]);
        }
        let plan = ActionPlan {
            dialect: Dialect::Replay,
            steps: steps.clone(),
            ..ActionPlan::default()
        };
        shown.extend([
            format!("{plan:?}"),
            format!("{plan:#?}"),
            format!("{:?}", Action::Replay { plan, speed: 1.0 }),
        ]);
        for shown in &shown {
            assert!(shown.contains("<redacted"), "{shown}");
            for code in CODES {
                assert!(!shown.contains(&code.to_string()), "{code} in {shown}");
                let letter = format!("{:?}", char::from(code as u8).to_string());
                assert!(!shown.contains(&letter), "{letter} in {shown}");
            }
            assert!(!shown.contains("WindowsCode"), "{shown}");
            assert!(!shown.contains("Named"), "{shown}");
        }
    }
    assert_eq!(
        format!("{:?}", by_code[0]),
        "Step { operation: KeyDown { key: <redacted> }, span: SourceSpan { line: 0, column: 0, \
         length: 0 }, target: Locked }"
    );
    // A configured hotkey is not typed input: its keys stay visible.
    let hotkey = Operation::Hotkey {
        keys: vec![Key::named("ctrl"), Key::WindowsCode { code: 86 }],
    };
    assert_eq!(
        format!("{hotkey:?}"),
        r#"Hotkey { keys: [Named { name: "ctrl" }, WindowsCode { code: 86 }] }"#
    );
}
