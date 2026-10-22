//! Native input runs on one hook path. Hooks never perform file I/O or execute actions.
use quicker_domain::*;
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

fn acceptance_input_policy(debug: bool, environment: Option<&str>, explicit_flag: bool) -> bool {
    environment == Some("1") && (debug || explicit_flag)
}

/// Explicit startup opt-in for Windows automation acceptance with external SendInput.
/// Cached once so data isolation and the keyboard hook use the same startup mode.
pub fn acceptance_test_input_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        cfg!(windows)
            && acceptance_input_policy(
                cfg!(debug_assertions),
                std::env::var("QUICKER_TEXT_TEST_INPUT").ok().as_deref(),
                std::env::args_os().any(|arg| arg == "--acceptance-test-input"),
            )
    })
}

#[cfg(test)]
mod acceptance_mode_tests {
    use super::acceptance_input_policy;

    #[test]
    fn release_requires_both_startup_gates() {
        for environment in [None, Some("0"), Some("true"), Some(""), Some("1 ")] {
            assert!(!acceptance_input_policy(false, environment, false));
            assert!(!acceptance_input_policy(false, environment, true));
        }
        assert!(!acceptance_input_policy(false, Some("1"), false));
        assert!(acceptance_input_policy(false, Some("1"), true));
    }

    #[test]
    fn debug_preserves_environment_opt_in() {
        assert!(!acceptance_input_policy(true, None, false));
        assert!(!acceptance_input_policy(true, None, true));
        assert!(acceptance_input_policy(true, Some("1"), false));
        assert!(acceptance_input_policy(true, Some("1"), true));
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Foreground {
    pub platform: String,
    pub bundle_id: String,
    pub id: u64,
    pub process: String,
    pub title: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExecutionResult {
    pub id: String,
    pub status: String,
    pub message: String,
    pub completed_steps: usize,
    pub side_effects: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct Capability {
    pub id: String,
    pub available: bool,
    pub reason: String,
}
pub struct Shared {
    pub config: Mutex<Config>,
    pub compiled: Mutex<CompiledConfig>,
    pub paused: AtomicBool,
    pub generation: AtomicU64,
    pub cancel: AtomicBool,
    pub busy: AtomicBool,
    pub last: Mutex<Option<ExecutionResult>>,
    pub recording: Mutex<Option<Vec<Step>>>,
    pub shutdown: AtomicBool,
}
impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            config: Mutex::new(Config::default()),
            compiled: Mutex::new(compile_config(&Config::default()).unwrap()),
            paused: AtomicBool::new(true),
            generation: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            last: Mutex::new(None),
            recording: Mutex::new(None),
            shutdown: AtomicBool::new(false),
        })
    }
}
pub mod fault;
pub mod health;
pub mod traits;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported;
#[cfg(not(any(windows, target_os = "macos")))]
pub use unsupported::*;

#[cfg(not(target_os = "macos"))]
pub fn capabilities() -> Vec<Capability> {
    #[cfg(windows)]
    let native = engine_ready();
    #[cfg(not(windows))]
    let native = false;
    let reason = if native {
        "Windows 输入功能已就绪；发送后仍需查看目标程序是否收到"
    } else if cfg!(windows) {
        "Windows 键盘监听暂不可用，软件保持暂停"
    } else {
        "当前系统尚不支持自动输入"
    };
    #[cfg(windows)]
    let target_status = input_status();
    #[cfg(windows)]
    let ime_observable = target_status["espanso_input"]["ready"]
        .as_bool()
        .unwrap_or(false);
    #[cfg(not(windows))]
    let ime_observable = false;
    ["keyboard_hook","keys","paste","html","file_clipboard","image_clipboard","mouse","recording","foreground","open"].into_iter().map(|id|Capability{id:id.into(),available:native,reason:reason.into()}).chain([Capability{id:"ime_committed_text".into(),available:native && ime_observable,reason:"Windows 使用 Espanso RawInput 监听与原生注入，保留当前输入法翻译出的字符并按匹配字符数退格。支持立即触发与空格或分号触发；回车和 Tab 触发停用。组合候选和目标收到的结果未知，不依赖 UIA 逐字读回。当前后端按退格处理，Esc 偏好配置保留但暂未应用".into()},Capability{id:"quicker_private_action".into(),available:false,reason:"当前版本不能执行原 Quicker 动作，仅保留导入信息；请改成支持的操作".into()}]).collect()
}
#[cfg(target_os = "macos")]
pub fn capabilities() -> Vec<Capability> {
    platform_capabilities()
}
pub fn start_recording(shared: &Arc<Shared>) -> Result<(), String> {
    #[cfg(any(windows, target_os = "macos"))]
    native_ready()?;
    if shared.busy.load(Ordering::SeqCst) {
        return Err("Wait for the running action to stop before recording".into());
    }
    let mut r = shared.recording.lock().map_err(|_| "recording lock")?;
    if r.is_some() {
        return Err("Recording already active".into());
    }
    shared.paused.store(true, Ordering::SeqCst);
    shared.cancel.store(false, Ordering::SeqCst);
    shared.generation.fetch_add(1, Ordering::SeqCst);
    *r = Some(vec![]);
    Ok(())
}
pub fn stop_recording(shared: &Arc<Shared>) -> Result<ActionPlan, String> {
    let steps = shared
        .recording
        .lock()
        .map_err(|_| "recording lock")?
        .take()
        .ok_or("Recording is not active")?;
    Ok(ActionPlan {
        steps,
        dialect: Dialect::Replay,
        ..Default::default()
    })
}
