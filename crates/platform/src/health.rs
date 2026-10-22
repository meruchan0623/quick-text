//! Platform-agnostic runtime vocabulary shared by the engine, the supervisor,
//! the Tauri shell and the frontend (development plan §7.2, B-03, B-35, B-44).
//!
//! These types are the 0.4 contract between modules. They carry no user
//! content: rule bodies, buffers, clipboard and selection text never appear in
//! any of them, so they are safe to log, emit and put into diagnostics.
use serde::{Deserialize, Serialize, Serializer};

/// Engine health (B-03 state machine). Display text comes only from the
/// Health mapping table in §7.2; see [`StatusText`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    #[default]
    Starting,
    Healthy,
    Degraded,
    Recovering,
    Failed,
}

impl HealthState {
    /// Whether the user may resume listening (replaces `native_ready()`).
    pub fn accepts_resume(self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }
}

/// Components watched by the supervisor. Required components put the engine
/// into Recovering/Failed; optional ones only degrade it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    Executor,
    KeyboardHook,
    MouseHook,
    RawInput,
    Supervisor,
    FocusEvents,
    Clipboard,
    Protection,
}

impl Component {
    pub const ALL: [Component; 8] = [
        Self::Executor,
        Self::KeyboardHook,
        Self::MouseHook,
        Self::RawInput,
        Self::Supervisor,
        Self::FocusEvents,
        Self::Clipboard,
        Self::Protection,
    ];
    pub fn required(self) -> bool {
        matches!(
            self,
            Self::Executor
                | Self::KeyboardHook
                | Self::MouseHook
                | Self::RawInput
                | Self::Supervisor
        )
    }
}

/// Stable machine-readable reason codes. Frontend text is keyed by these
/// codes (S-21); never put user content into a reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    Ok,
    /// A trigger arrived while another action was executing (B-35).
    DroppedBusy,
    Cancelled,
    Paused,
    EngineDown,
    FocusChanged,
    /// Buffer stamp/generation/prefix continuity check failed.
    StaleInput,
    /// Password, read-only or disabled native edit control.
    ProtectedControl,
    ModifierHeld,
    /// SendInput accepted fewer events than requested (UIPI / elevated target).
    InjectPartial,
    ClipboardBusy,
    /// The user copied something during the lease; restore was skipped.
    ClipboardChanged,
    /// One or more clipboard formats were skipped during backup/restore.
    ClipboardFormatSkipped,
    ClipboardTooLarge,
    ResolveFailed,
    PreflightFailed,
    ExecutorPanic,
    ExecutorTimeout,
    HookMissing,
    RawInputMissing,
    HookInstallFailed,
    Unsupported,
    Internal,
}

/// One component's health as seen by the supervisor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentHealth {
    pub component: Component,
    pub state: HealthState,
    pub reason: Option<ReasonCode>,
    /// Milliseconds since the Unix epoch at which `state` was entered.
    pub since_ms: i64,
}

/// Payload of the `engine-health` event and `get_health` (without `seq`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HealthSnapshot {
    pub state: HealthState,
    pub components: Vec<ComponentHealth>,
}

/// Why automatic expansion is paused (B-03, B-37). A bitmap so that engine
/// failures never clear a manual pause and vice versa.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct PauseReasons(u32);

impl PauseReasons {
    pub const MANUAL: Self = Self(1 << 0);
    pub const TIMED: Self = Self(1 << 1);
    pub const FULLSCREEN: Self = Self(1 << 2);
    pub const RECORDING: Self = Self(1 << 3);
    pub const SAFE_MODE: Self = Self(1 << 4);
    pub const ENGINE_DOWN: Self = Self(1 << 5);
    pub const SELF_TEST: Self = Self(1 << 6);
    const NAMES: [(Self, &'static str); 7] = [
        (Self::MANUAL, "manual"),
        (Self::TIMED, "timed"),
        (Self::FULLSCREEN, "fullscreen"),
        (Self::RECORDING, "recording"),
        (Self::SAFE_MODE, "safe_mode"),
        (Self::ENGINE_DOWN, "engine_down"),
        (Self::SELF_TEST, "self_test"),
    ];

    pub const fn empty() -> Self {
        Self(0)
    }
    pub const fn bits(self) -> u32 {
        self.0
    }
    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(bits & 0x7f)
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
    /// Reasons a user can see on the "paused" row of the Health mapping table
    /// (everything except ENGINE_DOWN and SELF_TEST).
    pub const USER_VISIBLE: Self = Self(
        Self::MANUAL.0 | Self::TIMED.0 | Self::FULLSCREEN.0 | Self::RECORDING.0 | Self::SAFE_MODE.0,
    );
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .filter(|(flag, _)| self.contains(*flag))
            .map(|(_, name)| *name)
            .collect()
    }
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        names.into_iter().fold(Self::empty(), |acc, name| {
            Self::NAMES
                .iter()
                .find(|(_, n)| *n == name)
                .map_or(acc, |(flag, _)| acc.with(*flag))
        })
    }
}

impl Serialize for PauseReasons {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.names().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PauseReasons {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let names = Vec::<String>::deserialize(deserializer)?;
        Ok(Self::from_names(names.iter().map(String::as_str)))
    }
}

