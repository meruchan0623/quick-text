//! Auto-update (docs/auto-update.md), on top of `tauri-plugin-updater`.
//!
//! - The channel is fixed at compile time by `QT_UPDATE_CHANNEL`: `stable` (the local release
//!   scripts), none for plain local builds, which never contact the update servers.
//! - The feed is the two TSim servers ([`crate::update::manifest_urls`]): every check reads both
//!   manifests at once and takes the newest version (production on a tie); one server failing
//!   does not fail the check. The minisign public key is compiled in (`update-keys/current.pub`);
//!   the plugin verifies every package against it, and `requireSignedVersion` (tauri.conf.json)
//!   binds the signature to the announced version, so an old signed package cannot be served as a
//!   newer one.
//! - 60 s after start, then every 6 h: check; a newer version is downloaded and verified in the
//!   background. Then the tray gets 「重启以更新到 x.y.z」 at the top and a notification is shown.
//!   Not restarting is fine too: quitting from the tray installs it (without restarting). Other
//!   exits (shutdown, logoff) do not install, so an installer is never killed halfway.
//! - The manual check in 设置 (`commands::update`) goes through the same check, download and
//!   install ([`check_now`], [`download_now`], [`install_from_settings`]).
//! - A downloaded package remembers the manifest address that announced it; the exit install
//!   restores the update through an updater for that same address.
use crate::{
    error::CommandError,
    update::{self, kind, Source, UpdateCheck, UpdateDownload, UpdateInfo},
};
use parking_lot::Mutex;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tauri::{
    menu::{Menu, MenuItem},
    AppHandle, Manager, Wry,
};
use tauri_plugin_updater::{Update, UpdaterExt};

/// `stable`, or `None` (no auto-update in this build).
pub const CHANNEL: Option<&str> = option_env!("QT_UPDATE_CHANNEL");
const PUBKEY: &str = include_str!("../update-keys/current.pub");
const FIRST_CHECK: Duration = Duration::from_secs(60);
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Tray menu id of the 「重启以更新」 entry.
pub const MENU_ID: &str = "update-install";

/// An update one server announced, with the manifest address it came from.
#[derive(Clone)]
struct Candidate {
    update: Update,
    source: Source,
    url: String,
}

impl Candidate {
    fn info(&self) -> UpdateInfo {
        UpdateInfo {
            version: self.update.version.clone(),
            notes: self.update.body.clone().unwrap_or_default(),
            pub_date: self
                .update
                .raw_json
                .get("pub_date")
                .and_then(|d| d.as_str())
                .map(str::to_owned),
            source: self.source,
        }
    }
}

struct Ready {
    candidate: Candidate,
    bytes: Vec<u8>,
}

/// Managed state.
pub struct AutoUpdate {
    urls: [(Source, String); 2],
    busy: AtomicBool,
    quit_requested: AtomicBool,
    /// What the last manual check found, for 「下载更新」.
    pending: Mutex<Option<Candidate>>,
    ready: Mutex<Option<Ready>>,
    /// The tray menu, to put the install entry on top.
    menu: Mutex<Option<Menu<Wry>>>,
}

/// Clears `busy` when a check or download ends, however it ends.
struct Busy<'a>(&'a AtomicBool);

impl<'a> Busy<'a> {
    fn take(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Starts the background loop when this build has an update channel.
pub fn init(app: &AppHandle, tray_menu: Menu<Wry>) {
    let Some(urls) = CHANNEL
        .zip(update::platform_segment())
        .and_then(|(channel, platform)| update::manifest_urls(channel, platform))
    else {
        tracing::info!(
            event = "autoupdate.disabled",
            "no update channel in this build"
        );
        return;
    };
    if PUBKEY.trim().is_empty() {
        tracing::warn!(
            event = "autoupdate.disabled",
            "no update public key in this build"
        );
        return;
    }
    tracing::info!(
        event = "autoupdate.enabled",
        channel = CHANNEL,
        "auto-update enabled"
    );
    app.manage(AutoUpdate {
        urls,
        busy: AtomicBool::new(false),
        quit_requested: AtomicBool::new(false),
        pending: Mutex::new(None),
        ready: Mutex::new(None),
        menu: Mutex::new(Some(tray_menu)),
    });
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio_sleep(FIRST_CHECK).await;
        loop {
            check(&app).await;
            tokio_sleep(INTERVAL).await;
        }
    });
}

async fn tokio_sleep(d: Duration) {
    // tauri's async runtime is tokio; a blocking sleep on a helper thread keeps us off its timer
    // feature set.
    let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(d)).await;
}

fn updater(app: &AppHandle, url: &str, restart: bool) -> Option<tauri_plugin_updater::Updater> {
    let endpoint = url.parse().ok()?;
    app.updater_builder()
        .endpoints(vec![endpoint])
        .and_then(|b| {
            b.pubkey(PUBKEY.trim())
                .timeout(REQUEST_TIMEOUT)
                .restart_after_install(restart)
                .build()
        })
        .map_err(
            |e| tracing::warn!(event = "autoupdate.unavailable", error = %e, "updater unavailable"),
        )
        .ok()
}

