//! The one process-wide `tracing` subscriber of the platform test binary.
//!
//! `set_global_default` succeeds once per process, so every test module that
//! inspects log events goes through this subscriber; installing a second one
//! would fail (at merge, the input and executor tests each brought their own
//! and the second to install panicked).
//!
//! * [`logs`] (input tests) captures the events the calling thread emits as
//!   " field=value" lines. Installed once and never replaced, with interest
//!   always on: a per-test `with_default` subscriber raced with
//!   tracing-core's shared callsite interest and max-level caches (another
//!   test thread installing or dropping its own subscriber could rebuild
//!   them mid-test, and an event was occasionally dropped before it reached
//!   the capture: an empty capture in
//!   `a_resync_waits_for_our_injections_to_settle`, seen once). With a
//!   single global subscriber whose interest is always on, those caches
//!   never change.
//! * [`install`], [`mark`] and [`since`] (executor tests) keep every event
//!   of every thread, with its `event` name and fields: the executor logs
//!   from its worker thread. Recording starts with the first [`install`] and
//!   skips threads inside [`unrecorded`], so it never allocates on a thread
//!   that counts its allocations (the hook path tests).
#![cfg_attr(
    not(feature = "fault-injection"),
    allow(
        dead_code,
        reason = "the process-wide recording serves the fault-injection executor tests"
    )
)]
use std::cell::{Cell, RefCell};
use std::fmt::{Debug, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once, PoisonError};
use tracing::field::{Field, Visit};
use tracing::subscriber::Interest;
use tracing::{span, Event, Metadata, Subscriber};

thread_local! {
    /// This thread's capture buffer; `None` when it is not capturing.
    static LINES: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    /// Set while this thread's events must not be recorded process-wide.
    static UNRECORDED: Cell<bool> = const { Cell::new(false) };
}
static INSTALL: Once = Once::new();
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Process-wide recording is on (after the first [`install`]).
static RECORDING: AtomicBool = AtomicBool::new(false);
static EVENTS: Mutex<Vec<Captured>> = Mutex::new(Vec::new());

/// One event recorded process-wide.
#[derive(Clone, Debug, Default)]
pub(super) struct Captured {
    pub(super) event: String,
    pub(super) fields: Vec<(String, String)>,
}
impl Captured {
    pub(super) fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }
    pub(super) fn mentions(&self, needle: &str) -> bool {
        self.fields.iter().any(|(_, value)| value.contains(needle))
    }
}

/// " field=value" (Debug) formatting of the per-thread capture.
struct Line(String);
impl Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}
/// Field-by-field recording of the process-wide capture.
struct Fields(Captured);
impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "event" {
            self.0.event = value.into();
        }
        self.0.fields.push((field.name().into(), value.into()));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0
            .fields
            .push((field.name().into(), format!("{value:?}")));
    }
}

struct Router;
impl Subscriber for Router {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::always()
    }
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn event(&self, event: &Event<'_>) {
        // Threads that are not capturing (the hook path under the
        // allocation counter among them) return before formatting.
        let _ = LINES.try_with(|lines| {
            let Ok(mut lines) = lines.try_borrow_mut() else {
                return;
            };
            if let Some(lines) = lines.as_mut() {
                let mut line = Line(event.metadata().level().to_string());
                event.record(&mut line);
                lines.push(line.0);
            }
        });
        if RECORDING.load(Ordering::SeqCst) && !UNRECORDED.try_with(Cell::get).unwrap_or(true) {
            let mut fields = Fields(Captured::default());
            event.record(&mut fields);
            EVENTS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(fields.0);
        }
    }
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}

/// Makes [`Router`] the global subscriber (once). True when it is.
fn global() -> bool {
    INSTALL.call_once(|| {
        INSTALLED.store(
            tracing::subscriber::set_global_default(Router).is_ok(),
            Ordering::SeqCst,
        );
    });
    INSTALLED.load(Ordering::SeqCst)
}

/// Runs `body` capturing the events this thread emits.
pub(super) fn logs<R>(body: impl FnOnce() -> R) -> (R, Vec<String>) {
    assert!(
        global(),
        "no other global tracing subscriber in the test binary"
    );
    let outer = LINES.with(|lines| lines.replace(Some(Vec::new())));
    let value = body();
    let captured = LINES.with(|lines| lines.replace(outer)).unwrap_or_default();
    (value, captured)
}

/// Runs `body` with this thread's events kept out of the process-wide
/// recording (it allocates), for code that counts its allocations.
pub(super) fn unrecorded<R>(body: impl FnOnce() -> R) -> R {
    let outer = UNRECORDED.with(|flag| flag.replace(true));
    let value = body();
    UNRECORDED.with(|flag| flag.set(outer));
    value
}

/// Installs the subscriber and starts recording every thread's events;
/// false if another global subscriber was installed first.
pub(super) fn install() -> bool {
    let installed = global();
    RECORDING.store(installed, Ordering::SeqCst);
    installed
}
/// Position to read new events from.
pub(super) fn mark() -> usize {
    EVENTS.lock().unwrap_or_else(PoisonError::into_inner).len()
}
/// Events recorded after `mark`.
pub(super) fn since(mark: usize) -> Vec<Captured> {
    EVENTS.lock().unwrap_or_else(PoisonError::into_inner)[mark..].to_vec()
}
