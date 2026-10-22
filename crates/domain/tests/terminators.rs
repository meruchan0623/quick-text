//! B-12 / S-12: default terminators and legacy Tab/Enter handling.
use quicker_domain::*;

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

#[test]
fn new_configuration_defaults_to_space_and_semicolon() {
    let config = Config::default();
    assert_eq!(config.globals.terminators, ["space", "semicolon"]);
    assert_eq!(Terminator::default().keys, ["space", "semicolon"]);
    assert!(config.terminator_diagnostics().is_empty());
    assert!(config.unsupported_terminators().is_empty());
}

#[test]
fn legacy_tab_and_enter_are_kept_and_only_warned() {
    // Shape written by 0.3.1 with its default terminator list.
    let raw = r#"{"version":1,"revision":3,"enabled":true,
        "globals":{"terminators":["space","tab","enter","semicolon"],"word_separators":" \t\r\n","blacklist":[],"ime_erasure":"escape"},
        "rules":[{"id":"r","match":{"kind":"literal","value":"abc"}}]}"#;
    let config: Config = serde_json::from_str(raw).unwrap();
    assert_eq!(
        config.globals.terminators,
        ["space", "tab", "enter", "semicolon"]
    );
    let diagnostics = config.terminator_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, diagnostic_code::TERMINATOR_UNSUPPORTED);
    assert_eq!(diagnostics[0].severity, Severity::Warning);
    assert_eq!(diagnostics[0].path, "globals.terminators");
    assert_eq!(config.unsupported_terminators(), ["tab", "enter"]);
    // Behavior is unchanged: the keys still compile and match where a runtime delivers them.
    let compiled = compile_config(&config).unwrap();
    let foreground = ForegroundContext::default();
    for key in ["space", "tab", "enter", "semicolon"] {
        assert!(compiled
            .match_input("abc", Some(key), &foreground)
            .is_some());
    }
    // Written back unchanged.
    let written = serde_json::to_value(&config).unwrap();
    assert_eq!(
        written["globals"]["terminators"],
        serde_json::json!(["space", "tab", "enter", "semicolon"])
    );
}

#[test]
fn rule_overrides_warn_only_where_terminators_apply() {
    let mut terminated = literal("terminated", "a");
    terminated.terminator = Some(Terminator {
        keys: vec!["enter".into()],
        consume: true,
    });
    let mut immediate = literal("immediate", "b");
    immediate.trigger_mode = TriggerMode::Immediate;
    immediate.terminator = Some(Terminator {
        keys: vec!["tab".into()],
        consume: true,
    });
    let config = Config {
        rules: vec![immediate, terminated],
        ..Config::default()
    };
    let diagnostics = config.terminator_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].path, "rules[1].terminator.keys");
    assert_eq!(config.unsupported_terminators(), ["enter"]);
}
