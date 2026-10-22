//! Capability table (B-13, S-16; IPC contract §5 `get_capabilities`).
//!
//! Each capability is decided on its own: first from what the platform
//! adapter offers on this build, then lowered by live component health.
//! Reasons are machine-readable codes; the frontend keys display text by
//! them, so no prose and no user content is carried here.
//!
//! - Windows: `available` while the input adapter is up (low-level hooks;
//!   keyboard listening and IME committed text also need RawInput).
//! - macOS (ADR-f): `experimental` wherever an adapter exists and is ready;
//!   `unavailable` when permissions are missing, the adapter is not running,
//!   or no adapter exists. A capability never reports more than the former
//!   `available` flag did.
//! - Other platforms: everything `unavailable` with `unsupported`.
use crate::health::{Component, ComponentHealth, HealthState, ReasonCode};
use serde::Serialize;
use Component::{Clipboard, Executor, FocusEvents, KeyboardHook, MouseHook, RawInput};

/// How usable a capability is (snake_case on the wire).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Available,
    Unavailable,
    /// Provided by an adapter without real-device acceptance (macOS, ADR-f).
    Experimental,
    /// Provided, but a component it depends on is degraded.
    Degraded,
}

/// Why a capability is not plainly available, serialized as a bare
/// snake_case code: a component [`ReasonCode`] or a [`PlatformReason`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum CapabilityReason {
    Health(ReasonCode),
    Platform(PlatformReason),
}

/// Capability reasons about the platform rather than a component.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformReason {
    /// The macOS adapter is ready but experimental (ADR-f).
    ExperimentalPlatform,
    /// macOS Accessibility, Input Monitoring or synthetic-input access is
    /// not granted.
    PermissionMissing,
    /// No adapter on this platform provides the capability.
    NotImplemented,
}

/// One entry of `get_capabilities`. `available` is kept for the legacy UI
/// and always equals `status != unavailable`; `reason` is null exactly when
/// `status` is `available`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Capability {
    pub id: String,
    pub status: CapabilityStatus,
    pub reason: Option<CapabilityReason>,
    pub available: bool,
}

/// Capabilities of this build lowered by live component health: a Degraded
/// dependency makes a capability `degraded`; a Starting, Recovering or
/// Failed one makes it `unavailable`; the worst one wins (ties keep the
/// earlier reason). Components absent from `components` count as healthy,
/// and healthy components never lift what the adapter cannot do.
pub fn capabilities_for(components: &[ComponentHealth]) -> Vec<Capability> {
    table(readiness(), components)
}

/// What the platform adapter offers right now, before component health.
#[allow(
    dead_code,
    reason = "each target constructs only its own variant; tests construct all"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Readiness {
    /// Windows: low-level hooks installed; RawInput detector registered.
    Windows { hooks: bool, raw_input: bool },
    /// macOS event tap and permissions (experimental, ADR-f).
    MacOs(MacOs),
    /// No native adapter on this platform.
    Unsupported,
}

#[allow(
    dead_code,
    reason = "each target constructs only its own variant; tests construct all"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MacOs {
    Ready,
    PermissionMissing,
    /// Permissions granted but the event tap is not running: not installed,
    /// Secure Input active, or an IME/dead-key layout.
    NotReady,
}

#[cfg(windows)]
fn readiness() -> Readiness {
    let raw_input = crate::input_status()["espanso_input"]["ready"]
        .as_bool()
        .unwrap_or(false);
    Readiness::Windows {
        hooks: crate::engine_ready(),
        raw_input,
    }
}

