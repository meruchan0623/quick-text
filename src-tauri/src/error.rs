//! Command error mapping (B-45).
use quicker_domain::Diagnostic;

pub fn diagnostics(e: Vec<Diagnostic>) -> String {
    serde_json::to_string(&e).unwrap_or_else(|_| "Validation failed".into())
}
