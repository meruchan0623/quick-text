//! B-43: a rule that does not compile (or an entry this version cannot read) stays off at
//! runtime, including when another rule references it as a reusable action.
use proptest::prelude::*;
use quicker_domain::*;
use serde_json::json;
use std::collections::BTreeMap;

fn text(value: &str) -> Action {
    Action::Text {
        method: OutputMethod::Keys,
        source: ContentSource::inline(value),
    }
}

fn reference(action_id: &str) -> Action {
    Action::Reusable {
        action_id: action_id.into(),
        parameters: BTreeMap::new(),
    }
}

/// Structured steps whose only step references `action_id`.
fn nested(action_id: &str) -> Action {
    Action::Steps {
        plan: ActionPlan {
            steps: vec![Step::new(
                Operation::Reusable {
                    action_id: action_id.into(),
                    parameters: BTreeMap::new(),
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        },
    }
}

fn rule(id: &str, kind: MatchKind, trigger: &str, action: Action) -> Rule {
    Rule {
        id: id.into(),
        match_spec: MatchSpec {
            kind,
            value: trigger.into(),
        },
        action,
        ..Rule::default()
    }
}

fn literal(id: &str, action: Action) -> Rule {
    rule(id, MatchKind::Literal, &format!("{id}{id}"), action)
}

/// A rule whose action is valid but whose regex this engine rejects.
fn skipped(id: &str) -> Rule {
    rule(id, MatchKind::Regex, "(?<=x)y", text("skipped rule ran"))
}

fn load(rules: serde_json::Value) -> LenientCompile {
    let stored = read_stored_config(json!({ "rules": rules }).to_string().as_bytes()).unwrap();
    compile_config_lenient(&stored.config).unwrap()
}

/// The error code from both entry points, which must agree.
fn refusal(result: &LenientCompile, action: &Action) -> String {
    let context = VariableContext::default();
    let compiled = result
        .compiled
        .expand_reusable(action, &context)
        .unwrap_err();
    let free = expand_reusable(action, &result.compiled.config, &context).unwrap_err();
    assert_eq!(compiled, free);
    compiled[0].code.clone()
}

fn expanded_text(result: &LenientCompile, action: &Action) -> String {
    let context = VariableContext::default();
    let compiled = result.compiled.expand_reusable(action, &context).unwrap();
    let free = expand_reusable(action, &result.compiled.config, &context).unwrap();
    assert_eq!(compiled, free);
    match &compiled.steps[..] {
        [step] => match &step.operation {
            Operation::Text { text, .. } => text.clone(),
            other => panic!("unexpected operation {other:?}"),
        },
        steps => panic!("unexpected steps {steps:?}"),
    }
}

#[test]
fn direct_reference_to_a_skipped_rule_is_refused() {
    let result = load(json!([literal("a", reference("b")), skipped("b")]));
    assert_eq!(result.skipped_rule_ids, ["b"]);
    let a = result.compiled.rule("a").unwrap().action.clone();
    assert_eq!(refusal(&result, &a), "action_skipped");
    assert_eq!(refusal(&result, &reference("b")), "action_skipped");
    // Persistence still keeps the skipped rule as written.
    let written = serde_json::to_value(&result.compiled.config).unwrap();
    assert_eq!(written["rules"][1]["id"], "b");
    assert_eq!(written["rules"][1]["match"]["value"], "(?<=x)y");
    assert_eq!(
        written["rules"][1]["action"],
        serde_json::to_value(text("skipped rule ran")).unwrap()
    );
}

#[test]
fn nested_references_to_a_skipped_rule_are_refused() {
    let result = load(json!([
        literal("a", nested("b")),
        literal("c", reference("d")),
        literal("d", nested("b")),
        skipped("b"),
    ]));
    assert_eq!(result.skipped_rule_ids, ["b"]);
    for id in ["a", "c", "d"] {
        let action = result.compiled.rule(id).unwrap().action.clone();
        assert_eq!(refusal(&result, &action), "action_skipped", "{id}");
    }
    let written = serde_json::to_value(&result.compiled.config).unwrap();
    assert_eq!(written["rules"][3]["id"], "b");
    assert_eq!(written["rules"][3]["match"]["value"], "(?<=x)y");
}

#[test]
fn unreadable_entries_cannot_be_referenced() {
    let result = load(json!([
        literal("a", reference("future")),
        literal("c", nested("future")),
        {"id": "future", "action": {"type": "teleport"}},
    ]));
    assert_eq!(result.compiled.config.unreadable_rules.len(), 1);
    for id in ["a", "c"] {
        let action = result.compiled.rule(id).unwrap().action.clone();
        assert_eq!(refusal(&result, &action), "action_unreadable", "{id}");
    }
    let written = serde_json::to_value(&result.compiled.config).unwrap();
    assert_eq!(written["rules"][2]["action"]["type"], "teleport");
}

#[test]
fn available_references_still_expand() {
    let result = load(json!([
        literal("a", reference("ok")),
        literal("c", nested("ok")),
        literal("ok", text("ok")),
        Rule {
            enabled: false,
            ..literal("off", text("off"))
        },
        literal("d", reference("off")),
    ]));
    assert!(result.skipped_rule_ids.is_empty());
    assert_eq!(expanded_text(&result, &reference("a")), "ok");
    assert_eq!(expanded_text(&result, &reference("c")), "ok");
    assert_eq!(refusal(&result, &reference("d")), "action_disabled");
    assert_eq!(refusal(&result, &reference("missing")), "action_unknown");
}

#[test]
fn a_shadowed_duplicate_is_not_reachable() {
    // The second "b" compiles on its own but is a duplicate, so it never runs.
    let result = load(json!([
        literal("a", reference("b")),
        skipped("b"),
        literal("b", text("duplicate ran")),
    ]));
    assert_eq!(result.skipped_rule_ids, ["b", "b"]);
    assert_eq!(refusal(&result, &reference("b")), "action_skipped");
}

fn spec() -> impl Strategy<Value = Rule> {
    (
        "[a-c]{0,1}",
        prop_oneof![Just("x"), Just(""), Just("(?<=a)b")],
        any::<bool>(),
        any::<bool>(),
        prop_oneof![Just(None), "[a-d]".prop_map(Some)],
    )
        .prop_map(|(id, trigger, is_regex, enabled, target)| {
            let kind = if is_regex {
                MatchKind::Regex
            } else {
                MatchKind::Literal
            };
            let action = match target {
                Some(target) => nested(&target),
                None => text("leaf"),
            };
            Rule {
                enabled,
                ..rule(&id, kind, trigger, action)
            }
        })
}

proptest! {
    /// Both entry points resolve exactly the rules `compile_config_lenient` kept.
    #[test]
    fn expansion_agrees_with_the_compiled_rules(rules in prop::collection::vec(spec(), 0..6)) {
        let config = Config { rules, ..Config::default() };
        let lenient = compile_config_lenient(&config).unwrap();
        let context = VariableContext::default();
        for id in ["a", "b", "c", "d"] {
            let action = reference(id);
            let compiled = lenient.compiled.expand_reusable(&action, &context);
            let free = expand_reusable(&action, &lenient.compiled.config, &context);
            prop_assert_eq!(&compiled, &free);
            if lenient.compiled.rule(id).is_none() {
                prop_assert!(compiled.is_err());
            }
        }
    }
}
