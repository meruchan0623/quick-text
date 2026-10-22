//! B-20 / S-17: literal before regex, then configured length, then list order.
use proptest::prelude::*;
use quicker_domain::*;
use std::collections::BTreeSet;

fn rule(id: &str, kind: MatchKind, trigger: &str) -> Rule {
    Rule {
        id: id.into(),
        match_spec: MatchSpec {
            kind,
            value: trigger.into(),
        },
        ..Rule::default()
    }
}
fn literal(id: &str, trigger: &str) -> Rule {
    rule(id, MatchKind::Literal, trigger)
}
fn regex(id: &str, pattern: &str) -> Rule {
    rule(id, MatchKind::Regex, pattern)
}
fn compile(rules: Vec<Rule>) -> CompiledConfig {
    compile_config(&Config {
        rules,
        ..Config::default()
    })
    .unwrap()
}
fn winner(rules: &[Rule], input: &str) -> Option<MatchResult> {
    compile(rules.to_vec()).match_input(input, Some("space"), &ForegroundContext::default())
}

#[test]
fn literal_beats_regex_with_longer_match_and_expression() {
    // The regex is listed first, has the longer source and matches more text.
    let rules = [regex("broad", r"\w+ \w+"), literal("abbrev", "ab")];
    let m = winner(&rules, "foo ab").unwrap();
    assert_eq!(m.rule_id, "abbrev");
    assert_eq!(m.erase_chars, 2);
    assert!(m.tied_rule_ids.is_empty());
}

#[test]
fn literals_rank_by_configured_length() {
    let mut short = literal("short", "ab");
    short.trigger_mode = TriggerMode::Immediate;
    let mut long = literal("long", "cab");
    long.trigger_mode = TriggerMode::Immediate;
    let m = compile(vec![short, long])
        .match_input("xcab", None, &ForegroundContext::default())
        .unwrap();
    assert_eq!(m.rule_id, "long");
    assert!(m.tied_rule_ids.is_empty());
}

#[test]
fn regexes_rank_by_expression_length_not_matched_length() {
    // "a+ b" matches all of "aaa b"; "[b-c]+" matches only "b" but its source is longer.
    let rules = [regex("wide_match", "a+ b"), regex("long_source", "[b-c]+")];
    let m = winner(&rules, "aaa b").unwrap();
    assert_eq!(m.rule_id, "long_source");
    assert_eq!(m.matched, "b");
}

#[test]
fn equal_tier_falls_back_to_list_order_and_reports_the_tie() {
    let rules = [
        regex("regex", "a."),
        literal("first", "ab"),
        literal("second", "AB"),
        literal("third", "ab"),
    ];
    let mut rules = rules.to_vec();
    rules[2].ignore_case = true;
    let m = winner(&rules, "ab").unwrap();
    assert_eq!(m.rule_id, "first");
    assert_eq!(m.tied_rule_ids, ["second", "third"]);

    let regexes = [regex("dot_b", ".b"), regex("a_dot", "a."), regex("x", "x")];
    let m = winner(&regexes, "ab").unwrap();
    assert_eq!(m.rule_id, "dot_b");
    assert_eq!(m.tied_rule_ids, ["a_dot"]);
    // The tie list is not serialized when empty, so earlier consumers see the old shape.
    let solo = winner(&[literal("solo", "ab")], "ab").unwrap();
    assert!(serde_json::to_value(&solo)
        .unwrap()
        .get("tied_rule_ids")
        .is_none());
}

#[test]
fn all_three_tiers_together_give_one_deterministic_order() {
    let mut disabled = literal("disabled", "x ab");
    disabled.enabled = false;
    let mut rules = vec![
        regex("regex_long", r"x \w+"),
        regex("regex_short", r"\S+"),
        literal("literal_short", "ab"),
        disabled,
        literal("literal_long", "x ab"),
        literal("literal_long_tie", "x ab"),
        regex("regex_longest", "[a-b]+"),
    ];
    // Lengths: regex_long 5, regex_short 3, regex_longest 6, literal_short 2, literal_long 4.
    let expected = [
        ("literal_long", vec!["literal_long_tie"]),
        ("literal_long_tie", vec![]),
        ("literal_short", vec![]),
        ("regex_longest", vec![]),
        ("regex_long", vec![]),
        ("regex_short", vec![]),
    ];
    for (id, tied) in expected {
        let m = winner(&rules, "x ab").unwrap();
        assert_eq!(m.rule_id, id);
        assert_eq!(m.tied_rule_ids, tied);
        // Same input, same configuration: same answer.
        assert_eq!(winner(&rules, "x ab").unwrap(), m);
        rules.retain(|r| r.id != id);
    }
    assert!(winner(&rules, "x ab").is_none());
}

#[test]
fn priority_values_sort_in_tier_order() {
    let p = |regex, length, order| MatchPriority {
        regex,
        length,
        order,
    };
    let mut values = [
        p(true, 9, 0),
        p(false, 1, 1),
        p(true, 2, 2),
        p(false, 3, 3),
        p(false, 3, 4),
        p(true, 9, 5),
    ];
    values.sort();
    assert_eq!(
        values.iter().map(|v| v.order).collect::<Vec<_>>(),
        [3, 4, 1, 0, 5, 2]
    );
    assert!(p(false, 3, 3).ties_with(&p(false, 3, 4)));
    assert!(!p(false, 3, 3).ties_with(&p(true, 3, 4)));
    assert!(!p(false, 3, 3).ties_with(&p(false, 2, 4)));
    let compiled = compile(vec![literal("a", "ab"), regex("b", "a.")]);
    assert_eq!(compiled.rules[1].priority(), p(true, 2, 1));
    assert_eq!(compiled.rules[1].order(), 1);
}

