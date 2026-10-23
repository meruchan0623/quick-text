//! Per-keystroke matching cost with 1000 rules (B-53; development plan §11 budget: matching
//! p99 < 1 ms). Run: `cargo bench -p quicker-domain --bench matching`, then
//! `scripts/check-bench-budget.ps1` (statistic and threshold: there and in
//! docs/performance-baseline.md).
//!
//! Synthetic and deterministic: a fixed-seed splitmix64 generator builds the same rules and the
//! same typed text on every run and every machine. Nothing is read from files, clocks or the
//! environment. The rule set (1000 rules, 40 families of 25 rules that share a 3-5 letter
//! prefix, so most triggers share their leading characters with 24 others):
//!   - 60 %: terminator-mode literals, prefix + 1-3 letters or digits;
//!   - 10 %: immediate literals, a lead character (`;` `/` `` ` `` `,`) + prefix + suffix;
//!   - 10 %: immediate literals that are those triggers without the lead character, so every
//!     immediate hit has two candidates to rank (S-17);
//!   - 10 %: terminator-mode regexes on the family prefix (`p(\d{1,3})`, `p-[a-z]{2,6}`,
//!     `p_[0-9a-f]{4}`);
//!   - 10 %: case-insensitive terminator-mode literals with a per-rule terminator override;
//!   - every 7th rule is case-insensitive as well; the global blacklist names five processes.
//!
//! One iteration is one keystroke, handled as `match_buffer` in
//! crates/platform/src/windows/rawinput.rs handles it: a terminator key (space, `;`) first tries
//! the terminator rules on the buffer before the key, and if none matched, the immediate rules
//! on the buffer including it; any other character tries the immediate rules. The buffer keeps
//! the last 1024 characters, as there.
//!
//! Benchmarks (group `match_1000_rules`):
//!   - `char_miss`: a letter typed after 64 characters of text, no match;
//!   - `char_immediate_hit`: the last character of an immediate trigger;
//!   - `terminator_miss`: space after a word that is no trigger (both calls; the most frequent
//!     keystroke with a terminator);
//!   - `terminator_hit`, `terminator_regex_hit`: space after a literal / regex trigger;
//!   - `terminator_miss_full_buffer`: the miss path with a full 1024-character buffer;
//!   - `typing_stream`: a typed paragraph with triggers among the words, one keystroke per
//!     iteration, cycling through it.
//!
//! Every case is checked (hit or miss, and which rule wins) before it is measured, so a change in
//! the matcher cannot quietly turn a hit benchmark into a miss.
use criterion::{criterion_group, criterion_main, Criterion, SamplingMode};
use quicker_domain::*;
use std::collections::BTreeSet;
use std::hint::black_box;
use std::time::Duration;

const RULES: usize = 1000;
const FAMILIES: usize = 40;
/// The detection buffer keeps this many characters (crates/platform/src/windows/rawinput.rs).
const BUFFER_CHARS: usize = 1024;
const LEADS: [char; 4] = [';', '/', '`', ','];
const SEED: u64 = 0x5154_4245_4e43_4831;

/// splitmix64: a fixed sequence for a fixed seed, no dependency.
struct SplitMix64(u64);
impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn letters(&mut self, len: usize) -> String {
        (0..len)
            .map(|_| char::from(b'a' + self.below(26) as u8))
            .collect()
    }
    /// 1-3 lowercase letters or digits.
    fn suffix(&mut self) -> String {
        let len = 1 + self.below(3);
        (0..len)
            .map(|_| match self.below(36) {
                n @ 0..=25 => char::from(b'a' + n as u8),
                n => char::from(b'0' + (n - 26) as u8),
            })
            .collect()
    }
    /// Lowercase words of 2-8 letters separated by single spaces, exactly `len` characters,
    /// ending in a letter.
    fn prose(&mut self, len: usize) -> String {
        let mut text = String::new();
        while text.len() < len {
            let word_len = 2 + self.below(7);
            text.push_str(&self.letters(word_len));
            text.push(' ');
        }
        text.truncate(len);
        if text.ends_with(' ') {
            text.pop();
            text.push('e');
        }
        text
    }
}

/// Draws suffixes until every trigger `make` builds from one is unused, then reserves them.
fn unique<const N: usize>(
    rng: &mut SplitMix64,
    used: &mut BTreeSet<String>,
    make: impl Fn(&str) -> [String; N],
) -> [String; N] {
    loop {
        let triggers = make(&rng.suffix());
        if triggers.iter().all(|trigger| !used.contains(trigger)) {
            used.extend(triggers.iter().cloned());
            return triggers;
        }
    }
}

