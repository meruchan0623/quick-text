//! Action plans: preflight and execution (erase, output steps, owned-key
//! cleanup, terminator and clipboard restore).
use super::*;
use super::{
    clipboard::{
        acquire_clipboard, clipboard_formats, written_before_erase, ClipboardLease, LeaseError,
        WinClipboard,
    },
    executor::{CandidateProof, RunReport},
    foreground::WinForeground,
    inject::{
        button, erase, hotkey, key, mouse_input, press, release_owned, send_terminator, unicode,
        vk, WinInjector, SENT_EFFECT,
    },
    keystate::{modifiers_held, physical},
};
use crate::health::ReasonCode;
use crate::traits::{ClipboardBackend, Clock, ForegroundProbe, Injector, SystemClock};
thread_local! {
    static AUTOMATIC_EXECUTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static STAGE: std::cell::Cell<Stage> = const { std::cell::Cell::new(Stage::Idle) };
    static STAGE_AT: std::cell::Cell<StageMarks> = const { std::cell::Cell::new([None; STAGES]) };
}
const STAGES: usize = 10;
/// When the run on this thread entered each [`Stage`] (indexed by stage).
pub(super) type StageMarks = [Option<Instant>; STAGES];
/// Where the run on this thread currently is. Logged (location only, never
/// content) when an executor task panics; the entry times give the history
/// its stage timings (B-35).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    Idle,
    Resolve,
    Preflight,
    Guard,
    Clipboard,
    Erase,
    Output,
    Cleanup,
    Restore,
    Done,
}
impl Stage {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Stage::Idle => "idle",
            Stage::Resolve => "resolve",
            Stage::Preflight => "preflight",
            Stage::Guard => "guard",
            Stage::Clipboard => "clipboard",
            Stage::Erase => "erase",
            Stage::Output => "output",
            Stage::Cleanup => "cleanup",
            Stage::Restore => "restore",
            Stage::Done => "done",
        }
    }
}
/// Enters `stage` at `at` (the run's own clock).
pub(super) fn set_stage(stage: Stage, at: Instant) {
    STAGE.with(|current| current.set(stage));
    STAGE_AT.with(|marks| {
        let mut entered = marks.get();
        entered[stage as usize] = Some(at);
        marks.set(entered);
    });
}
pub(super) fn current_stage() -> Stage {
    STAGE.with(std::cell::Cell::get)
}
/// Starts a new trace on this thread (no stage entered, nothing sent yet).
pub(super) fn begin_trace() {
    STAGE.with(|current| current.set(Stage::Idle));
    STAGE_AT.with(|marks| marks.set([None; STAGES]));
    SENT_EFFECT.with(|flag| flag.set(false));
}
pub(super) fn stage_marks() -> StageMarks {
    STAGE_AT.with(std::cell::Cell::get)
}
fn open(target: &str, args: &[String], cwd: &Option<String>) -> Result<(), String> {
    if args.is_empty() {
        unsafe {
            let result = ShellExecuteW(
                std::ptr::null_mut(),
                wide("open").as_ptr(),
                wide(target).as_ptr(),
                std::ptr::null(),
                cwd.as_ref()
                    .map(|s| wide(s))
                    .as_ref()
                    .map(|w| w.as_ptr())
                    .unwrap_or(std::ptr::null()),
                SW_SHOWNORMAL,
            );
            if result as usize <= 32 {
                return Err("Windows could not open target".into());
            }
        }
        Ok(())
    } else {
        let mut cmd = Command::new(target);
        cmd.args(args);
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
    }
}