const PATTERNS: &[&str] = &[
    "a",
    "b",
    "a+",
    "b+",
    "[ab]+",
    "[ab]",
    "a.",
    ".b",
    r"\w+",
    "[ab]{2}",
    "(a|b)+",
    "a ?b",
    r"\w+ \w+",
    "[ab]+ [ab]+",
];

#[derive(Debug, Clone)]
struct Spec {
    regex: bool,
    value: String,
    immediate: bool,
    ignore_case: bool,
}

fn spec() -> impl Strategy<Value = Spec> {
    prop_oneof![
        ("[abAB]( ?[abAB]){0,2}", any::<bool>(), any::<bool>()).prop_map(
            |(value, immediate, ignore_case)| Spec {
                regex: false,
                value,
                immediate,
                ignore_case,
            }
        ),
        (prop::sample::select(PATTERNS), any::<bool>()).prop_map(|(p, ignore_case)| Spec {
            regex: true,
            value: p.into(),
            immediate: false,
            ignore_case,
        }),
    ]
}

fn build(specs: &[Spec]) -> Vec<Rule> {
    specs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let kind = if s.regex {
                MatchKind::Regex
            } else {
                MatchKind::Literal
            };
            let mut r = rule(&format!("r{i}"), kind, &s.value);
            r.ignore_case = s.ignore_case;
            if s.immediate {
                r.trigger_mode = TriggerMode::Immediate;
            }
            r
        })
        .collect()
}

fn run(rules: &[Rule], input: &str, terminator: Option<&str>) -> Option<MatchResult> {
    compile(rules.to_vec()).match_input(input, terminator, &ForegroundContext::default())
}

proptest! {
    #[test]
    fn priority_is_a_total_order_and_selects_the_minimum(
        specs in prop::collection::vec(spec(), 1..8),
        input in "[abAB ]{1,8}",
        terminated in any::<bool>(),
    ) {
        let rules = build(&specs);
        let terminator = terminated.then_some("space");
        let compiled = compile(rules.clone());
        let priorities: Vec<_> = compiled.rules.iter().map(CompiledRule::priority).collect();
        for a in &priorities {
            for b in &priorities {
                prop_assert_eq!(a.cmp(b), b.cmp(a).reverse());
                prop_assert_eq!(a.cmp(b) == std::cmp::Ordering::Equal, a.order == b.order);
                for c in &priorities {
                    if a <= b && b <= c {
                        prop_assert!(a <= c);
                    }
                }
            }
        }
        // Oracle: which rules match on their own, ranked by the specified tiers.
        let mut matching: Vec<_> = rules
            .iter()
            .enumerate()
            .filter(|(_, r)| run(std::slice::from_ref(*r), &input, terminator).is_some())
            .map(|(i, r)| (MatchPriority::of(r, i), r.id.clone()))
            .collect();
        matching.sort();
        let got = compiled.match_input(&input, terminator, &ForegroundContext::default());
        prop_assert_eq!(&got, &compiled.match_input(&input, terminator, &ForegroundContext::default()));
        match matching.first() {
            None => prop_assert!(got.is_none()),
            Some((best, id)) => {
                let got = got.unwrap();
                prop_assert_eq!(&got.rule_id, id);
                let tied: Vec<_> = matching[1..]
                    .iter()
                    .filter(|(p, _)| p.ties_with(best))
                    .map(|(_, id)| id.clone())
                    .collect();
                prop_assert_eq!(got.tied_rule_ids, tied);
            }
        }
    }

    #[test]
    fn reordering_rules_changes_only_the_pick_within_a_tie(
        (specs, permutation) in prop::collection::vec(spec(), 1..8).prop_flat_map(|s| {
            let n = s.len();
            (Just(s), Just((0..n).collect::<Vec<_>>()).prop_shuffle())
        }),
        input in "[abAB ]{1,8}",
    ) {
        let original = build(&specs);
        // Ids stay attached to their rule while the list order changes.
        let reordered: Vec<_> = permutation.iter().map(|&i| original[i].clone()).collect();
        let a = run(&original, &input, Some("space"));
        let b = run(&reordered, &input, Some("space"));
        prop_assert_eq!(a.is_some(), b.is_some());
        if let (Some(a), Some(b)) = (a, b) {
            let rule_of = |id: &str| original.iter().find(|r| r.id == id).unwrap().clone();
            let tier = |id: &str| {
                let r = rule_of(id);
                (r.match_spec.kind == MatchKind::Regex, r.match_spec.value.chars().count())
            };
            prop_assert_eq!(tier(&a.rule_id), tier(&b.rule_id));
            let group = |m: &MatchResult| {
                let mut ids: BTreeSet<String> = m.tied_rule_ids.iter().cloned().collect();
                ids.insert(m.rule_id.clone());
                ids
            };
            prop_assert_eq!(group(&a), group(&b));
        }
    }
}
