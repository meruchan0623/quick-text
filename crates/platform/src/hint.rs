//! Typing hints (beginner overlay): the input threads report the word being typed to whoever
//! listens, which decides what to show.
//!
//! Off by default. While off, each call costs one atomic load: the key path does nothing else.
//! The listener runs on the input thread, so it must only hand the event on (a channel send).
//! The text is user input: never log it (B-09).
use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
};

/// At most this many characters of the typed text are sent (longer abbreviations are rare).
const SENT_CHARS: usize = 128;

pub enum HintEvent {
    /// `text` is the typed-key buffer, after every check that the key could have matched in
    /// this target (not paused, not protected, not blacklisted). `process` is the target's
    /// process name (Windows) or bundle id (macOS).
    Typed { text: String, process: String },
    /// The buffer was edited without a check of the target (Backspace): show the new text only
    /// if a hint is already showing.
    Edited { text: String },
    /// No hint applies any more (a rule ran, the focus moved, matching paused, a reset key).
    Clear,
}
impl fmt::Debug for HintEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Typed { process, .. } => f
                .debug_struct("Typed")
                .field("process", process)
                .finish_non_exhaustive(),
            Self::Edited { .. } => f.write_str("Edited"),
            Self::Clear => f.write_str("Clear"),
        }
    }
}

type Listener = Box<dyn Fn(HintEvent) + Send + Sync>;
static LISTENER: OnceLock<Listener> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(false);
/// A `Typed` or `Edited` was sent after the last `Clear`.
static SHOWING: AtomicBool = AtomicBool::new(false);

/// Sets the listener once; later calls are ignored.
pub fn set_listener(listener: impl Fn(HintEvent) + Send + Sync + 'static) {
    let _ = LISTENER.set(Box::new(listener));
}

/// Turns reporting on or off. Turning it off sends a last `Clear`.
pub fn set_enabled(on: bool) {
    if !ENABLED.swap(on, Ordering::SeqCst) || on {
        return;
    }
    clear_always();
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn send(event: HintEvent) {
    if let Some(listener) = LISTENER.get() {
        listener(event);
    }
}

fn tail(text: &str) -> String {
    let skip = text.chars().count().saturating_sub(SENT_CHARS);
    text.chars().skip(skip).collect()
}

/// A text key that passed every target check and matched no rule.
pub fn typed(text: &str, process: &str) {
    if !enabled() {
        return;
    }
    if text.is_empty() {
        return clear();
    }
    SHOWING.store(true, Ordering::SeqCst);
    send(HintEvent::Typed {
        text: tail(text),
        process: process.to_string(),
    });
}

/// The buffer changed without a target check (Backspace).
pub fn edited(text: &str) {
    if !enabled() || !SHOWING.load(Ordering::SeqCst) {
        return;
    }
    if text.is_empty() {
        return clear();
    }
    send(HintEvent::Edited { text: tail(text) });
}

/// Nothing to hint. Sent once per hint shown.
pub fn clear() {
    if enabled() && SHOWING.load(Ordering::Relaxed) {
        clear_always();
    }
}

fn clear_always() {
    if SHOWING.swap(false, Ordering::SeqCst) {
        send(HintEvent::Clear);
    }
}