#[cfg(target_os = "macos")]
fn readiness() -> Readiness {
    if crate::engine_ready() {
        return Readiness::MacOs(MacOs::Ready);
    }
    let status = crate::input_status();
    let granted = ["accessibility", "input_monitoring", "post_event_access"]
        .iter()
        .all(|flag| status[*flag].as_bool().unwrap_or(false));
    Readiness::MacOs(if granted {
        MacOs::NotReady
    } else {
        MacOs::PermissionMissing
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
fn readiness() -> Readiness {
    Readiness::Unsupported
}

/// What a capability needs from the platform adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Needs {
    /// The native input adapter (Windows hooks, macOS event tap).
    Adapter,
    /// The adapter plus RawInput translated characters (Windows listening).
    AdapterAndRawInput,
    /// Nothing on this platform provides it.
    NoAdapter,
}

/// A capability id, what it needs from the adapter, and the components it
/// depends on at run time.
struct Row {
    id: &'static str,
    needs: Needs,
    depends_on: &'static [Component],
}

const fn row(id: &'static str, needs: Needs, depends_on: &'static [Component]) -> Row {
    Row {
        id,
        needs,
        depends_on,
    }
}

/// Windows rows; unsupported platforms report the same ids.
const WINDOWS: &[Row] = &[
    row(
        "keyboard_hook",
        Needs::AdapterAndRawInput,
        &[KeyboardHook, RawInput],
    ),
    row("keys", Needs::Adapter, &[Executor]),
    row("paste", Needs::Adapter, &[Executor, Clipboard]),
    row("html", Needs::Adapter, &[Executor, Clipboard]),
    row("file_clipboard", Needs::Adapter, &[Executor, Clipboard]),
    row("image_clipboard", Needs::Adapter, &[Executor, Clipboard]),
    row("mouse", Needs::Adapter, &[Executor]),
    row("recording", Needs::Adapter, &[KeyboardHook, MouseHook]),
    row("foreground", Needs::Adapter, &[FocusEvents]),
    row("open", Needs::Adapter, &[Executor]),
    row(
        "ime_committed_text",
        Needs::AdapterAndRawInput,
        &[KeyboardHook, RawInput],
    ),
    row("quicker_private_action", Needs::NoAdapter, &[]),
];

const MACOS: &[Row] = &[
    row("keyboard_hook", Needs::Adapter, &[KeyboardHook]),
    row("keys", Needs::Adapter, &[Executor]),
    row("paste", Needs::Adapter, &[Executor, Clipboard]),
    row("html", Needs::Adapter, &[Executor, Clipboard]),
    row("file_clipboard", Needs::Adapter, &[Executor, Clipboard]),
    row("image_clipboard", Needs::Adapter, &[Executor, Clipboard]),
    row("mouse", Needs::Adapter, &[Executor]),
    row("recording", Needs::Adapter, &[KeyboardHook, MouseHook]),
    row("foreground", Needs::Adapter, &[FocusEvents]),
    row("open", Needs::Adapter, &[Executor]),
    // IME input sources are rejected; committed CJK text is not observable.
    row("ime_committed_text", Needs::NoAdapter, &[]),
    // The Windows 1/120 wheel delta has no verified macOS mapping.
    row("wheel_delta", Needs::NoAdapter, &[]),
    row("media_system_actions", Needs::Adapter, &[Executor]),
    row("window_ime_actions", Needs::NoAdapter, &[]),
    row("quicker_private_action", Needs::NoAdapter, &[]),
];

/// A status with the reason it carries; Experimental always carries
/// `experimental_platform` and Available none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Available,
    Experimental,
    Degraded(CapabilityReason),
    Unavailable(CapabilityReason),
}

impl Verdict {
    fn severity(self) -> u8 {
        match self {
            Self::Available => 0,
            Self::Experimental => 1,
            Self::Degraded(_) => 2,
            Self::Unavailable(_) => 3,
        }
    }
    fn worse(self, other: Self) -> Self {
        if other.severity() > self.severity() {
            other
        } else {
            self
        }
    }
    fn capability(self, id: &str) -> Capability {
        let (status, reason) = match self {
            Self::Available => (CapabilityStatus::Available, None),
            Self::Experimental => (
                CapabilityStatus::Experimental,
                Some(platform(PlatformReason::ExperimentalPlatform)),
            ),
            Self::Degraded(reason) => (CapabilityStatus::Degraded, Some(reason)),
            Self::Unavailable(reason) => (CapabilityStatus::Unavailable, Some(reason)),
        };
        Capability {
            id: id.into(),
            status,
            reason,
            available: status != CapabilityStatus::Unavailable,
        }
    }
}

fn health(code: ReasonCode) -> CapabilityReason {
    CapabilityReason::Health(code)
}

fn platform(code: PlatformReason) -> CapabilityReason {
    CapabilityReason::Platform(code)
}

fn table(readiness: Readiness, components: &[ComponentHealth]) -> Vec<Capability> {
    let rows = match readiness {
        Readiness::MacOs(_) => MACOS,
        Readiness::Windows { .. } | Readiness::Unsupported => WINDOWS,
    };
    rows.iter()
        .map(|row| {
            components
                .iter()
                .filter(|component| row.depends_on.contains(&component.component))
                .map(lowered_by)
                .fold(baseline(readiness, row.needs), Verdict::worse)
                .capability(row.id)
        })
        .collect()
}

/// What the adapter alone allows.
fn baseline(readiness: Readiness, needs: Needs) -> Verdict {
    match (readiness, needs) {
        (Readiness::Unsupported, _) => Verdict::Unavailable(health(ReasonCode::Unsupported)),
        (_, Needs::NoAdapter) => Verdict::Unavailable(platform(PlatformReason::NotImplemented)),
        (Readiness::Windows { hooks: false, .. }, _) => {
            Verdict::Unavailable(health(ReasonCode::HookMissing))
        }
        (
            Readiness::Windows {
                raw_input: false, ..
            },
            Needs::AdapterAndRawInput,
        ) => Verdict::Unavailable(health(ReasonCode::RawInputMissing)),
        (Readiness::Windows { .. }, _) => Verdict::Available,
        (Readiness::MacOs(MacOs::Ready), _) => Verdict::Experimental,
        (Readiness::MacOs(MacOs::PermissionMissing), _) => {
            Verdict::Unavailable(platform(PlatformReason::PermissionMissing))
        }
        (Readiness::MacOs(MacOs::NotReady), _) => {
            Verdict::Unavailable(health(ReasonCode::EngineDown))
        }
    }
}

/// What one component's health does to the capabilities depending on it.
fn lowered_by(component: &ComponentHealth) -> Verdict {
    let reason = health(match component.reason {
        Some(code) if code != ReasonCode::Ok => code,
        _ => default_reason(component.component),
    });
    match component.state {
        HealthState::Healthy => Verdict::Available,
        HealthState::Degraded => Verdict::Degraded(reason),
        HealthState::Starting | HealthState::Recovering | HealthState::Failed => {
            Verdict::Unavailable(reason)
        }
    }
}

/// The reason given when an unhealthy component reports none.
fn default_reason(component: Component) -> ReasonCode {
    match component {
        Component::Executor | Component::Supervisor => ReasonCode::EngineDown,
        Component::KeyboardHook | Component::MouseHook | Component::FocusEvents => {
            ReasonCode::HookMissing
        }
        Component::RawInput => ReasonCode::RawInputMissing,
        Component::Clipboard => ReasonCode::ClipboardBusy,
        Component::Protection => ReasonCode::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CapabilityStatus::{Available as A, Degraded as D, Experimental as E, Unavailable as U};
    use HealthState::{Degraded, Failed, Healthy, Recovering, Starting};

    const READY: Readiness = Readiness::Windows {
        hooks: true,
        raw_input: true,
    };
    const NO_HOOKS: Readiness = Readiness::Windows {
        hooks: false,
        raw_input: true,
    };
    const NO_RAW_INPUT: Readiness = Readiness::Windows {
        hooks: true,
        raw_input: false,
    };

    fn component(
        component: Component,
        state: HealthState,
        reason: Option<ReasonCode>,
    ) -> ComponentHealth {
        ComponentHealth {
            component,
            state,
            reason,
            since_ms: 0,
        }
    }
    fn verdict(capability: &Capability) -> (CapabilityStatus, Option<CapabilityReason>) {
        (capability.status, capability.reason)
    }
    fn get<'a>(table: &'a [Capability], id: &str) -> &'a Capability {
        table.iter().find(|capability| capability.id == id).unwrap()
    }
    fn ids(table: &[Capability]) -> Vec<&str> {
        table
            .iter()
            .map(|capability| capability.id.as_str())
            .collect()
    }

    #[test]
    fn windows_decides_each_capability_on_its_own() {
        let hook = Some(health(ReasonCode::HookMissing));
        let raw = Some(health(ReasonCode::RawInputMissing));
        let missing = Some(platform(PlatformReason::NotImplemented));
        // Everything ready, hooks missing, RawInput missing.
        let expected = [
            ("keyboard_hook", (A, None), (U, hook), (U, raw)),
            ("keys", (A, None), (U, hook), (A, None)),
            ("paste", (A, None), (U, hook), (A, None)),
            ("html", (A, None), (U, hook), (A, None)),
            ("file_clipboard", (A, None), (U, hook), (A, None)),
            ("image_clipboard", (A, None), (U, hook), (A, None)),
            ("mouse", (A, None), (U, hook), (A, None)),
            ("recording", (A, None), (U, hook), (A, None)),
            ("foreground", (A, None), (U, hook), (A, None)),
            ("open", (A, None), (U, hook), (A, None)),
            ("ime_committed_text", (A, None), (U, hook), (U, raw)),
            (
                "quicker_private_action",
                (U, missing),
                (U, missing),
                (U, missing),
            ),
        ];
        let tables = [
            table(READY, &[]),
            table(NO_HOOKS, &[]),
            table(NO_RAW_INPUT, &[]),
        ];
        for table in &tables {
            assert_eq!(ids(table), expected.map(|(id, ..)| id));
        }
        for (index, (id, ready, no_hooks, no_raw)) in expected.into_iter().enumerate() {
            assert_eq!(verdict(&tables[0][index]), ready, "{id}: ready");
            assert_eq!(verdict(&tables[1][index]), no_hooks, "{id}: no hooks");
            assert_eq!(verdict(&tables[2][index]), no_raw, "{id}: no RawInput");
        }
        // Hooks are checked first when both are missing.
        let neither = Readiness::Windows {
            hooks: false,
            raw_input: false,
        };
        assert_eq!(
            verdict(get(&table(neither, &[]), "keyboard_hook")),
            (U, hook)
        );
    }

    #[test]
    fn degraded_clipboard_degrades_only_clipboard_capabilities() {
        let clipboard = ["paste", "html", "file_clipboard", "image_clipboard"];
        let baseline = table(READY, &[]);
        let busy = table(
            READY,
            &[component(
                Clipboard,
                Degraded,
                Some(ReasonCode::ClipboardBusy),
            )],
        );
        for (before, after) in baseline.iter().zip(&busy) {
            if clipboard.contains(&after.id.as_str()) {
                let expected = (D, Some(health(ReasonCode::ClipboardBusy)));
                assert_eq!(verdict(after), expected, "{}", after.id);
                assert!(after.available, "{}", after.id);
            } else {
                assert_eq!(after, before);
            }
        }
    }

    #[test]
    fn component_health_maps_to_capability_status() {
        // A dependent capability for each component, and its default reason.
        let cases = [
            (Executor, "keys", ReasonCode::EngineDown),
            (KeyboardHook, "keyboard_hook", ReasonCode::HookMissing),
            (MouseHook, "recording", ReasonCode::HookMissing),
            (RawInput, "ime_committed_text", ReasonCode::RawInputMissing),
            (FocusEvents, "foreground", ReasonCode::HookMissing),
            (Clipboard, "paste", ReasonCode::ClipboardBusy),
        ];
        let states = [
            (Healthy, A),
            (Degraded, D),
            (Starting, U),
            (Recovering, U),
            (Failed, U),
        ];
        for (dependency, id, default) in cases {
            for (state, status) in states {
                let given = |reason| {
                    let table = table(READY, &[component(dependency, state, reason)]);
                    verdict(get(&table, id))
                };
                let reason = |code| (status != A).then_some(health(code));
                let case = format!("{dependency:?} {state:?}");
                assert_eq!(given(None), (status, reason(default)), "{case}");
                assert_eq!(
                    given(Some(ReasonCode::Ok)),
                    (status, reason(default)),
                    "{case}"
                );
                let code = ReasonCode::ExecutorTimeout;
                assert_eq!(given(Some(code)), (status, reason(code)), "{case}");
            }
        }
        assert_eq!(
            default_reason(Component::Supervisor),
            ReasonCode::EngineDown
        );
    }

    #[test]
    fn worst_dependency_wins_and_health_never_lifts_the_adapter() {
        let busy = component(Clipboard, Degraded, Some(ReasonCode::ClipboardBusy));
        let panicked = component(Executor, Recovering, Some(ReasonCode::ExecutorPanic));
        let down = (U, Some(health(ReasonCode::ExecutorPanic)));
        for components in [[busy.clone(), panicked.clone()], [panicked, busy.clone()]] {
            assert_eq!(verdict(get(&table(READY, &components), "paste")), down);
        }
        // Equal severity keeps the earlier reason.
        let edited = component(Clipboard, Degraded, Some(ReasonCode::ClipboardChanged));
        let slow = component(Executor, Degraded, Some(ReasonCode::ExecutorTimeout));
        let paste = verdict(get(&table(READY, &[edited, slow]), "paste")).1;
        assert_eq!(paste, Some(health(ReasonCode::ClipboardChanged)));
        // A degraded dependency never hides a missing adapter.
        let hook = (U, Some(health(ReasonCode::HookMissing)));
        assert_eq!(verdict(get(&table(NO_HOOKS, &[busy]), "paste")), hook);
        // Healthy components change nothing, ready or not.
        let healthy = Component::ALL.map(|c| component(c, Healthy, None));
        for readiness in [READY, NO_HOOKS, NO_RAW_INPUT] {
            assert_eq!(table(readiness, &healthy), table(readiness, &[]));
        }
    }

    #[test]
    fn macos_is_experimental_where_an_adapter_exists() {
        let missing = [
            "ime_committed_text",
            "wheel_delta",
            "window_ime_actions",
            "quicker_private_action",
        ];
        let not_implemented = Some(platform(PlatformReason::NotImplemented));
        let ready = table(Readiness::MacOs(MacOs::Ready), &[]);
        assert_eq!(
            ids(&ready),
            [
                "keyboard_hook",
                "keys",
                "paste",
                "html",
                "file_clipboard",
                "image_clipboard",
                "mouse",
                "recording",
                "foreground",
                "open",
                "ime_committed_text",
                "wheel_delta",
                "media_system_actions",
                "window_ime_actions",
                "quicker_private_action",
            ]
        );
        for capability in &ready {
            let expected = if missing.contains(&capability.id.as_str()) {
                (U, not_implemented)
            } else {
                (E, Some(platform(PlatformReason::ExperimentalPlatform)))
            };
            assert_eq!(verdict(capability), expected, "{}", capability.id);
        }
        let blocked = [
            (
                MacOs::PermissionMissing,
                platform(PlatformReason::PermissionMissing),
            ),
            (MacOs::NotReady, health(ReasonCode::EngineDown)),
        ];
        for (state, reason) in blocked {
            for capability in table(Readiness::MacOs(state), &[]) {
                let expected = if missing.contains(&capability.id.as_str()) {
                    (U, not_implemented)
                } else {
                    (U, Some(reason))
                };
                assert_eq!(
                    verdict(&capability),
                    expected,
                    "{state:?} {}",
                    capability.id
                );
            }
        }
        // Live health outranks the experimental label.
        let busy = [component(Clipboard, Degraded, None)];
        let paste = get(&table(Readiness::MacOs(MacOs::Ready), &busy), "paste").clone();
        assert_eq!(
            verdict(&paste),
            (D, Some(health(ReasonCode::ClipboardBusy)))
        );
    }

    #[test]
    fn unsupported_platform_offers_nothing() {
        let unsupported = table(Readiness::Unsupported, &[]);
        assert_eq!(ids(&unsupported), ids(&table(READY, &[])));
        for capability in &unsupported {
            let expected = (U, Some(health(ReasonCode::Unsupported)));
            assert_eq!(verdict(capability), expected, "{}", capability.id);
            assert!(!capability.available);
        }
    }

    #[test]
    fn available_mirrors_status_and_ids_are_unique() {
        let platforms = [
            READY,
            NO_HOOKS,
            NO_RAW_INPUT,
            Readiness::MacOs(MacOs::Ready),
            Readiness::MacOs(MacOs::PermissionMissing),
            Readiness::MacOs(MacOs::NotReady),
            Readiness::Unsupported,
        ];
        let healths = [
            vec![],
            Component::ALL.map(|c| component(c, Healthy, None)).to_vec(),
            Component::ALL
                .map(|c| component(c, Degraded, None))
                .to_vec(),
            Component::ALL.map(|c| component(c, Failed, None)).to_vec(),
            vec![component(Clipboard, Degraded, None)],
        ];
        for readiness in platforms {
            for components in &healths {
                let table = table(readiness, components);
                for capability in &table {
                    assert_eq!(capability.available, capability.status != U);
                    assert_eq!(capability.reason.is_none(), capability.status == A);
                }
                let mut unique = ids(&table);
                unique.sort_unstable();
                unique.dedup();
                assert_eq!(unique.len(), table.len(), "{readiness:?}");
            }
        }
    }

    #[test]
    fn serializes_to_the_ipc_contract_shape() {
        let busy = [component(
            Clipboard,
            Degraded,
            Some(ReasonCode::ClipboardBusy),
        )];
        let json = serde_json::to_string(&table(READY, &busy)).unwrap();
        assert_eq!(
            json,
            concat!(
                "[",
                r#"{"id":"keyboard_hook","status":"available","reason":null,"available":true},"#,
                r#"{"id":"keys","status":"available","reason":null,"available":true},"#,
                r#"{"id":"paste","status":"degraded","reason":"clipboard_busy","available":true},"#,
                r#"{"id":"html","status":"degraded","reason":"clipboard_busy","available":true},"#,
                r#"{"id":"file_clipboard","status":"degraded","reason":"clipboard_busy","available":true},"#,
                r#"{"id":"image_clipboard","status":"degraded","reason":"clipboard_busy","available":true},"#,
                r#"{"id":"mouse","status":"available","reason":null,"available":true},"#,
                r#"{"id":"recording","status":"available","reason":null,"available":true},"#,
                r#"{"id":"foreground","status":"available","reason":null,"available":true},"#,
                r#"{"id":"open","status":"available","reason":null,"available":true},"#,
                r#"{"id":"ime_committed_text","status":"available","reason":null,"available":true},"#,
                r#"{"id":"quicker_private_action","status":"unavailable","reason":"not_implemented","available":false}"#,
                "]"
            )
        );
        // S-16: the unwired "Esc closes the candidate window" note is gone.
        assert!(!json.contains("Esc"));
        let first = |readiness| serde_json::to_string(&table(readiness, &[])[0]).unwrap();
        assert_eq!(
            first(NO_RAW_INPUT),
            r#"{"id":"keyboard_hook","status":"unavailable","reason":"raw_input_missing","available":false}"#
        );
        assert_eq!(
            first(Readiness::MacOs(MacOs::Ready)),
            r#"{"id":"keyboard_hook","status":"experimental","reason":"experimental_platform","available":true}"#
        );
        assert_eq!(
            first(Readiness::MacOs(MacOs::PermissionMissing)),
            r#"{"id":"keyboard_hook","status":"unavailable","reason":"permission_missing","available":false}"#
        );
        assert_eq!(
            first(Readiness::Unsupported),
            r#"{"id":"keyboard_hook","status":"unavailable","reason":"unsupported","available":false}"#
        );
    }
}
