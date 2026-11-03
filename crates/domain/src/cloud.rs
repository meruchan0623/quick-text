//! Team phrases (「团队话术」, docs/cloud-phrases.md): the read-only phrase table Quicktext
//! downloads from the TSim customer-service system, and how it joins the personal
//! configuration in the engine. Parsing and merging only; the host downloads, caches and
//! decides when to merge. Nothing here is ever written into the personal configuration.
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

/// `format` of a downloaded table (§1).
pub const CLOUD_FORMAT: &str = "quicktext-cloud-phrases";
/// Highest `schema_version` this build reads (§1); a higher one is
/// [`CloudTableError::SchemaNewer`] and the cached table stays in use.
pub const CLOUD_SCHEMA_VERSION: u64 = 1;
/// Prefix of a team phrase's rule id in the effective configuration (§4). History entries,
/// execution results and notifications carry the prefixed id.
pub const CLOUD_RULE_PREFIX: &str = "cloud:";

// Admission limits, the server's own (TSim_CS spec §2.1), enforced here independently, before
// anything is cached or compiled: a table that reaches the client some other way cannot make it
// compile thousands of large patterns, at a sync or at every start from the cache.
/// Most entries in `rules` (readable or not); a longer table is refused whole
/// ([`CloudTableError::TooManyRules`]).
pub const CLOUD_MAX_RULES: usize = 2000;
/// Longest trigger, in characters ([`MATCH_TOO_LONG`]).
pub const CLOUD_MAX_TRIGGER_CHARS: usize = 64;
/// Longest inline text, in characters ([`BODY_TOO_LONG`]).
pub const CLOUD_MAX_BODY_CHARS: usize = 20_000;

/// `cloud:<id>`: a team phrase's id as the engine knows it.
pub fn cloud_rule_id(id: &str) -> String {
    format!("{CLOUD_RULE_PREFIX}{id}")
}

/// The team phrase id behind an engine rule id, `None` for a personal rule.
pub fn team_phrase_id(rule_id: &str) -> Option<&str> {
    rule_id.strip_prefix(CLOUD_RULE_PREFIX)
}

/// A downloaded (or cached) team phrase table.
#[derive(Debug, Clone, PartialEq)]
pub struct CloudTable {
    pub schema_version: u64,
    /// Content version, raised by the server on every save.
    pub revision: u64,
    /// RFC 3339, as the server wrote it; `None` for a table that was never saved.
    pub updated_at: Option<String>,
    /// The entries that read as a [`Rule`], in download order, ids as downloaded.
    pub rules: Vec<Rule>,
    /// Entries that did not read as a [`Rule`] (an unknown action type, a wrongly typed
    /// field, not an object): skipped, never shown or run.
    pub unreadable_count: usize,
}

/// Why a table was refused as a whole. Messages never quote the table's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudTableError {
    /// Not JSON, not an object, the wrong `format`, or a top-level field of the wrong type
    /// (`invalid_format`).
    Malformed { message: String },
    /// `schema_version` above [`CLOUD_SCHEMA_VERSION`] (`schema_newer`): written for a newer
    /// release; the cached table stays in use.
    SchemaNewer { version: u64 },
    /// More than [`CLOUD_MAX_RULES`] entries in `rules` (`too_many_rules`). Refused before
    /// any entry is read; the cached table stays in use.
    TooManyRules { count: usize },
}
impl CloudTableError {
    /// The error code of the sync status (§3).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed { .. } => "invalid_format",
            Self::SchemaNewer { .. } => "schema_newer",
            Self::TooManyRules { .. } => "too_many_rules",
        }
    }
}
impl fmt::Display for CloudTableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { message } => write!(f, "team phrase table is malformed: {message}"),
            Self::SchemaNewer { version } => write!(
                f,
                "team phrase table schema {version} is newer than this release reads (supported: 1..={CLOUD_SCHEMA_VERSION})"
            ),
            Self::TooManyRules { count } => write!(
                f,
                "team phrase table has {count} entries (at most {CLOUD_MAX_RULES})"
            ),
        }
    }
}
impl std::error::Error for CloudTableError {}

fn malformed(message: impl Into<String>) -> CloudTableError {
    CloudTableError::Malformed {
        message: message.into(),
    }
}

