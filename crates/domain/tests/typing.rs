//! The shared typed-key buffer (`typing.rs`): a key sequence edits and
//! matches the same way whichever platform classified the keys.
use quicker_domain::*;

fn rule(id: &str, trigger: &str, mode: TriggerMode) -> Rule {
    Rule {
        id: id.into(),
        match_spec: MatchSpec {
            kind: MatchKind::Literal,
            value: trigger.into(),
        },
        trigger_mode: mode,
        ..Rule::default()
    }
}

/// Types `keys` (one edit per key) and returns the last match.
fn type_keys(
    compiled: &CompiledConfig,
    keys: &[KeyEdit],
) -> Option<(String, Option<&'static str>)> {
    let foreground = ForegroundContext::default();
    let mut buffer = String::new();
    let mut last = None;
    for key in keys {
        let previous = typed_terminator(key).map(|name| (buffer.clone(), name));
        apply_key_edit(&mut buffer, key);
        if !matches!(key, KeyEdit::Text(_)) {
            continue;
        }
        last = match_typed(
            compiled,
            &buffer,
            previous.as_ref().map(|(text, name)| (text.as_str(), *name)),
            &foreground,
        )
        .map(|(matched, terminator)| (matched.rule_id, terminator));
        if last.is_some() {
            buffer.clear();
        }
    }
    last
}

fn chars(s: &str) -> Vec<KeyEdit> {
    s.chars().map(|c| KeyEdit::Text(c.to_string())).collect()
}

#[test]
fn a_terminator_rule_matches_its_prefix_before_an_immediate_one() {
    let config = Config {
        rules: vec![
            rule("immediate", "btw ", TriggerMode::Immediate),
            rule("terminated", "btw", TriggerMode::Terminator),
        ],
        ..Config::default()
    };
    let compiled = compile_config(&config).unwrap();
    assert_eq!(
        type_keys(&compiled, &chars("btw ")),
        Some(("terminated".into(), Some("space")))
    );
}

#[test]
fn an_unmatched_terminator_stays_in_the_buffer() {
    let config = Config {
        rules: vec![rule("phrase", "a b", TriggerMode::Immediate)],
        ..Config::default()
    };
    let compiled = compile_config(&config).unwrap();
    assert_eq!(
        type_keys(&compiled, &chars("a b")),
        Some(("phrase".into(), None))
    );
}

#[test]
fn non_ascii_text_and_backspace_edit_the_buffer() {
    let config = Config {
        rules: vec![rule("cafe", "café", TriggerMode::Immediate)],
        ..Config::default()
    };
    let compiled = compile_config(&config).unwrap();
    let mut keys = chars("cafx");
    keys.push(KeyEdit::Backspace);
    keys.push(KeyEdit::Neutral);
    keys.extend(chars("é"));
    assert_eq!(type_keys(&compiled, &keys), Some(("cafe".into(), None)));
}

#[test]
fn navigation_resets_the_prefix() {
    let config = Config {
        rules: vec![rule("abc", "abc", TriggerMode::Immediate)],
        ..Config::default()
    };
    let compiled = compile_config(&config).unwrap();
    let mut keys = chars("ab");
    keys.push(key_edit(KeyClass::Navigation, None, false));
    keys.extend(chars("c"));
    assert_eq!(type_keys(&compiled, &keys), None);
}
