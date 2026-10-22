//! Commands marked deprecated in §7.4. 0.5 replaces them (`save_rule(s)`,
//! save-is-publish, `undo_last_change` / `restore_revision`, `pick_window`,
//! `export`, `import_preview` / `import_commit`) and keeps these names as
//! aliases that call the new implementations; 0.6 removes them.
use crate::{
    app_state::{AppState, Managed},
    commands::import_export::{dialog, CONFIG_FILTERS},
    error::{CommandError, IoOp},
    store::merge_import_metadata,
};
use quicker_domain::*;
use std::{fs, path::PathBuf};
use tauri::{Manager, State};

#[tauri::command]
pub fn save_draft(config: Config, store: State<Managed>) -> Result<AppState, CommandError> {
    let mut s = store.lock();
    s.save(config)?;
    Ok(s.state())
}
#[tauri::command]
pub fn apply_settings(store: State<Managed>) -> Result<AppState, CommandError> {
    let mut s = store.lock();
    let c = s.draft.clone();
    s.publish(c)?;
    Ok(s.state())
}
#[tauri::command]
pub fn discard_draft(store: State<Managed>) -> Result<AppState, CommandError> {
    let mut s = store.lock();
    s.discard()?;
    Ok(s.state())
}
#[tauri::command]
pub fn rollback(store: State<Managed>) -> Result<AppState, CommandError> {
    let mut s = store.lock();
    s.rollback_previous()?;
    Ok(s.state())
}
#[tauri::command]
pub async fn get_foreground(
    app: tauri::AppHandle,
) -> Result<quicker_platform::Foreground, CommandError> {
    if let Some(w) = app.get_webview_window("main") {
        w.hide().map_err(CommandError::internal)?;
    }
    let result = tauri::async_runtime::spawn_blocking(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        quicker_platform::foreground()
    })
    .await
    .map_err(CommandError::internal)?;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
    Ok(result)
}
/// Returns `Ok(None)` when the user cancels the save dialog (B-42).
#[tauri::command]
pub fn export_config(
    path: Option<String>,
    store: State<Managed>,
) -> Result<Option<String>, CommandError> {
    // Pick the target before taking the store lock: the dialog is modal and
    // must not block other commands and the tray while it is open.
    let target = path.map(PathBuf::from).or_else(|| {
        dialog(CONFIG_FILTERS)
            .set_file_name("quicker-text-package.json")
            .save_file()
    });
    let Some(target) = target else {
        return Ok(None);
    };
    let s = store.lock();
    let unresolved = s
        .unresolved
        .iter()
        .filter(|x| s.draft.rules.iter().any(|r| r.id == x.rule_id))
        .cloned()
        .collect();
    let package =
        export_package_with_unresolved(&s.draft, unresolved).map_err(CommandError::validation)?;
    fs::write(&target, package).map_err(|e| CommandError::io(IoOp::Write, Some(&target), &e))?;
    Ok(Some(target.to_string_lossy().into_owned()))
}
/// Without `path`, asks for a JSON package; `Ok(None)` when the user cancels.
#[tauri::command]
pub fn import_config(
    path: Option<String>,
    strategy: String,
    store: State<Managed>,
) -> Result<Option<serde_json::Value>, CommandError> {
    let path = match path {
        Some(path) => PathBuf::from(path),
        None => match dialog(CONFIG_FILTERS).pick_file() {
            Some(path) => path,
            None => return Ok(None),
        },
    };
    let source =
        fs::read_to_string(&path).map_err(|e| CommandError::io(IoOp::Read, Some(&path), &e))?;
    let imported = import_package(&source).map_err(CommandError::validation)?;
    let mut incoming = imported.config;
    let mut s = store.lock();
    let mut draft = s.draft.clone();
    let mut report: Vec<String> = imported
        .items
        .iter()
        .map(|x| {
            format!(
                "{}: {:?} {}",
                x.rule_id,
                x.status,
                x.diagnostics
                    .iter()
                    .map(|d| d.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })
        .collect();
    for r in &mut incoming.rules {
        if matches!(
            r.action,
            Action::Quicker { .. }
                | Action::Reusable { .. }
                | Action::Open { .. }
                | Action::Steps { .. }
                | Action::Replay { .. }
                | Action::SendKeys { .. }
        ) {
            r.enabled = false;
            report.push(format!(
                "{}: imported side-effect action disabled pending review",
                r.id
            ));
        }
    }
    let conflicts: Vec<_> = incoming
        .rules
        .iter()
        .filter(|r| draft.rules.iter().any(|x| x.id == r.id))
        .map(|r| r.id.clone())
        .collect();
    report.push(format!(
        "{} conflicts: {}; strategy: {}",
        conflicts.len(),
        conflicts.join(", "),
        strategy
    ));
    if strategy == "keep_both" {
        draft = merge_import(
            &draft,
            &ImportReport {
                config: incoming.clone(),
                items: vec![],
                unresolved: vec![],
            },
            ConflictStrategy::KeepBoth,
        )
        .map_err(CommandError::validation)?;
    } else if strategy == "replace" {
        incoming.revision = s.active.revision;
        draft = incoming.clone();
    } else {
        for r in incoming.rules.iter().cloned() {
            if let Some(i) = draft.rules.iter().position(|x| x.id == r.id) {
                if strategy == "overwrite" {
                    draft.rules[i] = r
                } else {
                    report.push(format!("{}: conflicting id skipped", r.id));
                }
            } else {
                draft.rules.push(r)
            }
        }
    }
    let unresolved = merge_import_metadata(
        &strategy,
        &s.draft,
        &incoming,
        &draft,
        &s.unresolved,
        &imported.unresolved,
    );
    s.pending_import = Some((draft.clone(), unresolved));
    Ok(Some(serde_json::json!({"config":draft,"report":report})))
}
