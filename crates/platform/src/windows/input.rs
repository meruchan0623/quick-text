//! Input-source facade: starts RawInput + hooks and reports their health.
use super::*;
use super::{
    hook::{test_input_enabled, HOOK_READY},
    ime::ime_active,
};
use crate::health::{Component, ComponentHealth, HealthState, ReasonCode};
pub fn input_status() -> serde_json::Value {
    let layout = unsafe {
        GetKeyboardLayout(GetWindowThreadProcessId(
            GetForegroundWindow(),
            std::ptr::null_mut(),
        )) as usize
    };
    serde_json::json!({"reason":LAST_GATE.get().map(|g|g.lock().unwrap().clone()).unwrap_or_else(||"Engine starting".into()),"hooks_ready":engine_ready(),"layout":format!("0x{:04X}",layout & 0xffff),"layout_hkl":format!("0x{layout:016X}"),"external_injected_keys":if test_input_enabled(){"explicit_test_mode"}else{"filtered"},"ime_active":ime_active(),"target_observation":observer::status(),"physical_input":rawinput::status(),"espanso_input":rawinput::status(),"automatic_trigger_modes":["espanso_rawinput_immediate","espanso_rawinput_space","espanso_rawinput_semicolon"],"terminator_capture":"space/semicolon post-input; enter/tab disabled"})
}
pub fn engine_ready() -> bool {
    HOOK_READY.load(Ordering::SeqCst)
}
pub fn native_ready() -> Result<(), String> {
    if !engine_ready() {
        return Err("Native keyboard/mouse hooks are unavailable".into());
    }
    Ok(())
}
/// Starts the RawInput detector (which also starts the UIA protection
/// sampler) and then the hook thread.
pub(crate) fn start_input_sources(shared: Arc<Shared>) -> Result<(), String> {
    // Independent protection sampling is started by the RawInput bridge.
    rawinput::start(shared)?;
    hook::spawn_hooks();
    Ok(())
}
/// Health of the input sources, derived from the existing readiness flags.
/// Until the supervisor (B-06) tracks transitions, "not ready" is reported as
/// `Starting` (install failure and not-yet-started are indistinguishable) and
/// `since_ms` is the observation time.
#[allow(dead_code, reason = "first caller is the supervisor (B-06)")]
pub(crate) fn input_health() -> Vec<ComponentHealth> {
    let now = chrono::Utc::now().timestamp_millis();
    let hooks = HOOK_READY.load(Ordering::SeqCst);
    let raw = rawinput::READY.load(Ordering::SeqCst);
    let entry = |component, ready: bool, missing| ComponentHealth {
        component,
        state: if ready {
            HealthState::Healthy
        } else {
            HealthState::Starting
        },
        reason: (!ready).then_some(missing),
        since_ms: now,
    };
    vec![
        entry(Component::KeyboardHook, hooks, ReasonCode::HookMissing),
        entry(Component::MouseHook, hooks, ReasonCode::HookMissing),
        entry(Component::FocusEvents, hooks, ReasonCode::HookMissing),
        entry(Component::RawInput, raw, ReasonCode::RawInputMissing),
    ]
}
/// Re-installs one input source after a failure.
#[allow(dead_code, reason = "first caller is the supervisor (B-06)")]
pub(crate) fn reattach(component: Component) -> Result<(), String> {
    // TODO(B-06): reinstall the hook / RawInput window for `component` on its
    // own thread with backoff; until then the supervisor cannot recover it.
    Err(format!("reattach {component:?} is not implemented"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_health_reports_unstarted_sources_without_claiming_health() {
        let health = input_health();
        assert_eq!(
            health.iter().map(|h| h.component).collect::<Vec<_>>(),
            [
                Component::KeyboardHook,
                Component::MouseHook,
                Component::FocusEvents,
                Component::RawInput
            ]
        );
        // Tests never install hooks or start RawInput.
        assert!(health.iter().all(|h| h.state == HealthState::Starting));
        assert!(reattach(Component::KeyboardHook).is_err());
    }
}
