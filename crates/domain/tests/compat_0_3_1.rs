//! B-43: configuration written by 0.4 must stay readable by 0.3.1 (plan §9, L3), and 0.4
//! must read what 0.3.1 wrote without losing anything. `compat/model_v0_3_1.rs` is the 0.3.1
//! model verbatim (`git show b359598:crates/domain/src/model.rs`), `deny_unknown_fields` and all.
#[allow(dead_code, clippy::all)]
#[path = "compat/model_v0_3_1.rs"]
mod v0_3_1;

use quicker_domain::*;
use serde_json::json;
use std::collections::BTreeMap;

fn rule(id: &str, action: Action) -> Rule {
    Rule {
        id: id.into(),
        match_spec: MatchSpec {
            kind: MatchKind::Literal,
            value: format!(";{id}"),
        },
        action,
        ..Rule::default()
    }
}

fn plan(dialect: Dialect, steps: Vec<Step>) -> ActionPlan {
    ActionPlan {
        dialect,
        source: Some("source".into()),
        steps,
        ..ActionPlan::default()
    }
}

/// Every operation, key and target policy (and every mouse button and button state).
fn all_steps() -> Vec<Step> {
    let mut operations = vec![
        Operation::Text {
            text: "text".into(),
            method: OutputMethod::Paste,
        },
        Operation::KeyDown {
            key: Key::named("shift"),
        },
        Operation::KeyUp {
            key: Key::WindowsCode { code: 0x10 },
        },
        Operation::KeyPress {
            key: Key::PlatformPrimary,
        },
        Operation::Hotkey {
            keys: vec![Key::PlatformPrimary, Key::named("c")],
        },
        Operation::Delay { ms: 250 },
        Operation::MouseMove {
            x: 0.5,
            y: 12.0,
            relative: true,
            percent: true,
        },
        Operation::Wheel {
            axis: WheelAxis::Horizontal,
            amount: -3,
            unit: WheelUnit::Delta,
        },
        Operation::Wheel {
            axis: WheelAxis::Vertical,
            amount: 1,
            unit: WheelUnit::Clicks,
        },
        Operation::PasteFiles {
            paths: vec![r"C:\a.txt".into()],
        },
        Operation::PasteImage {
            path: r"C:\a.png".into(),
        },
        Operation::Html {
            html: "<b>x</b>".into(),
            plain_text: "x".into(),
        },
        Operation::Open {
            target: "notepad".into(),
            args: vec!["a.txt".into()],
            working_directory: Some(r"C:\".into()),
        },
        Operation::Common {
            function: "lock".into(),
        },
        Operation::Reusable {
            action_id: "reusable".into(),
            parameters: BTreeMap::from([("k".into(), json!({"n": 1}))]),
        },
    ];
    for button in [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::X1,
        MouseButton::X2,
    ] {
        for state in [
            ButtonState::Down,
            ButtonState::Up,
            ButtonState::Click,
            ButtonState::DoubleClick,
        ] {
            operations.push(Operation::MouseButton { button, state });
        }
    }
    operations
        .into_iter()
        .enumerate()
        .map(|(i, operation)| Step {
            operation,
            span: SourceSpan {
                line: i + 1,
                column: 2,
                length: 3,
            },
            target: if i % 2 == 0 {
                TargetPolicy::Locked
            } else {
                TargetPolicy::Declared {
                    process: "app.exe".into(),
                }
            },
        })
        .collect()
}

/// Every action, content source, parameter source and rule-level option.
fn sample() -> Config {
    let file = |encoding| ContentSource::File {
        path: r"C:\snippet.txt".into(),
        encoding,
    };
    let mut rules = vec![
        rule("none", Action::None),
        rule(
            "text",
            Action::Text {
                method: OutputMethod::Keys,
                source: ContentSource::inline("hello"),
            },
        ),
        rule(
            "text-file",
            Action::Text {
                method: OutputMethod::Paste,
                source: file(Encoding::Utf16Le),
            },
        ),
        rule(
            "html",
            Action::Html {
                html: "<i>x</i>".into(),
                plain_text: "x".into(),
            },
        ),
        rule(
            "hotkey",
            Action::Hotkey {
                keys: vec![Key::named("ctrl"), Key::WindowsCode { code: 65 }],
            },
        ),
        rule(
            "sendkeys",
            Action::SendKeys {
                source: file(Encoding::Gbk),
                syntax_variables: vec!["v".into()],
            },
        ),
        rule(
            "sendkeys-inline",
            Action::SendKeys {
                source: file(Encoding::Utf16Be),
                syntax_variables: vec![],
            },
        ),
        rule(
            "steps",
            Action::Steps {
                plan: plan(Dialect::Structured, all_steps()),
            },
        ),
        rule(
            "steps-sendkeys",
            Action::Steps {
                plan: plan(Dialect::SendKeys, vec![]),
            },
        ),
        rule(
            "steps-inputscript",
            Action::Steps {
                plan: plan(Dialect::Inputscript, vec![]),
            },
        ),
        rule(
            "replay",
            Action::Replay {
                plan: plan(Dialect::Replay, all_steps()),
                speed: 1.5,
            },
        ),
        rule(
            "open",
            Action::Open {
                target: "https://example.com".into(),
                args: vec!["--x".into()],
                working_directory: None,
            },
        ),
        rule(
            "reusable",
            Action::Reusable {
                action_id: "a".into(),
                parameters: BTreeMap::from([("p".into(), json!([1, "two", null]))]),
            },
        ),
        rule(
            "common",
            Action::Common {
                function: "screenshot".into(),
            },
        ),
        rule(
            "quicker",
            Action::Quicker {
                action_id: "q".into(),
                parameter: "param".into(),
            },
        ),
    ];
    let parameters = [
        ParameterSource::DefaultMatch,
        ParameterSource::Fixed {
            value: "fixed".into(),
        },
        ParameterSource::FirstCapture,
        ParameterSource::CaptureTemplate {
            template: "q={$1}".into(),
        },
        ParameterSource::Null,
    ];
    for (i, (rule, parameter)) in rules.iter_mut().zip(parameters.iter().cycle()).enumerate() {
        rule.parameter = parameter.clone();
        rule.enabled = i % 3 != 0;
        rule.group = format!("group {}", i % 2);
        rule.description = format!("rule {i}");
        rule.ignore_case = i % 2 == 1;
        if i % 4 == 1 {
            rule.match_spec = MatchSpec {
                kind: MatchKind::Regex,
                value: r"(\d+)kg".into(),
            };
        }
        rule.trigger_mode = if i % 5 == 4 {
            TriggerMode::Immediate
        } else {
            TriggerMode::Terminator
        };
        rule.terminator = (i % 3 == 2).then(|| Terminator {
            keys: vec!["semicolon".into()],
            consume: i % 2 == 0,
        });
        rule.ime_erasure = [None, Some(ImeErasure::Escape), Some(ImeErasure::Backspace)][i % 3];
        rule.notification = Notification {
            enabled: i % 2 == 0,
            message: format!("done {i}"),
        };
    }
    Config {
        revision: 42,
        enabled: false,
        globals: GlobalSettings {
            terminators: vec!["space".into(), "comma".into()],
            word_separators: " ,".into(),
            blacklist: vec!["game.exe".into(), "com.example.app".into()],
            ime_erasure: ImeErasure::Backspace,
            ..GlobalSettings::default()
        },
        rules,
        ..Config::default()
    }
}

/// 0.3.1 reads `text` and writes it back byte for byte.
fn assert_0_3_1_reads(text: &str) {
    let old: v0_3_1::Config = serde_json::from_str(text).expect("0.3.1 must read 0.4 output");
    let rewritten = if text.contains('\n') {
        serde_json::to_string_pretty(&old).unwrap()
    } else {
        serde_json::to_string(&old).unwrap()
    };
    assert_eq!(rewritten, text);
}

#[test]
fn default_config_written_by_0_4_reads_in_0_3_1() {
    let config = Config::default();
    assert_0_3_1_reads(&serde_json::to_string(&config).unwrap());
    let text = serde_json::to_string_pretty(&config).unwrap();
    assert_0_3_1_reads(&text);
    let old: v0_3_1::Config = serde_json::from_str(&text).unwrap();
    assert_eq!(old.globals.terminators, ["space", "semicolon"]);
}

#[test]
fn sample_config_written_by_0_4_reads_in_0_3_1() {
    let config = sample();
    assert_eq!(config.rules.len(), 15);
    assert_0_3_1_reads(&serde_json::to_string(&config).unwrap());
    assert_0_3_1_reads(&serde_json::to_string_pretty(&config).unwrap());
    // What 0.4 itself reads back from that file is again readable by 0.3.1.
    let stored =
        read_stored_config(serde_json::to_string_pretty(&config).unwrap().as_bytes()).unwrap();
    assert_eq!(stored.config, config);
    assert!(stored.diagnostics.is_empty());
    assert_0_3_1_reads(&serde_json::to_string_pretty(&stored.config).unwrap());
}

#[test]
fn config_written_by_0_3_1_reads_in_0_4_without_loss() {
    let old_default = serde_json::to_string_pretty(&v0_3_1::Config::default()).unwrap();
    let mut old_sample: v0_3_1::Config =
        serde_json::from_str(&serde_json::to_string(&sample()).unwrap()).unwrap();
    old_sample.globals.terminators = v0_3_1::Config::default().globals.terminators;
    let old_sample = serde_json::to_string_pretty(&old_sample).unwrap();
    for text in [old_default, old_sample] {
        let stored = read_stored_config(text.as_bytes()).unwrap();
        assert!(stored.config.extra.is_empty());
        assert!(stored.config.globals.extra.is_empty());
        assert!(stored.config.unreadable_rules.is_empty());
        assert!(stored.config.rules.iter().all(|r| r.extra.is_empty()));
        // The 0.3.1 default terminators are kept and only warned about (B-12).
        assert_eq!(
            stored.config.globals.terminators,
            ["space", "tab", "enter", "semicolon"]
        );
        let codes: Vec<_> = stored.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes, [diagnostic_code::TERMINATOR_UNSUPPORTED]);
        assert_eq!(serde_json::to_string_pretty(&stored.config).unwrap(), text);
        let direct: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(direct, stored.config);
    }
}

