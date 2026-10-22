use crate::*;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeSet;
#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub rule: Rule,
    regex: Option<Regex>,
    literal: Option<Regex>,
    order: usize,
}
impl CompiledRule {
    /// Position of the rule in the configured list.
    pub fn order(&self) -> usize {
        self.order
    }
    pub fn priority(&self) -> MatchPriority {
        MatchPriority::of(&self.rule, self.order)
    }
}
/// S-17 priority between rules that match the same input; the smallest value wins.
///
/// Three tiers, as in Quicker 2.3.1: literal triggers before regex triggers; within a kind,
/// the longer configured trigger (literal text or regex source, in characters) first;
/// then the earlier rule in the list. `order` is unique per rule, so this is a total order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MatchPriority {
    pub regex: bool,
    pub length: usize,
    pub order: usize,
}
impl MatchPriority {
    pub fn of(rule: &Rule, order: usize) -> Self {
        Self {
            regex: rule.match_spec.kind == MatchKind::Regex,
            length: rule.match_spec.value.chars().count(),
            order,
        }
    }
    /// Same kind and same configured length: only the list order separates the two rules,
    /// which is the conflict reported through `MatchResult::tied_rule_ids`.
    pub fn ties_with(&self, other: &Self) -> bool {
        self.regex == other.regex && self.length == other.length
    }
}
impl Ord for MatchPriority {
    fn cmp(&self, other: &Self) -> Ordering {
        self.regex
            .cmp(&other.regex)
            .then(other.length.cmp(&self.length))
            .then(self.order.cmp(&other.order))
    }
}
impl PartialOrd for MatchPriority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
#[derive(Debug, Clone)]
pub struct CompiledConfig {
    pub config: Config,
    pub rules: Vec<CompiledRule>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchResult {
    pub rule_id: String,
    pub matched: String,
    pub captures: Vec<Option<String>>,
    pub erase_chars: usize,
    pub parameter: Option<String>,
    pub consume_terminator: bool,
    pub revision: u64,
    /// Other rules that matched in the same priority tier and lost only on list order
    /// (S-17), in list order. The runtime records these as a conflict for the health page.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tied_rule_ids: Vec<String>,
}
fn diag(code: &str, message: impl Into<String>, path: &str) -> Diagnostic {
    Diagnostic::error(code, message, SourceSpan::default()).at(path)
}
fn capture_indices(template: &str) -> Result<Vec<usize>, String> {
    let mut captures = vec![];
    let mut remaining = template;
    while let Some(start) = remaining.find("{$") {
        remaining = &remaining[start + 2..];
        let end = remaining.find('}').ok_or("Unclosed capture template")?;
        let index = remaining[..end]
            .parse::<usize>()
            .map_err(|_| "Capture placeholder must be {$N}")?;
        if index == 0 {
            return Err("Capture template requires group 1 or above".into());
        }
        captures.push(index);
        remaining = &remaining[end + 1..];
    }
    Ok(captures)
}
/// Encodes UTF-8 bytes of capture values once. Literal template text is unchanged.
pub fn url_encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
pub fn resolve_parameter(
    source: &ParameterSource,
    matched: &str,
    captures: &[Option<String>],
) -> Result<Option<String>, String> {
    match source {
        ParameterSource::DefaultMatch => Ok(Some(matched.into())),
        ParameterSource::Fixed { value } => Ok(Some(value.clone())),
        ParameterSource::Null => Ok(None),
        ParameterSource::FirstCapture => captures
            .get(1)
            .and_then(Clone::clone)
            .map(Some)
            .ok_or("First capture did not participate".into()),
        ParameterSource::CaptureTemplate { template } => {
            capture_indices(template)?;
            let mut out = String::new();
            let mut tail = template.as_str();
            while let Some(start) = tail.find("{$") {
                out.push_str(&tail[..start]);
                let after = &tail[start + 2..];
                let end = after.find('}').ok_or("Unclosed capture template")?;
                let index: usize = after[..end].parse().map_err(|_| "Invalid capture")?;
                let capture = captures
                    .get(index)
                    .and_then(|s| s.as_ref())
                    .ok_or("Capture unavailable")?;
                out.push_str(&url_encode(capture));
                tail = &after[end + 1..];
            }
            out.push_str(tail);
            Ok(Some(out))
        }
    }
}
impl CompiledConfig {
    pub fn match_input(
        &self,
        input: &str,
        terminator: Option<&str>,
        foreground: &ForegroundContext,
    ) -> Option<MatchResult> {
        if !self.config.enabled {
            return None;
        }
        let foreground_name = match foreground.platform {
            Platform::Windows => &foreground.process,
            Platform::Macos => &foreground.bundle_id,
        };
        if self
            .config
            .globals
            .blacklist
            .iter()
            .any(|p| p.eq_ignore_ascii_case(foreground_name))
        {
            return None;
        }
        let mut matches: Vec<(MatchPriority, MatchResult)> = vec![];
        for compiled in &self.rules {
            let rule = &compiled.rule;
            match rule.trigger_mode {
                TriggerMode::Immediate => {
                    if terminator.is_some() {
                        continue;
                    }
                }
                TriggerMode::Terminator => {
                    let effective = rule
                        .terminator
                        .as_ref()
                        .map(|t| &t.keys)
                        .unwrap_or(&self.config.globals.terminators);
                    if !terminator.is_some_and(|t| effective.iter().any(|x| x == t)) {
                        continue;
                    }
                }
            }
            let re = compiled.regex.as_ref().or(compiled.literal.as_ref())?;
            if let Some(caps) = re.captures(input) {
                let full = caps.get(0)?;
                if full.as_str().is_empty() {
                    continue;
                }
                // Word boundary is based on configured separators, not Rust's Unicode regex boundary.
                if rule.trigger_mode == TriggerMode::Terminator && full.start() > 0 {
                    let before = input[..full.start()].chars().next_back()?;
                    if !self.config.globals.word_separators.contains(before) {
                        continue;
                    }
                }
                let captures: Vec<_> = (0..caps.len())
                    .map(|i| caps.get(i).map(|s| s.as_str().to_string()))
                    .collect();
                let parameter = match resolve_parameter(&rule.parameter, full.as_str(), &captures) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let erase_chars = full.as_str().chars().count();
                matches.push((
                    compiled.priority(),
                    MatchResult {
                        rule_id: rule.id.clone(),
                        matched: full.as_str().into(),
                        captures,
                        erase_chars,
                        parameter,
                        consume_terminator: rule
                            .terminator
                            .as_ref()
                            .map(|t| t.consume)
                            .unwrap_or(true),
                        revision: self.config.revision,
                        tied_rule_ids: vec![],
                    },
                ));
            }
        }
        matches.sort_by_key(|(priority, _)| *priority);
        let mut ranked = matches.into_iter();
        let (best, mut result) = ranked.next()?;
        result.tied_rule_ids = ranked
            .take_while(|(priority, _)| priority.ties_with(&best))
            .map(|(_, r)| r.rule_id)
            .collect();
        Some(result)
    }
    pub fn rule(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.rule.id == id).map(|r| &r.rule)
    }
}
pub fn compile_config(config: &Config) -> Result<CompiledConfig, Vec<Diagnostic>> {
    let mut errors = config_errors(config);
    let mut rules = vec![];
    let mut ids = BTreeSet::new();
    let positions = config.rule_positions();
    for (order, (rule, position)) in config.rules.iter().zip(positions).enumerate() {
        match compile_rule(rule, order, position, &mut ids) {
            Ok(Some(compiled)) => rules.push(compiled),
            Ok(None) => {}
            Err(rule_errors) => errors.extend(rule_errors),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(CompiledConfig {
        config: config.clone(),
        rules,
    })
}
/// Errors that concern the whole configuration rather than one rule.
fn config_errors(config: &Config) -> Vec<Diagnostic> {
    let mut errors = vec![];
    if !(1..=SUPPORTED_SCHEMA_VERSION).contains(&config.version) {
        // B-43: a newer schema has its own code so hosts can open it read-only.
        errors.push(if config.version > SUPPORTED_SCHEMA_VERSION {
            diag(
                diagnostic_code::SCHEMA_NEWER,
                "Configuration was written by a newer release",
                "version",
            )
        } else {
            diag(
                diagnostic_code::SCHEMA_VERSION,
                "Unsupported configuration version",
                "version",
            )
        });
    }
    if config.globals.terminators.iter().any(|t| t.is_empty()) {
        errors.push(diag(
            "terminator",
            "Empty terminator is invalid",
            "globals.terminators",
        ));
    }
    errors
}
/// Compiles one rule; `Ok(None)` for a disabled rule, of which only the ID is checked.
/// `order` is the rule's index in `Config::rules`; `position` its index in the serialized
/// `rules` array (which also holds unreadable entries), used in diagnostic paths.
fn compile_rule(
    rule: &Rule,
    order: usize,
    position: usize,
    ids: &mut BTreeSet<String>,
) -> Result<Option<CompiledRule>, Vec<Diagnostic>> {
    let base = format!("rules[{position}]");
    let mut errors = vec![];
    if rule.id.trim().is_empty() || !ids.insert(rule.id.clone()) {
        errors.push(diag(
            "rule_id",
            "Rule ID must be nonempty and unique",
            &format!("{base}.id"),
        ));
    }
    if !rule.enabled {
        return if errors.is_empty() {
            Ok(None)
        } else {
            Err(errors)
        };
    }
    if rule.match_spec.value.is_empty() {
        errors.push(diag(
            "match_empty",
            "Trigger must not be empty",
            &format!("{base}.match.value"),
        ));
        return Err(errors);
    }
    if rule.match_spec.value.len() > 8192 {
        errors.push(diag(
            "match_limit",
            "Trigger exceeds 8192 bytes",
            &format!("{base}.match.value"),
        ));
        return Err(errors);
    }
    if rule.trigger_mode == TriggerMode::Immediate && rule.match_spec.kind == MatchKind::Regex {
        errors.push(diag(
            "immediate_regex",
            "Immediate mode requires a literal trigger",
            &format!("{base}.trigger_mode"),
        ));
    }
    if rule
        .terminator
        .as_ref()
        .is_some_and(|t| t.keys.is_empty() || t.keys.iter().any(String::is_empty))
    {
        errors.push(diag(
            "terminator",
            "Override requires nonempty terminators; use null to inherit",
            &format!("{base}.terminator"),
        ));
    }
    let pattern = match rule.match_spec.kind {
        MatchKind::Literal => format!("{}$", regex::escape(&rule.match_spec.value)),
        MatchKind::Regex => format!("(?:{})$", rule.match_spec.value),
    };
    let re = match RegexBuilder::new(&pattern)
        .case_insensitive(rule.ignore_case)
        .size_limit(1_048_576)
        .build()
    {
        Ok(re) => re,
        Err(_) => {
            errors.push(diag(
                "regex_unsupported",
                "Invalid/unsupported Rust regex; .NET lookaround and backreferences need rewriting",
                &format!("{base}.match.value"),
            ));
            return Err(errors);
        }
    };
    if re.is_match("") {
        errors.push(diag(
            "empty_match",
            "Regex must not match an empty input",
            &format!("{base}.match.value"),
        ));
    }
    match &rule.parameter {
        ParameterSource::FirstCapture if re.captures_len() < 2 => errors.push(diag(
            "capture_missing",
            "First capture requires regex group 1",
            &format!("{base}.parameter"),
        )),
        ParameterSource::CaptureTemplate { template } => match capture_indices(template) {
            Ok(indices) => {
                if indices.iter().any(|i| *i >= re.captures_len()) {
                    errors.push(diag(
                        "capture_missing",
                        "Template references a nonexistent capture",
                        &format!("{base}.parameter"),
                    ));
                }
            }
            Err(m) => errors.push(diag("capture_template", m, &format!("{base}.parameter"))),
        },
        _ => {}
    }
    for mut d in validate_action(&rule.action) {
        d.path = format!(
            "{base}.action{}",
            if d.path.is_empty() {
                String::new()
            } else {
                format!(".{}", d.path)
            }
        );
        errors.push(d);
    }
    let (regex, literal) = match rule.match_spec.kind {
        MatchKind::Regex => (Some(re), None),
        MatchKind::Literal => (None, Some(re)),
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Some(CompiledRule {
        rule: rule.clone(),
        regex,
        literal,
        order,
    }))
}
pub fn validate_action(action: &Action) -> Vec<Diagnostic> {
    let mut errors = vec![];
    if let Action::SendKeys {
        syntax_variables, ..
    } = action
    {
        errors.extend(validate_syntax_variables(syntax_variables));
    }
    match action {
        Action::SendKeys {
            source: ContentSource::Inline { value },
            ..
        } => {
            let mut dummy = VariableContext {
                clipboard: Some(String::new()),
                selection: Some(String::new()),
                ..VariableContext::default()
            };
            let mut rest = value.as_str();
            while let Some(start) = rest.find("{param:") {
                let after = &rest[start + 7..];
                let Some(end) = after.find('}') else {
                    break;
                };
                dummy.parameters.insert(
                    after[..end].into(),
                    serde_json::Value::String(String::new()),
                );
                rest = &after[end + 1..];
            }
            if let Err(mut e) =
                render_template(value, &dummy, true).and_then(|v| parse_sendkeys(&v))
            {
                errors.append(&mut e);
            }
        }
        Action::Steps { plan } => errors.extend(validate_plan(plan)),
        Action::Replay { plan, speed } => {
            errors.extend(validate_plan(plan));
            if let Err(mut e) = scale_replay(plan, *speed) {
                errors.append(&mut e);
            }
        }
        Action::Hotkey { keys } => validate_keys(keys, &mut errors, SourceSpan::default()),
        Action::Open {
            target,
            args,
            working_directory,
        } => validate_open(target, args, working_directory.as_deref(), &mut errors),
        Action::Text {
            source: ContentSource::File { path, .. },
            ..
        }
        | Action::SendKeys {
            source: ContentSource::File { path, .. },
            ..
        } => {
            if path.is_empty() || path.contains('\0') {
                errors.push(diag(
                    "file_path",
                    "File path must be nonempty and contain no NUL",
                    "source.path",
                ));
            }
        }
        Action::Reusable { action_id, .. } | Action::Quicker { action_id, .. } => {
            if action_id.trim().is_empty() {
                errors.push(diag("action_id", "Action ID is required", "action_id"));
            }
        }
        Action::Common { function } => {
            if function.trim().is_empty() {
                errors.push(diag(
                    "common_function",
                    "Common function is required",
                    "function",
                ));
            }
        }
        _ => {}
    }
    errors
}
fn validate_keys(keys: &[Key], errors: &mut Vec<Diagnostic>, span: SourceSpan) {
    if keys.is_empty() || keys.len() > 8 {
        errors.push(Diagnostic::error(
            "hotkey",
            "Shortcut requires 1..8 keys",
            span.clone(),
        ));
    }
    let mut seen = BTreeSet::new();
    for key in keys {
        if !seen.insert(canonical_key(key)) {
            errors.push(Diagnostic::error(
                "key_duplicate",
                "Duplicate shortcut key",
                span.clone(),
            ));
        }
        validate_key(key, errors, span.clone());
    }
}
fn validate_key(key: &Key, errors: &mut Vec<Diagnostic>, span: SourceSpan) {
    match key {
        Key::Named { name } => {
            if let Err(m) = parse_key(name) {
                errors.push(Diagnostic::error("key", m, span));
            }
        }
        Key::WindowsCode { code } => {
            if !(1..=255).contains(code) {
                errors.push(Diagnostic::error(
                    "key_code",
                    "Windows key code outside 1..255",
                    span,
                ));
            }
        }
        Key::PlatformPrimary => {}
    }
}
fn validate_open(
    target: &str,
    args: &[String],
    directory: Option<&str>,
    errors: &mut Vec<Diagnostic>,
) {
    if target.trim().is_empty()
        || target.contains('\0')
        || args.iter().any(|s| s.contains('\0'))
        || directory.is_some_and(|s| s.contains('\0'))
    {
        errors.push(diag(
            "open_target",
            "Open target/arguments must be valid and contain no NUL",
            "target",
        ));
    }
}
pub fn validate_plan(plan: &ActionPlan) -> Vec<Diagnostic> {
    let mut errors = vec![];
    let mut keys = BTreeSet::new();
    let mut buttons = BTreeSet::new();
    if plan.version != SCHEMA_VERSION {
        errors.push(diag(
            "plan_version",
            "Unsupported action plan version",
            "version",
        ));
    }
    if plan.steps.len() > MAX_STEPS {
        errors.push(diag(
            "step_limit",
            "Action plan exceeds 10000 steps",
            "steps",
        ));
    }
    for (index, step) in plan.steps.iter().enumerate() {
        let mut local = vec![];
        let span = step.span.clone();
        if let TargetPolicy::Declared { process } = &step.target {
            if process.trim().is_empty() {
                local.push(Diagnostic::error(
                    "target",
                    "Declared target must name a process/bundle",
                    span.clone(),
                ));
            }
        }
        match &step.operation {
            Operation::KeyDown { key } => {
                validate_key(key, &mut local, span.clone());
                if !keys.insert(canonical_key(key)) {
                    local.push(Diagnostic::error(
                        "key_pair",
                        "Key is already held by this plan",
                        span.clone(),
                    ));
                }
            }
            Operation::KeyUp { key } => {
                validate_key(key, &mut local, span.clone());
                if !keys.remove(&canonical_key(key)) {
                    local.push(Diagnostic::error(
                        "key_pair",
                        "Key release has no preceding press",
                        span.clone(),
                    ));
                }
            }
            Operation::KeyPress { key } => validate_key(key, &mut local, span.clone()),
            Operation::Hotkey { keys } => validate_keys(keys, &mut local, span.clone()),
            Operation::MouseButton { button, state } => {
                let name = format!("{button:?}");
                match state {
                    ButtonState::Down => {
                        if !buttons.insert(name) {
                            local.push(Diagnostic::error(
                                "button_pair",
                                "Mouse button is already held",
                                span.clone(),
                            ));
                        }
                    }
                    ButtonState::Up => {
                        if !buttons.remove(&name) {
                            local.push(Diagnostic::error(
                                "button_pair",
                                "Mouse release has no preceding press",
                                span.clone(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            Operation::Delay { ms } => {
                if *ms > MAX_DELAY_MS {
                    local.push(Diagnostic::error(
                        "delay_limit",
                        "Delay exceeds one hour",
                        span.clone(),
                    ));
                }
            }
            Operation::MouseMove {
                x,
                y,
                relative,
                percent,
            } => {
                if !x.is_finite()
                    || !y.is_finite()
                    || (*percent
                        && (*relative || !(0.0..=100.0).contains(x) || !(0.0..=100.0).contains(y)))
                {
                    local.push(Diagnostic::error(
                        "coordinate",
                        "Coordinates must be finite; percentages are absolute 0..100",
                        span.clone(),
                    ));
                }
            }
            Operation::PasteFiles { paths } => {
                if paths.is_empty() || paths.iter().any(|s| s.is_empty() || s.contains('\0')) {
                    local.push(Diagnostic::error(
                        "file_path",
                        "File list requires nonempty valid paths",
                        span.clone(),
                    ));
                }
            }
            Operation::PasteImage { path } => {
                if path.is_empty() || path.contains('\0') {
                    local.push(Diagnostic::error(
                        "image_path",
                        "Image path is invalid",
                        span.clone(),
                    ));
                }
            }
            Operation::Open {
                target,
                args,
                working_directory,
            } => validate_open(target, args, working_directory.as_deref(), &mut local),
            _ => {}
        }
        for mut d in local {
            d.path = format!("steps[{index}].{}", d.path);
            errors.push(d);
        }
    }
    if !keys.is_empty() {
        errors.push(diag("key_pair", "Plan ends with held keys", "steps"));
    }
    if !buttons.is_empty() {
        errors.push(diag(
            "button_pair",
            "Plan ends with held mouse buttons",
            "steps",
        ));
    }
    errors
}
