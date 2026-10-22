//! Tray icon, its menu, and the status watcher that drives the tooltip and
//! failure notifications.
use crate::{app_state::Managed, events};
use quicker_platform::Shared;
use std::sync::{atomic::Ordering, Arc};
use tauri::{AppHandle, Manager};

const TRAY_ID: &str = "quicker-text-tray";

/// Polls the engine every 250ms to refresh the tooltip and notify about
/// failed executions. Replaced by EventBus subscribers in 0.4 (B-44).
pub fn spawn_status_watcher(handle: AppHandle, watcher: Arc<Shared>) {
    std::thread::spawn(move || {
        use tauri_plugin_notification::NotificationExt;
        let mut seen = String::new();
        let mut last_tooltip = String::new();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let tooltip = if watcher.paused.load(Ordering::SeqCst) {
                "Quicker Text · 已暂停"
            } else if watcher.busy.load(Ordering::SeqCst) {
                "Quicker Text · 执行中"
            } else {
                "Quicker Text · 监听中"
            };
            if tooltip != last_tooltip {
                if let Some(tray) = handle.tray_by_id(TRAY_ID) {
                    let _ = tray.set_tooltip(Some(tooltip));
                }
                last_tooltip = tooltip.into();
            }
            let last = watcher.last.lock().unwrap().clone();
            if let Some(last) = last {
                let stamp = format!("{}:{}", last.id, last.status);
                if stamp == seen {
                    continue;
                }
                seen = stamp;
                let notification = watcher
                    .config
                    .lock()
                    .unwrap()
                    .rules
                    .iter()
                    .find(|r| last.id.starts_with(&format!("{}:", r.id)))
                    .map(|r| r.notification.clone());
                if let Some(n) = notification.filter(|n| n.enabled) {
                    let title = match last.status.as_str() {
                        "unknown" => "输入已发送 · 目标结果未知",
                        "cancelled" => "动作已停止",
                        _ => "动作执行失败",
                    };
                    let _ = handle
                        .notification()
                        .builder()
                        .title(title)
                        .body(if n.message.is_empty() {
                            "请在文本指令管理中查看执行状态"
                        } else {
                            &n.message
                        })
                        .show();
                }
            }
        }
    });
}

pub fn build(app: &tauri::App) -> tauri::Result<()> {
    let menu = tauri::menu::Menu::with_items(
        app,
        &[
            &tauri::menu::MenuItem::with_id(app, "show", "打开管理界面", true, None::<&str>)?,
            &tauri::menu::MenuItem::with_id(app, "pause", "暂停 / 恢复", true, None::<&str>)?,
            &tauri::menu::MenuItem::with_id(app, "quit", "退出全部", true, None::<&str>)?,
        ],
    )?;
    tauri::tray::TrayIconBuilder::with_id(TRAY_ID)
        .icon(app.default_window_icon().cloned().unwrap_or_else(|| {
            tauri::image::Image::new_owned([27, 88, 162, 255].repeat(32 * 32), 32, 32)
        }))
        .tooltip("Quicker Text · 默认暂停")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            "pause" => {
                let state = app.state::<Managed>();
                let s = state.lock();
                let old = s.engine.paused.load(Ordering::SeqCst);
                // Safe mode keeps the engine paused (B-04).
                if old
                    && (s.safe_mode().is_some()
                        || quicker_platform::native_ready().is_err()
                        || s.engine.recording.lock().unwrap().is_some())
                {
                    return;
                }
                s.engine.paused.store(!old, Ordering::SeqCst);
                s.engine.generation.fetch_add(1, Ordering::SeqCst);
                events::emit_engine_status(app, s.state());
            }
            "quit" => {
                let state = app.state::<Managed>();
                let s = state.lock();
                s.engine.cancel.store(true, Ordering::SeqCst);
                s.engine.paused.store(true, Ordering::SeqCst);
                crate::store::clear_run_marker_on_session_end();
                app.exit(0)
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}
