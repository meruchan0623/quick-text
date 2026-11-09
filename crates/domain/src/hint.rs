//! Typing hints for beginners: the rules whose abbreviation starts with the word being typed.
//!
//! Only what the overlay shows is computed here; nothing runs. The typed text and the bodies in
//! the result are user data: never log them (B-09).
use crate::*;
use serde::Serialize;
use std::fmt;

/// At most this many characters of a body go to the overlay.
const PREVIEW_CHARS: usize = 300;

/// One rule the typed word may become.
#[derive(Clone, PartialEq, Serialize)]
pub struct HintItem {
    pub rule_id: String,
    pub abbreviation: String,
    pub description: String,
    /// `"personal"` or `"team"` (a `cloud:` rule).
    pub source: &'static str,
    /// `"immediate"` or `"terminator"`.
    pub trigger: &'static str,
    /// What the rule outputs, as a short text (a body, or a label for other actions).
    pub preview: String,
    /// The preview is a label such as `[图片] qr.png` rather than the body itself.
    pub preview_is_label: bool,
}
impl fmt::Debug for HintItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HintItem")
            .field("rule_id", &self.rule_id)
            .field("abbreviation", &Redacted(&self.abbreviation))
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// The rules for the word being typed, best first.
#[derive(Clone, PartialEq, Serialize)]
pub struct HintMatch {
    /// The typed part every item's abbreviation starts with.
    pub typed: String,
    /// How many rules match, which can exceed `items.len()`.
    pub total: usize,
    pub items: Vec<HintItem>,
}
impl fmt::Debug for HintMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HintMatch")
            .field("typed", &Redacted(&self.typed))
            .field("total", &self.total)
            .field("items", &self.items.len())
            .finish()
    }
}

impl CompiledConfig {
    /// The literal rules whose abbreviation starts with a word at the end of `typed`, or `None`
    /// when there is none. A word starts at the beginning of `typed` or after a word separator,
    /// so ordinary prose (`...the`) does not suggest an abbreviation starting with `e`. Of the
    /// words ending the text, the longest that some abbreviation starts with wins: with `a b`
    /// typed and a rule `a bc`, that rule is offered rather than those starting with `b`.
    ///
    /// Regex rules are left out (they have no text to show). Rules switched off, replaced by a
    /// team phrase or not compiled are not in `self.rules`, so never offered. The configuration's
    /// switch and blacklist apply as for matching.
    pub fn hint_candidates(
        &self,
        typed: &str,
        foreground: &ForegroundContext,
        limit: usize,
    ) -> Option<HintMatch> {
        if !self.config.enabled || typed.is_empty() {
            return None;
        }
        let process = match foreground.platform {
            Platform::Windows => &foreground.process,
            Platform::Macos => &foreground.bundle_id,
        };
        if self
            .config
            .globals
            .blacklist
            .iter()
            .any(|p| p.eq_ignore_ascii_case(process))
        {
            return None;
        }
        let separators = &self.config.globals.word_separators;
        // Byte offsets where a word may start, longest word first.
        let mut starts = vec![0];
        for (index, c) in typed.char_indices() {
            if separators.contains(c) {
                starts.push(index + c.len_utf8());
            }
        }
        for start in starts {
            let word = &typed[start..];
            if word.is_empty() {
                // The text ends with a separator: no word is being typed.
                return None;
            }
            let mut found: Vec<&CompiledRule> = self
                .rules
                .iter()
                .filter(|compiled| starts_with(&compiled.rule, word))
                .collect();
            if found.is_empty() {
                continue;
            }
            let typed_chars = word.chars().count();
            // Exact first (it runs now, or on the terminator), then the fewest characters left,
            // then the matching priority (S-17).
            found.sort_by_key(|compiled| {
                let length = compiled.rule.match_spec.value.chars().count();
                (length != typed_chars, length, compiled.priority())
            });
            let total = found.len();
            let items = found
                .into_iter()
                .take(limit)
                .map(|compiled| item(&compiled.rule))
                .collect();
            return Some(HintMatch {
                typed: word.to_string(),
                total,
                items,
            });
        }
        None
    }
}

fn starts_with(rule: &Rule, word: &str) -> bool {
    if rule.match_spec.kind != MatchKind::Literal {
        return false;
    }
    let abbreviation = &rule.match_spec.value;
    if rule.ignore_case {
        abbreviation
            .to_lowercase()
            .starts_with(&word.to_lowercase())
    } else {
        abbreviation.starts_with(word)
    }
}

fn item(rule: &Rule) -> HintItem {
    let (preview, preview_is_label) = preview(&rule.action);
    HintItem {
        rule_id: rule.id.clone(),
        abbreviation: rule.match_spec.value.clone(),
        description: rule.description.clone(),
        source: if rule.id.starts_with(CLOUD_RULE_PREFIX) {
            "team"
        } else {
            "personal"
        },
        trigger: match rule.trigger_mode {
            TriggerMode::Immediate => "immediate",
            TriggerMode::Terminator => "terminator",
        },
        preview: truncate(&preview),
        preview_is_label,
    }
}