#[test]
fn legacy_scope_is_dropped_by_both_versions() {
    let mut value = serde_json::to_value(sample()).unwrap();
    value["rules"][1]["scope"] = json!({"windows_processes": ["other.exe"]});
    let text = value.to_string();
    let old: v0_3_1::Config = serde_json::from_str(&text).unwrap();
    let stored = read_stored_config(text.as_bytes()).unwrap();
    assert!(stored.config.rules[1].extra.is_empty());
    let written = serde_json::to_string(&stored.config).unwrap();
    assert!(!written.contains("scope"));
    assert_eq!(written, serde_json::to_string(&old).unwrap());
}

/// The boundary: 0.4 writes back fields and entries it does not know only when it read them,
/// that is, from a file 0.3.1 could not read in the first place (plan §9: 0.5 data needs the
/// `backup-pre-0.5/` copy to go back to 0.3.1).
#[test]
fn fields_from_newer_releases_are_kept_by_0_4_and_rejected_by_0_3_1() {
    let mut value = serde_json::to_value(sample()).unwrap();
    value["groups"] = json!([{"id": "g", "name": "G", "color_index": 1, "order": 0}]);
    value["globals"]["output_default"] = json!("paste");
    value["rules"][0]["word_start_only"] = json!(true);
    let text = serde_json::to_string_pretty(&value).unwrap();
    let stored = read_stored_config(text.as_bytes()).unwrap();
    assert!(stored.diagnostics.is_empty());
    assert_eq!(stored.config.extra.len(), 1);
    assert_eq!(stored.config.globals.extra.len(), 1);
    assert_eq!(stored.config.rules[0].extra.len(), 1);
    // `value` has sorted keys, so compare values; the file 0.4 writes is then stable.
    assert_eq!(serde_json::to_value(&stored.config).unwrap(), value);
    let written = serde_json::to_string_pretty(&stored.config).unwrap();
    let reread = read_stored_config(written.as_bytes()).unwrap().config;
    assert_eq!(serde_json::to_string_pretty(&reread).unwrap(), written);
    assert!(serde_json::from_str::<v0_3_1::Config>(&text).is_err());
    assert!(serde_json::from_str::<v0_3_1::Config>(&written).is_err());
    // Without the newer fields the same configuration is 0.3.1-readable again.
    let mut config = stored.config;
    config.extra.clear();
    config.globals.extra.clear();
    config.rules[0].extra.clear();
    assert_eq!(config, sample());
    assert_0_3_1_reads(&serde_json::to_string_pretty(&config).unwrap());
}
