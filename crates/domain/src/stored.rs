//! Tolerant reading of stored configuration (B-43). Parsing only; the host does the I/O.
use crate::*;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fmt;

/// A stored configuration and what reading it had to work around.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredConfig {
    pub config: Config,
    /// Warnings only: [`diagnostic_code::FIELD_RESET`], [`diagnostic_code::RULE_UNREADABLE`]
    /// and [`diagnostic_code::TERMINATOR_UNSUPPORTED`], in that order.
    pub diagnostics: Vec<Diagnostic>,
}

/// Why a stored configuration could not be read at all. Messages never quote file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredConfigError {
    /// Not UTF-8, not JSON, the root is not an object, or `rules` is not an array
    /// ([`diagnostic_code::CONFIG_MALFORMED`]).
    Malformed { message: String, span: SourceSpan },
    /// `version` is above [`SUPPORTED_SCHEMA_VERSION`] ([`diagnostic_code::SCHEMA_NEWER`]):
    /// written by a newer release. Open read-only and never overwrite (SafeMode(NewerSchema)).
    NewerSchema { version: u64 },
    /// `version` is 0 or not an unsigned integer ([`diagnostic_code::SCHEMA_VERSION`]).
    UnsupportedVersion,
}
impl StoredConfigError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed { .. } => diagnostic_code::CONFIG_MALFORMED,
            Self::NewerSchema { .. } => diagnostic_code::SCHEMA_NEWER,
            Self::UnsupportedVersion => diagnostic_code::SCHEMA_VERSION,
        }
    }
    pub fn diagnostic(&self) -> Diagnostic {
        let span = match self {
            Self::Malformed { span, .. } => span.clone(),
            _ => SourceSpan::default(),
        };
        let path = match self {
            Self::Malformed { .. } => "",
            _ => "version",
        };
        Diagnostic::error(self.code(), self.to_string(), span).at(path)
    }
}
impl fmt::Display for StoredConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { message, .. } => write!(f, "Configuration is malformed: {message}"),
            Self::NewerSchema { version } => write!(
                f,
                "Configuration version {version} was written by a newer release (supported: 1..={SUPPORTED_SCHEMA_VERSION})"
            ),
            Self::UnsupportedVersion => f.write_str("Unsupported configuration version"),
        }
    }
}
impl std::error::Error for StoredConfigError {}