/// The body before expansion (`{cliptext}` stays as it is: nothing is read for a hint).
fn preview(action: &Action) -> (String, bool) {
    let source = |source: &ContentSource| match source {
        ContentSource::Inline { value } => (value.clone(), false),
        ContentSource::File { path, .. } => (format!("[文件] {}", file_name(path)), true),
    };
    match action {
        Action::Text { source: s, .. } | Action::SendKeys { source: s, .. } => source(s),
        Action::Html { plain_text, .. } => (plain_text.clone(), false),
        Action::Hotkey { keys } => (format!("[组合键] {}", keys_label(keys)), true),
        Action::Steps { plan } => (format!("[多步骤] {} 步", plan.steps.len()), true),
        Action::Replay { .. } => ("[录制回放]".into(), true),
        Action::Open { target, .. } => (format!("[打开] {target}"), true),
        Action::Image { name, .. } => (format!("[图片] {name}"), true),
        Action::Common { function } => (format!("[常用功能] {function}"), true),
        Action::Reusable { .. } | Action::Quicker { .. } => ("[动作]".into(), true),
        Action::None => ("[无操作]".into(), true),
    }
}

fn keys_label(keys: &[Key]) -> String {
    keys.iter()
        .map(|key| match key {
            Key::Named { name } => name.clone(),
            Key::WindowsCode { code } => format!("#{code}"),
            Key::PlatformPrimary => "Ctrl/Cmd".into(),
        })
        .collect::<Vec<_>>()
        .join("+")
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn truncate(text: &str) -> String {
    match text.char_indices().nth(PREVIEW_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, abbreviation: &str, body: &str) -> Rule {
        Rule {
            id: id.into(),
            enabled: true,
            match_spec: MatchSpec {
                kind: MatchKind::Literal,
                value: abbreviation.into(),
            },
            action: Action::Text {
                method: OutputMethod::default(),
                source: ContentSource::Inline { value: body.into() },
            },
            ..Rule::default()
        }
    }

    fn compiled(rules: Vec<Rule>) -> CompiledConfig {
        compile_config(&Config {
            rules,
            ..Config::default()
        })
        .unwrap()
    }

    fn windows(process: &str) -> ForegroundContext {
        ForegroundContext {
            platform: Platform::Windows,
            process: process.into(),
            bundle_id: String::new(),
        }
    }

    fn ids(found: &HintMatch) -> Vec<&str> {
        found.items.iter().map(|i| i.rule_id.as_str()).collect()
    }

    #[test]
    fn offers_rules_starting_with_the_word_exact_then_shortest() {
        let config = compiled(vec![
            rule("sig", "thsig", "Best regards"),
            rule("x", "thx", "Thanks"),
            rule("th", "th", "the"),
            rule("other", "brb", "be right back"),
        ]);
        let found = config
            .hint_candidates("th", &windows("notepad.exe"), 10)
            .unwrap();
        assert_eq!(found.typed, "th");
        assert_eq!(found.total, 3);
        assert_eq!(ids(&found), ["th", "x", "sig"]);
        assert_eq!(found.items[1].preview, "Thanks");
        assert_eq!(found.items[1].source, "personal");
    }

    #[test]
    fn only_a_word_counts_and_a_trailing_separator_ends_it() {
        let config = compiled(vec![rule("e", "eml", "mail")]);
        let fg = windows("notepad.exe");
        // `e` ends the word `the`, which no abbreviation starts with.
        assert!(config.hint_candidates("the", &fg, 10).is_none());
        assert!(config.hint_candidates("the e", &fg, 10).is_some());
        assert!(config.hint_candidates("the em ", &fg, 10).is_none());
    }

    #[test]
    fn the_longest_word_some_abbreviation_starts_with_wins() {
        let config = compiled(vec![rule("ab", "a bc", "1"), rule("b", "bz", "2")]);
        let found = config
            .hint_candidates("a b", &windows("x.exe"), 10)
            .unwrap();
        assert_eq!(found.typed, "a b");
        assert_eq!(ids(&found), ["ab"]);
        let found = config
            .hint_candidates("q b", &windows("x.exe"), 10)
            .unwrap();
        assert_eq!(ids(&found), ["b"]);
    }

    #[test]
    fn regex_disabled_blacklisted_and_switched_off_offer_nothing() {
        let mut regex = rule("re", "th.*", "x");
        regex.match_spec.kind = MatchKind::Regex;
        let mut off = rule("off", "thoff", "x");
        off.enabled = false;
        let config = compiled(vec![regex, off]);
        assert!(config
            .hint_candidates("th", &windows("a.exe"), 10)
            .is_none());

        let mut config = compiled(vec![rule("x", "thx", "x")]);
        config.config.globals.blacklist = vec!["Secret.EXE".into()];
        assert!(config
            .hint_candidates("th", &windows("secret.exe"), 10)
            .is_none());
        assert!(config
            .hint_candidates("th", &windows("a.exe"), 10)
            .is_some());
        config.config.enabled = false;
        assert!(config
            .hint_candidates("th", &windows("a.exe"), 10)
            .is_none());
    }

    #[test]
    fn ignore_case_limit_labels_and_truncation() {
        let mut upper = rule("u", "THX", &"字".repeat(400));
        upper.ignore_case = true;
        let mut image = rule("img", "thimg", "");
        image.action = Action::Image {
            asset: format!("{}.png", "a".repeat(64)),
            name: "qr.png".into(),
        };
        let config = compiled(vec![upper, image, rule("z", "thz", "z")]);
        let found = config.hint_candidates("th", &windows("a.exe"), 2).unwrap();
        assert_eq!(found.total, 3);
        assert_eq!(found.items.len(), 2);
        let u = &found.items[0];
        assert_eq!(u.rule_id, "u");
        assert_eq!(u.preview.chars().count(), PREVIEW_CHARS + 1);
        let all = config.hint_candidates("th", &windows("a.exe"), 10).unwrap();
        let img = all.items.iter().find(|i| i.rule_id == "img").unwrap();
        assert_eq!(img.preview, "[图片] qr.png");
        assert!(img.preview_is_label);
    }
}
