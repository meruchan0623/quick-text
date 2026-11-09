//! Application settings (0.5 adds `set_settings`, `get_prefs`, `set_prefs`).
use crate::{
    app_state::Managed,
    autostart,
    error::CommandError,
    hint::HintControl,
    store::prefs::{self, CandidateHintPrefs, HINT_DURATION_MS},
};
use serde::Serialize;
use tauri::State;

#[tauri::command]
pub fn set_autostart(enabled: bool, store: State<Managed>) -> Result<bool, CommandError> {
    store.lock().ensure_writable()?;
    autostart::set_enabled(enabled)
}

/// Whether to start listening on its own at the next start (B-10, contract
/// §5): writes `prefs.json` `app.auto_listen_on_start` and returns the value
/// now stored. Allowed in safe mode, unlike `set_autostart`: prefs live
/// apart from the configuration, revisions and exports that safe mode
/// protects, nothing changes before the next start, and the write is a
/// read-modify-write that never replaces a prefs file it could not read.
#[tauri::command(rename_all = "snake_case")]
pub fn set_auto_listen(enabled: bool, store: State<Managed>) -> Result<bool, CommandError> {
    write_auto_listen(&store, enabled)
}

fn write_auto_listen(store: &Managed, enabled: bool) -> Result<bool, CommandError> {
    // The store lock is held only to read the directory, not for the write.
    let dir = store.lock().dir().to_path_buf();
    let changed = prefs::update_prefs(&dir, |prefs| {
        let changed = prefs.app.auto_listen_on_start != enabled;
        prefs.app.auto_listen_on_start = enabled;
        changed
    })?;
    tracing::info!(
        event = "prefs.auto_listen",
        enabled,
        changed,
        "auto listen on start set"
    );
    Ok(enabled)
}

/// `candidate_hint` in `load_state` and the answer of `set_candidate_hint`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct CandidateHint {
    pub enabled: bool,
    pub duration_ms: u64,
}
impl From<&CandidateHintPrefs> for CandidateHint {
    fn from(prefs: &CandidateHintPrefs) -> Self {
        Self {
            enabled: prefs.enabled,
            duration_ms: prefs.duration_ms,
        }
    }
}

/// The beginner typing hint (settings 「新手提示」): writes `prefs.json`
/// `app.candidate_hint` and applies it at once. Allowed in safe mode, like
/// `set_auto_listen`: it is a preference of this machine, not configuration.
#[tauri::command(rename_all = "snake_case")]
pub fn set_candidate_hint(
    enabled: bool,
    duration_ms: u64,
    store: State<Managed>,
    hint: State<HintControl>,
) -> Result<CandidateHint, CommandError> {
    let stored = write_candidate_hint(&store, enabled, duration_ms)?;
    hint.apply(stored.clone());
    Ok(CandidateHint::from(&stored))
}

fn write_candidate_hint(
    store: &Managed,
    enabled: bool,
    duration_ms: u64,
) -> Result<CandidateHintPrefs, CommandError> {
    if !HINT_DURATION_MS.contains(&duration_ms) {
        return Err(CommandError::validation(vec![
            quicker_domain::Diagnostic::error(
                "candidate_hint_duration",
                "显示时长须在 1 到 10 秒之间",
                quicker_domain::SourceSpan::default(),
            )
            .at("candidate_hint.duration_ms"),
        ]));
    }
    let dir = store.lock().dir().to_path_buf();
    let mut stored = CandidateHintPrefs::default();
    prefs::update_prefs(&dir, |prefs| {
        let hint = &mut prefs.app.candidate_hint;
        let changed = hint.enabled != enabled || hint.duration_ms != duration_ms;
        hint.enabled = enabled;
        hint.duration_ms = duration_ms;
        stored = hint.clone();
        changed
    })?;
    tracing::info!(
        event = "prefs.candidate_hint",
        enabled,
        duration_ms,
        "candidate hint set"
    );
    Ok(stored)
}

/// 「预览效果」: shows the hint window once with sample rules.
#[tauri::command]
pub fn preview_candidate_hint(hint: State<HintControl>) {
    hint.preview();
}

/// The hint page laid out list `seq` and it is `height` CSS pixels high.
#[tauri::command]
pub fn candidate_hint_measured(seq: u64, height: f64, hint: State<HintControl>) {
    if height.is_finite() && height > 0.0 {
        hint.measured(seq, height);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{error::SafeModeReason, store::Store};

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("quicker-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn set_auto_listen_writes_prefs_and_returns_the_stored_value() {
        let dir = temp_dir();
        let store = Managed::new(Store::load(dir.clone()));
        // On by default.
        assert!(prefs::auto_listen_on_start(&dir));
        assert!(!write_auto_listen(&store, false).unwrap());
        assert!(!prefs::auto_listen_on_start(&dir));
        // Asking again for the stored value succeeds and changes nothing.
        assert!(!write_auto_listen(&store, false).unwrap());
        assert!(write_auto_listen(&store, true).unwrap());
        assert!(prefs::auto_listen_on_start(&dir));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn set_candidate_hint_writes_prefs_and_rejects_out_of_range_durations() {
        let dir = temp_dir();
        let store = Managed::new(Store::load(dir.clone()));
        assert_eq!(prefs::candidate_hint(&dir), CandidateHintPrefs::default());
        assert!(prefs::candidate_hint(&dir).enabled);
        assert_eq!(prefs::candidate_hint(&dir).duration_ms, 2000);
        let stored = write_candidate_hint(&store, false, 2000).unwrap();
        assert!(!stored.enabled);
        assert!(!prefs::candidate_hint(&dir).enabled);
        let stored = write_candidate_hint(&store, true, 3500).unwrap();
        assert!(stored.enabled);
        assert_eq!(prefs::candidate_hint(&dir).duration_ms, 3500);
        assert!(write_candidate_hint(&store, true, 999).is_err());
        assert!(write_candidate_hint(&store, true, 10_001).is_err());
        assert_eq!(prefs::candidate_hint(&dir).duration_ms, 3500);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A wrong stored value takes its default and leaves the file readable.
    #[test]
    fn a_wrong_candidate_hint_value_takes_its_default() {
        let dir = temp_dir();
        std::fs::write(
            dir.join("prefs.json"),
            br#"{"app":{"auto_listen_on_start":false,"candidate_hint":{"enabled":"yes","duration_ms":50}}}"#,
        )
        .unwrap();
        assert_eq!(prefs::candidate_hint(&dir), CandidateHintPrefs::default());
        assert!(!prefs::auto_listen_on_start(&dir));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Prefs are not the configuration safe mode protects.
    #[test]
    fn set_auto_listen_is_allowed_in_safe_mode() {
        let dir = temp_dir();
        std::fs::write(dir.join("active.json"), b"{").unwrap();
        let store = Managed::new(Store::load(dir.clone()));
        assert_eq!(
            store.lock().safe_mode(),
            Some(SafeModeReason::NoValidConfig)
        );
        assert!(!write_auto_listen(&store, false).unwrap());
        assert!(!prefs::auto_listen_on_start(&dir));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
