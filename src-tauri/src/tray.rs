//! Tray icon, its menu, and the EventBus observer that drives the tooltip,
//! the pause/resume label and run notifications (B-44: no more polling of
//! the engine flags).
use crate::{app_state::Managed, error::CommandError, events::Observer};
use quicker_domain::Rule;
use quicker_platform::{
    health::{EngineState, HealthSnapshot, HealthState, HistoryEntry, PauseReasons, StatusText},
    ExecutionResult, Shared,
};
use std::sync::{atomic::Ordering, Arc, PoisonError};
use tauri::{menu::MenuItem, AppHandle, Manager, Wry};
use tauri_plugin_notification::NotificationExt;

const TRAY_ID: &str = "quicker-text-tray";
const PAUSE_ID: &str = "pause";

/// The pause/resume entry's label for the current state: listening offers
/// to pause, anything else offers to resume (the self_test_failed banner
/// points at this entry).
fn pause_label(listening: bool) -> &'static str {
    if listening {
        "暂停监听"
    } else {
        "恢复监听"
    }
}

/// Builds the tray; returns the pause/resume entry so [`TrayStatus`] can
/// relabel it.
pub fn build(app: &tauri::App) -> tauri::Result<MenuItem<Wry>> {
    let pause = MenuItem::with_id(app, PAUSE_ID, pause_label(false), true, None::<&str>)?;
    let menu = tauri::menu::Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "show", "打开管理界面", true, None::<&str>)?,
            &pause,
            &MenuItem::with_id(app, "quit", "退出全部", true, None::<&str>)?,
        ],
    )?;
    let starting = StatusText::resolve(HealthState::Starting, PauseReasons::MANUAL, false)
        .tooltip(PauseReasons::MANUAL);
    tauri::tray::TrayIconBuilder::with_id(TRAY_ID)
        .icon(app.default_window_icon().cloned().unwrap_or_else(|| {
            tauri::image::Image::new_owned([27, 88, 162, 255].repeat(32 * 32), 32, 32)
        }))
        .tooltip(starting)
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            PAUSE_ID => toggle_pause(app),
            "quit" => {
                let state = app.state::<Managed>();
                let s = state.lock();
                s.engine.cancel.store(true, Ordering::SeqCst);
                s.runtime.set_pause(PauseReasons::MANUAL, true);
                crate::store::clear_run_marker_on_session_end();
                app.exit(0)
            }
            _ => {}
        })
        .build(app)?;
    Ok(pause)
}

/// Toggles the manual reason only, as `set_paused` does: a listening engine
/// pauses, anything else asks to resume. A refused resume (safe mode, engine
/// not up) is explained in a system notification (B-03) instead of being
/// dropped silently. The resulting `engine-state` reaches the frontend
/// through the EventBus.
fn toggle_pause(app: &AppHandle) {
    let refused = {
        let state = app.state::<Managed>();
        let s = state.lock();
        let pause = s.runtime.engine_state().listening;
        s.set_manual_pause(pause, quicker_platform::engine_ready)
            .err()
    };
    if let Some(error) = refused {
        tracing::info!(event = "tray.resume_refused", error = %error, "tray resume refused");
        notify(app, "无法恢复监听", resume_refused(&error));
    }
}

/// The notification body for a refused tray resume.
pub(crate) fn resume_refused(error: &CommandError) -> &'static str {
    match error {
        CommandError::SafeMode { .. } => "安全模式下不能恢复监听。请打开管理界面处理配置问题。",
        CommandError::EngineUnavailable {
            health: HealthState::Starting,
        } => "输入引擎仍在启动，请稍后再试。",
        CommandError::EngineUnavailable {
            health: HealthState::Recovering,
        } => "输入引擎正在恢复，请稍后再试。",
        _ => "输入监听已停止。请打开管理界面查看引擎状态。",
    }
}

/// Shows a system notification. The plugin dispatches it in the background;
/// a failure is only logged.
pub(crate) fn notify(handle: &AppHandle, title: &str, body: &str) {
    if let Err(e) = handle
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show()
    {
        tracing::warn!(event = "tray.notify_failed", error = %e, "notification not shown");
    }
}

/// Keeps the tray in step with the engine, as an EventBus [`Observer`].
pub struct TrayStatus {
    handle: AppHandle,
    pause_item: MenuItem<Wry>,
    tooltip: Option<&'static str>,
    listening: Option<bool>,
    /// Windows: run notifications follow history entries. Elsewhere the
    /// engine records no history, so a polling thread does it (`None` here).
    notifier: Option<LastNotifier>,
}

impl TrayStatus {
    pub fn new(handle: AppHandle, pause_item: MenuItem<Wry>, engine: Arc<Shared>) -> Self {
        let notifier = LastNotifier {
            handle: handle.clone(),
            engine,
            seen: String::new(),
        };
        let notifier = if cfg!(windows) {
            Some(notifier)
        } else {
            notifier.spawn_polling();
            None
        };
        Self {
            handle,
            pause_item,
            tooltip: None,
            listening: None,
            notifier,
        }
    }
}