/// Reads a downloaded body or the `table` of the cache file (§1). A UTF-8 BOM is skipped.
/// serde's messages can quote the input, so a JSON error is reported by category and
/// position only.
pub fn parse_cloud_table(bytes: &[u8]) -> Result<CloudTable, CloudTableError> {
    let value: Value = serde_json::from_slice(strip_utf8_bom(bytes)).map_err(|e| {
        malformed(format!(
            "not JSON ({:?} at line {}, column {})",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;
    cloud_table_from_value(value)
}

/// [`parse_cloud_table`] for a value already parsed. `format` must be [`CLOUD_FORMAT`],
/// `schema_version` in `1..=CLOUD_SCHEMA_VERSION` (higher: [`CloudTableError::SchemaNewer`]),
/// `revision` an unsigned integer, `updated_at` a string or null and `rules` an array. Each
/// `rules` entry is read with the [`Rule`] deserializer; one that does not read is skipped
/// and counted in [`CloudTable::unreadable_count`], the others are kept.
pub fn cloud_table_from_value(value: Value) -> Result<CloudTable, CloudTableError> {
    let Value::Object(mut fields) = value else {
        return Err(malformed("the root is not an object"));
    };
    if fields.get("format").and_then(Value::as_str) != Some(CLOUD_FORMAT) {
        return Err(malformed(format!("format is not {CLOUD_FORMAT}")));
    }
    let schema_version = match fields.get("schema_version").and_then(Value::as_u64) {
        Some(version) if version > CLOUD_SCHEMA_VERSION => {
            return Err(CloudTableError::SchemaNewer { version })
        }
        Some(version) if version >= 1 => version,
        _ => return Err(malformed("schema_version is not a positive integer")),
    };
    let revision = fields
        .get("revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| malformed("revision is not an unsigned integer"))?;
    let updated_at = match fields.get("updated_at") {
        None | Some(Value::Null) => None,
        Some(Value::String(at)) => Some(at.clone()),
        Some(_) => return Err(malformed("updated_at is not a string or null")),
    };
    let Some(Value::Array(entries)) = fields.remove("rules") else {
        return Err(malformed("rules is not an array"));
    };
    if entries.len() > CLOUD_MAX_RULES {
        return Err(CloudTableError::TooManyRules {
            count: entries.len(),
        });
    }
    let mut rules = Vec::with_capacity(entries.len());
    let mut unreadable_count = 0;
    for raw in entries {
        match Rule::deserialize(&raw) {
            Ok(rule) => rules.push(rule),
            Err(_) => unreadable_count += 1,
        }
    }
    Ok(CloudTable {
        schema_version,
        revision,
        updated_at,
        rules,
        unreadable_count,
    })
}

/// Where a team phrase stands after [`merge_cloud`] (§4), decided in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudRuleStatus {
    /// In the engine as `cloud:<id>`. An enabled personal rule with the same trigger does not
    /// run then: the team phrase wins (`CloudRuleState::overrides`).
    Active,
    /// `enabled` is false in the downloaded table.
    DisabledRemote,
    /// Switched off on this computer (`app.cloud.disabled_ids`).
    DisabledLocal,
    /// Does not compile (or is not a plain text phrase); `problem` says why.
    Invalid,
}

/// One team phrase as the frontend lists it (`CloudState.rules`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CloudRuleState {
    /// The downloaded id, without [`CLOUD_RULE_PREFIX`].
    pub id: String,
    /// The rule exactly as downloaded (its `id` unprefixed too).
    pub rule: Rule,
    pub status: CloudRuleStatus,
    /// Whether the id is in `disabled_ids`, whatever the status (a phrase the server
    /// switched off can be switched off locally too).
    pub disabled_local: bool,
    /// For [`CloudRuleStatus::Active`]: the ids of the enabled personal rules with the same
    /// trigger, which the phrase replaces: they are not in the engine while it is active.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<String>,
    /// For [`CloudRuleStatus::Invalid`]: why, as text shown to the user (Chinese, one
    /// sentence per reason, joined by `；`). Never quotes the trigger or the phrase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// For [`CloudRuleStatus::Invalid`]: the diagnostic codes behind `problem`, for logs.
    /// Not serialized.
    #[serde(skip)]
    pub problem_codes: Vec<String>,
}

/// The engine's configuration with the team phrases merged in, and each phrase's status.
#[derive(Debug, Clone)]
pub struct CloudMerge {
    /// The effective configuration (`compiled.config`) and its compiled rules: the personal
    /// configuration's version, revision, `enabled` and globals, its rules in their order
    /// (except the ones an active team phrase replaces), then the active team phrases as
    /// `cloud:<id>` in download order. Without a table or with an empty one this is exactly
    /// the personal compilation.
    pub compiled: CompiledConfig,
    /// Every readable team phrase, in download order.
    pub rules: Vec<CloudRuleState>,
}

/// Diagnostic code of a team phrase that is not a plain text phrase.
pub const ACTION_NOT_ALLOWED: &str = "action_not_allowed";

/// Team phrases only paste or type their own inline text, or send their own inline key
/// combination (the server allows nothing else, TSim_CS spec §2.1). Anything else (opening
/// programs, reading local files, key combinations with syntax variables,
/// reusable actions) is refused here as well, so a table that reaches the client some other
/// way still cannot run more than text. The text itself must not read local data either
/// ([`cloud_text_reads_local_data`]).
pub fn cloud_action_allowed(action: &Action) -> bool {
    cloud_inline_text(action).is_some()
}

/// The inline text of a team phrase action: plain text, or a key combination
/// (`send_keys`, e.g. `您好{Enter}`) without syntax variables, so the keys come only
/// from the phrase itself. `None` for anything else.
fn cloud_inline_text(action: &Action) -> Option<&str> {
    match action {
        Action::Text {
            source: ContentSource::Inline { value },
            ..
        } => Some(value),
        Action::SendKeys {
            source: ContentSource::Inline { value },
            syntax_variables,
        } if syntax_variables.is_empty() => Some(value),
        _ => None,
    }
}

/// Merges `table` into the compiled personal configuration (§4).
///
/// Each readable phrase gets the first status that applies: `disabled_remote` (`enabled`
/// false), `disabled_local` (its id in `disabled_ids`), `invalid`, else `active`. `invalid`
/// covers an empty id, a `cloud:<id>`
/// a personal rule already uses, an action other than inline text
/// ([`cloud_action_allowed`]), and the admission limits: a regex trigger
/// ([`REGEX_NOT_ALLOWED`]), a trigger over [`CLOUD_MAX_TRIGGER_CHARS`] or a text over
/// [`CLOUD_MAX_BODY_CHARS`] characters, text that reads local data
/// ([`cloud_text_reads_local_data`]). All of these are decided before anything is
/// compiled; only the phrases left are then compiled by [`compile_config_lenient`] (on
/// their own, with the personal version and globals, so a duplicate id inside the table is
/// refused for the later entry), and what it refuses is `invalid` too. With at most
/// [`CLOUD_MAX_RULES`] literal triggers of at most [`CLOUD_MAX_TRIGGER_CHARS`] characters,
/// that compile stays small.
///
/// A team phrase that ends up `active` wins over a personal rule with the same trigger: an
/// enabled personal rule with the same `match.kind` and the same `match.value` (compared in
/// lower case when either side ignores case) is left out of the effective configuration, and
/// the phrase lists its id in `overrides`. Switching the phrase off (remotely, on this
/// computer) or having it refused (`invalid`) brings the personal rule back, since only a
/// phrase that really runs replaces anything. The other personal rules keep their compiled
/// form and order, and the active phrases follow them, so between overlapping triggers the
/// existing priority holds (literal before regex, so a team phrase beats a personal regex
/// on the same input; longer before shorter).
pub fn merge_cloud(
    personal: &CompiledConfig,
    table: Option<&CloudTable>,
    disabled_ids: &BTreeSet<String>,
) -> CloudMerge {
    let Some(table) = table.filter(|table| !table.rules.is_empty()) else {
        return CloudMerge {
            compiled: personal.clone(),
            rules: vec![],
        };
    };
    let (mut states, candidates) = classify(&personal.config, table, disabled_ids);
    let candidate_config = Config {
        version: personal.config.version,
        revision: personal.config.revision,
        enabled: personal.config.enabled,
        globals: personal.config.globals.clone(),
        rules: candidates.iter().map(|(_, rule)| rule.clone()).collect(),
        ..Config::default()
    };
    let mut compiled = personal.clone();
    // The compiled candidates, which are the active phrases (below).
    let mut active = vec![];
    match compile_config_lenient(&candidate_config) {
        Err(errors) => {
            // Only errors of the whole configuration (version, global terminators), which the
            // personal configuration shares: no phrase can run.
            let codes: Vec<&str> = errors.iter().map(|d| d.code.as_str()).collect();
            for (index, _) in &candidates {
                states[*index].mark_invalid(&codes);
            }
        }
        Ok(lenient) => {
            // The candidate config has no unreadable entries, so a diagnostic's
            // `rules[<n>]` is the candidate's index.
            let mut by_position: BTreeMap<usize, Vec<Diagnostic>> = BTreeMap::new();
            for diagnostic in lenient.diagnostics {
                if let Some(position) = rule_position(&diagnostic.path) {
                    by_position.entry(position).or_default().push(diagnostic);
                }
            }
            for (position, diagnostics) in &by_position {
                if let Some((index, _)) = candidates.get(*position) {
                    let codes: Vec<&str> = diagnostics.iter().map(|d| d.code.as_str()).collect();
                    states[*index].mark_invalid(&codes);
                }
            }
            // Every compiled candidate is active: it passed every earlier check and has no
            // diagnostic.
            active = lenient.compiled.rules;
        }
    }
    // Only now is it known which phrases are active, so which personal rules they replace.
    let replaced = mark_overrides(&personal.config.rules, &mut states);
    remove_replaced(&mut compiled, &replaced);
    for rule in active {
        // Its order is its index in the effective `rules`.
        let order = compiled.config.rules.len();
        compiled.config.rules.push(rule.rule.clone());
        compiled.rules.push(rule.at_order(order));
    }
    CloudMerge {
        compiled,
        rules: states,
    }
}

/// The statuses [`merge_cloud`] would give, without compiling anything: a phrase that passes
/// every check before compiling is `active` here even if it would not compile (and then lists
/// the personal rules it would replace). For safe mode (the store), where team phrases do not
/// run: the list stays visible and nothing is compiled; leaving safe mode merges, and
/// compiles, for real.
pub fn cloud_rule_states(
    personal: &Config,
    table: Option<&CloudTable>,
    disabled_ids: &BTreeSet<String>,
) -> Vec<CloudRuleState> {
    table.map_or_else(Vec::new, |table| {
        let (mut states, _) = classify(personal, table, disabled_ids);
        mark_overrides(&personal.rules, &mut states);
        states
    })
}

/// Every check of [`merge_cloud`] before compiling: the states, and the candidates still to
/// compile as (index into the states, the rule as the engine would run it).
fn classify(
    personal: &Config,
    table: &CloudTable,
    disabled_ids: &BTreeSet<String>,
) -> (Vec<CloudRuleState>, Vec<(usize, Rule)>) {
    let personal_ids: BTreeSet<&str> = personal.rules.iter().map(|rule| rule.id.as_str()).collect();
    let mut states = Vec::with_capacity(table.rules.len());
    let mut candidates: Vec<(usize, Rule)> = vec![];
    for rule in &table.rules {
        let disabled_local = disabled_ids.contains(&rule.id);
        let mut state = CloudRuleState {
            id: rule.id.clone(),
            rule: rule.clone(),
            status: CloudRuleStatus::Active,
            disabled_local,
            overrides: vec![],
            problem: None,
            problem_codes: vec![],
        };
        if !rule.enabled {
            state.status = CloudRuleStatus::DisabledRemote;
        } else if disabled_local {
            state.status = CloudRuleStatus::DisabledLocal;
        } else if let Some(code) = precheck(rule, &personal_ids) {
            state.mark_invalid(&[code]);
        } else {
            candidates.push((
                states.len(),
                Rule {
                    id: cloud_rule_id(&rule.id),
                    ..rule.clone()
                },
            ));
        }
        states.push(state);
    }
    (states, candidates)
}

impl CloudRuleState {
    fn mark_invalid(&mut self, codes: &[&str]) {
        self.status = CloudRuleStatus::Invalid;
        let mut seen = BTreeSet::new();
        let codes: Vec<&str> = codes.iter().copied().filter(|c| seen.insert(*c)).collect();
        let mut texts = BTreeSet::new();
        self.problem = Some(
            codes
                .iter()
                .map(|code| cloud_problem_text(code))
                .filter(|text| texts.insert(text.clone()))
                .collect::<Vec<_>>()
                .join("；"),
        );
        self.problem_codes = codes.into_iter().map(str::to_string).collect();
    }
}

/// Diagnostic code of a team phrase whose id is empty, repeated in the table, or already
/// used (as `cloud:<id>`) by a personal rule.
pub const CLOUD_RULE_ID: &str = "rule_id";

/// Diagnostic code of a team phrase whose text would pull data from this computer into
/// what it types ([`cloud_text_reads_local_data`]).
pub const LOCAL_DATA_PLACEHOLDER: &str = "local_data_placeholder";

/// Whether a team phrase's inline text contains a placeholder that `render_template` fills
/// from this computer: `{cliptext}` (the clipboard), `{selection}` (the selected text) or
/// `{param:…}` (action parameters). A phrase that someone else writes must only type fixed
/// text, so these are refused; the check is a plain substring test and errs on the side of
/// refusing. The date placeholder `{0:…}` stays allowed.
pub fn cloud_text_reads_local_data(text: &str) -> bool {
    text.contains("{cliptext}") || text.contains("{selection}") || text.contains("{param:")
}

/// Diagnostic code of a team phrase with a regex trigger: refused before compiling, so a
/// table cannot make the client build large patterns.
pub const REGEX_NOT_ALLOWED: &str = "regex_not_allowed";
/// Diagnostic code of a trigger over [`CLOUD_MAX_TRIGGER_CHARS`] characters.
pub const MATCH_TOO_LONG: &str = "match_too_long";
/// Diagnostic code of an inline text over [`CLOUD_MAX_BODY_CHARS`] characters.
pub const BODY_TOO_LONG: &str = "body_too_long";

/// The checks made before compiling (what compiling the candidates on their own cannot see,
/// and the admission limits, so a phrase over them is never compiled); the diagnostic code.
fn precheck(rule: &Rule, personal_ids: &BTreeSet<&str>) -> Option<&'static str> {
    if rule.id.trim().is_empty() || personal_ids.contains(cloud_rule_id(&rule.id).as_str()) {
        return Some(CLOUD_RULE_ID);
    }
    let Some(value) = cloud_inline_text(&rule.action) else {
        return Some(ACTION_NOT_ALLOWED);
    };
    if rule.match_spec.kind != MatchKind::Literal {
        return Some(REGEX_NOT_ALLOWED);
    }
    if rule.match_spec.value.chars().count() > CLOUD_MAX_TRIGGER_CHARS {
        return Some(MATCH_TOO_LONG);
    }
    if value.chars().count() > CLOUD_MAX_BODY_CHARS {
        return Some(BODY_TOO_LONG);
    }
    cloud_text_reads_local_data(value).then_some(LOCAL_DATA_PLACEHOLDER)
}

/// What the user reads for one diagnostic code of a team phrase (`CloudRuleState::problem`).
/// The codes are [`compile_config`]'s plus [`ACTION_NOT_ALLOWED`]; an unknown one is named.
pub fn cloud_problem_text(code: &str) -> String {
    match code {
        CLOUD_RULE_ID => "话术编号为空、重复，或与我的指令的编号冲突".into(),
        ACTION_NOT_ALLOWED => "团队话术只能粘贴、键入纯文本或使用文字与按键组合".into(),
        REGEX_NOT_ALLOWED => "团队话术只支持普通缩写，不支持正则".into(),
        MATCH_TOO_LONG => format!("缩写超过 {CLOUD_MAX_TRIGGER_CHARS} 个字符"),
        BODY_TOO_LONG => format!("正文超过 {CLOUD_MAX_BODY_CHARS} 个字符"),
        LOCAL_DATA_PLACEHOLDER => {
            "正文含 {cliptext}、{selection} 或 {param:…}，团队话术只输出固定文字".into()
        }
        "match_empty" => "缩写为空".into(),
        "match_limit" => "缩写过长".into(),
        "immediate_regex" => "「立即执行」只能用于普通缩写，不能用于正则".into(),
        "terminator" => "结束键设置无效".into(),
        "regex_unsupported" => "正则表达式无效或不受支持".into(),
        "empty_match" => "正则表达式会匹配空输入".into(),
        "capture_missing" | "capture_template" => "参数设置引用了不存在的捕获组".into(),
        diagnostic_code::SCHEMA_NEWER | diagnostic_code::SCHEMA_VERSION => {
            "我的指令的配置版本不受支持，团队话术暂不可用".into()
        }
        other => format!("无法使用（{other}）"),
    }
}

/// `n` of a diagnostic path `rules[n]` or `rules[n].<field>`.
fn rule_position(path: &str) -> Option<usize> {
    let rest = path.strip_prefix("rules[")?;
    rest[..rest.find(']')?].parse().ok()
}

/// Which enabled personal rules the active team phrases replace (§4): those with the same
/// `match.kind` and the same `match.value`, compared in lower case when either side ignores
/// case. Each active phrase gets the ids of the rules it replaces in `overrides` (in personal
/// order); the result is their positions in `personal`, a rule counted once however many
/// phrases cover it. Phrases in any other status replace nothing.
fn mark_overrides(personal: &[Rule], states: &mut [CloudRuleState]) -> BTreeSet<usize> {
    // Only rules whose lower-case values are equal can be the same trigger.
    let mut by_trigger: HashMap<(bool, String), Vec<usize>> = HashMap::new();
    for (position, rule) in personal.iter().enumerate().filter(|(_, rule)| rule.enabled) {
        let regex = rule.match_spec.kind == MatchKind::Regex;
        by_trigger
            .entry((regex, rule.match_spec.value.to_lowercase()))
            .or_default()
            .push(position);
    }
    let mut replaced = BTreeSet::new();
    if by_trigger.is_empty() {
        return replaced;
    }
    for state in states
        .iter_mut()
        .filter(|state| state.status == CloudRuleStatus::Active)
    {
        let regex = state.rule.match_spec.kind == MatchKind::Regex;
        let value = state.rule.match_spec.value.as_str();
        let Some(positions) = by_trigger.get(&(regex, value.to_lowercase())) else {
            continue;
        };
        for &position in positions {
            let mine = &personal[position];
            if state.rule.ignore_case || mine.ignore_case || mine.match_spec.value == value {
                state.overrides.push(mine.id.clone());
                replaced.insert(position);
            }
        }
    }
    replaced
}

/// Takes the personal rules at `replaced` (positions in the personal `config.rules`) out of
/// the effective configuration. The rules after them move up, so each compiled rule's `order`
/// is renumbered to stay its index in `config.rules`.
fn remove_replaced(compiled: &mut CompiledConfig, replaced: &BTreeSet<usize>) {
    if replaced.is_empty() {
        return;
    }
    let mut moved_to = Vec::with_capacity(compiled.config.rules.len());
    let mut next = 0;
    for position in 0..compiled.config.rules.len() {
        if replaced.contains(&position) {
            moved_to.push(None);
        } else {
            moved_to.push(Some(next));
            next += 1;
        }
    }
    let mut position = 0;
    compiled.config.rules.retain(|_| {
        position += 1;
        moved_to[position - 1].is_some()
    });
    compiled.rules = std::mem::take(&mut compiled.rules)
        .into_iter()
        .filter_map(|rule| {
            let order = moved_to.get(rule.order()).copied().flatten()?;
            Some(rule.at_order(order))
        })
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_rule(id: &str, trigger: &str, body: &str) -> Rule {
        Rule {
            id: id.into(),
            match_spec: MatchSpec {
                kind: MatchKind::Literal,
                value: trigger.into(),
            },
            action: Action::Text {
                method: OutputMethod::Paste,
                source: ContentSource::inline(body),
            },
            ..Rule::default()
        }
    }
    fn table(rules: Vec<Rule>) -> CloudTable {
        CloudTable {
            schema_version: 1,
            revision: 12,
            updated_at: Some("2026-10-28T08:30:00+00:00".into()),
            rules,
            unreadable_count: 0,
        }
    }
    fn personal(rules: Vec<Rule>) -> CompiledConfig {
        compile_config(&Config {
            revision: 7,
            rules,
            ..Config::default()
        })
        .unwrap()
    }
    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }
    fn statuses(merge: &CloudMerge) -> Vec<(String, CloudRuleStatus)> {
        merge
            .rules
            .iter()
            .map(|state| (state.id.clone(), state.status))
            .collect()
    }
    fn wire_rule(id: &str, trigger: &str) -> Value {
        json!({
            "id": id,
            "enabled": true,
            "group": "退款",
            "description": "退款到账时间",
            "ignore_case": false,
            "match": { "kind": "literal", "value": trigger },
            "trigger_mode": "terminator",
            "terminator": null,
            "ime_erasure": null,
            "action": { "type": "text", "method": "paste", "source": { "type": "inline", "value": "您好，退款一般 3–5 个工作日原路退回。" } },
            "parameter": { "type": "default_match" },
            "notification": { "enabled": false, "message": "" }
        })
    }
    fn wire_table(rules: Vec<Value>) -> Value {
        json!({
            "format": "quicktext-cloud-phrases",
            "schema_version": 1,
            "revision": 12,
            "updated_at": "2026-10-28T08:30:00+00:00",
            "rules": rules
        })
    }
    fn parse(value: &Value) -> Result<CloudTable, CloudTableError> {
        parse_cloud_table(&serde_json::to_vec(value).unwrap())
    }

    /// §1: the example of the contract reads as one text rule.
    #[test]
    fn the_contract_example_reads() {
        let table = parse(&wire_table(vec![wire_rule("6f1c", "tkdz")])).unwrap();
        assert_eq!(table.schema_version, 1);
        assert_eq!(table.revision, 12);
        assert_eq!(
            table.updated_at.as_deref(),
            Some("2026-10-28T08:30:00+00:00")
        );
        assert_eq!(table.unreadable_count, 0);
        assert_eq!(table.rules.len(), 1);
        let rule = &table.rules[0];
        assert_eq!(rule.id, "6f1c");
        assert_eq!(rule.group, "退款");
        assert_eq!(rule.match_spec.value, "tkdz");
        assert!(cloud_action_allowed(&rule.action));
        // A table that was never saved: revision 0, no rules, updated_at null; with a BOM.
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend(
            serde_json::to_vec(&json!({
                "format": "quicktext-cloud-phrases", "schema_version": 1,
                "revision": 0, "updated_at": null, "rules": []
            }))
            .unwrap(),
        );
        let empty = parse_cloud_table(&bytes).unwrap();
        assert_eq!(
            (empty.revision, empty.updated_at, empty.rules.len()),
            (0, None, 0)
        );
    }

    /// §1: entries that do not read are skipped and counted; the rest are kept in order.
    #[test]
    fn unreadable_entries_are_counted_and_skipped() {
        let mut unknown_action = wire_rule("b", "bb");
        unknown_action["action"] = json!({ "type": "teleport", "where": "moon" });
        let mut wrong_type = wire_rule("c", "cc");
        wrong_type["enabled"] = json!("yes");
        let mut unknown_nested = wire_rule("d", "dd");
        unknown_nested["match"]["case"] = json!(true);
        let mut unknown_top_level = wire_rule("e", "ee");
        unknown_top_level["added_later"] = json!({ "x": 1 });
        let table = parse(&wire_table(vec![
            wire_rule("a", "aa"),
            unknown_action,
            wrong_type,
            json!("not an object"),
            unknown_nested,
            unknown_top_level,
        ]))
        .unwrap();
        assert_eq!(table.unreadable_count, 4);
        let ids: Vec<_> = table.rules.iter().map(|rule| rule.id.as_str()).collect();
        assert_eq!(ids, ["a", "e"]);
        // Unknown top-level rule fields are kept, as in a stored configuration.
        assert_eq!(table.rules[1].extra["added_later"], json!({ "x": 1 }));
    }

    /// §1, §3: what refuses the whole table, and with which code. Messages never quote it.
    #[test]
    fn malformed_tables_are_refused_with_their_code() {
        let secret = "zq-secret-phrase";
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (format!("{{\"format\": \"{secret}").into_bytes(), "invalid_format"),
            (b"".to_vec(), "invalid_format"),
            (b"[1, 2]".to_vec(), "invalid_format"),
            (
                serde_json::to_vec(&json!({ "format": secret, "schema_version": 1, "revision": 1, "rules": [] })).unwrap(),
                "invalid_format",
            ),
            (
                serde_json::to_vec(&json!({ "schema_version": 1, "revision": 1, "rules": [] })).unwrap(),
                "invalid_format",
            ),
            (
                serde_json::to_vec(&json!({ "format": CLOUD_FORMAT, "schema_version": 0, "revision": 1, "rules": [] })).unwrap(),
                "invalid_format",
            ),
            (
                serde_json::to_vec(&json!({ "format": CLOUD_FORMAT, "schema_version": "1", "revision": 1, "rules": [] })).unwrap(),
                "invalid_format",
            ),
            (
                serde_json::to_vec(&json!({ "format": CLOUD_FORMAT, "schema_version": 1, "revision": secret, "rules": [] })).unwrap(),
                "invalid_format",
            ),
            (
                serde_json::to_vec(&json!({ "format": CLOUD_FORMAT, "schema_version": 1, "revision": -1, "rules": [] })).unwrap(),
                "invalid_format",
            ),
            (
                serde_json::to_vec(&json!({ "format": CLOUD_FORMAT, "schema_version": 1, "revision": 1, "updated_at": 5, "rules": [] })).unwrap(),
                "invalid_format",
            ),
            (
                serde_json::to_vec(&json!({ "format": CLOUD_FORMAT, "schema_version": 1, "revision": 1, "rules": { "a": secret } })).unwrap(),
                "invalid_format",
            ),
            (
                serde_json::to_vec(&json!({ "format": CLOUD_FORMAT, "schema_version": 1, "revision": 1 })).unwrap(),
                "invalid_format",
            ),
        ];
        for (bytes, code) in cases {
            let error = parse_cloud_table(&bytes).unwrap_err();
            assert_eq!(error.code(), code, "{error}");
            assert!(!error.to_string().contains(secret), "{error}");
            assert!(!format!("{error:?}").contains(secret), "{error:?}");
        }
    }

    /// §1: a higher schema is `schema_newer`, whatever else the table holds.
    #[test]
    fn a_newer_schema_is_schema_newer() {
        let mut table = wire_table(vec![json!({ "id": 1 })]);
        table["schema_version"] = json!(2);
        table["rules"] = json!("whatever a newer release puts here");
        let error = parse(&table).unwrap_err();
        assert_eq!(error, CloudTableError::SchemaNewer { version: 2 });
        assert_eq!(error.code(), "schema_newer");
    }

    /// §4: without a table, or with an empty one, the effective configuration is the
    /// personal one, exactly.
    #[test]
    fn without_team_phrases_the_effective_config_is_the_personal_one() {
        let personal = personal(vec![text_rule("p", "pp", "body")]);
        for table in [None, Some(table(vec![]))] {
            let merge = merge_cloud(&personal, table.as_ref(), &none());
            assert_eq!(merge.compiled.config, personal.config);
            assert_eq!(merge.compiled.rules.len(), personal.rules.len());
            assert!(merge.rules.is_empty());
        }
    }

    /// §4: active phrases follow the personal rules as `cloud:<id>`, in download order;
    /// version, revision, the global switch and globals stay the personal ones.
    #[test]
    fn active_phrases_follow_the_personal_rules_with_a_prefix() {
        let mut personal_config = Config {
            revision: 7,
            rules: vec![text_rule("p1", "aa", "x"), text_rule("p2", "bb", "y")],
            ..Config::default()
        };
        personal_config.globals.terminators = vec!["semicolon".into()];
        let personal = compile_config(&personal_config).unwrap();
        let merge = merge_cloud(
            &personal,
            Some(&table(vec![
                text_rule("t2", "zz", "two"),
                text_rule("t1", "yy", "one"),
            ])),
            &none(),
        );
        let ids: Vec<_> = merge
            .compiled
            .config
            .rules
            .iter()
            .map(|rule| rule.id.as_str())
            .collect();
        assert_eq!(ids, ["p1", "p2", "cloud:t2", "cloud:t1"]);
        let compiled: Vec<_> = merge
            .compiled
            .rules
            .iter()
            .map(|rule| (rule.rule.id.as_str(), rule.order()))
            .collect();
        assert_eq!(
            compiled,
            [("p1", 0), ("p2", 1), ("cloud:t2", 2), ("cloud:t1", 3)]
        );
        let config = &merge.compiled.config;
        assert_eq!(
            (config.version, config.revision, config.enabled),
            (1, 7, true)
        );
        assert_eq!(config.globals, personal_config.globals);
        assert_eq!(
            statuses(&merge),
            [
                ("t2".to_string(), CloudRuleStatus::Active),
                ("t1".to_string(), CloudRuleStatus::Active)
            ]
        );
        // The listed rule is the downloaded one, unprefixed.
        assert_eq!(merge.rules[0].rule, text_rule("t2", "zz", "two"));
        // The engine matches a team phrase under its prefixed id and the personal revision.
        let hit = merge
            .compiled
            .match_input("yy", Some("semicolon"), &ForegroundContext::default())
            .unwrap();
        assert_eq!((hit.rule_id.as_str(), hit.revision), ("cloud:t1", 7));
        assert_eq!(team_phrase_id(&hit.rule_id), Some("t1"));
        assert_eq!(team_phrase_id("p1"), None);
        assert!(merge.compiled.rule("cloud:t1").is_some());
    }

    /// §4: the order of the checks: disabled_remote, disabled_local, invalid, else active.
    #[test]
    fn statuses_are_decided_in_order() {
        let personal = personal(vec![text_rule("mine", "same", "personal body")]);
        let mut remote_off = text_rule("remote", "same", "x");
        remote_off.enabled = false;
        remote_off.match_spec.value.clear(); // would be invalid too
        let mut local_off = text_rule("local", "same", "x");
        local_off.match_spec.kind = MatchKind::Literal;
        // The same trigger as the personal rule, but compiling refuses the empty override: a
        // phrase that does not run replaces nothing.
        let mut broken_same_trigger = text_rule("refused", "same", "x");
        broken_same_trigger.terminator = Some(Terminator {
            keys: vec![],
            consume: true,
        });
        // Passes every check before compiling; compiling refuses the empty override.
        let mut broken = text_rule("broken", "other", "x");
        broken.terminator = Some(Terminator {
            keys: vec![],
            consume: true,
        });
        let fine = text_rule("fine", "fine", "x");
        let disabled = BTreeSet::from(["local".to_string(), "ghost".to_string()]);
        let merge = merge_cloud(
            &personal,
            Some(&table(vec![
                remote_off,
                local_off,
                broken_same_trigger,
                broken,
                fine,
            ])),
            &disabled,
        );
        assert_eq!(
            statuses(&merge),
            [
                ("remote".to_string(), CloudRuleStatus::DisabledRemote),
                ("local".to_string(), CloudRuleStatus::DisabledLocal),
                ("refused".to_string(), CloudRuleStatus::Invalid),
                ("broken".to_string(), CloudRuleStatus::Invalid),
                ("fine".to_string(), CloudRuleStatus::Active),
            ]
        );
        assert_eq!(merge.rules[2].problem.as_deref(), Some("结束键设置无效"));
        assert_eq!(merge.rules[3].problem.as_deref(), Some("结束键设置无效"));
        assert_eq!(merge.rules[3].problem_codes, ["terminator"]);
        assert!(merge.rules.iter().all(|state| state.overrides.is_empty()));
        // `disabled_local` follows `disabled_ids` whatever the status.
        let local: Vec<_> = merge
            .rules
            .iter()
            .map(|state| state.disabled_local)
            .collect();
        assert_eq!(local, [false, true, false, false, false]);
        let both = merge_cloud(
            &personal,
            Some(&table(vec![{
                let mut rule = text_rule("local", "zz", "x");
                rule.enabled = false;
                rule
            }])),
            &disabled,
        );
        assert_eq!(both.rules[0].status, CloudRuleStatus::DisabledRemote);
        assert!(both.rules[0].disabled_local);
        let ids: Vec<_> = merge
            .compiled
            .config
            .rules
            .iter()
            .map(|rule| rule.id.as_str())
            .collect();
        assert_eq!(ids, ["mine", "cloud:fine"]);
    }

    /// §4: an active phrase replaces the enabled personal rules with its trigger: kind and
    /// value must both match, case is folded when either side ignores it, and `overrides`
    /// lists every such personal rule (in their order), not just the first.
    #[test]
    fn overriding_follows_the_trigger_rules() {
        let mut disabled = text_rule("off", "dis", "x");
        disabled.enabled = false;
        let mut regex = text_rule("re", "rx", "x");
        regex.match_spec.kind = MatchKind::Regex;
        let mut folding = text_rule("fold", "MiXeD", "x");
        folding.ignore_case = true;
        let personal = personal(vec![
            disabled,
            regex,
            text_rule("upper-first", "CASE", "x"),
            text_rule("exact", "case", "x"),
            folding,
            text_rule("second-same", "case", "x"),
        ]);
        let mut cloud_folding = text_rule("t-fold", "Case", "x");
        cloud_folding.ignore_case = true;
        let merge = merge_cloud(
            &personal,
            Some(&table(vec![
                text_rule("t-dis", "dis", "x"),
                text_rule("t-rx", "rx", "x"),
                text_rule("t-exact", "case", "x"),
                text_rule("t-other-case", "CaSe", "x"),
                text_rule("t-mixed", "mixed", "x"),
                cloud_folding,
            ])),
            &none(),
        );
        let found: Vec<_> = merge
            .rules
            .iter()
            .map(|state| (state.id.as_str(), state.status, state.overrides.clone()))
            .collect();
        let active = CloudRuleStatus::Active;
        assert_eq!(
            found,
            [
                ("t-dis", active, vec![]),
                ("t-rx", active, vec![]),
                (
                    "t-exact",
                    active,
                    vec!["exact".to_string(), "second-same".to_string()]
                ),
                ("t-other-case", active, vec![]),
                ("t-mixed", active, vec!["fold".to_string()]),
                (
                    "t-fold",
                    active,
                    vec![
                        "upper-first".to_string(),
                        "exact".to_string(),
                        "second-same".to_string()
                    ]
                ),
            ]
        );
        // The rules that are replaced are gone from the engine; `off` (disabled) and `re`
        // (a regex) have no phrase with their trigger and stay.
        let ids: Vec<_> = merge
            .compiled
            .config
            .rules
            .iter()
            .map(|rule| rule.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "off",
                "re",
                "cloud:t-dis",
                "cloud:t-rx",
                "cloud:t-exact",
                "cloud:t-other-case",
                "cloud:t-mixed",
                "cloud:t-fold"
            ]
        );
    }

    /// §4: while a phrase is active, typing its trigger runs the phrase and the personal rule
    /// with the same trigger is not in the engine; the other personal rules keep working and
    /// keep their relative order (`order` stays the index in the effective `config.rules`).
    #[test]
    fn an_active_phrase_wins_over_the_personal_rule_with_its_trigger() {
        let personal = personal(vec![
            text_rule("first", "aa", "personal first"),
            text_rule("dup", "tk", "personal dup"),
            text_rule("last", "zz", "personal last"),
        ]);
        let phrases = table(vec![text_rule("t", "tk", "team")]);
        let context = ForegroundContext::default();
        let merge = merge_cloud(&personal, Some(&phrases), &none());
        let hit = merge
            .compiled
            .match_input("tk", Some("space"), &context)
            .unwrap();
        assert_eq!(hit.rule_id, "cloud:t");
        assert!(hit.tied_rule_ids.is_empty());
        assert!(merge.compiled.rule("dup").is_none());
        let ids: Vec<_> = merge
            .compiled
            .config
            .rules
            .iter()
            .map(|rule| rule.id.as_str())
            .collect();
        assert_eq!(ids, ["first", "last", "cloud:t"]);
        let orders: Vec<_> = merge
            .compiled
            .rules
            .iter()
            .map(|rule| (rule.rule.id.as_str(), rule.order()))
            .collect();
        assert_eq!(orders, [("first", 0), ("last", 1), ("cloud:t", 2)]);
        for kept in [("aa", "first"), ("zz", "last")] {
            let hit = merge
                .compiled
                .match_input(kept.0, Some("space"), &context)
                .unwrap();
            assert_eq!(hit.rule_id, kept.1);
        }
        // The personal configuration itself is untouched.
        assert_eq!(personal.config.rules.len(), 3);
        assert!(personal.rule("dup").is_some());

        // A phrase that does not run replaces nothing: the personal rule answers again.
        let switched_off = BTreeSet::from(["t".to_string()]);
        let mut remote_off = text_rule("t", "tk", "team");
        remote_off.enabled = false;
        let mut broken = text_rule("t", "tk", "team");
        broken.terminator = Some(Terminator {
            keys: vec![],
            consume: true,
        });
        let mut regex = text_rule("t", "tk", "team");
        regex.match_spec.kind = MatchKind::Regex;
        for (table, disabled) in [
            (table(vec![text_rule("t", "tk", "team")]), switched_off),
            (table(vec![remote_off]), none()),
            (table(vec![broken]), none()),
            (table(vec![regex]), none()),
        ] {
            let merge = merge_cloud(&personal, Some(&table), &disabled);
            assert_ne!(merge.rules[0].status, CloudRuleStatus::Active);
            assert!(merge.rules[0].overrides.is_empty());
            let ids: Vec<_> = merge
                .compiled
                .config
                .rules
                .iter()
                .map(|rule| rule.id.as_str())
                .collect();
            assert_eq!(ids, ["first", "dup", "last"]);
            let hit = merge
                .compiled
                .match_input("tk", Some("space"), &context)
                .unwrap();
            assert_eq!(hit.rule_id, "dup");
        }
    }

    /// A disabled personal rule is not replaced (it is not in the engine anyway): it has
    /// no `overrides` entry, and stays in the effective configuration as it was.
    #[test]
    fn a_disabled_personal_rule_is_left_alone() {
        let mut off = text_rule("off", "tk", "personal");
        off.enabled = false;
        let personal = personal(vec![off, text_rule("on", "zz", "x")]);
        let merge = merge_cloud(
            &personal,
            Some(&table(vec![text_rule("t", "tk", "team")])),
            &none(),
        );
        assert!(merge.rules[0].overrides.is_empty());
        let ids: Vec<_> = merge
            .compiled
            .config
            .rules
            .iter()
            .map(|rule| rule.id.as_str())
            .collect();
        assert_eq!(ids, ["off", "on", "cloud:t"]);
        let orders: Vec<_> = merge
            .compiled
            .rules
            .iter()
            .map(|rule| rule.order())
            .collect();
        assert_eq!(orders, [1, 2]);
    }

    /// §4: a phrase that does not compile, or is more than text, is `invalid` with a
    /// problem; the personal rules and the other phrases are not affected.
    #[test]
    fn invalid_phrases_stay_out_and_say_why() {
        let personal = personal(vec![
            text_rule("p", "pp", "x"),
            text_rule("cloud:taken", "zz", "x"),
        ]);
        let mut empty = text_rule("empty", "", "x");
        empty.match_spec.value.clear();
        let mut immediate_regex = text_rule("imm", "a+b", "x");
        immediate_regex.match_spec.kind = MatchKind::Regex;
        immediate_regex.trigger_mode = TriggerMode::Immediate;
        let mut open = text_rule("open", "op", "x");
        open.action = Action::Open {
            target: "calc.exe".into(),
            args: vec![],
            working_directory: None,
        };
        let mut file = text_rule("file", "fi", "x");
        file.action = Action::Text {
            method: OutputMethod::Paste,
            source: ContentSource::File {
                path: "/etc/passwd".into(),
                encoding: Encoding::Utf8,
            },
        };
        let mut keys_with_vars = text_rule("kv", "kv", "x");
        keys_with_vars.action = Action::SendKeys {
            source: ContentSource::Inline {
                value: "{cliptext}".into(),
            },
            syntax_variables: vec!["cliptext".into()],
        };
        let mut blank_id = text_rule("  ", "bl", "x");
        blank_id.id = "  ".into();
        let merge = merge_cloud(
            &personal,
            Some(&table(vec![
                text_rule("dup", "d1", "first"),
                empty,
                immediate_regex,
                open,
                file,
                keys_with_vars,
                text_rule("dup", "d2", "second"),
                blank_id,
                text_rule("taken", "tk", "x"),
                text_rule("ok", "ok", "x"),
            ])),
            &none(),
        );
        let found: Vec<_> = merge
            .rules
            .iter()
            .map(|state| (state.status, state.problem_codes.first().cloned()))
            .collect();
        use CloudRuleStatus::*;
        let invalid = |code: &str| (Invalid, Some(code.to_string()));
        assert_eq!(
            found,
            [
                (Active, None),
                invalid("match_empty"),
                invalid(REGEX_NOT_ALLOWED),
                invalid(ACTION_NOT_ALLOWED),
                invalid(ACTION_NOT_ALLOWED),
                invalid(ACTION_NOT_ALLOWED),
                invalid("rule_id"),
                invalid("rule_id"),
                invalid("rule_id"),
                (Active, None),
            ]
        );
        let ids: Vec<_> = merge
            .compiled
            .rules
            .iter()
            .map(|rule| (rule.rule.id.as_str(), rule.order()))
            .collect();
        assert_eq!(
            ids,
            [
                ("p", 0),
                ("cloud:taken", 1),
                ("cloud:dup", 2),
                ("cloud:ok", 3)
            ]
        );
        // The first `dup` is the one that runs.
        let dup = merge.compiled.rule("cloud:dup").unwrap();
        assert_eq!(dup.match_spec.value, "d1");
        // Problems are the user's text and never quote the trigger or the body.
        assert_eq!(merge.rules[1].problem.as_deref(), Some("缩写为空"));
        assert_eq!(
            merge.rules[3].problem.as_deref(),
            Some("团队话术只能粘贴、键入纯文本或使用文字与按键组合")
        );
        for state in &merge.rules {
            if let Some(problem) = &state.problem {
                assert!(
                    !problem.contains("a+b") && !problem.contains("passwd"),
                    "{problem}"
                );
            }
        }
        assert_eq!(
            cloud_problem_text("something_new"),
            "无法使用（something_new）"
        );
    }

    /// §4: a phrase whose text would read the clipboard, the selection or action
    /// parameters is `invalid` and never reaches the engine, so nothing in the effective
    /// configuration makes a run read them. Dates and look-alikes stay allowed.
    #[test]
    fn phrases_that_read_local_data_are_invalid() {
        let personal = personal(vec![text_rule("mine", "pp", "fixed")]);
        let merge = merge_cloud(
            &personal,
            Some(&table(vec![
                text_rule("clip", "c1", "前缀 {cliptext} 后缀"),
                text_rule("sel", "c2", "{selection}"),
                text_rule("param", "c3", "订单 {param:action_parameter}"),
                text_rule("param-open", "c4", "{param:"),
                text_rule("date", "c5", "今天是 {0:yyyy-MM-dd}"),
                text_rule("lookalike", "c6", "{CLIPTEXT} {clip} {param} cliptext"),
            ])),
            &none(),
        );
        let found: Vec<_> = merge
            .rules
            .iter()
            .map(|state| (state.id.as_str(), state.status, state.problem_codes.clone()))
            .collect();
        let invalid = |id| {
            (
                id,
                CloudRuleStatus::Invalid,
                vec![LOCAL_DATA_PLACEHOLDER.to_string()],
            )
        };
        assert_eq!(
            found,
            [
                invalid("clip"),
                invalid("sel"),
                invalid("param"),
                invalid("param-open"),
                ("date", CloudRuleStatus::Active, vec![]),
                ("lookalike", CloudRuleStatus::Active, vec![]),
            ]
        );
        assert_eq!(
            merge.rules[0].problem.as_deref(),
            Some("正文含 {cliptext}、{selection} 或 {param:…}，团队话术只输出固定文字")
        );
        let ids: Vec<_> = merge
            .compiled
            .config
            .rules
            .iter()
            .map(|rule| rule.id.as_str())
            .collect();
        assert_eq!(ids, ["mine", "cloud:date", "cloud:lookalike"]);
        assert!(merge.compiled.rule("cloud:clip").is_none());
        let config = &merge.compiled.config;
        for token in ["cliptext", "selection"] {
            assert!(
                config
                    .rules
                    .iter()
                    .all(|rule| !action_uses_variable(&rule.action, config, token)),
                "{token}"
            );
        }
        // Even switched off remotely or locally they keep their earlier status.
        let mut remote_off = text_rule("clip", "c1", "{cliptext}");
        remote_off.enabled = false;
        let merge = merge_cloud(&personal, Some(&table(vec![remote_off])), &none());
        assert_eq!(merge.rules[0].status, CloudRuleStatus::DisabledRemote);
    }

    /// Review round 3: a table over [`CLOUD_MAX_RULES`] entries is refused whole, before
    /// any entry is read (a 10000-entry table of huge regexes costs nothing).
    #[test]
    fn a_table_over_the_rule_limit_is_refused_whole() {
        let at_limit = wire_table(
            (0..CLOUD_MAX_RULES)
                .map(|i| wire_rule(&format!("id{i}"), &format!("t{i}")))
                .collect(),
        );
        assert_eq!(parse(&at_limit).unwrap().rules.len(), CLOUD_MAX_RULES);
        let mut over = at_limit.clone();
        over["rules"]
            .as_array_mut()
            .unwrap()
            .push(json!("even an unreadable entry counts"));
        let error = parse(&over).unwrap_err();
        assert_eq!(error, CloudTableError::TooManyRules { count: 2001 });
        assert_eq!(error.code(), "too_many_rules");
        let started = std::time::Instant::now();
        let hostile = wire_table(
            (0..10_000)
                .map(|i| {
                    let mut rule = wire_rule(&format!("id{i}"), "[a-z]{10000}");
                    rule["match"]["kind"] = json!("regex");
                    rule["action"]["source"]["value"] = json!("x");
                    rule
                })
                .collect(),
        );
        assert_eq!(
            parse(&hostile).unwrap_err(),
            CloudTableError::TooManyRules { count: 10_000 }
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// Review round 3: regex triggers, triggers over 64 characters and texts over 20000
    /// characters are `invalid` before anything is compiled (none reaches the compiled
    /// rules); the limits themselves are allowed.
    #[test]
    fn phrases_over_the_admission_limits_are_never_compiled() {
        let personal = personal(vec![]);
        let mut regex = text_rule("regex", "[a-z]{10000}", "x");
        regex.match_spec.kind = MatchKind::Regex;
        let mut harmless_regex = text_rule("regex-small", "ab+", "x");
        harmless_regex.match_spec.kind = MatchKind::Regex;
        let long_trigger = "缩".repeat(CLOUD_MAX_TRIGGER_CHARS + 1);
        let long_body = "字".repeat(CLOUD_MAX_BODY_CHARS + 1);
        let merge = merge_cloud(
            &personal,
            Some(&table(vec![
                regex,
                harmless_regex,
                text_rule("trigger-65", &long_trigger, "x"),
                text_rule("trigger-64", &"t".repeat(CLOUD_MAX_TRIGGER_CHARS), "x"),
                text_rule("body-20001", "b1", &long_body),
                text_rule("body-20000", "b2", &"字".repeat(CLOUD_MAX_BODY_CHARS)),
            ])),
            &none(),
        );
        let found: Vec<_> = merge
            .rules
            .iter()
            .map(|state| {
                (
                    state.id.as_str(),
                    state.status,
                    state.problem_codes.first().map(String::as_str),
                )
            })
            .collect();
        use CloudRuleStatus::*;
        assert_eq!(
            found,
            [
                ("regex", Invalid, Some(REGEX_NOT_ALLOWED)),
                ("regex-small", Invalid, Some(REGEX_NOT_ALLOWED)),
                ("trigger-65", Invalid, Some(MATCH_TOO_LONG)),
                ("trigger-64", Active, None),
                ("body-20001", Invalid, Some(BODY_TOO_LONG)),
                ("body-20000", Active, None),
            ]
        );
        assert_eq!(
            merge.rules[0].problem.as_deref(),
            Some("团队话术只支持普通缩写，不支持正则")
        );
        assert_eq!(
            merge.rules[2].problem.as_deref(),
            Some("缩写超过 64 个字符")
        );
        assert_eq!(
            merge.rules[4].problem.as_deref(),
            Some("正文超过 20000 个字符")
        );
        let compiled: Vec<_> = merge
            .compiled
            .rules
            .iter()
            .map(|rule| rule.rule.id.as_str())
            .collect();
        assert_eq!(compiled, ["cloud:trigger-64", "cloud:body-20000"]);
    }

    /// Review round 3: [`cloud_rule_states`] (safe mode) makes every check before compiling
    /// and compiles nothing: a phrase only the compiler would refuse still reads `active`.
    #[test]
    fn rule_states_without_compiling() {
        let personal = personal(vec![text_rule("mine", "same", "x")]);
        let mut needs_compiling = text_rule("compile-error", "ce", "x");
        needs_compiling.terminator = Some(Terminator {
            keys: vec![],
            consume: true,
        });
        let mut regex = text_rule("regex", "a+", "x");
        regex.match_spec.kind = MatchKind::Regex;
        let table = table(vec![
            needs_compiling,
            regex,
            text_rule("overrides", "same", "x"),
        ]);
        let states = cloud_rule_states(&personal.config, Some(&table), &none());
        let statuses: Vec<_> = states.iter().map(|state| state.status).collect();
        use CloudRuleStatus::*;
        assert_eq!(statuses, [Active, Invalid, Active]);
        // Without compiling nothing is known to fail, but what an active phrase replaces is
        // listed all the same.
        assert!(states[0].overrides.is_empty());
        assert_eq!(states[2].overrides, ["mine"]);
        let merged = merge_cloud(&personal, Some(&table), &none());
        let statuses: Vec<_> = merged.rules.iter().map(|state| state.status).collect();
        assert_eq!(statuses, [Invalid, Invalid, Active]);
        assert_eq!(merged.rules[2].overrides, ["mine"]);
        assert!(cloud_rule_states(&personal.config, None, &none()).is_empty());
    }

    /// §4: overlapping triggers keep the existing priority (team phrases are literal: a
    /// literal beats a personal regex on the same input, the longer trigger wins; equal
    /// triggers are not left to the priority: the team phrase replaces the personal rule).
    /// The global switch also stops team phrases.
    #[test]
    fn priority_and_the_global_switch_cover_team_phrases() {
        let mut personal_regex = text_rule("p-re", "a.c", "x");
        personal_regex.match_spec.kind = MatchKind::Regex;
        let mut personal_short = text_rule("p-short", "dz", "x");
        personal_short.trigger_mode = TriggerMode::Immediate;
        let personal = personal(vec![personal_regex, personal_short]);
        let mut cloud_long = text_rule("t-long", "tkdz", "x");
        cloud_long.trigger_mode = TriggerMode::Immediate;
        let merge = merge_cloud(
            &personal,
            Some(&table(vec![text_rule("t-abc", "abc", "x"), cloud_long])),
            &none(),
        );
        let context = ForegroundContext::default();
        let literal = merge
            .compiled
            .match_input("abc", Some("space"), &context)
            .unwrap();
        assert_eq!(literal.rule_id, "cloud:t-abc");
        assert!(literal.tied_rule_ids.is_empty());
        // Both immediate triggers end the input; the longer one wins, team phrase or not.
        let longer = merge.compiled.match_input("tkdz", None, &context).unwrap();
        assert_eq!(longer.rule_id, "cloud:t-long");
        assert!(longer.tied_rule_ids.is_empty());
        let mut off = merge.compiled.clone();
        off.config.enabled = false;
        assert!(off.match_input("tkdz", None, &context).is_none());
    }

    /// `CloudRuleState` serializes as `CloudState.rules[]` (§5).
    #[test]
    fn rule_states_serialize_as_the_contract_says() {
        let personal = personal(vec![text_rule("mine", "same", "x")]);
        let merge = merge_cloud(
            &personal,
            Some(&table(vec![
                text_rule("a", "aa", "x"),
                text_rule("b", "same", "x"),
            ])),
            &none(),
        );
        let value = serde_json::to_value(&merge.rules).unwrap();
        assert_eq!(value[0]["id"], "a");
        assert_eq!(value[0]["status"], "active");
        assert_eq!(value[0]["rule"]["id"], "a");
        assert_eq!(value[0]["rule"]["match"]["value"], "aa");
        assert_eq!(value[0]["disabled_local"], false);
        assert!(value[0].get("overrides").is_none());
        assert!(value[0].get("problem").is_none());
        assert!(value[0].get("problem_codes").is_none());
        assert_eq!(value[1]["status"], "active");
        assert_eq!(value[1]["overrides"], json!(["mine"]));
        assert!(value
            .as_array()
            .unwrap()
            .iter()
            .all(|rule| rule.get("shadowed_by").is_none()));
        for (status, wire) in [
            (CloudRuleStatus::DisabledRemote, "disabled_remote"),
            (CloudRuleStatus::DisabledLocal, "disabled_local"),
            (CloudRuleStatus::Invalid, "invalid"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), wire);
        }
    }

    /// B-09: `Debug` of a table and a merge never shows triggers or phrase bodies.
    #[test]
    fn debug_output_is_redacted() {
        let personal = personal(vec![]);
        let table = table(vec![text_rule("a", "zqabbr", "phrase-body")]);
        let merge = merge_cloud(&personal, Some(&table), &none());
        for shown in [format!("{table:?}"), format!("{merge:?}")] {
            assert!(
                !shown.contains("zqabbr") && !shown.contains("phrase-body"),
                "{shown}"
            );
        }
    }

    #[test]
    fn inline_send_keys_without_syntax_variables_is_admitted() {
        let mut combo = text_rule("combo", "nh", "x");
        combo.action = Action::SendKeys {
            source: ContentSource::Inline {
                value: "您好{Enter}".into(),
            },
            syntax_variables: vec![],
        };
        assert!(cloud_action_allowed(&combo.action));
        let merge = merge_cloud(&personal(vec![]), Some(&table(vec![combo])), &none());
        assert_eq!(merge.rules[0].status, CloudRuleStatus::Active);
    }
}
