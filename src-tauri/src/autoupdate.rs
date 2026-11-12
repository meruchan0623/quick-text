//! Background auto-update (docs/auto-update.md), on top of `tauri-plugin-updater`.
//!
//! - The channel is fixed at compile time by `QT_UPDATE_CHANNEL`: `stable` (CI and the local
//!   build scripts; `beta` only in the retired 0.4.4 beta build), none for plain local builds,
//!   which never contact the update feed.
//! - The feed is the fixed address
//!   `https://github.com/meruchan0623/quick-text/releases/download/updater-<channel>/latest.json`.
//!   The minisign public key is compiled in (`update-keys/current.pub`); the plugin verifies every
//!   package against it, and `requireSignedVersion` (tauri.conf.json) binds the signature to the
//!   announced version, so an old signed package cannot be served as a newer one.
//! - 60 s after start, then every 6 h: check; a newer version is downloaded and verified in the
//!   background. Then the tray gets 「重启以更新到 x.y.z」 at the top and a notification is shown.
//!   Not restarting is fine too: quitting from the tray installs it (without restarting). Other
//!   exits (shutdown, logoff) do not install, so an installer is never killed halfway.
//!
//! The manual check in the settings page (`crate::update`) is unchanged.
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

/// `beta`, `stable`, or `None` (no auto-update in this build).
pub const CHANNEL: Option<&str> = option_env!("QT_UPDATE_CHANNEL");
const PUBKEY: &str = include_str!("../update-keys/current.pub");
const FIRST_CHECK: Duration = Duration::from_secs(60);
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Tray menu id of the 「重启以更新」 entry.
pub const MENU_ID: &str = "update-install";

pub fn manifest_url(channel: &str) -> Option<String> {
    matches!(channel, "beta" | "stable").then(|| {
        format!(
            "https://github.com/meruchan0623/quick-text/releases/download/updater-{channel}/latest.json"
        )
    })
}

struct Ready {
    update: Update,
    bytes: Vec<u8>,
}

/// Managed state.
pub struct AutoUpdate {
    url: String,
    busy: AtomicBool,
    quit_requested: AtomicBool,
    ready: Mutex<Option<Ready>>,
    /// The tray menu, to put the install entry on top.
    menu: Mutex<Option<Menu<Wry>>>,
}

/// Starts the background loop when this build has an update channel.
pub fn init(app: &AppHandle, tray_menu: Menu<Wry>) {
    let Some(url) = CHANNEL.and_then(manifest_url) else {
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
        url,
        busy: AtomicBool::new(false),
        quit_requested: AtomicBool::new(false),
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

async fn check(app: &AppHandle) {
    let Some(state) = app.try_state::<AutoUpdate>() else {
        return;
    };
    if state.ready.lock().is_some() || state.busy.swap(true, Ordering::AcqRel) {
        return;
    }
    run_check(app, &state).await;
    state.busy.store(false, Ordering::Release);
}

async fn run_check(app: &AppHandle, state: &AutoUpdate) {
    let Some(updater) = updater(app, &state.url, true) else {
        return;
    };
    let update = match updater.check().await {
        Ok(Some(u)) => u,
        Ok(None) => {
            tracing::info!(event = "autoupdate.up_to_date", "no newer version");
            return;
        }
        Err(e) => {
            tracing::warn!(event = "autoupdate.check_failed", error = %e, "update check failed");
            return;
        }
    };
    let target = update.version.clone();
    tracing::info!(event = "autoupdate.found", version = %target, "downloading update");
    // `download` verifies the minisign signature (and the signed version) before returning.
    let bytes = match update.download(|_, _| {}, || {}).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(event = "autoupdate.download_failed", error = %e, "update download failed");
            return;
        }
    };
    tracing::info!(event = "autoupdate.ready", version = %target, "update downloaded and verified");
    *state.ready.lock() = Some(Ready { update, bytes });
    show_tray_entry(app, state, &target);
    crate::tray::notify(
        app,
        "Quicktext 有新版本",
        &format!(
            "{target} 已下载。可在托盘菜单里重启更新；不重启的话，下次从托盘退出时会自动安装。"
        ),
    );
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
    tracing::info!(event = "autoupdate.install", version = %ready.update.version, "installing update");
    // On Windows the plugin starts the installer and ends the process right away, without the
    // normal exit path: end the session the way the tray's 「退出全部」 does first.
    crate::tray::end_session(app);
    crate::diagnostics::logging::flush();
    match ready.update.install(&ready.bytes) {
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
    // The same verified package, through an updater that does not restart the app afterwards.
    let restored = updater(app, &state.url, false).and_then(|u| {
        u.restore_update(ready.update.raw_json.clone())
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
    fn feed_address_is_fixed_per_channel() {
        assert_eq!(
            manifest_url("beta").as_deref(),
            Some("https://github.com/meruchan0623/quick-text/releases/download/updater-beta/latest.json")
        );
        assert!(manifest_url("stable").is_some());
        assert_eq!(manifest_url("nightly"), None);
    }

    #[test]
    fn the_embedded_key_is_a_tauri_minisign_public_key() {
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(PUBKEY.trim())
            .expect("base64");
        let text = String::from_utf8(decoded).expect("utf8");
        assert!(text.starts_with("untrusted comment: minisign public key"));
    }
}
