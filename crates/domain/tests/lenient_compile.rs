//! B-43 (`load_or_fallback`): rule-by-rule compilation; a failing rule only stays off.
use proptest::prelude::*;
use quicker_domain::*;
use serde_json::json;

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

fn regex(id: &str, pattern: &str) -> Rule {
    Rule {
        match_spec: MatchSpec {
            kind: MatchKind::Regex,
            value: pattern.into(),
        },
        ..literal(id, "")
    }
}

#[test]
fn failing_rules_are_skipped_and_the_rest_compile() {
    let stored = read_stored_config(
        json!({"rules": [
            literal("a", "aa"),
            regex("lookaround", "(?<=x)y"),
            {"id": "future", "action": {"type": "teleport"}},
            Rule { enabled: false, ..regex("off", "(?<=x)y") },
            literal("a", "dup"),
            literal("b", "bb"),
        ]})
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    assert!(compile_config(&stored.config).is_err());
    let result = compile_config_lenient(&stored.config).unwrap();
    let compiled: Vec<_> = result
        .compiled
        .rules
        .iter()
        .map(|r| r.rule.id.as_str())
        .collect();
    assert_eq!(compiled, ["a", "b"]);
    assert_eq!(result.skipped_rule_ids, ["lookaround", "a"]);
    let codes: Vec<_> = result
        .diagnostics
        .iter()
        .map(|d| (d.code.as_str(), d.path.as_str()))
        .collect();
    // Paths index the serialized `rules` array, which still holds the unreadable entry.
    assert_eq!(
        codes,
        [
            ("regex_unsupported", "rules[1].match.value"),
            ("rule_id", "rules[4].id"),
        ]
    );
    assert_eq!(result.compiled.config, stored.config);
    let foreground = ForegroundContext::default();
    let hit = result
        .compiled
        .match_input("bb", Some("space"), &foreground)
        .unwrap();
    assert_eq!(hit.rule_id, "b");
    assert!(result
        .compiled
        .match_input("y", Some("space"), &foreground)
        .is_none());
}

#[test]
fn configuration_level_errors_still_fail() {
    let newer = Config {
        version: SUPPORTED_SCHEMA_VERSION + 1,
        ..Config::default()
    };
    let errors = compile_config_lenient(&newer).unwrap_err();
    assert_eq!(errors[0].code, diagnostic_code::SCHEMA_NEWER);
    let mut empty_terminator = Config::default();
    empty_terminator.globals.terminators.push(String::new());
    assert!(compile_config_lenient(&empty_terminator).is_err());
}

fn spec() -> impl Strategy<Value = Rule> {
    (
        "[a-c]{0,2}",
        prop_oneof![
            "[a-z]{1,3}",
            Just(String::new()),
            Just("(?<=a)b".to_string())
        ],
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(id, trigger, is_regex, enabled)| Rule {
            enabled,
            ..if is_regex {
                regex(&id, &trigger)
            } else {
                literal(&id, &trigger)
            }
        })
}

proptest! {
    #[test]
    fn lenient_compile_agrees_with_compile_config(rules in prop::collection::vec(spec(), 0..8)) {
        let config = Config { rules, ..Config::default() };
        let lenient = compile_config_lenient(&config).unwrap();
        let enabled = config.rules.iter().filter(|r| r.enabled).count();
        prop_assert_eq!(lenient.compiled.rules.len() + lenient.skipped_rule_ids.len(), enabled);
        match compile_config(&config) {
            Ok(strict) => {
                prop_assert!(lenient.skipped_rule_ids.is_empty());
                prop_assert!(lenient.diagnostics.is_empty());
                prop_assert_eq!(strict.rules.len(), lenient.compiled.rules.len());
            }
            Err(errors) => prop_assert_eq!(errors, lenient.diagnostics),
        }
    }
}
