//! Update commands (see [`crate::update`]): `check_update`, `download_update`,
//! `reveal_update`. Arguments use Tauri's default camelCase names
//! (`includePrerelease`), as src/lib/update.ts sends them.
use crate::{
    app_state::DataDir,
    commands::diagnostics::reveal_file,
    error::{CommandError, EntityKind, IoOp},
    update::{self, kind, Platform, UpdateCheck, UpdateDownload, UpdateProgress},
};
use parking_lot::Mutex;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
use tauri::{AppHandle, Emitter, Manager, State};

/// One download at a time; the files this process downloaded (the only ones `reveal_update`
/// shows).
#[derive(Default)]
pub struct UpdateState {
    downloading: AtomicBool,
    downloaded: Mutex<HashSet<PathBuf>>,
}

/// Clears `downloading` when the download ends, however it ends.
struct Running<'a>(&'a AtomicBool);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// This build's version (`tauri.conf.json` `version`, as stamped by CI).
#[tauri::command]
pub fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

/// The newest release on GitHub (prereleases only when `include_prerelease`), compared with this build's version (`tauri.conf.json` `version`, as stamped by CI).
#[tauri::command]
pub async fn check_update(
    app: AppHandle,
    include_prerelease: bool,
) -> Result<UpdateCheck, CommandError> {
    let current = app.package_info().version.clone();
    let releases = update::list_releases(&update::user_agent(&current)).await?;
    let check = update::evaluate(&releases, &current, include_prerelease, Platform::current());
    tracing::info!(
        event = "update.checked",
        current = %check.current,
        latest = check.latest.as_ref().map(|l| l.version.as_str()),
        update_available = check.update_available,
        include_prerelease,
        "update check done"
    );
    Ok(check)
}

/// Downloads this platform's package of the release `tag` into the Downloads folder (the data
/// directory's `updates/` when there is none), verified against the release's SHA256SUMS file.
/// Progress goes out as `update-download-progress`.
#[tauri::command]
pub async fn download_update(
    app: AppHandle,
    state: State<'_, UpdateState>,
    data_dir: State<'_, DataDir>,
    tag: String,
) -> Result<UpdateDownload, CommandError> {
    if state
        .downloading
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(update::fail(
            kind::BUSY,
            "正在下载更新，请等这次下载完成",
            "a download is already running",
        ));
    }
    let _running = Running(&state.downloading);
    let user_agent = update::user_agent(&app.package_info().version);
    let release = update::release_by_tag(&user_agent, &tag).await?;
    let platform = Platform::current();
    let asset = update::pick_asset(&release, platform)
        .cloned()
        .ok_or_else(|| {
            update::fail(
                kind::NO_ASSET,
                "这个版本没有适用于本机系统的安装包，请到发布页查看",
                format!("{tag}: no asset for {platform:?}"),
            )
        })?;
    let sums = update::pick_sums(&release, platform).cloned();
    let dir = app
        .path()
        .download_dir()
        .ok()
        .filter(|d| d.is_dir())
        .unwrap_or_else(|| data_dir.0.join("updates"));
    let emitter = app.clone();
    let result = update::download(&user_agent, &asset, sums.as_ref(), &dir, |downloaded, total| {
        let payload = UpdateProgress {
            tag: tag.clone(),
            downloaded,
            total,
        };
        if let Err(e) = emitter.emit(update::PROGRESS_EVENT, payload) {
            tracing::debug!(event = "update.progress_emit_failed", error = %e, "progress not delivered");
        }
    })
    .await?;
    state.downloaded.lock().insert(PathBuf::from(&result.path));
    Ok(result)
}

/// Shows a file `download_update` produced in this run in Finder / Explorer.
#[tauri::command]
pub fn reveal_update(state: State<UpdateState>, path: String) -> Result<(), CommandError> {
    let path = PathBuf::from(path);
    if !state.downloaded.lock().contains(&path) {
        return Err(update::fail(
            kind::NOT_DOWNLOADED,
            "只能打开本次运行中下载的更新文件",
            "path was not downloaded by this process",
        ));
    }
    if !path.is_file() {
        return Err(CommandError::NotFound {
            kind: EntityKind::File,
            id: crate::error::file_label(&path).unwrap_or_default(),
        });
    }
    reveal_file(&path).map_err(|e| CommandError::io(IoOp::Open, Some(&path), &e))
}
