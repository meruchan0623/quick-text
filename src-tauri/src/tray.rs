//! Tray icon, its menu, and the EventBus observer that drives the tooltip,
//! the pause/resume label and run notifications (B-44: no more polling of
//! the engine flags).
use crate::{app_state::Managed, error::CommandError, events::Observer};
use quicker_domain::Rule;
#[cfg(any(not(windows), test))]
use quicker_platform::ExecutionResult;
use quicker_platform::{
    health::{
        EngineState, HealthSnapshot, HealthState, HistoryEntry, Outcome, PauseReasons, StatusText,
        TriggerKind,
    },
    Shared,
};
use std::collections::BTreeSet;
use std::sync::{atomic::Ordering, Arc, PoisonError};
use tauri::{menu::MenuItem, AppHandle, Manager, Wry};
use tauri_plugin_notification::NotificationExt;

const TRAY_ID: &str = "quicktext-tray";
const PAUSE_ID: &str = "pause";

fn tray_icon(app: &tauri::App) -> tauri::image::Image<'_> {
    #[cfg(windows)]
    {
        // Load the ICO frame at the notification area's actual system size,
        // preserving the hand-tuned small glyph instead of shrinking a taskbar icon.
        #[link(name = "user32")]
        extern "system" {
            fn GetSystemMetrics(index: i32) -> i32;
        }
        const SM_CXSMICON: i32 = 49;
        const SM_CYSMICON: i32 = 50;
        // SAFETY: GetSystemMetrics accepts these constant indices and owns no memory.
        let (width, height) = unsafe {
            (
                GetSystemMetrics(SM_CXSMICON).max(16) as u32,
                GetSystemMetrics(SM_CYSMICON).max(16) as u32,
            )
        };
        if let Ok(icon) = tauri::image::Image::from_icon_resource(
            tauri::utils::platform::WINDOWS_APP_ICON_RESOURCE_ID,
            width,
            height,
        ) {
            return icon;
        }
    }
    app.default_window_icon()
        .cloned()
        .unwrap_or_else(|| tauri::include_image!("icons/32x32.png"))
}

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
        .icon(tray_icon(app))
        .tooltip(starting)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main(app),
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

/// Shows and focuses the management window (restored if minimized).
pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
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
    /// The rules (`Shared::config`) the run notifications look up.
    engine: Arc<Shared>,
    /// Windows: run notifications follow history entries. Elsewhere the
    /// engine records no history, so a polling thread does it and this
    /// stays unused.
    notices: HistoryNotices,
}

