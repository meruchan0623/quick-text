//! Diagnostics commands (§7.4 「诊断」, docs/ipc-contract-0.4.md §5).
//!
//! Every command uses `rename_all = "snake_case"`, so the frontend passes
//! arguments with the contract's names (`ttl_hours`, not Tauri's default
//! camelCase `ttlHours`).
use crate::{
    diagnostics::level::{LevelState, LogLevel, LogLevelControl},
    error::CommandError,
};
use tauri::State;

/// `ttl_hours: null` restores the default level (`info`).
#[tauri::command(rename_all = "snake_case")]
pub fn set_log_level(
    control: State<LogLevelControl>,
    level: LogLevel,
    ttl_hours: Option<f64>,
) -> Result<LevelState, CommandError> {
    control.set(level, ttl_hours)
}