fn rule(index: usize, kind: MatchKind, mode: TriggerMode, value: String) -> Rule {
    Rule {
        id: format!("bench-{index:04}"),
        ignore_case: index.is_multiple_of(7),
        match_spec: MatchSpec { kind, value },
        trigger_mode: mode,
        action: Action::Text {
            method: OutputMethod::Keys,
            source: ContentSource::inline(format!("expansion {index}")),
        },
        ..Rule::default()
    }
}

/// The 1000 rules, and the triggers the hit cases type.
struct Synthetic {
    config: Config,
    families: Vec<String>,
    /// Triggers of the rules, by index (regex rules: the pattern).
    triggers: Vec<String>,
}

fn synthetic(rng: &mut SplitMix64) -> Synthetic {
    let mut families = vec![];
    let mut seen = BTreeSet::new();
    while families.len() < FAMILIES {
        let len = 3 + rng.below(3);
        let prefix = rng.letters(len);
        if seen.insert(prefix.clone()) {
            families.push(prefix);
        }
    }
    let mut used = BTreeSet::new();
    let mut rules = Vec::with_capacity(RULES);
    let mut paired = String::new();
    for index in 0..RULES {
        let block = index / 10;
        let prefix = families[block % FAMILIES].clone();
        let next = match index % 10 {
            0..=5 => {
                let [trigger] = unique(rng, &mut used, |s| [format!("{prefix}{s}")]);
                rule(index, MatchKind::Literal, TriggerMode::Terminator, trigger)
            }
            6 => {
                let lead = LEADS[block % LEADS.len()];
                let [long, short] = unique(rng, &mut used, |s| {
                    [format!("{lead}{prefix}{s}"), format!("{prefix}{s}")]
                });
                paired = short;
                rule(index, MatchKind::Literal, TriggerMode::Immediate, long)
            }
            7 => rule(
                index,
                MatchKind::Literal,
                TriggerMode::Immediate,
                std::mem::take(&mut paired),
            ),
            8 => {
                let pattern = match (block / FAMILIES) % 3 {
                    0 => format!(r"{prefix}(\d{{1,3}})"),
                    1 => format!("{prefix}-[a-z]{{2,6}}"),
                    _ => format!("{prefix}_[0-9a-f]{{4}}"),
                };
                rule(index, MatchKind::Regex, TriggerMode::Terminator, pattern)
            }
            _ => {
                let [trigger] = unique(rng, &mut used, |s| [format!("{prefix}{s}")]);
                let mut next = rule(index, MatchKind::Literal, TriggerMode::Terminator, trigger);
                next.ignore_case = true;
                next.terminator = Some(Terminator {
                    keys: if block % 2 == 0 {
                        vec!["semicolon".into()]
                    } else {
                        vec!["space".into(), "semicolon".into()]
                    },
                    consume: block % 3 != 0,
                });
                next
            }
        };
        rules.push(next);
    }
    let triggers = rules.iter().map(|r| r.match_spec.value.clone()).collect();
    let config = Config {
        revision: 1,
        globals: GlobalSettings {
            blacklist: [
                "keepass.exe",
                "1password.exe",
                "mstsc.exe",
                "vmconnect.exe",
                "putty.exe",
            ]
            .map(String::from)
            .to_vec(),
            ..GlobalSettings::default()
        },
        rules,
        ..Config::default()
    };
    Synthetic {
        config,
        families,
        triggers,
    }
}