/// Reads `active.json`, `draft.json`, `previous.json` or `revisions/<n>.json` content:
/// strips a UTF-8 BOM, gates `version` to `1..=SUPPORTED_SCHEMA_VERSION`, then reads leniently.
/// A wrongly typed top-level or `globals` field falls back to its default, a `rules` entry that
/// does not read is kept verbatim in [`Config::unreadable_rules`], and unknown fields stay in
/// `extra`; each of these is a warning. Writing the result back keeps everything it kept.
pub fn read_stored_config(bytes: &[u8]) -> Result<StoredConfig, StoredConfigError> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|e| StoredConfigError::Malformed {
        message: e.to_string(),
        span: SourceSpan::default(),
    })?;
    let value: Value = serde_json::from_str(text).map_err(|e| StoredConfigError::Malformed {
        message: e.to_string(),
        span: SourceSpan {
            line: e.line(),
            column: e.column(),
            length: 1,
        },
    })?;
    if let Some(version) = value.get("version") {
        match version.as_u64() {
            Some(v) if v > u64::from(SUPPORTED_SCHEMA_VERSION) => {
                return Err(StoredConfigError::NewerSchema { version: v })
            }
            Some(v) if v >= 1 => {}
            _ => return Err(StoredConfigError::UnsupportedVersion),
        }
    }
    let mut diagnostics = vec![];
    let config = config_from_value(value, Mode::Lenient, &mut diagnostics).map_err(|message| {
        StoredConfigError::Malformed {
            message,
            span: SourceSpan::default(),
        }
    })?;
    diagnostics.extend(config.terminator_diagnostics());
    Ok(StoredConfig {
        config,
        diagnostics,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `Deserialize for Config`: a wrongly typed known field is an error.
    Strict,
    /// Stored files: a wrongly typed known field falls back to its default with a warning.
    Lenient,
}

fn field_reset(path: &str) -> Diagnostic {
    Diagnostic::warning(
        diagnostic_code::FIELD_RESET,
        "Field has the wrong type and was reset to its default",
        SourceSpan::default(),
    )
    .at(path)
}

/// Reads one known field; `Ok(None)` when it is absent or (leniently) reset.
fn read_field<T: DeserializeOwned>(
    value: Option<Value>,
    path: &str,
    mode: Mode,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Option<T>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    match T::deserialize(&value) {
        Ok(v) => Ok(Some(v)),
        Err(e) if mode == Mode::Strict => Err(format!("{path}: {e}")),
        Err(_) => {
            diagnostics.push(field_reset(path));
            Ok(None)
        }
    }
}

/// Lenient read of a `#[serde(default)]` struct with an `extra` map: only the fields that do
/// not read fall back to their defaults.
fn read_struct_leniently<T: DeserializeOwned + Default>(
    value: &Value,
    path: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> T {
    if let Ok(v) = T::deserialize(value) {
        return v;
    }
    let Some(fields) = value.as_object() else {
        diagnostics.push(field_reset(path));
        return T::default();
    };
    let mut kept = Map::new();
    for (key, field) in fields {
        let single = Value::Object(Map::from_iter([(key.clone(), field.clone())]));
        if T::deserialize(&single).is_ok() {
            kept.insert(key.clone(), field.clone());
        } else {
            diagnostics.push(field_reset(&format!("{path}.{key}")));
        }
    }
    T::deserialize(&Value::Object(kept)).unwrap_or_else(|_| {
        diagnostics.push(field_reset(path));
        T::default()
    })
}

fn rule_unreadable(entry: &UnreadableRule) -> Diagnostic {
    let fields = entry.unreadable_fields();
    let message = if fields.is_empty() {
        "Rule entry could not be read; it is kept unchanged and stays off".to_string()
    } else {
        format!(
            "Rule entry could not be read ({}); it is kept unchanged and stays off",
            fields.join(", ")
        )
    };
    Diagnostic::warning(
        diagnostic_code::RULE_UNREADABLE,
        message,
        SourceSpan::default(),
    )
    .at(format!("rules[{}]", entry.index))
}

/// Shared by `Deserialize for Config` and [`read_stored_config`]. Unknown fields go to
/// `extra` and unreadable rule entries to `unreadable_rules` in both modes; `version` is
/// read as is (callers gate it).
pub(crate) fn config_from_value(
    value: Value,
    mode: Mode,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Config, String> {
    let Value::Object(mut fields) = value else {
        return Err("configuration must be a JSON object".into());
    };
    let mut config = Config::default();
    if let Some(version) = read_field(
        fields.remove("version"),
        "version",
        Mode::Strict,
        diagnostics,
    )? {
        config.version = version;
    }
    if let Some(revision) = read_field(fields.remove("revision"), "revision", mode, diagnostics)? {
        config.revision = revision;
    }
    if let Some(enabled) = read_field(fields.remove("enabled"), "enabled", mode, diagnostics)? {
        config.enabled = enabled;
    }
    if let Some(globals) = fields.remove("globals") {
        config.globals = match mode {
            Mode::Strict => {
                GlobalSettings::deserialize(&globals).map_err(|e| format!("globals: {e}"))?
            }
            Mode::Lenient => read_struct_leniently(&globals, "globals", diagnostics),
        };
    }
    match fields.remove("rules") {
        None => {}
        Some(Value::Array(entries)) => {
            for (index, raw) in entries.into_iter().enumerate() {
                match Rule::deserialize(&raw) {
                    Ok(rule) => config.rules.push(rule),
                    Err(_) => {
                        let entry = UnreadableRule { index, raw };
                        diagnostics.push(rule_unreadable(&entry));
                        config.unreadable_rules.push(entry);
                    }
                }
            }
        }
        Some(_) => return Err("rules must be an array".into()),
    }
    config.extra = fields;
    Ok(config)
}