pub fn preflight(plan: &ActionPlan) -> Result<(), String> {
    let diagnostics = validate_plan(plan);
    if !diagnostics.is_empty() {
        return Err(diagnostics
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
            .join("; "));
    }
    for step in &plan.steps {
        match &step.operation {
            Operation::Text {
                text,
                method: OutputMethod::Keys,
            } if text.contains('\0') => {
                return Err("Unicode key output cannot contain NUL".into());
            }
            Operation::KeyDown { key } | Operation::KeyUp { key } | Operation::KeyPress { key } => {
                vk(key)?;
            }
            Operation::Hotkey { keys } => {
                for k in keys {
                    vk(k)?;
                }
            }
            // Fixed texts (F5-1 H): an error never echoes the rule's values
            // (a path, a decoder's details), since it can reach a log.
            Operation::PasteFiles { paths } => {
                if paths.iter().any(|p| !std::path::Path::new(p).exists()) {
                    return Err("A file to paste does not exist".into());
                }
            }
            Operation::PasteImage { path } => {
                image::open(path).map_err(|error| match error {
                    image::ImageError::IoError(_) => "Image file could not be read",
                    _ => "Image could not be decoded",
                })?;
            }
            Operation::Open {
                target,
                args,
                working_directory,
            } => {
                if !(target.starts_with("https://")
                    || target.starts_with("http://")
                    || std::path::Path::new(target).is_absolute()
                        && std::path::Path::new(target).exists())
                {
                    return Err(
                        "Open target must be an existing absolute path or HTTP(S) URL".into(),
                    );
                }
                if !args.is_empty() && target.starts_with("http") {
                    return Err("URL arguments are not supported".into());
                }
                if let Some(c) = working_directory {
                    if !std::path::Path::new(c).is_dir() {
                        return Err("Working directory does not exist".into());
                    }
                }
            }
            Operation::Reusable { .. } => {
                return Err("Reusable action id is not registered; map it explicitly".into())
            }
            Operation::Common { function } => {
                common_keys(function)?;
            }
            _ => {}
        }
    }
    Ok(())
}
fn common_keys(name: &str) -> Result<Vec<Key>, String> {
    let names: Vec<&str> = match name {
        "copy" => vec!["CTRL", "C"],
        "paste" => vec!["CTRL", "V"],
        "cut" => vec!["CTRL", "X"],
        "undo" => vec!["CTRL", "Z"],
        "redo" => vec!["CTRL", "Y"],
        "select_all" => vec!["CTRL", "A"],
        "volume_up" => vec!["VOLUMEUP"],
        "volume_down" => vec!["VOLUMEDOWN"],
        "volume_mute" => vec!["VOLUMEMUTE"],
        "media_play_pause" => vec!["MEDIAPLAYPAUSE"],
        "media_next" => vec!["MEDIANEXTTRACK"],
        "media_previous" => vec!["MEDIAPREVIOUSTRACK"],
        "browser_back" => vec!["BROWSERBACK"],
        "browser_forward" => vec!["BROWSERFORWARD"],
        // A fixed text (F5-1 H): never the rule's function name.
        _ => return Err("Common function has no adapter; choose a supported function".into()),
    };
    Ok(names.into_iter().map(Key::named).collect())
}
/// Run-scoped execution state: the match proof `rawinput::guard` checks and
/// the automatic-execution flag. Both are cleared on drop, so an early
/// return or unwinding never leaves a proof installed on the thread.
struct FenceLease;
impl FenceLease {
    fn install(proof: Option<rawinput::Proof>) -> Self {
        AUTOMATIC_EXECUTION.with(|automatic| automatic.set(proof.is_some()));
        rawinput::install(proof);
        FenceLease
    }
}
impl Drop for FenceLease {
    fn drop(&mut self) {
        rawinput::install(None);
        AUTOMATIC_EXECUTION.with(|automatic| automatic.set(false));
    }
}
// no-regression #6: every step re-checks pause/cancel/generation/focus/protection.
fn guarded_context(
    probe: &impl ForegroundProbe,
    s: &Shared,
    target: &Foreground,
    generation: u64,
) -> Result<(), String> {
    // B-01: a task fenced by the supervisor stops here, before any further
    // input, clipboard write or launch (also records task progress).
    super::executor::checkpoint()?;
    if pause_blocks_execution(
        s.paused.load(Ordering::SeqCst),
        AUTOMATIC_EXECUTION.with(|automatic| automatic.get()),
    ) {
        return Err("Engine paused; input stopped".into());
    }
    if s.cancel.load(Ordering::SeqCst) {
        return Err("Cancelled".into());
    }
    if s.generation.load(Ordering::SeqCst) != generation {
        return Err("User input changed during action; stopped safely".into());
    }
    let now = probe.foreground();
    if !probe.same_window(&now, target) {
        return Err("Foreground window/process changed; stopped safely".into());
    }
    rawinput::guard(s, probe)?;
    if let Some(error) = probe.protection() {
        return Err(error);
    }
    Ok(())
}
fn guarded(
    probe: &impl ForegroundProbe,
    s: &Shared,
    target: &Foreground,
    generation: u64,
) -> Result<(), String> {
    guarded_context(probe, s, target, generation)?;
    if physical(VK_CONTROL)
        || physical(VK_MENU)
        || physical(VK_SHIFT)
        || physical(VK_LWIN)
        || physical(VK_RWIN)
    {
        return Err("Physical modifier is held; input stopped".into());
    }
    Ok(())
}
fn pause_blocks_execution(paused: bool, automatic: bool) -> bool {
    paused && automatic
}
pub fn execute(
    s: Arc<Shared>,
    plan: ActionPlan,
    target: Foreground,
    generation: u64,
    erase: usize,
    terminator: Option<String>,
    speed: f64,
) -> Result<ExecutionResult, String> {
    // Manual run (no rule), recorded in the history (B-35). A run of the
    // command goes through its claim's `ManualTicket::execute` instead
    // (F5-1): with an executor, a run that names no claim is refused
    // without input, so this path only runs unsupervised before
    // `start_engine`.
    let process = target.process.clone();
    let core = super::executor::manual_core(&s);
    let run = manual_run(s, plan, target, generation, erase, terminator, speed);
    super::executor::run_manual(core, None, &process, run).map_err(|error| error.text)
}

