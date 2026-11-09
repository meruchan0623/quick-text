//! Beginner typing hint overlay (settings 「新手提示」).
//!
//! The input threads report the word being typed (`quicker_platform::hint`); a worker thread
//! here finds the rules it may become (`CompiledConfig::hint_candidates`) and shows them in the
//! `hint` window: on the monitor of the caret (else the pointer), in the right quarter of its
//! work area, from 18% of its height, at most half of it high. The window never takes the
//! focus, ignores the pointer, and hides `duration_ms` after the last key, when a rule runs, or
//! when the word ends.
//!
//! The flow for one hint: the worker emits `candidate-hint` (the items, the width and the
//! maximum height in CSS pixels); the page lays the list out, measures it and answers with
//! `candidate_hint_measured`; the worker then sizes, places and shows the window. Typed text and
//! bodies are user data and are never logged (B-09).
use crate::store::prefs::CandidateHintPrefs;
use quicker_domain::{ForegroundContext, HintItem, HintMatch, Platform};
use quicker_platform::{hint::HintEvent, Shared};
use serde::Serialize;
use std::{
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};
use tauri::{
    AppHandle, Emitter, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

pub const LABEL: &str = "hint";
const EVENT: &str = "candidate-hint";
/// Items sent at most; the page shows as many as fit.
const LIMIT: usize = 40;

enum Msg {
    Input(HintEvent),
    Settings(CandidateHintPrefs),
    Preview,
    Measured { seq: u64, height: f64 },
}

/// `candidate-hint` payload.
#[derive(Clone, Serialize)]
struct Payload<'a> {
    seq: u64,
    /// `None` hides the list.
    hint: Option<&'a HintMatch>,
    /// CSS pixels.
    width: f64,
    max_height: f64,
    duration_ms: u64,
}

/// Where the window goes, in physical pixels.
#[derive(Clone, Copy)]
struct Place {
    x: i32,
    y: i32,
    width: u32,
    max_height: u32,
    scale: f64,
}

/// Managed state: the worker's inbox.
pub struct HintControl(mpsc::Sender<Msg>);

impl HintControl {
    /// Builds the hidden window and starts the worker. Without a window the hint stays off,
    /// and the rest of the app is unaffected.
    pub fn start(app: &AppHandle, engine: Arc<Shared>, prefs: CandidateHintPrefs) -> Self {
        let (tx, rx) = mpsc::channel();
        let window = match build_window(app) {
            Ok(window) => Some(window),
            Err(error) => {
                tracing::warn!(event = "hint.window_failed", error = %error);
                None
            }
        };
        let input = tx.clone();
        quicker_platform::hint::set_listener(move |event| {
            let _ = input.send(Msg::Input(event));
        });
        quicker_platform::hint::set_enabled(prefs.enabled && window.is_some());
        if let Some(window) = window {
            let app = app.clone();
            let enabled = prefs.enabled;
            let duration_ms = prefs.duration_ms;
            let spawned = std::thread::Builder::new()
                .name("quicktext-hint".into())
                .spawn(move || {
                    Worker {
                        app,
                        window,
                        engine,
                        enabled,
                        duration: Duration::from_millis(duration_ms),
                        seq: 0,
                        pending: None,
                        shown: None,
                        process: String::new(),
                    }
                    .run(rx)
                });
            if spawned.is_err() {
                quicker_platform::hint::set_enabled(false);
            }
        }
        Self(tx)
    }

    pub fn apply(&self, prefs: CandidateHintPrefs) {
        quicker_platform::hint::set_enabled(prefs.enabled);
        let _ = self.0.send(Msg::Settings(prefs));
    }

    pub fn preview(&self) {
        let _ = self.0.send(Msg::Preview);
    }

    pub fn measured(&self, seq: u64, height: f64) {
        let _ = self.0.send(Msg::Measured { seq, height });
    }
}

fn build_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("hint.html".into()))
        .title("Quicktext 候选提示")
        .decorations(false)
        .resizable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .focusable(false)
        .focused(false)
        .visible(false)
        .shadow(false)
        .inner_size(320.0, 200.0);
    // Rounded corners need a transparent window; macOS only has that with the private API.
    #[cfg(windows)]
    let builder = builder.transparent(true);
    let window = builder.build()?;
    let _ = window.set_ignore_cursor_events(true);
    Ok(window)
}

struct Worker {
    app: AppHandle,
    window: WebviewWindow,
    engine: Arc<Shared>,
    enabled: bool,
    duration: Duration,
    /// The last `candidate-hint` sent.
    seq: u64,
    /// Waiting for the page's height: the place to show it at.
    pending: Option<(u64, Place)>,
    /// Visible until this instant.
    shown: Option<Instant>,
    /// The target of the hint showing (for `Edited`).
    process: String,
}