impl Observer for TrayStatus {
    /// Both setters wait for the main thread; this runs on the bus thread
    /// with nothing locked, and only on a change.
    fn status(&mut self, health: &HealthSnapshot, state: &EngineState) {
        let tooltip = StatusText::resolve(health.state, state.paused_reasons, state.busy)
            .tooltip(state.paused_reasons);
        if self.tooltip != Some(tooltip) {
            if let Some(tray) = self.handle.tray_by_id(TRAY_ID) {
                let _ = tray.set_tooltip(Some(tooltip));
            }
            self.tooltip = Some(tooltip);
        }
        if self.listening != Some(state.listening) {
            let _ = self.pause_item.set_text(pause_label(state.listening));
            self.listening = Some(state.listening);
        }
    }

    fn executed(&mut self, _entry: &HistoryEntry) {
        if let Some(notifier) = &mut self.notifier {
            notifier.check();
        }
    }
}

/// Notifies about the engine's last run result when its rule asks for it
/// (unchanged from 0.3: unknown / cancelled / failed results of a rule with
/// `notification.enabled`). Each result is notified once.
struct LastNotifier {
    handle: AppHandle,
    engine: Arc<Shared>,
    seen: String,
}

impl LastNotifier {
    fn check(&mut self) {
        let last = self
            .engine
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(last) = last else {
            return;
        };
        let stamp = format!("{}:{}", last.id, last.status);
        if stamp == self.seen {
            return;
        }
        self.seen = stamp;
        let notification = {
            let config = self
                .engine
                .config
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            run_notification(&last, &config.rules)
        };
        if let Some((title, body)) = notification {
            notify(&self.handle, title, &body);
        }
    }

    /// Without history events (macOS), the last result is polled as in 0.3.
    /// 待实测 (macOS): run notifications.
    fn spawn_polling(mut self) {
        let spawned = std::thread::Builder::new()
            .name("qt-tray-notify".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                self.check();
            });
        if let Err(e) = spawned {
            tracing::warn!(event = "tray.notify_start_failed", error = %e, "run notifications off");
        }
    }
}

/// The title and body to show for `last`, if its rule (the id before the
/// generation, `"{rule.id}:{generation}"`) has notifications enabled.
/// Manual runs (`"run-{gen}"`) match no rule.
fn run_notification(last: &ExecutionResult, rules: &[Rule]) -> Option<(&'static str, String)> {
    let rule = rules.iter().find(|rule| {
        last.id
            .strip_prefix(rule.id.as_str())
            .is_some_and(|rest| rest.starts_with(':'))
    })?;
    let notification = &rule.notification;
    if !notification.enabled {
        return None;
    }
    let title = match last.status.as_str() {
        "unknown" => "输入已发送 · 目标结果未知",
        "cancelled" => "动作已停止",
        _ => "动作执行失败",
    };
    let body = if notification.message.is_empty() {
        "请在文本指令管理中查看执行状态".to_owned()
    } else {
        notification.message.clone()
    };
    Some((title, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SafeModeReason;
    use quicker_domain::Notification;

    fn rule(id: &str, enabled: bool, message: &str) -> Rule {
        Rule {
            id: id.into(),
            notification: Notification {
                enabled,
                message: message.into(),
            },
            ..Rule::default()
        }
    }

    fn result(id: &str, status: &str) -> ExecutionResult {
        ExecutionResult {
            id: id.into(),
            status: status.into(),
            message: String::new(),
            completed_steps: 0,
            side_effects: false,
        }
    }

    #[test]
    fn run_notifications_follow_the_rule_of_the_result() {
        let rules = [
            rule("a", true, ""),
            rule("ab", true, "custom"),
            rule("quiet", false, "x"),
        ];
        assert_eq!(
            run_notification(&result("a:3", "failed"), &rules),
            Some(("动作执行失败", "请在文本指令管理中查看执行状态".into()))
        );
        // "ab:1" belongs to "ab", not to its prefix "a".
        assert_eq!(
            run_notification(&result("ab:1", "unknown"), &rules),
            Some(("输入已发送 · 目标结果未知", "custom".into()))
        );
        assert_eq!(
            run_notification(&result("ab:2", "cancelled"), &rules).map(|n| n.0),
            Some("动作已停止")
        );
        assert_eq!(run_notification(&result("quiet:1", "failed"), &rules), None);
        assert_eq!(run_notification(&result("run-4", "failed"), &rules), None);
        assert_eq!(run_notification(&result("gone:1", "failed"), &rules), None);
    }

    #[test]
    fn a_refused_tray_resume_says_why() {
        assert_eq!(
            resume_refused(&CommandError::SafeMode {
                reason: SafeModeReason::Locked
            }),
            "安全模式下不能恢复监听。请打开管理界面处理配置问题。"
        );
        assert_eq!(
            resume_refused(&CommandError::EngineUnavailable {
                health: HealthState::Starting
            }),
            "输入引擎仍在启动，请稍后再试。"
        );
        assert_eq!(
            resume_refused(&CommandError::EngineUnavailable {
                health: HealthState::Recovering
            }),
            "输入引擎正在恢复，请稍后再试。"
        );
        assert_eq!(
            resume_refused(&CommandError::engine_unavailable("test")),
            "输入监听已停止。请打开管理界面查看引擎状态。"
        );
        assert_eq!(pause_label(true), "暂停监听");
        assert_eq!(pause_label(false), "恢复监听");
    }
}
