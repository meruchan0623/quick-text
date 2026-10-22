#![allow(clippy::field_reassign_with_default)]
use quicker_domain::*;

#[test]
fn old_b_json_defaults_dynamic_values_to_literal() {
    let action: Action = serde_json::from_str(
        r#"{"type":"send_keys","source":{"type":"inline","value":"{cliptext}"}}"#,
    )
    .unwrap();
    let mut context = VariableContext::default();
    context.clipboard = Some("^c{ENTER}".into());
    let plan = preflight_action(&action, &context).unwrap();
    assert!(matches!(&plan.steps[0].operation,Operation::Text{text,..} if text=="^c{ENTER}"));
}

#[test]
fn b_action_explicit_syntax_source_is_parsed_and_validated() {
    let action = Action::SendKeys {
        source: ContentSource::inline("{cliptext}"),
        syntax_variables: vec!["cliptext".into()],
    };
    let mut context = VariableContext::default();
    context.clipboard = Some("^c".into());
    let plan = preflight_action(&action, &context).unwrap();
    assert!(matches!(plan.steps[0].operation, Operation::Hotkey { .. }));
    context.clipboard = Some("{UNKNOWN_KEY}".into());
    assert!(preflight_action(&action, &context).is_err());
}

#[test]
fn b_syntax_source_does_not_grant_unselected_variables_syntax() {
    let action = Action::SendKeys {
        source: ContentSource::inline("{cliptext}{selection}"),
        syntax_variables: vec!["cliptext".into()],
    };
    let mut context = VariableContext::default();
    context.clipboard = Some("^c".into());
    context.selection = Some("%{TAB}".into());
    let plan = preflight_action(&action, &context).unwrap();
    assert!(matches!(plan.steps[0].operation, Operation::Hotkey { .. }));
    assert!(matches!(&plan.steps[1].operation,Operation::Text{text,..} if text=="%{TAB}"));
}

#[test]
fn invalid_or_duplicate_syntax_sources_are_compile_errors() {
    for names in [
        vec!["unknown".into()],
        vec!["cliptext".into(), "cliptext".into()],
        vec!["param:".into()],
    ] {
        let action = Action::SendKeys {
            source: ContentSource::inline("plain"),
            syntax_variables: names,
        };
        assert!(!validate_action(&action).is_empty());
        assert!(preflight_action(&action, &VariableContext::default()).is_err());
    }
}

#[test]
fn structured_steps_render_variables_once_without_promoting_dynamic_text() {
    let mut context = VariableContext::default();
    context.clipboard = Some("{selection}^c".into());
    context.selection = Some("synthetic private value".into());
    let action = Action::Steps {
        plan: ActionPlan {
            steps: vec![Step::new(
                Operation::Text {
                    text: "{cliptext}".into(),
                    method: OutputMethod::Keys,
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        },
    };
    let plan = expand_reusable(&action, &Config::default(), &context).unwrap();
    assert!(matches!(&plan.steps[0].operation,Operation::Text{text,..} if text=="{selection}^c"));
    let missing = Action::Steps {
        plan: ActionPlan {
            steps: vec![Step::new(
                Operation::Text {
                    text: "{selection}".into(),
                    method: OutputMethod::Keys,
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        },
    };
    assert!(expand_reusable(&missing, &Config::default(), &VariableContext::default()).is_err());
}

#[test]
fn syntax_reusable_parameter_requires_explicit_b_action_setting() {
    let action = Action::SendKeys {
        source: ContentSource::inline("{param:shortcut}"),
        syntax_variables: vec!["param:shortcut".into()],
    };
    let mut context = VariableContext::default();
    context
        .parameters
        .insert("shortcut".into(), serde_json::json!("^c"));
    let plan = preflight_action(&action, &context).unwrap();
    assert!(matches!(plan.steps[0].operation, Operation::Hotkey { .. }));
}