/// One keystroke as `match_buffer` receives it.
struct Keystroke {
    /// The buffer before the key and the terminator's name, when the key is a terminator.
    before_terminator: Option<(String, &'static str)>,
    /// The buffer after the key.
    text: String,
}

fn keystroke(buffer: &str, typed: char) -> Keystroke {
    let terminator = match typed {
        ' ' => Some("space"),
        ';' => Some("semicolon"),
        _ => None,
    };
    let mut text = format!("{buffer}{typed}");
    while text.chars().count() > BUFFER_CHARS {
        text.remove(0);
    }
    Keystroke {
        before_terminator: terminator.map(|name| (buffer.to_string(), name)),
        text,
    }
}

/// `match_buffer` (crates/platform/src/windows/rawinput.rs), call for call.
fn process(
    compiled: &CompiledConfig,
    key: &Keystroke,
    foreground: &ForegroundContext,
) -> Option<MatchResult> {
    key.before_terminator
        .as_ref()
        .and_then(|(before, name)| compiled.match_input(before, Some(name), foreground))
        .or_else(|| compiled.match_input(&key.text, None, foreground))
}

/// Types `text` from an empty buffer: one keystroke per character.
fn typed(text: &str) -> Vec<Keystroke> {
    let mut buffer = String::new();
    text.chars()
        .map(|c| {
            let key = keystroke(&buffer, c);
            buffer.clone_from(&key.text);
            key
        })
        .collect()
}

/// Types `word` after `before`: the keystrokes of the word's characters are not measured; the
/// last keystroke is returned.
fn last_keystroke(before: &str, word: &str) -> Keystroke {
    let mut buffer = before.to_string();
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        if chars.peek().is_none() {
            return keystroke(&buffer, c);
        }
        buffer.push(c);
    }
    keystroke(&buffer, ' ')
}

fn winner(result: Option<MatchResult>) -> Option<String> {
    result.map(|matched| matched.rule_id)
}

fn matching(c: &mut Criterion) {
    let mut rng = SplitMix64(SEED);
    let data = synthetic(&mut rng);
    let Ok(compiled) = compile_config(&data.config) else {
        panic!("the synthetic configuration must compile");
    };
    assert_eq!(compiled.rules.len(), RULES);
    let foreground = ForegroundContext {
        platform: Platform::Windows,
        process: "notepad.exe".into(),
        bundle_id: String::new(),
    };
    let text = rng.prose(64);
    let full = rng.prose(BUFFER_CHARS);
    let regex_family = &data.families[10];
    assert_eq!(data.triggers[108], format!(r"{regex_family}(\d{{1,3}})"));

    let cases: [(&str, Keystroke, Option<&str>); 6] = [
        ("char_miss", keystroke(&text, 'e'), None),
        (
            "char_immediate_hit",
            last_keystroke(&format!("{text} "), &data.triggers[506]),
            Some("bench-0506"),
        ),
        ("terminator_miss", keystroke(&text, ' '), None),
        (
            "terminator_hit",
            keystroke(&format!("{text} {}", data.triggers[500]), ' '),
            Some("bench-0500"),
        ),
        (
            "terminator_regex_hit",
            keystroke(&format!("{text} {regex_family}907"), ' '),
            Some("bench-0108"),
        ),
        ("terminator_miss_full_buffer", keystroke(&full, ' '), None),
    ];
    for (name, key, expected) in &cases {
        let got = winner(process(&compiled, key, &foreground));
        assert_eq!(got.as_deref(), *expected, "benchmark case {name}");
    }
    assert_eq!(cases[5].1.text.chars().count(), BUFFER_CHARS);

    // A paragraph: 72 words, every sixth one a trigger (terminator literal, immediate,
    // regex in turn).
    let mut words = vec![];
    for n in 0..72 {
        words.push(match (n % 6, (n / 6) % 3) {
            (5, 0) => data.triggers[(n / 6) * 70 % RULES].clone(),
            (5, 1) => data.triggers[((n / 6) * 70 + 6) % RULES].clone(),
            (5, _) => format!("{}42", data.families[((n / 6) * 7) % 40]),
            _ => {
                let len = 2 + rng.below(7);
                rng.letters(len)
            }
        });
    }
    let stream = typed(&words.join(" "));
    let hits = stream
        .iter()
        .filter(|key| process(&compiled, key, &foreground).is_some())
        .count();
    assert!(
        hits >= 8 && hits < stream.len() / 10,
        "typing stream: {hits} hits in {} keystrokes",
        stream.len()
    );

    let mut group = c.benchmark_group("match_1000_rules");
    // Flat: every sample runs the same number of iterations, so the per-sample means that
    // scripts/check-bench-budget.ps1 ranks are comparable with each other.
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(200);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(6));
    for (name, key, _) in &cases {
        group.bench_function(*name, |b| {
            b.iter(|| process(black_box(&compiled), black_box(key), black_box(&foreground)))
        });
    }
    let mut position = 0;
    group.bench_function("typing_stream", |b| {
        b.iter(|| {
            let key = &stream[position];
            position = (position + 1) % stream.len();
            process(black_box(&compiled), black_box(key), black_box(&foreground))
        })
    });
    group.finish();
}

criterion_group!(benches, matching);
criterion_main!(benches);