impl TrayStatus {
    pub fn new(handle: AppHandle, pause_item: MenuItem<Wry>, engine: Arc<Shared>) -> Self {
        #[cfg(not(windows))]
        LastNotifier {
            handle: handle.clone(),
            engine: Arc::clone(&engine),
            seen: String::new(),
        }
        .spawn_polling();
        Self {
            handle,
            pause_item,
            tooltip: None,
            listening: None,
            engine,
            notices: HistoryNotices::default(),
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

    /// Called for every history entry, before the `execution-result` rate
    /// limit, so a burst loses no notification.
    fn executed(&mut self, entry: &HistoryEntry) {
        if !cfg!(windows) {
            return;
        }
        if let Some((title, body)) = self.notices.take(entry, &self.engine) {
            notify(&self.handle, title, &body);
        }
    }
}

/// Windows run notifications: one per history entry whose rule asks for it,
/// built from the entry alone (rule id, outcome), each `seq` once. Never
/// from `Shared::last`: a run queued behind another may be overwritten
/// there before the bus sees its entry, and a run the executor fences
/// (budget timeout) never sets it, while every run has exactly one entry.
/// Not merged or rate-limited: a burst of failures notifies each one.
///
/// Review A8: "each `seq` once" is a bounded set of seen seqs, not "above
/// the highest seen". The runtime queues entries in seq order, but an
/// unseen lower seq arriving after a higher one (any reordering on the way)
/// still notifies, and a redelivered one does not. The set keeps the last
/// [`NOTICE_WINDOW`] seqs; older ones fold into `floor`, at or below which
/// every seq counts as taken, so an entry more than that many entries late
/// is dropped rather than risk a duplicate.
#[derive(Default)]
struct HistoryNotices {
    /// Seqs above `floor` already taken.
    seen: BTreeSet<u64>,
    /// Every seq at or below this counts as taken (history seqs start at 1).
    floor: u64,
}

/// How many taken seqs [`HistoryNotices`] remembers individually.
const NOTICE_WINDOW: usize = 256;

impl HistoryNotices {
    fn take(&mut self, entry: &HistoryEntry, engine: &Shared) -> Option<(&'static str, String)> {
        if entry.seq <= self.floor || !self.seen.insert(entry.seq) {
            return None;
        }
        if self.seen.len() > NOTICE_WINDOW {
            if let Some(oldest) = self.seen.pop_first() {
                self.floor = oldest;
            }
        }
        let config = engine.config.lock().unwrap_or_else(PoisonError::into_inner);
        entry_notification(entry, &config.rules)
    }
}

/// The notification for one history entry, if its rule (`rule_id`, the
/// rule's own id) has notifications enabled. Manual runs have no rule.
fn entry_notification(entry: &HistoryEntry, rules: &[Rule]) -> Option<(&'static str, String)> {
    if entry.trigger == TriggerKind::Manual {
        return None;
    }
    let rule = rules.iter().find(|rule| rule.id == entry.rule_id)?;
    rule_notification(rule, entry.outcome)
}

/// Title and body for one run of `rule` that ended with `outcome`, if the
/// rule asks for notifications (0.3: every run of such a rule; a completed
/// run still reads "结果未知" because consumption by the target is not
/// verified).
fn rule_notification(rule: &Rule, outcome: Outcome) -> Option<(&'static str, String)> {
    let notification = &rule.notification;
    if !notification.enabled {
        return None;
    }
    let title = match outcome {
        Outcome::Succeeded | Outcome::Unknown => "输入已发送 · 目标结果未知",
        Outcome::Cancelled => "动作已停止",
        Outcome::DroppedBusy => "动作未执行 · 上一个动作仍在运行",
        Outcome::Failed | Outcome::Skipped => "动作执行失败",
    };
    let body = if notification.message.is_empty() {
        "请在文本指令管理中查看执行状态".to_owned()
    } else {
        notification.message.clone()
    };
    Some((title, body))
}

/// Notifies about the engine's last run result when its rule asks for it
/// (macOS, unchanged from 0.3: unknown / cancelled / failed results of a
/// rule with `notification.enabled`). Each result is notified once.
#[cfg(not(windows))]
struct LastNotifier {
    handle: AppHandle,
    engine: Arc<Shared>,
    seen: String,
}

#[cfg(not(windows))]
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

/// macOS: the title and body to show for `last`, if its rule (the id before
/// the generation, `"{rule.id}:{generation}"`) has notifications enabled.
/// Manual runs (`"run-{gen}"`) match no rule.
#[cfg(any(not(windows), test))]
fn run_notification(last: &ExecutionResult, rules: &[Rule]) -> Option<(&'static str, String)> {
    let rule = rules.iter().find(|rule| {
        last.id
            .strip_prefix(rule.id.as_str())
            .is_some_and(|rest| rest.starts_with(':'))
    })?;
    let outcome = match last.status.as_str() {
        "unknown" => Outcome::Unknown,
        "cancelled" => Outcome::Cancelled,
        _ => Outcome::Failed,
    };
    rule_notification(rule, outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SafeModeReason;
    use quicker_domain::Notification;
    use quicker_platform::health::ReasonCode;

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

    fn entry(seq: u64, rule_id: &str, outcome: Outcome, reason: ReasonCode) -> HistoryEntry {
        HistoryEntry {
            seq,
            at_ms: 0,
            rule_id: rule_id.into(),
            trigger: TriggerKind::Immediate,
            outcome,
            reason,
            process: None,
            completed_steps: 0,
            side_effects: false,
            skipped_formats: Vec::new(),
            timings: Default::default(),
        }
    }

    fn engine_with(rules: Vec<Rule>) -> Arc<Shared> {
        let engine = Shared::new();
        engine.config.lock().unwrap().rules = rules;
        engine
    }

    const FAILED: &str = "动作执行失败";
    const SENT: &str = "输入已发送 · 目标结果未知";

    /// Review A3: two runs queued back to back, the second settling (and
    /// overwriting `Shared::last`) before the bus delivers the first entry.
    /// Each entry notifies by its own rule and outcome.
    #[test]
    fn queued_results_each_notify_by_their_own_entry() {
        let engine = engine_with(vec![rule("a", true, ""), rule("quiet", false, "x")]);
        // Shared::last already holds the second run's success.
        *engine.last.lock().unwrap() = Some(result("a:8", "unknown"));
        let mut notices = HistoryNotices::default();
        let fail = entry(1, "a", Outcome::Failed, ReasonCode::ClipboardBusy);
        let done = entry(2, "a", Outcome::Succeeded, ReasonCode::Ok);
        assert_eq!(notices.take(&fail, &engine).map(|n| n.0), Some(FAILED));
        assert_eq!(notices.take(&done, &engine).map(|n| n.0), Some(SENT));

        // A failure of a quiet rule queued before a notifying rule's success:
        // only the second notifies.
        let mut notices = HistoryNotices::default();
        let quiet_fail = entry(3, "quiet", Outcome::Failed, ReasonCode::ClipboardBusy);
        let done = entry(4, "a", Outcome::Succeeded, ReasonCode::Ok);
        assert_eq!(notices.take(&quiet_fail, &engine), None);
        assert_eq!(notices.take(&done, &engine).map(|n| n.0), Some(SENT));

        // And the other way round: the notifying failure, then a quiet success.
        let mut notices = HistoryNotices::default();
        let fail = entry(5, "a", Outcome::Failed, ReasonCode::ClipboardBusy);
        let quiet_done = entry(6, "quiet", Outcome::Succeeded, ReasonCode::Ok);
        assert_eq!(notices.take(&fail, &engine).map(|n| n.0), Some(FAILED));
        assert_eq!(notices.take(&quiet_done, &engine), None);
    }

    /// Review A3: the executor fences a run past its budget and records
    /// Failed / executor_timeout without touching `Shared::last`, which
    /// still holds an older, already notified result.
    #[test]
    fn a_fenced_run_notifies_although_shared_last_is_unchanged() {
        let engine = engine_with(vec![rule("a", true, "超时了")]);
        *engine.last.lock().unwrap() = Some(result("a:1", "unknown"));
        let mut notices = HistoryNotices::default();
        assert_eq!(
            notices.take(&entry(1, "a", Outcome::Unknown, ReasonCode::Ok), &engine),
            Some((SENT, "超时了".into()))
        );
        let fenced = entry(2, "a", Outcome::Failed, ReasonCode::ExecutorTimeout);
        assert_eq!(
            notices.take(&fenced, &engine),
            Some((FAILED, "超时了".into()))
        );
    }

    #[test]
    fn history_notices_take_each_seq_once_and_skip_manual_runs() {
        let engine = engine_with(vec![rule("a", true, ""), rule("ab", true, "")]);
        let mut notices = HistoryNotices::default();
        let first = entry(5, "a", Outcome::Failed, ReasonCode::ClipboardBusy);
        assert!(notices.take(&first, &engine).is_some());
        // Redelivered: nothing.
        assert_eq!(notices.take(&first, &engine), None);
        // The id matches exactly: "ab" is not "a".
        assert_eq!(
            notices
                .take(&entry(6, "ab", Outcome::Cancelled, ReasonCode::Ok), &engine)
                .map(|n| n.0),
            Some("动作已停止")
        );
        assert_eq!(
            notices
                .take(
                    &entry(7, "a", Outcome::DroppedBusy, ReasonCode::DroppedBusy),
                    &engine
                )
                .map(|n| n.0),
            Some("动作未执行 · 上一个动作仍在运行")
        );
        assert_eq!(
            notices
                .take(&entry(8, "a", Outcome::Skipped, ReasonCode::Ok), &engine)
                .map(|n| n.0),
            Some(FAILED)
        );
        let mut manual = entry(9, "", Outcome::Failed, ReasonCode::Ok);
        manual.trigger = TriggerKind::Manual;
        assert_eq!(notices.take(&manual, &engine), None);
        assert_eq!(
            notices.take(&entry(10, "gone", Outcome::Failed, ReasonCode::Ok), &engine),
            None
        );
    }

    /// Review A8: seq 2 reaches the observer before seq 1. Both notify,
    /// each exactly once.
    #[test]
    fn an_unseen_lower_seq_still_notifies_once() {
        let engine = engine_with(vec![rule("a", true, "")]);
        let mut notices = HistoryNotices::default();
        let one = entry(1, "a", Outcome::Failed, ReasonCode::ClipboardBusy);
        let two = entry(2, "a", Outcome::Succeeded, ReasonCode::Ok);
        assert_eq!(notices.take(&two, &engine).map(|n| n.0), Some(SENT));
        assert_eq!(notices.take(&one, &engine).map(|n| n.0), Some(FAILED));
        assert_eq!(notices.take(&two, &engine), None);
        assert_eq!(notices.take(&one, &engine), None);
    }

    /// Review A8: the seen set is bounded. Past the window the oldest seqs
    /// fold into the floor: still never notified twice, and anything that
    /// late is dropped; inside the window a gap is still filled.
    #[test]
    fn the_seen_window_is_bounded_and_never_notifies_twice() {
        let engine = engine_with(vec![rule("a", true, "")]);
        let mut notices = HistoryNotices::default();
        let window = u64::try_from(NOTICE_WINDOW).unwrap();
        // Seq 2 is missing; 1 and 3..=window+2 arrive.
        for seq in std::iter::once(1).chain(3..=window + 2) {
            let run = entry(seq, "a", Outcome::Failed, ReasonCode::Ok);
            assert!(notices.take(&run, &engine).is_some(), "seq {seq}");
        }
        assert!(notices.seen.len() <= NOTICE_WINDOW);
        // 1 folded into the floor: a redelivery is still refused.
        assert_eq!(notices.floor, 1);
        let one = entry(1, "a", Outcome::Failed, ReasonCode::Ok);
        assert_eq!(notices.take(&one, &engine), None);
        // 2 is above the floor and unseen: it notifies, once.
        let two = entry(2, "a", Outcome::Failed, ReasonCode::Ok);
        assert!(notices.take(&two, &engine).is_some());
        assert_eq!(notices.floor, 2);
        assert_eq!(notices.take(&two, &engine), None);
        // The newest are still remembered individually.
        let newest = entry(window + 2, "a", Outcome::Failed, ReasonCode::Ok);
        assert_eq!(notices.take(&newest, &engine), None);
        assert!(notices.seen.len() <= NOTICE_WINDOW);
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
