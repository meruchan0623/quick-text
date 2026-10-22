//! Observability (§7.1 「可观测性」, B-09, S-08): log files, crash records
//! and the diagnostics bundle. The log event naming convention is documented
//! in [`logging`].
pub mod level;
pub mod logging;
pub mod panic;
pub mod redact;
