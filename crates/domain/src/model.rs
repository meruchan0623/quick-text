use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
// B-09: types holding abbreviations or phrase bodies print `<redacted len=N>` in Debug.
use crate::{Redacted, RedactedKey, RedactedLen, RedactedList, RedactedValue, RedactedValues};
use std::fmt;
/// Schema version this build writes.
pub const SCHEMA_VERSION: u32 = 1;
/// Highest schema version this build reads; `version` must be in `1..=SUPPORTED_SCHEMA_VERSION`
/// (B-43). Anything higher was written by a newer release and is never overwritten.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;
pub const MAX_STEPS: usize = 10000;
pub const MAX_DELAY_MS: u64 = 3_600_000;
fn version() -> u32 {
    SCHEMA_VERSION
}
fn yes() -> bool {
    true
}
/// Terminator keys that older configurations may contain but the Windows input path
/// never delivers (B-12 / S-12). Existing values are kept as configured; readers only
/// warn with [`diagnostic_code::TERMINATOR_UNSUPPORTED`].
pub const UNSUPPORTED_TERMINATORS: [&str; 2] = ["tab", "enter"];
/// Stable diagnostic codes that hosts match on instead of message text.
pub mod diagnostic_code {
    /// `version` is above [`crate::SUPPORTED_SCHEMA_VERSION`]: open read-only, never overwrite.
    pub const SCHEMA_NEWER: &str = "schema_newer";
    /// `version` is not a supported schema version (for example 0).
    pub const SCHEMA_VERSION: &str = "schema_version";
    /// Stored configuration is not UTF-8 JSON, its root is not an object, or `rules` is not
    /// an array: nothing can be read from it.
    pub const CONFIG_MALFORMED: &str = "config_malformed";
    /// A top-level or global field had the wrong type and fell back to its default.
    pub const FIELD_RESET: &str = "field_reset";
    /// A rule entry could not be read; it is kept verbatim and disabled at runtime.
    pub const RULE_UNREADABLE: &str = "rule_unreadable";
    /// The configuration still uses `tab` / `enter` terminators (B-12).
    pub const TERMINATOR_UNSUPPORTED: &str = "terminator_unsupported";
}
fn default_terminators() -> Vec<String> {
    // New configurations only offer the keys every runtime delivers (B-12).
    ["space", "semicolon"].map(str::to_string).to_vec()
}
fn default_separators() -> String {
    " \t\r\n".into()
}
/// Serialized as the 0.3.1 object (`version`, `revision`, `enabled`, `globals`, `rules`) plus
/// `extra`; `unreadable_rules` go back into `rules` and never add a key (B-43). Deserializing
/// keeps unknown fields and unreadable rule entries but rejects wrongly typed known fields;
/// stored files go through [`crate::read_stored_config`], which also resets those.
#[derive(Clone, PartialEq)]
pub struct Config {
    pub version: u32,
    pub revision: u64,
    pub enabled: bool,
    pub globals: GlobalSettings,
    /// Rules this build understands; the only ones that compile and match.
    pub rules: Vec<Rule>,
    /// `rules` entries this build could not read, kept verbatim (B-43).
    pub unreadable_rules: Vec<UnreadableRule>,
    /// Top-level fields this build does not know, written back unchanged (B-43).
    pub extra: Map<String, Value>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: version(),
            revision: 0,
            enabled: true,
            globals: GlobalSettings::default(),
            rules: vec![],
            unreadable_rules: vec![],
            extra: Map::new(),
        }
    }
}
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("version", &self.version)
            .field("revision", &self.revision)
            .field("enabled", &self.enabled)
            .field("globals", &self.globals)
            .field("rules", &self.rules)
            .field("unreadable_rules", &self.unreadable_rules)
            .field("extra", &RedactedValues(&self.extra))
            .finish()
    }
}
/// Keys of the serialized [`Config`] object; `extra` never shadows them.
pub(crate) const CONFIG_FIELDS: [&str; 5] = ["version", "revision", "enabled", "globals", "rules"];
/// A `rules` entry this build could not read (an unknown action type, a wrongly typed field).
/// It never compiles, so it stays off at runtime, and it is written back verbatim (B-43).
#[derive(Clone, PartialEq)]
pub struct UnreadableRule {
    /// Position in the serialized `rules` array. When the list has since become shorter the
    /// entry is written after the last rule.
    pub index: usize,
    /// The entry exactly as read.
    pub raw: Value,
}
impl fmt::Debug for UnreadableRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnreadableRule")
            .field("index", &self.index)
            .field("raw", &RedactedValue(&self.raw))
            .finish()
    }
}
impl UnreadableRule {
    /// The entry's `id`, when it has a string one.
    pub fn id(&self) -> Option<&str> {
        self.raw.get("id")?.as_str()
    }
    /// Top-level fields of the entry that do not read as their rule field. Names only, so
    /// they can be logged without rule content; empty when the entry is not an object.
    pub fn unreadable_fields(&self) -> Vec<String> {
        let Some(fields) = self.raw.as_object() else {
            return vec![];
        };
        fields
            .iter()
            .filter(|(key, value)| {
                let single = Value::Object(Map::from_iter([((*key).clone(), (*value).clone())]));
                Rule::deserialize(&single).is_err()
            })
            .map(|(key, _)| key.clone())
            .collect()
    }
}
pub(crate) enum RuleEntry<'a> {
    Readable(&'a Rule),
    Unreadable(&'a UnreadableRule),
}
impl Config {
    /// The serialized `rules` array: readable rules in order, each unreadable entry at its
    /// recorded index (entries past the end follow the last rule, in index order).
    pub(crate) fn rule_entries(&self) -> Vec<RuleEntry<'_>> {
        let mut unreadable: Vec<_> = self.unreadable_rules.iter().collect();
        unreadable.sort_by_key(|entry| entry.index);
        let mut unreadable = unreadable.into_iter().peekable();
        let mut readable = self.rules.iter();
        let mut entries = Vec::with_capacity(self.rules.len() + self.unreadable_rules.len());
        loop {
            let position = entries.len();
            if let Some(entry) = unreadable.next_if(|entry| entry.index <= position) {
                entries.push(RuleEntry::Unreadable(entry));
            } else if let Some(rule) = readable.next() {
                entries.push(RuleEntry::Readable(rule));
            } else if let Some(entry) = unreadable.next() {
                entries.push(RuleEntry::Unreadable(entry));
            } else {
                return entries;
            }
        }
    }
    /// Index of each of `rules` in the serialized `rules` array, for diagnostic paths that
    /// must point at the entry an editor shows.
    pub(crate) fn rule_positions(&self) -> Vec<usize> {
        self.rule_entries()
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches!(entry, RuleEntry::Readable(_)))
            .map(|(position, _)| position)
            .collect()
    }
}
struct RuleEntries<'a>(&'a Config);
impl Serialize for RuleEntries<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let entries = self.0.rule_entries();
        let mut seq = serializer.serialize_seq(Some(entries.len()))?;
        for entry in entries {
            match entry {
                RuleEntry::Readable(rule) => seq.serialize_element(rule)?,
                RuleEntry::Unreadable(entry) => seq.serialize_element(&entry.raw)?,
            }
        }
        seq.end()
    }
}
impl Serialize for Config {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let extra: Vec<_> = self
            .extra
            .iter()
            .filter(|(key, _)| !CONFIG_FIELDS.contains(&key.as_str()))
            .collect();
        let mut map = serializer.serialize_map(Some(CONFIG_FIELDS.len() + extra.len()))?;
        map.serialize_entry("version", &self.version)?;
        map.serialize_entry("revision", &self.revision)?;
        map.serialize_entry("enabled", &self.enabled)?;
        map.serialize_entry("globals", &self.globals)?;
        map.serialize_entry("rules", &RuleEntries(self))?;
        for (key, value) in extra {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}
impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        crate::stored::config_from_value(value, crate::stored::Mode::Strict, &mut vec![])
            .map_err(serde::de::Error::custom)
    }
}
impl Config {
    /// Locations that still use [`UNSUPPORTED_TERMINATORS`]: the global setting and the
    /// overrides of terminator-mode rules (immediate rules never consult terminators).
    fn unsupported_terminator_uses(&self) -> impl Iterator<Item = (String, &[String])> {
        let globals = std::iter::once((
            "globals.terminators".to_string(),
            self.globals.terminators.as_slice(),
        ));
        let rules = self
            .rules
            .iter()
            .zip(self.rule_positions())
            .filter_map(|(rule, index)| {
                let keys = &rule.terminator.as_ref()?.keys;
                (rule.trigger_mode == TriggerMode::Terminator)
                    .then(|| (format!("rules[{index}].terminator.keys"), keys.as_slice()))
            });
        globals.chain(rules).filter(|(_, keys)| {
            keys.iter()
                .any(|k| UNSUPPORTED_TERMINATORS.contains(&k.as_str()))
        })
    }
    /// Distinct legacy terminator keys still in use, in [`UNSUPPORTED_TERMINATORS`] order
    /// (the `keys` of the `terminator_unsupported` startup problem).
    pub fn unsupported_terminators(&self) -> Vec<String> {
        let used: Vec<_> = self.unsupported_terminator_uses().collect();
        UNSUPPORTED_TERMINATORS
            .iter()
            .filter(|key| used.iter().any(|(_, keys)| keys.iter().any(|k| k == *key)))
            .map(|key| key.to_string())
            .collect()
    }
    /// One warning per location that uses a legacy terminator. The keys stay configured
    /// and compile unchanged; only the warning is new (B-12).
    pub fn terminator_diagnostics(&self) -> Vec<Diagnostic> {
        self.unsupported_terminator_uses()
            .map(|(path, _)| {
                Diagnostic::warning(
                    diagnostic_code::TERMINATOR_UNSUPPORTED,
                    "Tab and Enter terminators are not delivered on Windows; the setting is kept unchanged",
                    SourceSpan::default(),
                )
                .at(path)
            })
            .collect()
    }
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalSettings {
    pub terminators: Vec<String>,
    pub word_separators: String,
    pub blacklist: Vec<String>,
    pub ime_erasure: ImeErasure,
    /// Fields this build does not know, written back unchanged (B-43).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            terminators: default_terminators(),
            word_separators: default_separators(),
            blacklist: vec![],
            ime_erasure: ImeErasure::Escape,
            extra: Map::new(),
        }
    }
}
impl fmt::Debug for GlobalSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlobalSettings")
            .field("terminators", &self.terminators)
            .field("word_separators", &self.word_separators)
            .field("blacklist", &self.blacklist)
            .field("ime_erasure", &self.ime_erasure)
            .field("extra", &RedactedValues(&self.extra))
            .finish()
    }
}
/// Unknown fields of a rule are kept in `extra` (B-43); nested values stay strict, so an
/// entry with an unknown action type or nested field is kept whole as an [`UnreadableRule`].
#[derive(Clone, PartialEq, Serialize)]
pub struct Rule {
    pub id: String,
    pub enabled: bool,
    pub group: String,
    pub description: String,
    pub ignore_case: bool,
    #[serde(rename = "match")]
    pub match_spec: MatchSpec,
    pub trigger_mode: TriggerMode,
    pub terminator: Option<Terminator>,
    /// None inherits the global setting; legacy string values remain explicit overrides.
    pub ime_erasure: Option<ImeErasure>,
    pub action: Action,
    pub parameter: ParameterSource,
    pub notification: Notification,
    /// Fields this build does not know, written back unchanged (B-43).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Deserialize)]
#[serde(remote = "Rule", default)]
struct RuleFields {
    pub id: String,
    pub enabled: bool,
    pub group: String,
    pub description: String,
    pub ignore_case: bool,
    #[serde(rename = "match")]
    pub match_spec: MatchSpec,
    pub trigger_mode: TriggerMode,
    pub terminator: Option<Terminator>,
    /// None inherits the global setting; legacy string values remain explicit overrides.
    pub ime_erasure: Option<ImeErasure>,
    pub action: Action,
    pub parameter: ParameterSource,
    pub notification: Notification,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
impl Default for RuleFields {
    fn default() -> Self {
        let rule = Rule::default();
        Self {
            id: rule.id,
            enabled: rule.enabled,
            group: rule.group,
            description: rule.description,
            ignore_case: rule.ignore_case,
            match_spec: rule.match_spec,
            trigger_mode: rule.trigger_mode,
            terminator: rule.terminator,
            ime_erasure: rule.ime_erasure,
            action: rule.action,
            parameter: rule.parameter,
            notification: rule.notification,
            extra: rule.extra,
        }
    }
}
// Accept the removed field only while reading legacy configuration. It is never
// retained in the rule model (not even in `extra`) or serialized back into active configuration.
impl<'de> Deserialize<'de> for Rule {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = Value::deserialize(deserializer)?;
        if let Some(fields) = value.as_object_mut() {
            fields.remove("scope");
        }
        RuleFields::deserialize(value).map_err(serde::de::Error::custom)
    }
}
impl Default for Rule {
    fn default() -> Self {
        Self {
            id: String::new(),
            enabled: yes(),
            group: String::new(),
            description: String::new(),
            ignore_case: false,
            match_spec: MatchSpec::default(),
            trigger_mode: TriggerMode::Terminator,
            terminator: None,
            ime_erasure: None,
            action: Action::None,
            parameter: ParameterSource::DefaultMatch,
            notification: Notification::default(),
            extra: Map::new(),
        }
    }
}
impl Rule {
    pub fn effective_ime_erasure(&self, globals: &GlobalSettings) -> ImeErasure {
        self.ime_erasure.unwrap_or(globals.ime_erasure)
    }
}
impl fmt::Debug for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rule")
            .field("id", &self.id)
            .field("enabled", &self.enabled)
            .field("group", &self.group)
            .field("description", &self.description)
            .field("ignore_case", &self.ignore_case)
            .field("match_spec", &self.match_spec)
            .field("trigger_mode", &self.trigger_mode)
            .field("terminator", &self.terminator)
            .field("ime_erasure", &self.ime_erasure)
            .field("action", &self.action)
            .field("parameter", &self.parameter)
            .field("notification", &self.notification)
            .field("extra", &RedactedValues(&self.extra))
            .finish()
    }
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchSpec {
    pub kind: MatchKind,
    pub value: String,
}
/// The abbreviation (literal or regex source) is redacted.
impl fmt::Debug for MatchSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MatchSpec")
            .field("kind", &self.kind)
            .field("value", &Redacted(&self.value))
            .finish()
    }
}
impl Default for MatchSpec {
    fn default() -> Self {
        Self {
            kind: MatchKind::Literal,
            value: String::new(),
        }
    }
}
macro_rules! enum_default {($name:ident,$default:ident, $($variant:ident),+) => {
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")] pub enum $name {$($variant),+}
impl Default for $name{fn default()->Self{Self::$default}}
};}
enum_default!(MatchKind, Literal, Literal, Regex);
enum_default!(TriggerMode, Terminator, Terminator, Immediate);
enum_default!(ImeErasure, Escape, Escape, Backspace);
enum_default!(OutputMethod, Keys, Keys, Paste);
enum_default!(Encoding, Utf8, Utf8, Utf16Le, Utf16Be, Gbk);
enum_default!(
    Dialect,
    Structured,
    Structured,
    SendKeys,
    Inputscript,
    Replay
);
enum_default!(MouseButton, Left, Left, Right, Middle, X1, X2);
enum_default!(ButtonState, Click, Down, Up, Click, DoubleClick);
enum_default!(WheelAxis, Vertical, Vertical, Horizontal);
enum_default!(WheelUnit, Clicks, Clicks, Delta);
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Terminator {
    pub keys: Vec<String>,
    pub consume: bool,
}
impl Default for Terminator {
    fn default() -> Self {
        Self {
            keys: default_terminators(),
            consume: true,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ForegroundContext {
    pub platform: Platform,
    pub process: String,
    pub bundle_id: String,
}
enum_default!(Platform, Windows, Windows, Macos);
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notification {
    pub enabled: bool,
    pub message: String,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParameterSource {
    DefaultMatch,
    Fixed { value: String },
    FirstCapture,
    CaptureTemplate { template: String },
    Null,
}
impl fmt::Debug for ParameterSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DefaultMatch => f.write_str("DefaultMatch"),
            Self::Fixed { value } => f
                .debug_struct("Fixed")
                .field("value", &Redacted(value))
                .finish(),
            Self::FirstCapture => f.write_str("FirstCapture"),
            Self::CaptureTemplate { template } => f
                .debug_struct("CaptureTemplate")
                .field("template", &Redacted(template))
                .finish(),
            Self::Null => f.write_str("Null"),
        }
    }
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentSource {
    Inline {
        value: String,
    },
    File {
        path: String,
        #[serde(default)]
        encoding: Encoding,
    },
}
impl ContentSource {
    pub fn inline(value: impl Into<String>) -> Self {
        Self::Inline {
            value: value.into(),
        }
    }
}
/// The phrase body of `Inline` is redacted; a file source shows its path.
impl fmt::Debug for ContentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inline { value } => f
                .debug_struct("Inline")
                .field("value", &Redacted(value))
                .finish(),
            Self::File { path, encoding } => f
                .debug_struct("File")
                .field("path", path)
                .field("encoding", encoding)
                .finish(),
        }
    }
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    None,
    Text {
        method: OutputMethod,
        source: ContentSource,
    },
    Html {
        html: String,
        plain_text: String,
    },
    Hotkey {
        keys: Vec<Key>,
    },
    SendKeys {
        source: ContentSource,
        #[serde(default)]
        syntax_variables: Vec<String>,
    },
    Steps {
        plan: ActionPlan,
    },
    Replay {
        plan: ActionPlan,
        speed: f64,
    },
    Open {
        target: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        working_directory: Option<String>,
    },
    Reusable {
        action_id: String,
        #[serde(default)]
        parameters: BTreeMap<String, serde_json::Value>,
    },
    Common {
        function: String,
    },
    Quicker {
        action_id: String,
        #[serde(default)]
        parameter: String,
    },
}
/// HTML and plain-text bodies, parameter values, plans and what an `Open` opens (target and
/// arguments: paths, URLs and parameters can carry user data) are redacted; kinds, methods,
/// hotkeys, working directories and ids are shown.
impl fmt::Debug for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Text { method, source } => f
                .debug_struct("Text")
                .field("method", method)
                .field("source", source)
                .finish(),
            Self::Html { html, plain_text } => f
                .debug_struct("Html")
                .field("html", &Redacted(html))
                .field("plain_text", &Redacted(plain_text))
                .finish(),
            Self::Hotkey { keys } => f.debug_struct("Hotkey").field("keys", keys).finish(),
            Self::SendKeys {
                source,
                syntax_variables,
            } => f
                .debug_struct("SendKeys")
                .field("source", source)
                .field("syntax_variables", syntax_variables)
                .finish(),
            Self::Steps { plan } => f.debug_struct("Steps").field("plan", plan).finish(),
            Self::Replay { plan, speed } => f
                .debug_struct("Replay")
                .field("plan", plan)
                .field("speed", speed)
                .finish(),
            Self::Open {
                target,
                args,
                working_directory,
            } => f
                .debug_struct("Open")
                .field("target", &Redacted(target))
                .field("args", &RedactedList(args))
                .field("working_directory", working_directory)
                .finish(),
            Self::Reusable {
                action_id,
                parameters,
            } => f
                .debug_struct("Reusable")
                .field("action_id", action_id)
                .field("parameters", &RedactedValues(parameters))
                .finish(),
            Self::Common { function } => f
                .debug_struct("Common")
                .field("function", function)
                .finish(),
            Self::Quicker {
                action_id,
                parameter,
            } => f
                .debug_struct("Quicker")
                .field("action_id", action_id)
                .field("parameter", &Redacted(parameter))
                .finish(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Key {
    Named { name: String },
    WindowsCode { code: u16 },
    PlatformPrimary,
}
impl Key {
    pub fn named(name: impl Into<String>) -> Self {
        Self::Named { name: name.into() }
    }
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionPlan {
    pub version: u32,
    pub dialect: Dialect,
    pub source: Option<String>,
    pub steps: Vec<Step>,
}
impl Default for ActionPlan {
    fn default() -> Self {
        Self {
            version: version(),
            dialect: Dialect::Structured,
            source: None,
            steps: vec![],
        }
    }
}
/// The script source is redacted, and so are the steps (shown as their count): a recorded
/// plan is the key sequence that was typed, which spells the text out key by key.
impl fmt::Debug for ActionPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionPlan")
            .field("version", &self.version)
            .field("dialect", &self.dialect)
            .field("source", &self.source.as_deref().map(Redacted))
            .field("steps", &RedactedLen(self.steps.len()))
            .finish()
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub operation: Operation,
    #[serde(default)]
    pub span: SourceSpan,
    #[serde(default)]
    pub target: TargetPolicy,
}
impl Step {
    pub fn new(operation: Operation, span: SourceSpan) -> Self {
        Self {
            operation,
            span,
            target: TargetPolicy::Locked,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceSpan {
    pub line: usize,
    pub column: usize,
    pub length: usize,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetPolicy {
    #[default]
    Locked,
    Declared {
        process: String,
    },
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Text {
        text: String,
        method: OutputMethod,
    },
    KeyDown {
        key: Key,
    },
    KeyUp {
        key: Key,
    },
    KeyPress {
        key: Key,
    },
    Hotkey {
        keys: Vec<Key>,
    },
    Delay {
        ms: u64,
    },
    MouseMove {
        x: f64,
        y: f64,
        #[serde(default)]
        relative: bool,
        #[serde(default)]
        percent: bool,
    },
    MouseButton {
        button: MouseButton,
        state: ButtonState,
    },
    Wheel {
        axis: WheelAxis,
        amount: i32,
        unit: WheelUnit,
    },
    PasteFiles {
        paths: Vec<String>,
    },
    PasteImage {
        path: String,
    },
    Html {
        html: String,
        plain_text: String,
    },
    Open {
        target: String,
        args: Vec<String>,
        working_directory: Option<String>,
    },
    Common {
        function: String,
    },
    Reusable {
        action_id: String,
        parameters: BTreeMap<String, serde_json::Value>,
    },
}
/// Text and HTML bodies, parameter values, what an `Open` opens and the key of a key
/// operation are redacted: a recorded macro (`Vec<Step>`) is the typed key sequence, one key
/// operation per key, so each one hides its key, not only the enclosing [`ActionPlan`].
/// Hotkeys, delays, pointer moves, paths, working directories and ids are shown.
impl fmt::Debug for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { text, method } => f
                .debug_struct("Text")
                .field("text", &Redacted(text))
                .field("method", method)
                .finish(),
            Self::KeyDown { .. } => f
                .debug_struct("KeyDown")
                .field("key", &RedactedKey)
                .finish(),
            Self::KeyUp { .. } => f.debug_struct("KeyUp").field("key", &RedactedKey).finish(),
            Self::KeyPress { .. } => f
                .debug_struct("KeyPress")
                .field("key", &RedactedKey)
                .finish(),
            Self::Hotkey { keys } => f.debug_struct("Hotkey").field("keys", keys).finish(),
            Self::Delay { ms } => f.debug_struct("Delay").field("ms", ms).finish(),
            Self::MouseMove {
                x,
                y,
                relative,
                percent,
            } => f
                .debug_struct("MouseMove")
                .field("x", x)
                .field("y", y)
                .field("relative", relative)
                .field("percent", percent)
                .finish(),
            Self::MouseButton { button, state } => f
                .debug_struct("MouseButton")
                .field("button", button)
                .field("state", state)
                .finish(),
            Self::Wheel { axis, amount, unit } => f
                .debug_struct("Wheel")
                .field("axis", axis)
                .field("amount", amount)
                .field("unit", unit)
                .finish(),
            Self::PasteFiles { paths } => {
                f.debug_struct("PasteFiles").field("paths", paths).finish()
            }
            Self::PasteImage { path } => f.debug_struct("PasteImage").field("path", path).finish(),
            Self::Html { html, plain_text } => f
                .debug_struct("Html")
                .field("html", &Redacted(html))
                .field("plain_text", &Redacted(plain_text))
                .finish(),
            Self::Open {
                target,
                args,
                working_directory,
            } => f
                .debug_struct("Open")
                .field("target", &Redacted(target))
                .field("args", &RedactedList(args))
                .field("working_directory", working_directory)
                .finish(),
            Self::Common { function } => f
                .debug_struct("Common")
                .field("function", function)
                .finish(),
            Self::Reusable {
                action_id,
                parameters,
            } => f
                .debug_struct("Reusable")
                .field("action_id", action_id)
                .field("parameters", &RedactedValues(parameters))
                .finish(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub path: String,
    pub span: SourceSpan,
    pub severity: Severity,
}
impl Diagnostic {
    pub fn error(code: &str, message: impl Into<String>, span: SourceSpan) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            path: String::new(),
            span,
            severity: Severity::Error,
        }
    }
    pub fn warning(code: &str, message: impl Into<String>, span: SourceSpan) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(code, message, span)
        }
    }
    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }
}
