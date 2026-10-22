//! `CommandError`, the error type of every Tauri command (B-45,
//! docs/ipc-contract-0.4.md §2). Errors carry a code and structured detail
//! only; the frontend translates them. Original error text goes to the log.
use quicker_domain::{Diagnostic, SourceSpan};
use quicker_platform::health::HealthState;
use serde::Serialize;
use std::{fmt::Display, path::Path};

#[derive(Debug, thiserror::Error, Serialize)]
#[serde(tag = "code", content = "detail", rename_all = "snake_case")]
pub enum CommandError {
    #[error("validation failed: {}", summary(.diagnostics))]
    Validation { diagnostics: Vec<Diagnostic> },
    #[allow(dead_code)] // 0.5 (save_rule).
    #[error("rule {id} conflicts with {current}")]
    RuleConflict { id: String, current: String },
    #[error("revision conflict; current revision is {current}")]
    RevisionConflict { current: u64 },
    #[allow(dead_code)] // Safe mode (B-04).
    #[error("safe mode is active ({reason:?})")]
    SafeMode { reason: SafeModeReason },
    #[error("{op:?} failed (path: {path:?}, os error: {os_code:?})")]
    Io {
        op: IoOp,
        path: Option<String>,
        os_code: Option<i32>,
    },
    #[error("{kind:?} {id} not found")]
    NotFound { kind: EntityKind, id: String },
    #[error("the executor is busy")]
    Busy,
    #[error("engine unavailable ({health:?})")]
    EngineUnavailable { health: HealthState },
    #[allow(dead_code)] // Only raised on platforms without a login item.
    #[error("unsupported capability {capability}")]
    Unsupported { capability: String },
    #[allow(dead_code)] // 0.5 (global hotkeys).
    #[error("hotkey {combo} is already in use")]
    HotkeyConflict { combo: String },
    #[allow(dead_code)] // 0.5 (interaction tokens).
    #[error("token expired")]
    TokenExpired,
    #[allow(dead_code)] // 0.5 (forms); dialogs return `Ok(None)` instead.
    #[error("cancelled")]
    Cancelled,
    #[error("internal error (trace id {trace_id})")]
    Internal { trace_id: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // Safe mode (B-04).
pub enum SafeModeReason {
    CrashLoop,
    Locked,
    NewerSchema,
    NoValidConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // Every operation the contract names; not all are used yet.
pub enum IoOp {
    Read,
    Write,
    Rename,
    CreateDir,
    Remove,
    Open,
    Dialog,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // Every entity the contract names; not all are used yet.
pub enum EntityKind {
    Rule,
    Group,
    Revision,
    File,
    History,
    Window,
}

impl CommandError {
    /// Logs `err` under a fresh trace id; the frontend receives only the id.
    pub fn internal(err: impl Display) -> Self {
        let trace_id = uuid::Uuid::new_v4().to_string();
        tracing::error!(event = "command.internal", trace_id = %trace_id, error = %err, "command failed");
        Self::Internal { trace_id }
    }
    pub fn validation(diagnostics: Vec<Diagnostic>) -> Self {
        Self::Validation { diagnostics }
    }
    /// The input engine cannot run. Until the supervisor reports real health
    /// (B-03), a platform that refuses to start or resume counts as `Failed`;
    /// the platform's reason goes to the log.
    pub fn engine_unavailable(reason: impl Display) -> Self {
        tracing::warn!(event = "engine.unavailable", reason = %reason, "engine unavailable");
        Self::EngineUnavailable {
            health: HealthState::Failed,
        }
    }
    /// A single error-level diagnostic about a file rather than a rule field.
    pub fn invalid_file(code: &str, message: impl Into<String>, path: &Path) -> Self {
        Self::validation(vec![Diagnostic::error(
            code,
            message,
            SourceSpan::default(),
        )
        .at(file_label(path).unwrap_or_default())])
    }
    /// I/O failure. Only the file name reaches the frontend or the log (§2: no
    /// full paths into the user profile); the OS error text goes to the log.
    pub fn io(op: IoOp, path: Option<&Path>, err: &std::io::Error) -> Self {
        let path = path.and_then(file_label);
        tracing::warn!(event = "command.io", ?op, path = ?path, error = %err, "command io failed");
        Self::Io {
            op,
            path,
            os_code: err.raw_os_error(),
        }
    }
}

pub fn file_label(path: &Path) -> Option<String> {
    path.file_name().map(|x| x.to_string_lossy().into_owned())
}

fn summary(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shape(e: CommandError) -> serde_json::Value {
        serde_json::to_value(e).unwrap()
    }

    /// One case per variant, checked against docs/ipc-contract-0.4.md §2.
    #[test]
    fn every_variant_matches_the_ipc_contract() {
        let diagnostic = Diagnostic::error(
            "rule_schema",
            "bad",
            SourceSpan {
                line: 1,
                column: 2,
                length: 3,
            },
        )
        .at("rules[0]");
        assert_eq!(
            shape(CommandError::validation(vec![diagnostic])),
            json!({"code":"validation","detail":{"diagnostics":[{
                "code":"rule_schema","message":"bad","path":"rules[0]",
                "span":{"line":1,"column":2,"length":3},"severity":"error"}]}})
        );
        assert_eq!(
            shape(CommandError::RuleConflict {
                id: "r1".into(),
                current: "v2".into()
            }),
            json!({"code":"rule_conflict","detail":{"id":"r1","current":"v2"}})
        );
        assert_eq!(
            shape(CommandError::RevisionConflict { current: 7 }),
            json!({"code":"revision_conflict","detail":{"current":7}})
        );
        assert_eq!(
            shape(CommandError::Io {
                op: IoOp::CreateDir,
                path: Some("active.json".into()),
                os_code: Some(5)
            }),
            json!({"code":"io","detail":{"op":"create_dir","path":"active.json","os_code":5}})
        );
        assert_eq!(
            shape(CommandError::Io {
                op: IoOp::Dialog,
                path: None,
                os_code: None
            }),
            json!({"code":"io","detail":{"op":"dialog","path":null,"os_code":null}})
        );
        assert_eq!(
            shape(CommandError::NotFound {
                kind: EntityKind::Revision,
                id: "previous".into()
            }),
            json!({"code":"not_found","detail":{"kind":"revision","id":"previous"}})
        );
        assert_eq!(shape(CommandError::Busy), json!({"code":"busy"}));
        assert_eq!(
            shape(CommandError::EngineUnavailable {
                health: HealthState::Recovering
            }),
            json!({"code":"engine_unavailable","detail":{"health":"recovering"}})
        );
        assert_eq!(
            shape(CommandError::Unsupported {
                capability: "autostart".into()
            }),
            json!({"code":"unsupported","detail":{"capability":"autostart"}})
        );
        assert_eq!(
            shape(CommandError::HotkeyConflict {
                combo: "Ctrl+Alt+Space".into()
            }),
            json!({"code":"hotkey_conflict","detail":{"combo":"Ctrl+Alt+Space"}})
        );
        assert_eq!(
            shape(CommandError::TokenExpired),
            json!({"code":"token_expired"})
        );
        assert_eq!(shape(CommandError::Cancelled), json!({"code":"cancelled"}));
        assert_eq!(
            shape(CommandError::Internal {
                trace_id: "t".into()
            }),
            json!({"code":"internal","detail":{"trace_id":"t"}})
        );
    }

    #[test]
    fn enum_values_are_snake_case() {
        let reasons = [
            (SafeModeReason::CrashLoop, "crash_loop"),
            (SafeModeReason::Locked, "locked"),
            (SafeModeReason::NewerSchema, "newer_schema"),
            (SafeModeReason::NoValidConfig, "no_valid_config"),
        ];
        for (reason, code) in reasons {
            assert_eq!(
                shape(CommandError::SafeMode { reason }),
                json!({"code":"safe_mode","detail":{"reason":code}})
            );
        }
        let ops = [
            (IoOp::Read, "read"),
            (IoOp::Write, "write"),
            (IoOp::Rename, "rename"),
            (IoOp::CreateDir, "create_dir"),
            (IoOp::Remove, "remove"),
            (IoOp::Open, "open"),
            (IoOp::Dialog, "dialog"),
            (IoOp::Other, "other"),
        ];
        for (op, code) in ops {
            assert_eq!(serde_json::to_value(op).unwrap(), json!(code));
        }
        let kinds = [
            (EntityKind::Rule, "rule"),
            (EntityKind::Group, "group"),
            (EntityKind::Revision, "revision"),
            (EntityKind::File, "file"),
            (EntityKind::History, "history"),
            (EntityKind::Window, "window"),
        ];
        for (kind, code) in kinds {
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(code));
        }
    }

    #[test]
    fn internal_returns_a_fresh_trace_id_without_the_message() {
        let a = shape(CommandError::internal("secret path C:\\Users\\x"));
        let b = shape(CommandError::internal("other"));
        let id = a["detail"]["trace_id"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok());
        assert_ne!(a, b);
        assert!(!a.to_string().contains("secret"));
    }

    #[test]
    fn io_keeps_only_the_file_name_and_os_code() {
        let err = std::io::Error::from_raw_os_error(5);
        let e = CommandError::io(
            IoOp::Write,
            Some(Path::new("/home/someone/data/active.json")),
            &err,
        );
        assert_eq!(
            shape(e),
            json!({"code":"io","detail":{"op":"write","path":"active.json","os_code":5}})
        );
    }
}
