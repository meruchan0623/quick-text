use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_STEPS: usize = 10000;
pub const MAX_DELAY_MS: u64 = 3_600_000;
fn version() -> u32 {
    SCHEMA_VERSION
}
fn yes() -> bool {
    true
}
fn default_terminators() -> Vec<String> {
    ["space", "tab", "enter", "semicolon"]
        .map(str::to_string)
        .to_vec()
}
fn default_separators() -> String {
    " \t\r\n".into()
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub revision: u64,
    pub enabled: bool,
    pub globals: GlobalSettings,
    pub rules: Vec<Rule>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: version(),
            revision: 0,
            enabled: true,
            globals: GlobalSettings::default(),
            rules: vec![],
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalSettings {
    pub terminators: Vec<String>,
    pub word_separators: String,
    pub blacklist: Vec<String>,
    pub ime_erasure: ImeErasure,
}
impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            terminators: default_terminators(),
            word_separators: default_separators(),
            blacklist: vec![],
            ime_erasure: ImeErasure::Escape,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
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
}
#[derive(Deserialize)]
#[serde(remote = "Rule", default, deny_unknown_fields)]
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
        }
    }
}
// Accept the removed field only while reading legacy configuration. It is never
// retained in the rule model or serialized back into active configuration.
impl<'de> Deserialize<'de> for Rule {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
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
        }
    }
}
impl Rule {
    pub fn effective_ime_erasure(&self, globals: &GlobalSettings) -> ImeErasure {
        self.ime_erasure.unwrap_or(globals.ime_erasure)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchSpec {
    pub kind: MatchKind,
    pub value: String,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParameterSource {
    DefaultMatch,
    Fixed { value: String },
    FirstCapture,
    CaptureTemplate { template: String },
    Null,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }
}