/// Payload of the `engine-state` event (without `seq`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EngineState {
    /// True when the engine is healthy enough and no pause reason is set.
    pub listening: bool,
    pub paused_reasons: PauseReasons,
    pub busy: bool,
    /// Executable name of the foreground process, never the window title.
    pub current_process: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    Immediate,
    Terminator,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    /// Input was sent but the target's reaction is unknown.
    Unknown,
    Failed,
    /// Nothing was changed on screen (resolve/preflight refused).
    Skipped,
    Cancelled,
    DroppedBusy,
}

/// Per-stage durations in microseconds, measured with a monotonic clock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StageTimings {
    pub queue_us: Option<u64>,
    pub resolve_us: Option<u64>,
    pub erase_us: Option<u64>,
    pub output_us: Option<u64>,
    pub restore_us: Option<u64>,
    pub total_us: Option<u64>,
}

/// One execution-history row (B-35). Contains no rule body, buffer or
/// clipboard content — only identifiers, codes and timings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Monotonic per-process sequence number of the history ring.
    pub seq: u64,
    /// Milliseconds since the Unix epoch.
    pub at_ms: i64,
    pub rule_id: String,
    pub trigger: TriggerKind,
    pub outcome: Outcome,
    pub reason: ReasonCode,
    /// Executable name only (e.g. `notepad.exe`).
    pub process: Option<String>,
    pub completed_steps: usize,
    pub side_effects: bool,
    /// Clipboard format ids skipped during backup/restore (S-07).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_formats: Vec<u32>,
    pub timings: StageTimings,
}

/// Events the engine pushes out through an [`crate::traits::EventSink`].
/// The Tauri EventBus adds the global `seq` and fans them out (B-44).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineEvent {
    Health(HealthSnapshot),
    State(EngineState),
    Execution(HistoryEntry),
}

/// Status text from the §7.2 Health mapping table: the single source for
/// tray tooltip, tray icon choice and the legacy UI status row (S-03).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusText {
    Starting,
    Failed,
    Recovering,
    Paused,
    Degraded,
    Listening,
    Executing,
}

impl StatusText {
    /// Rows are evaluated top to bottom; the executing overlay applies only on
    /// top of Healthy/Degraded without a pause reason.
    pub fn resolve(health: HealthState, paused: PauseReasons, busy: bool) -> Self {
        match health {
            HealthState::Starting => Self::Starting,
            HealthState::Failed => Self::Failed,
            HealthState::Recovering => Self::Recovering,
            _ if paused.intersects(PauseReasons::USER_VISIBLE) => Self::Paused,
            _ if busy => Self::Executing,
            HealthState::Degraded => Self::Degraded,
            HealthState::Healthy => Self::Listening,
        }
    }
    /// Tray tooltip text (without the timed-pause clock suffix).
    pub fn tooltip(self, paused: PauseReasons) -> &'static str {
        match self {
            Self::Starting => "Quicker Text · 正在启动…",
            Self::Failed => "Quicker Text · 输入监听已停止",
            Self::Recovering => "Quicker Text · 正在恢复…",
            Self::Paused if paused.contains(PauseReasons::SAFE_MODE) => {
                "Quicker Text · 已暂停（安全模式）"
            }
            Self::Paused => "Quicker Text · 已暂停",
            Self::Degraded => "Quicker Text · 监听中（部分功能降级）",
            Self::Listening => "Quicker Text · 监听中",
            Self::Executing => "Quicker Text · 执行中",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_reasons_round_trip_as_names() {
        let reasons = PauseReasons::MANUAL.with(PauseReasons::ENGINE_DOWN);
        let json = serde_json::to_string(&reasons).unwrap();
        assert_eq!(json, r#"["manual","engine_down"]"#);
        let back: PauseReasons = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reasons);
        assert!(back.without(PauseReasons::ENGINE_DOWN) == PauseReasons::MANUAL);
    }

    #[test]
    fn mapping_table_order() {
        use HealthState::*;
        let none = PauseReasons::empty();
        let manual = PauseReasons::MANUAL;
        let down = PauseReasons::ENGINE_DOWN;
        assert_eq!(
            StatusText::resolve(Starting, manual, true),
            StatusText::Starting
        );
        assert_eq!(StatusText::resolve(Failed, down, false), StatusText::Failed);
        assert_eq!(
            StatusText::resolve(Recovering, down, true),
            StatusText::Recovering
        );
        assert_eq!(
            StatusText::resolve(Healthy, manual, true),
            StatusText::Paused
        );
        assert_eq!(
            StatusText::resolve(Degraded, manual, false),
            StatusText::Paused
        );
        assert_eq!(
            StatusText::resolve(Healthy, down, true),
            StatusText::Executing
        );
        assert_eq!(
            StatusText::resolve(Degraded, none, false),
            StatusText::Degraded
        );
        assert_eq!(
            StatusText::resolve(Healthy, none, false),
            StatusText::Listening
        );
        assert_eq!(
            StatusText::Paused.tooltip(PauseReasons::SAFE_MODE),
            "Quicker Text · 已暂停（安全模式）"
        );
    }

    #[test]
    fn required_components() {
        assert!(Component::RawInput.required());
        assert!(!Component::Clipboard.required());
        assert!(HealthState::Degraded.accepts_resume());
        assert!(!HealthState::Recovering.accepts_resume());
    }
}
