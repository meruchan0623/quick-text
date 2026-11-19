//! Update commands of the settings page: `app_version`, `check_update`, `download_update`,
//! `install_update`. They run the same dual-server check, download and install as the background
//! auto-update ([`crate::autoupdate`]); arguments use Tauri's default camelCase names, as
//! src/lib/update.ts sends them.
use crate::{
    autoupdate,
    error::CommandError,
    update::{self, UpdateCheck, UpdateDownload, UpdateProgress},
};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

/// At most this often a progress event (plus the last one).
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

/// This build's version (`tauri.conf.json` `version`).
#[tauri::command]
pub fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

/// Reads both update servers and reports the newest version above this build's.
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<UpdateCheck, CommandError> {
    let check = autoupdate::check_now(&app).await?;
    tracing::info!(
        event = "update.checked",
        current = %check.current,
        latest = check.latest.as_ref().map(|l| l.version.as_str()),
        source = check.latest.as_ref().map(|l| format!("{:?}", l.source)),
        ready = check.ready,
        "update check done"
    );
    Ok(check)
}

/// Downloads and verifies the version the last check found. Progress goes out as
/// `update-download-progress`.
#[tauri::command]
pub async fn download_update(
    app: AppHandle,
    version: String,
) -> Result<UpdateDownload, CommandError> {
    let emitter = app.clone();
    let mut last: Option<Instant> = None;
    let done = autoupdate::download_now(&app, &version, |downloaded, total| {
        let finished = total.is_some_and(|t| downloaded >= t);
        if !finished && last.is_some_and(|at| at.elapsed() < PROGRESS_INTERVAL) {
            return;
        }
        last = Some(Instant::now());
        let payload = UpdateProgress {
            version: version.clone(),
            downloaded,
            total,
        };
        if let Err(e) = emitter.emit(update::PROGRESS_EVENT, payload) {
            tracing::debug!(event = "update.progress_emit_failed", error = %e, "progress not delivered");
        }
    })
    .await?;
    tracing::info!(event = "update.downloaded", version = %done.version, "update downloaded from settings");
    Ok(done)
}

/// Installs the downloaded update and restarts; returns only when that did not happen.
#[tauri::command]
pub fn install_update(app: AppHandle) -> Result<(), CommandError> {
    autoupdate::install_from_settings(&app)
}