impl Worker {
    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        loop {
            let deadline = self.shown.map(|until| until.max(Instant::now()));
            let msg = match deadline {
                Some(until) => match rx.recv_timeout(until - Instant::now()) {
                    Ok(msg) => msg,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        self.hide();
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => return,
                },
            };
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Settings(prefs) => {
                self.enabled = prefs.enabled;
                self.duration = Duration::from_millis(prefs.duration_ms);
                if !self.enabled {
                    self.hide();
                }
            }
            Msg::Input(HintEvent::Clear) => self.hide(),
            Msg::Input(HintEvent::Typed { text, process }) => {
                if self.enabled {
                    self.process = process;
                    self.update(&text);
                }
            }
            Msg::Input(HintEvent::Edited { text }) => {
                if self.enabled && self.visible_or_pending() {
                    self.update(&text);
                }
            }
            Msg::Preview => {
                let sample = sample();
                self.present(&sample);
            }
            Msg::Measured { seq, height } => self.show(seq, height),
        }
    }

    fn visible_or_pending(&self) -> bool {
        self.shown.is_some() || self.pending.is_some()
    }

    fn update(&mut self, text: &str) {
        let found = {
            let Ok(compiled) = self.engine.compiled.lock() else {
                return self.hide();
            };
            compiled.hint_candidates(text, &foreground(&self.process), LIMIT)
        };
        match found {
            Some(found) => self.present(&found),
            None => self.hide(),
        }
    }

    /// Sends the list to the page; the window shows once the page has measured it.
    fn present(&mut self, found: &HintMatch) {
        let Some(place) = place(&self.app) else {
            return self.hide();
        };
        self.seq += 1;
        let payload = Payload {
            seq: self.seq,
            hint: Some(found),
            width: f64::from(place.width) / place.scale,
            max_height: f64::from(place.max_height) / place.scale,
            duration_ms: self.duration.as_millis() as u64,
        };
        if self.window.emit_to(LABEL, EVENT, payload).is_err() {
            return self.hide();
        }
        self.pending = Some((self.seq, place));
        // The timer runs from the key, also while the page measures.
        self.shown = Some(Instant::now() + self.duration);
    }

    fn show(&mut self, seq: u64, height: f64) {
        let Some((pending, place)) = self.pending else {
            return;
        };
        if pending != seq || self.shown.is_none() {
            return;
        }
        self.pending = None;
        let height = ((height * place.scale).ceil() as u32).clamp(1, place.max_height.max(1));
        #[cfg(windows)]
        {
            if let Ok(hwnd) = self.window.hwnd() {
                quicker_platform::show_without_activation(
                    hwnd.0 as isize,
                    place.x,
                    place.y,
                    place.width as i32,
                    height as i32,
                );
                return;
            }
        }
        let _ = self.window.set_size(PhysicalSize::new(place.width, height));
        let _ = self
            .window
            .set_position(PhysicalPosition::new(place.x, place.y));
        let _ = self.window.show();
    }

    fn hide(&mut self) {
        self.pending = None;
        if self.shown.take().is_some() {
            let _ = self.window.hide();
            self.seq += 1;
            let _ = self.window.emit_to(
                LABEL,
                EVENT,
                Payload {
                    seq: self.seq,
                    hint: None,
                    width: 0.0,
                    max_height: 0.0,
                    duration_ms: 0,
                },
            );
        }
    }
}

fn foreground(process: &str) -> ForegroundContext {
    ForegroundContext {
        platform: if cfg!(target_os = "macos") {
            Platform::Macos
        } else {
            Platform::Windows
        },
        process: process.to_string(),
        bundle_id: process.to_string(),
    }
}

/// The right quarter of the work area of the monitor with the caret (else the pointer).
fn place(app: &AppHandle) -> Option<Place> {
    let (x, y) = quicker_platform::hint_anchor()
        .map(|(x, y)| (f64::from(x), f64::from(y)))
        .or_else(|| app.cursor_position().ok().map(|p| (p.x, p.y)))?;
    let monitor = app
        .monitor_from_point(x, y)
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let scale = monitor.scale_factor();
    let area = monitor.work_area();
    let (left, top) = (area.position.x, area.position.y);
    let (width, height) = (area.size.width as f64, area.size.height as f64);
    let gutter = 16.0 * scale;
    let panel = (width / 4.0 - 2.0 * gutter).max(200.0 * scale);
    Some(Place {
        x: left + (width - panel - gutter).round() as i32,
        y: top + (height * 0.18).round() as i32,
        width: panel.round() as u32,
        max_height: (height * 0.5).round() as u32,
        scale,
    })
}

/// What 「预览效果」 shows.
fn sample() -> HintMatch {
    let item =
        |id: &str, abbreviation: &str, description: &str, preview: &str, team: bool| HintItem {
            rule_id: id.into(),
            abbreviation: abbreviation.into(),
            description: description.into(),
            source: if team { "team" } else { "personal" },
            trigger: "terminator",
            preview: preview.into(),
            preview_is_label: false,
        };
    HintMatch {
        typed: "th".into(),
        total: 3,
        items: vec![
            item(
                "preview-1",
                "thx",
                "感谢回复",
                "Thanks very much for your reply.",
                false,
            ),
            item(
                "preview-2",
                "thks",
                "感谢客户",
                "非常感谢您的耐心等待，问题已经为您处理完毕。",
                true,
            ),
            item(
                "preview-3",
                "thsig",
                "邮件签名",
                "Best regards,\nQuicktext Support",
                false,
            ),
        ],
    }
}

/// Applies the stored setting at startup.
pub fn initial_prefs(dir: &std::path::Path) -> CandidateHintPrefs {
    crate::store::prefs::candidate_hint(dir)
}