/// Reads both manifests at once. `Ok(None)`: no newer version; `Err`: both servers failed.
async fn find(app: &AppHandle, urls: &[(Source, String); 2]) -> Result<Option<Candidate>, String> {
    let tasks: Vec<_> = urls
        .iter()
        .cloned()
        .map(|(source, url)| {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let Some(updater) = updater(&app, &url, true) else {
                    return Err(format!("{source:?}: updater unavailable"));
                };
                match updater.check().await {
                    Ok(found) => Ok(found.map(|update| Candidate {
                        update,
                        source,
                        url,
                    })),
                    Err(e) => {
                        tracing::warn!(event = "autoupdate.source_failed", source = ?source, error = %e, "update server not usable");
                        Err(format!("{source:?}: {e}"))
                    }
                }
            })
        })
        .collect();
    let mut answers = Vec::with_capacity(tasks.len());
    for task in tasks {
        let answer = match task.await {
            Ok(answer) => answer,
            Err(e) => Err(format!("check task failed: {e}")),
        };
        answers.push(answer.and_then(|found| {
            match found {
                None => Ok(None),
                Some(c) => semver::Version::parse(&c.update.version)
                    .map(|v| Some((v, c)))
                    .map_err(|e| format!("bad version: {e}")),
            }
        }));
    }
    update::pick_newest(answers)
        .map(|best| best.map(|(_, c)| c))
        .map_err(|errors| errors.join("; "))
}

async fn check(app: &AppHandle) {
    let Some(state) = app.try_state::<AutoUpdate>() else {
        return;
    };
    if state.ready.lock().is_some() {
        return;
    }
    let Some(_busy) = Busy::take(&state.busy) else {
        return;
    };
    let candidate = match find(app, &state.urls).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            tracing::info!(event = "autoupdate.up_to_date", "no newer version");
            return;
        }
        Err(e) => {
            tracing::warn!(event = "autoupdate.check_failed", error = %e, "update check failed");
            return;
        }
    };
    let target = candidate.update.version.clone();
    tracing::info!(event = "autoupdate.found", version = %target, source = ?candidate.source, "downloading update");
    if fetch(app, &state, candidate, |_, _| {}).await.is_err() {
        return;
    }
    crate::tray::notify(
        app,
        "Quicktext 有新版本",
        &format!(
            "{target} 已下载。可在托盘菜单里重启更新；不重启的话，下次从托盘退出时会自动安装。"
        ),
    );
}

/// Downloads and verifies `candidate`, then offers it in the tray.
async fn fetch(
    app: &AppHandle,
    state: &AutoUpdate,
    candidate: Candidate,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let target = candidate.update.version.clone();
    let mut downloaded = 0u64;
    // `download` verifies the minisign signature (and the signed version) before returning.
    let bytes = match candidate
        .update
        .download(
            |chunk, total| {
                downloaded += chunk as u64;
                progress(downloaded, total);
            },
            || {},
        )
        .await
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(event = "autoupdate.download_failed", error = %e, "update download failed");
            return Err(e.to_string());
        }
    };
    tracing::info!(event = "autoupdate.ready", version = %target, source = ?candidate.source, "update downloaded and verified");
    *state.ready.lock() = Some(Ready { candidate, bytes });
    *state.pending.lock() = None;
    show_tray_entry(app, state, &target);
    Ok(())
}

fn state_of(app: &AppHandle) -> Result<tauri::State<'_, AutoUpdate>, CommandError> {
    app.try_state::<AutoUpdate>().ok_or_else(|| {
        update::fail(
            kind::DISABLED,
            "这个版本没有内置更新渠道（本机构建），不能检查更新",
            "no update channel in this build",
        )
    })
}

fn busy_error() -> CommandError {
    update::fail(
        kind::BUSY,
        "正在检查或下载更新，请稍后再试",
        "a check or download is already running",
    )
}

/// The manual 「检查更新」: the same dual-server check as the background loop.
pub async fn check_now(app: &AppHandle) -> Result<UpdateCheck, CommandError> {
    let current = app.package_info().version.to_string();
    let state = state_of(app)?;
    if let Some(ready) = state.ready.lock().as_ref() {
        return Ok(UpdateCheck {
            current,
            latest: Some(ready.candidate.info()),
            update_available: true,
            ready: true,
        });
    }
    let _busy = Busy::take(&state.busy).ok_or_else(busy_error)?;
    let found = find(app, &state.urls)
        .await
        .map_err(|e| update::fail(kind::NETWORK, "无法连接更新服务器，请检查网络连接后重试", e))?;
    let latest = found.as_ref().map(Candidate::info);
    *state.pending.lock() = found;
    Ok(UpdateCheck {
        current,
        update_available: latest.is_some(),
        latest,
        ready: false,
    })
}