/// A manual run's body (no rule), for [`execute`] and
/// `ManualTicket::execute`: automatic erasure needs observed text proof, so
/// a manual run never erases.
pub(super) fn manual_run(
    s: Arc<Shared>,
    plan: ActionPlan,
    target: Foreground,
    generation: u64,
    erase: usize,
    terminator: Option<String>,
    speed: f64,
) -> impl FnOnce() -> RunReport {
    move || {
        if erase != 0 {
            RunReport::text(Err("Automatic erasure requires observed text proof".into()))
        } else {
            execute_inner(s, plan, target, generation, None, terminator, speed)
        }
    }
}
pub(super) fn execute_inner(
    s: Arc<Shared>,
    plan: ActionPlan,
    target: Foreground,
    generation: u64,
    proof: Option<CandidateProof>,
    terminator: Option<String>,
    speed: f64,
) -> RunReport {
    run_plan(
        &Ports {
            // Manual output takes the executor's send lock too (S1), behind
            // the supervised run's epoch fence (F5-1); its clipboard lease
            // takes the run's clipboard claim.
            injector: &super::executor::serialized(&WinInjector),
            foreground: &WinForeground,
            clock: &SystemClock,
            clipboard: || super::executor::manual_clipboard(WinClipboard::new),
        },
        &s,
        plan,
        target,
        generation,
        proof,
        terminator,
        speed,
    )
}
/// Ports one action run uses (S-23): production wires the Windows adapters,
/// pipeline tests wire `crate::mock`.
pub(super) struct Ports<'a, I, F, K, B> {
    pub(super) injector: &'a I,
    pub(super) foreground: &'a F,
    pub(super) clock: &'a K,
    /// Opens the backend for a clipboard lease (paste plans only).
    pub(super) clipboard: B,
}
/// Why a run stopped: the message its result carries and, when the
/// clipboard lease stopped it, the lease's reason and the formats behind a
/// refused backup ([`LeaseError`]), passed on as they are. Every other stop
/// is text only, which the executor maps (`REASONS`).
pub(super) struct Stop {
    reason: Option<ReasonCode>,
    message: String,
    formats: Vec<u32>,
}
impl From<String> for Stop {
    fn from(message: String) -> Self {
        Self {
            reason: None,
            message,
            formats: vec![],
        }
    }
}
impl From<&str> for Stop {
    fn from(message: &str) -> Self {
        Self::from(message.to_owned())
    }
}
impl From<LeaseError> for Stop {
    fn from(error: LeaseError) -> Self {
        Self {
            reason: Some(error.reason),
            message: error.message,
            formats: error.formats,
        }
    }
}
/// A run's result with the lease's reason when the lease stopped it.
type Ran = (ExecutionResult, Option<ReasonCode>);
/// Single execution entry for production and pipeline tests: installs the
/// match proof for the stamp guard (uninstalled on drop), then runs optional
/// trigger erasure, output steps, owned-input cleanup, terminator and
/// clipboard restore. The report carries the clipboard lease's reason when
/// the lease stopped the run, and the formats its backup skipped, or
/// refused, for the history entry (B-08, B-35).
#[allow(
    clippy::too_many_arguments,
    reason = "execute_inner's parameters plus the port bundle"
)]
pub(super) fn run_plan<I, F, K, C, B>(
    ports: &Ports<'_, I, F, K, B>,
    s: &Shared,
    plan: ActionPlan,
    target: Foreground,
    generation: u64,
    proof: Option<CandidateProof>,
    terminator: Option<String>,
    speed: f64,
) -> RunReport
where
    I: Injector,
    F: ForegroundProbe,
    K: Clock,
    C: ClipboardBackend,
    B: Fn() -> Result<C, String>,
{
    let mut skipped_formats = vec![];
    let ran = run_steps(
        ports,
        s,
        plan,
        target,
        generation,
        proof,
        terminator,
        speed,
        &mut skipped_formats,
    );
    let (result, reason) = match ran {
        Ok((result, reason)) => (Ok(result), reason),
        Err(stop) => {
            // A refused backup opened no lease: its formats are the only ones.
            skipped_formats.extend(stop.formats);
            (Err(stop.message), stop.reason)
        }
    };
    RunReport {
        result,
        reason,
        skipped_formats,
    }
}
/// [`run_plan`]'s result alone, for the pipeline tests that compare it.
#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    reason = "execute_inner's parameters plus the port bundle"
)]
pub(super) fn execute_with_proof<I, F, K, C, B>(
    ports: &Ports<'_, I, F, K, B>,
    s: &Shared,
    plan: ActionPlan,
    target: Foreground,
    generation: u64,
    proof: Option<CandidateProof>,
    terminator: Option<String>,
    speed: f64,
) -> Result<ExecutionResult, String>
where
    I: Injector,
    F: ForegroundProbe,
    K: Clock,
    C: ClipboardBackend,
    B: Fn() -> Result<C, String>,
{
    run_plan(ports, s, plan, target, generation, proof, terminator, speed).result
}
/// [`run_plan`]'s body. `skipped` receives the formats the lease's backup
/// skipped as soon as the lease is open.
#[allow(
    clippy::too_many_arguments,
    reason = "run_plan's parameters plus the skipped formats it reports"
)]
fn run_steps<I, F, K, C, B>(
    ports: &Ports<'_, I, F, K, B>,
    s: &Shared,
    plan: ActionPlan,
    mut target: Foreground,
    generation: u64,
    proof: Option<CandidateProof>,
    terminator: Option<String>,
    speed: f64,
    skipped: &mut Vec<u32>,
) -> Result<Ran, Stop>
where
    I: Injector,
    F: ForegroundProbe,
    K: Clock,
    C: ClipboardBackend,
    B: Fn() -> Result<C, String>,
{
    let (injector, probe, clock) = (ports.injector, ports.foreground, ports.clock);
    let proof = proof.map(|proof| match proof {
        CandidateProof::Espanso(proof) => proof,
    });
    let erasure = proof.as_ref().map(rawinput::Proof::erasure);
    SENT_EFFECT.with(|flag| flag.set(false));
    let _fence_lease = FenceLease::install(proof);
    set_stage(Stage::Preflight, clock.now());
    if !speed.is_finite() || speed <= 0.0 {
        return Err("Replay speed must be positive and finite".into());
    }
    preflight(&plan)?;
    set_stage(Stage::Guard, clock.now());
    // A shifted or AltGr trigger can match before the modifier is released.
    // Wait for release while keeping every cancellation and focus check active.
    if erasure.is_some() {
        let wait = clock.now();
        while modifiers_held()
            && clock.now().saturating_duration_since(wait) < Duration::from_secs(1)
        {
            guarded_context(probe, s, &target, generation)?;
            clock.sleep(Duration::from_millis(5));
        }
    }
    guarded(probe, s, &target, generation)?;
    // All potentially failing source/format checks happen before trigger deletion.
    set_stage(Stage::Clipboard, clock.now());
    let mut clipboard = acquire_clipboard(&plan, || -> Result<_, Stop> {
        Ok(ClipboardLease::new((ports.clipboard)()?)?)
    })?;
    let mut payloads = std::collections::BTreeMap::new();
    if let Some(lease) = clipboard.as_ref() {
        skipped.extend_from_slice(lease.skipped_formats());
        for (index, step) in plan.steps.iter().enumerate() {
            if let Some(formats) = clipboard_formats(&step.operation)? {
                payloads.insert(index, lease.prepare(formats)?);
            }
        }
    }
    guarded(probe, s, &target, generation)?;
    if let Some(lease) = clipboard.as_mut() {
        // Plan §5.4 principle 4, "resolve first, then delete": a paste no
        // earlier step can disturb is written and verified, baseline
        // included, before the trigger is erased, so a refused write leaves
        // the trigger in place. Its step then only re-verifies. Written but
        // not erased, the lease's drop restores the clipboard. Without a
        // trigger to erase, the write stays at its step.
        let early = written_before_erase(&plan)
            .filter(|_| erasure.is_some())
            .and_then(|index| payloads.remove(&index))
            .map(|mut prepared| -> Result<(), Stop> {
                lease.write_prepared(&mut prepared)?;
                lease.verify_current()?;
                Ok(guarded(probe, s, &target, generation)?)
            });
        match early {
            Some(Err(stop)) => {
                let result = ExecutionResult {
                    id: "erase".into(),
                    status: "failed".into(),
                    message: stop.message,
                    completed_steps: 0,
                    side_effects: SENT_EFFECT.with(|flag| flag.get()),
                };
                return Ok((result, stop.reason));
            }
            Some(Ok(())) => {}
            None => lease.verify_current()?,
        }
    }
    if let Some(erasure) = &erasure {
        set_stage(Stage::Erase, clock.now());
        let erased = erase(injector, clock, erasure, || {
            guarded(probe, s, &target, generation)
        });
        if let Err(message) = erased {
            let result = ExecutionResult {
                id: "erase".into(),
                status: "failed".into(),
                message,
                completed_steps: 0,
                side_effects: SENT_EFFECT.with(|flag| flag.get()),
            };
            return Ok((result, None));
        }
    }
    set_stage(Stage::Output, clock.now());
    let mut owned = BTreeSet::new();
    let mut owned_mouse = vec![];
    let mut completed = 0;
    let started = clock.now();
    let mut failure: Option<Stop> = None;
    for step in &plan.steps {
        if clock.now().saturating_duration_since(started) > Duration::from_secs(120) {
            failure = Some("Execution timeout".into());
            break;
        }
        if let TargetPolicy::Declared { process } = &step.target {
            let now = probe.foreground();
            if !now.process.eq_ignore_ascii_case(process) {
                failure = Some("Declared next target is not foreground".into());
                break;
            }
            if erasure.is_some() && !probe.same_window(&now, &target) {
                failure = Some(
                    "Automatic shortcut action changed its declared target; stopped safely".into(),
                );
                break;
            }
            target = now;
        }
        if let Err(e) = guarded(probe, s, &target, generation) {
            failure = Some(e.into());
            break;
        }
        // The match proof's focus/caret stamp protects the first output only:
        // once we send, our own keys (Enter, Tab) move focus and caret, and a
        // stale stamp would stop every later step. Window, generation, pause,
        // cancel and modifier checks still run before each step.
        rawinput::install(None);
        let result = (|| -> Result<(), Stop> {
            match &step.operation {
                Operation::Text {
                    text,
                    method: OutputMethod::Keys,
                } => {
                    unicode(injector, text, || guarded(probe, s, &target, generation))?;
                }
                Operation::Text {
                    method: OutputMethod::Paste,
                    ..
                }
                | Operation::Html { .. }
                | Operation::PasteFiles { .. }
                | Operation::PasteImage { .. } => {
                    if let Some(prepared) = payloads.get_mut(&completed) {
                        guarded(probe, s, &target, generation)?;
                        clipboard.as_mut().unwrap().write_prepared(prepared)?;
                    }
                    // Clipboard preparation/retry may take time: revalidate
                    // immediately before submitting the paste chord.
                    clipboard.as_ref().unwrap().verify_current()?;
                    guarded(probe, s, &target, generation)?;
                    hotkey(injector, &[Key::named("CTRL"), Key::named("V")])?;
                    clock.sleep(clipboard.as_ref().unwrap().restore_delay());
                }
                Operation::KeyDown { key: k } => {
                    let v = vk(k)?;
                    if physical(v) {
                        return Err("Requested key is physically held".into());
                    }
                    key(injector, v, false)?;
                    owned.insert(v);
                }
                Operation::KeyUp { key: k } => {
                    let v = vk(k)?;
                    release_owned(&mut owned, v, physical(v), || key(injector, v, true))?;
                }
                Operation::KeyPress { key: k } => {
                    let v = vk(k)?;
                    if physical(v) {
                        return Err("Requested key is physically held".into());
                    }
                    press(injector, v)?;
                }
                Operation::Hotkey { keys } => hotkey(injector, keys)?,

                Operation::Delay { ms } => {
                    let until = clock.now() + Duration::from_millis(*ms);
                    while clock.now() < until {
                        if clock.now().saturating_duration_since(started) > Duration::from_secs(120)
                        {
                            return Err("Execution timeout".into());
                        }
                        guarded(probe, s, &target, generation)?;
                        clock.sleep(Duration::from_millis(10));
                    }
                }
                Operation::MouseMove {
                    x,
                    y,
                    relative,
                    percent,
                } => {
                    if *relative {
                        mouse_input(injector, MOUSEEVENTF_MOVE, 0, *x as i32, *y as i32)?
                    } else {
                        unsafe {
                            let (left, top, w, h) = (
                                GetSystemMetrics(SM_XVIRTUALSCREEN),
                                GetSystemMetrics(SM_YVIRTUALSCREEN),
                                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                                GetSystemMetrics(SM_CYVIRTUALSCREEN),
                            );
                            let (px, py) = if *percent {
                                (
                                    *x * (GetSystemMetrics(SM_CXSCREEN) - 1) as f64 / 100.0,
                                    *y * (GetSystemMetrics(SM_CYSCREEN) - 1) as f64 / 100.0,
                                )
                            } else {
                                (*x, *y)
                            };
                            mouse_input(
                                injector,
                                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                                0,
                                ((px - left as f64) * 65535.0 / (w - 1) as f64) as i32,
                                ((py - top as f64) * 65535.0 / (h - 1) as f64) as i32,
                            )?
                        }
                    }
                }
                Operation::MouseButton { button: b, state } => {
                    let physical_key = match b {
                        MouseButton::Left => VK_LBUTTON,
                        MouseButton::Right => VK_RBUTTON,
                        MouseButton::Middle => VK_MBUTTON,
                        MouseButton::X1 => VK_XBUTTON1,
                        MouseButton::X2 => VK_XBUTTON2,
                    };
                    if physical(physical_key) {
                        return Err("Physical mouse button conflicts with action".into());
                    }
                    button(injector, clock, *b, *state)?;
                    if *state == ButtonState::Down {
                        owned_mouse.push(*b)
                    }
                    if *state == ButtonState::Up {
                        owned_mouse.retain(|x| x != b)
                    }
                }
                Operation::Wheel { axis, amount, unit } => mouse_input(
                    injector,
                    if *axis == WheelAxis::Vertical {
                        MOUSEEVENTF_WHEEL
                    } else {
                        MOUSEEVENTF_HWHEEL
                    },
                    if *unit == WheelUnit::Clicks {
                        amount.saturating_mul(120)
                    } else {
                        *amount
                    } as u32,
                    0,
                    0,
                )?,
                Operation::Open {
                    target,
                    args,
                    working_directory,
                } => open(target, args, working_directory)?,
                Operation::Common { function } => {
                    hotkey(injector, &common_keys(function)?)?;
                }
                Operation::Reusable { .. } => return Err("Unregistered reusable action".into()),
            }
            Ok(())
        })();
        match result {
            Ok(()) => completed += 1,
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    set_stage(Stage::Cleanup, clock.now());
    for v in owned {
        if !physical(v) {
            if let Err(error) = key(injector, v, true) {
                failure.get_or_insert_with(|| format!("Owned key cleanup failed: {error}").into());
            }
        }
    }
    for b in owned_mouse {
        let v = match b {
            MouseButton::Left => VK_LBUTTON,
            MouseButton::Right => VK_RBUTTON,
            MouseButton::Middle => VK_MBUTTON,
            MouseButton::X1 => VK_XBUTTON1,
            MouseButton::X2 => VK_XBUTTON2,
        };
        if !physical(v) {
            if let Err(error) = button(injector, clock, b, ButtonState::Up) {
                failure.get_or_insert_with(|| {
                    format!("Owned mouse button cleanup failed: {error}").into()
                });
            }
        }
    }
    if failure.is_none() {
        if let Some(t) = terminator {
            if let Err(error) =
                guarded(probe, s, &target, generation).and_then(|_| send_terminator(injector, &t))
            {
                failure = Some(error.into());
            }
        }
    }
    if let Some(mut lease) = clipboard {
        set_stage(Stage::Restore, clock.now());
        if let Err(e) = lease.restore() {
            failure = Some(Stop {
                reason: Some(e.reason),
                message: format!("Clipboard restore failed after input: {}", e.message),
                formats: vec![],
            });
        }
    }
    set_stage(Stage::Done, clock.now());
    let reason = failure.as_ref().and_then(|stop| stop.reason);
    let result = ExecutionResult {
        id: format!("run-{}", s.generation.load(Ordering::SeqCst)),
        status: if failure.as_ref().map(|stop| stop.message.as_str()) == Some("Cancelled") {
            "cancelled"
        } else if failure.is_some() {
            "failed"
        } else {
            "unknown"
        }
        .into(),
        message: failure.map_or_else(
            || "Input was sent; target consumption cannot be confirmed".into(),
            |stop| stop.message,
        ),
        completed_steps: completed,
        side_effects: SENT_EFFECT.with(|flag| flag.get()) || completed > 0,
    };
    // An executor task, or a supervised manual run, publishes its result
    // when it settles and only while it still owns the run (B-01, F5-1);
    // unsupervised manual runs publish here.
    if !super::executor::publishes_on_settle() {
        *s.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result.clone());
    }
    Ok((result, reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pause_blocks_only_automatic_actions() {
        assert!(pause_blocks_execution(true, true));
        assert!(!pause_blocks_execution(true, false));
        assert!(!pause_blocks_execution(false, true));
    }
    #[test]
    fn missing_file_rejected_before_executor() {
        let plan = ActionPlan {
            steps: vec![Step::new(
                Operation::PasteFiles {
                    paths: vec!["C:\\nonexistent-quicktext-fixture\\missing.txt".into()],
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        };
        assert!(preflight(&plan).is_err());
    }
    #[test]
    fn every_windows_key_name_the_parser_accepts_has_a_code() {
        for name in [
            "Enter",
            "PRIOR",
            "NEXT",
            "APPS",
            "RWIN",
            "SHIFTKEY",
            "LCONTROLKEY",
            "ADD",
            "DECIMAL",
            "NUMPAD0",
            "NUMPAD9",
            "OEMPLUS",
            "OEMBACKSLASH",
            "BROWSERHOME",
            "SCROLL",
            "BREAK",
        ] {
            assert!(quicker_domain::parse_key(name).is_ok(), "{name}");
            assert!(vk(&Key::named(name)).is_ok(), "{name}");
        }
        assert_eq!(vk(&Key::named("NUMPAD7")), Ok(0x67));
    }
    #[test]
    fn repeated_enter_presses_pass_preflight() {
        let plan = quicker_domain::parse_sendkeys("ab{Enter 3}~").unwrap();
        assert_eq!(plan.steps.len(), 5);
        assert!(preflight(&plan).is_ok());
    }
    #[test]
    fn unknown_native_key_fails_preflight() {
        let plan = ActionPlan {
            steps: vec![Step::new(
                Operation::KeyPress {
                    key: Key::named("CMD"),
                },
                SourceSpan::default(),
            )],
            ..Default::default()
        };
        assert!(preflight(&plan).is_err());
    }
}