/// The manual 「下载更新」 of the version the last check found.
pub async fn download_now(
    app: &AppHandle,
    version: &str,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<UpdateDownload, CommandError> {
    let state = state_of(app)?;
    if let Some(ready) = state.ready.lock().as_ref() {
        if ready.candidate.update.version == version {
            return Ok(UpdateDownload {
                version: version.to_owned(),
            });
        }
    }
    let _busy = Busy::take(&state.busy).ok_or_else(busy_error)?;
    let candidate = state
        .pending
        .lock()
        .clone()
        .filter(|c| c.update.version == version)
        .ok_or_else(|| {
            update::fail(
                kind::STALE,
                "请先重新检查更新",
                format!("{version} is not the version the last check found"),
            )
        })?;
    fetch(app, &state, candidate, progress)
        .await
        .map_err(|e| update::fail(kind::DOWNLOAD, "下载或签名校验没有通过，请稍后重试", e))?;
    Ok(UpdateDownload {
        version: version.to_owned(),
    })
}

/// 「重启并更新」 in 设置. Returns only when the install did not start.
pub fn install_from_settings(app: &AppHandle) -> Result<(), CommandError> {
    let state = state_of(app)?;
    if state.ready.lock().is_none() {
        return Err(update::fail(
            kind::NOT_READY,
            "还没有下载好的更新",
            "nothing downloaded",
        ));
    }
    install_now(app);
    Err(update::fail(
        kind::INSTALL,
        "安装没有完成，请稍后再试",
        "install returned",
    ))
}

fn show_tray_entry(app: &AppHandle, state: &AutoUpdate, version: &str) {
    let Some(menu) = state.menu.lock().clone() else {
        return;
    };
    if menu.get(MENU_ID).is_some() {
        return;
    }
    let item = MenuItem::with_id(
        app,
        MENU_ID,
        format!("重启以更新到 {version}"),
        true,
        None::<&str>,
    );
    if let Err(e) = item.and_then(|item| menu.insert(&item, 0)) {
        tracing::warn!(event = "autoupdate.tray_failed", error = %e, "tray entry not added");
    }
}

/// Tray 「重启以更新」: install now and restart into the new version.
pub fn install_now(app: &AppHandle) {
    let Some(state) = app.try_state::<AutoUpdate>() else {
        return;
    };
    let Some(ready) = state.ready.lock().take() else {
        return;
    };
    tracing::info!(event = "autoupdate.install", version = %ready.candidate.update.version, "installing update");
    // On Windows the plugin starts the installer and ends the process right away, without the
    // normal exit path: end the session the way the tray's 「退出全部」 does first.
    crate::tray::end_session(app);
    crate::diagnostics::logging::flush();
    match ready.candidate.update.install(&ready.bytes) {
        Ok(()) => app.restart(),
        Err(e) => {
            tracing::warn!(event = "autoupdate.install_failed", error = %e, "install failed");
            crate::tray::notify(
                app,
                "Quicktext 更新失败",
                "安装没有完成，请稍后从托盘菜单再试一次。",
            );
            *state.ready.lock() = Some(ready);
        }
    }
}

/// The tray's 「退出全部」 was chosen (the only exit that installs a downloaded update).
pub fn note_quit(app: &AppHandle) {
    if let Some(state) = app.try_state::<AutoUpdate>() {
        state.quit_requested.store(true, Ordering::Release);
    }
}

/// `RunEvent::Exit`: after a tray quit, install a downloaded update without restarting.
pub fn install_on_exit(app: &AppHandle) {
    let Some(state) = app.try_state::<AutoUpdate>() else {
        return;
    };
    if !state.quit_requested.load(Ordering::Acquire) {
        return;
    }
    let Some(ready) = state.ready.lock().take() else {
        return;
    };
    // The same verified package, through an updater for the manifest address that announced it,
    // that does not restart the app afterwards.
    let restored = updater(app, &ready.candidate.url, false).and_then(|u| {
        u.restore_update(ready.candidate.update.raw_json.clone())
            .ok()
            .flatten()
    });
    let Some(update) = restored else {
        tracing::warn!(
            event = "autoupdate.install_failed",
            "update could not be restored on exit"
        );
        return;
    };
    tracing::info!(event = "autoupdate.install_on_exit", version = %update.version, "installing update on exit");
    crate::diagnostics::logging::flush();
    if let Err(e) = update.install(&ready.bytes) {
        tracing::warn!(event = "autoupdate.install_failed", error = %e, "install on exit failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_key_is_a_tauri_minisign_public_key() {
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(PUBKEY.trim())
            .expect("base64");
        let text = String::from_utf8(decoded).expect("utf8");
        assert!(text.starts_with("untrusted comment: minisign public key"));
    }

    #[test]
    fn the_embedded_key_is_the_one_the_servers_trust() {
        // CONTRACT.md §3.4: key id 9F53EFA021ACF8E7 is the quicktext key the TSim servers accept.
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(PUBKEY.trim())
            .unwrap();
        assert!(String::from_utf8(decoded)
            .unwrap()
            .contains("9F53EFA021ACF8E7"));
    }

    #[test]
    fn the_busy_guard_is_exclusive_and_released() {
        let flag = AtomicBool::new(false);
        let first = Busy::take(&flag);
        assert!(first.is_some());
        assert!(Busy::take(&flag).is_none());
        drop(first);
        assert!(Busy::take(&flag).is_some());
    }
}
