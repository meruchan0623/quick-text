#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use quicker_domain::*;
use quicker_platform::{ExecutionResult, Shared};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc, Mutex},
};
use tauri::{Emitter, Manager, State};

struct Store {
    dir: PathBuf,
    draft: Config,
    active: Config,
    previous: Option<Config>,
    disk_hash: Option<Vec<u8>>,
    engine: Arc<Shared>,
    unresolved: Vec<MigrationItem>,
    active_unresolved: Vec<MigrationItem>,
    previous_unresolved: Vec<MigrationItem>,
    pending_import: Option<(Config, Vec<MigrationItem>)>,
}
#[derive(Clone, Serialize)]
struct AppState {
    draft: Config,
    active: Config,
    revision: u64,
    dirty: bool,
    paused: bool,
    engine_status: String,
    last_result: Option<ExecutionResult>,
    capabilities: Vec<quicker_platform::Capability>,
    autostart: bool,
    engine_detail: serde_json::Value,
    platform: String,
}
impl Store {
    fn state(&self) -> AppState {
        AppState {
            draft: self.draft.clone(),
            active: self.active.clone(),
            revision: self.active.revision,
            dirty: self.draft != self.active,
            paused: self.engine.paused.load(Ordering::SeqCst),
            engine_status: if self.engine.busy.load(Ordering::SeqCst) {
                "running"
            } else if self.engine.paused.load(Ordering::SeqCst) {
                "paused"
            } else {
                "listening"
            }
            .into(),
            last_result: self.engine.last.lock().unwrap().clone(),
            capabilities: quicker_platform::capabilities(),
            autostart: autostart_enabled(),
            engine_detail: quicker_platform::input_status(),
            platform: if cfg!(target_os = "macos") {
                "macos"
            } else {
                "windows"
            }
            .into(),
        }
    }
    fn load(dir: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join("active.json");
        let active = if path.exists() {
            read_config(&path)?
        } else {
            Config::default()
        };
        compile_config(&active).map_err(diagnostics)?;
        let draft = if dir.join("draft.json").exists() {
            read_config(&dir.join("draft.json"))?
        } else {
            active.clone()
        };
        let historical = dir
            .join("revisions")
            .join(format!("{}.json", active.revision.saturating_sub(1)));
        let previous = if active.revision > 0 && historical.exists() {
            Some(read_config(&historical)?)
        } else if dir.join("previous.json").exists() {
            Some(read_config(&dir.join("previous.json"))?)
        } else {
            None
        };
        let engine = Shared::new();
        *engine.config.lock().unwrap() = active.clone();
        *engine.compiled.lock().unwrap() = compile_config(&active).map_err(diagnostics)?;
        let disk_hash = hash_file(&path)?;
        let unresolved = read_metadata(&dir.join("migration-unresolved.json"));
        let active_metadata_path = dir
            .join("revisions")
            .join(format!("{}.metadata.json", active.revision));
        let active_unresolved = if active_metadata_path.exists() {
            read_metadata(&active_metadata_path)
        } else if dir.join("active-migration-unresolved.json").exists() {
            read_metadata(&dir.join("active-migration-unresolved.json"))
        } else {
            metadata_for_config(&active, &unresolved)
        };
        // Persist the legacy fallback before a later draft import can replace it.
        fs::create_dir_all(dir.join("revisions")).map_err(|e| e.to_string())?;
        atomic_json(&active_metadata_path, &active_unresolved)?;
        let previous_unresolved = previous
            .as_ref()
            .map(|config| {
                let path = dir
                    .join("revisions")
                    .join(format!("{}.metadata.json", config.revision));
                if path.exists() {
                    read_metadata(&path)
                } else {
                    metadata_for_config(config, &unresolved)
                }
            })
            .unwrap_or_default();
        Ok(Self {
            dir,
            draft,
            active,
            previous,
            disk_hash,
            engine,
            unresolved,
            active_unresolved,
            previous_unresolved,
            pending_import: None,
        })
    }
    fn publish(&mut self, config: Config) -> Result<(), String> {
        let metadata = metadata_for_config(&config, &self.unresolved);
        self.publish_with_metadata(config, metadata)
    }
    fn publish_with_metadata(
        &mut self,
        mut config: Config,
        metadata: Vec<MigrationItem>,
    ) -> Result<(), String> {
        config.revision = self
            .active
            .revision
            .checked_add(1)
            .ok_or("Revision counter exhausted; preserve data and start a new profile")?;
        let compiled = compile_config(&config).map_err(diagnostics)?;
        if hash_file(&self.dir.join("active.json"))? != self.disk_hash {
            return Err("Active configuration was edited externally; reload or resolve the conflict before applying".into());
        }
        let was_paused = self.engine.paused.swap(true, Ordering::SeqCst);
        self.engine.cancel.store(true, Ordering::SeqCst);
        self.engine.generation.fetch_add(1, Ordering::SeqCst);
        if self.engine.busy.load(Ordering::SeqCst) {
            self.engine.paused.store(was_paused, Ordering::SeqCst);
            return Err("An action is still running; cancel it and apply again".into());
        }
        config.revision = match self.active.revision.checked_add(1) {
            Some(revision) => revision,
            None => {
                self.engine.paused.store(was_paused, Ordering::SeqCst);
                return Err(
                    "Revision counter exhausted; preserve data and start a new profile".into(),
                );
            }
        };
        let old = self.active.clone();
        let outcome = (|| {
            let revisions = self.dir.join("revisions");
            fs::create_dir_all(&revisions).map_err(|e| e.to_string())?;
            let backup = revisions.join(format!("{}.json", old.revision));
            if backup.exists() {
                if read_config(&backup)? != old {
                    return Err("Historical revision conflict; existing snapshot preserved".into());
                }
            } else {
                atomic_json(&backup, &old)?;
            }
            atomic_json(
                &revisions.join(format!("{}.metadata.json", old.revision)),
                &self.active_unresolved,
            )?;
            atomic_json(&self.dir.join("draft.json"), &config)?;
            atomic_json(
                &revisions.join(format!("{}.metadata.json", config.revision)),
                &metadata,
            )?;
            atomic_json(&self.dir.join("migration-unresolved.json"), &metadata)?;
            atomic_json(
                &self.dir.join("active-migration-unresolved.json"),
                &metadata,
            )?;
            atomic_json(&self.dir.join("active.json"), &config)?;
            Ok::<_, String>(())
        })();
        if let Err(e) = outcome {
            self.engine.paused.store(was_paused, Ordering::SeqCst);
            return Err(e);
        }
        *self.engine.compiled.lock().unwrap() = compiled;
        *self.engine.config.lock().unwrap() = config.clone();
        self.active = config.clone();
        self.previous = Some(old);
        self.previous_unresolved = self.active_unresolved.clone();
        self.active_unresolved = metadata.clone();
        self.unresolved = metadata;
        self.pending_import = None;
        self.draft = config;
        self.disk_hash = Some(
            Sha256::digest(
                serde_json::to_vec_pretty(&self.active).expect("validated config serialization"),
            )
            .to_vec(),
        );
        self.engine.paused.store(was_paused, Ordering::SeqCst);
        Ok(())
    }
    fn save(&mut self, config: Config) -> Result<(), String> {
        let metadata = self
            .pending_import
            .as_ref()
            .filter(|(preview, _)| preview == &config)
            .map(|(_, metadata)| metadata.clone())
            .unwrap_or_else(|| metadata_for_config(&config, &self.unresolved));
        atomic_json(&self.dir.join("draft.json"), &config)?;
        atomic_json(&self.dir.join("migration-unresolved.json"), &metadata)?;
        self.unresolved = metadata;
        self.pending_import = None;
        self.draft = config;
        Ok(())
    }
    fn discard(&mut self) -> Result<(), String> {
        atomic_json(&self.dir.join("draft.json"), &self.active)?;
        atomic_json(
            &self.dir.join("migration-unresolved.json"),
            &self.active_unresolved,
        )?;
        self.draft = self.active.clone();
        self.unresolved = self.active_unresolved.clone();
        self.pending_import = None;
        Ok(())
    }
    fn rollback_previous(&mut self) -> Result<(), String> {
        let config = self.previous.clone().ok_or("No previous revision")?;
        self.publish_with_metadata(config, self.previous_unresolved.clone())
    }
}
fn read_metadata(path: &Path) -> Vec<MigrationItem> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}
fn metadata_for_config(config: &Config, metadata: &[MigrationItem]) -> Vec<MigrationItem> {
    metadata
        .iter()
        .filter(|item| config.rules.iter().any(|rule| rule.id == item.rule_id))
        .cloned()
        .collect()
}
fn diagnostics(e: Vec<Diagnostic>) -> String {
    serde_json::to_string(&e).unwrap_or_else(|_| "Validation failed".into())
}
fn read_config(path: &Path) -> Result<Config, String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Configuration read failed: {e}"))
}
fn hash_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(
        Sha256::digest(fs::read(path).map_err(|e| e.to_string())?).to_vec(),
    ))
}
fn atomic_json<T: Serialize + ?Sized>(path: &Path, config: &T) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?;
    use std::io::Write;
    let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    f.write_all(&bytes).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let from: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            if windows_replace(from.as_ptr(), to.as_ptr()) == 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
    }
    #[cfg(not(windows))]
    fs::rename(tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "MoveFileExW"]
    fn move_file_ex(from: *const u16, to: *const u16, flags: u32) -> i32;
}
#[cfg(windows)]
unsafe fn windows_replace(from: *const u16, to: *const u16) -> i32 {
    move_file_ex(from, to, 1 | 8)
}
type Managed = Mutex<Store>;
#[tauri::command]
fn load_state(store: State<Managed>) -> Result<AppState, String> {
    Ok(store.lock().map_err(|_| "Store lock")?.state())
}
#[tauri::command]
fn save_draft(config: Config, store: State<Managed>) -> Result<AppState, String> {
    let mut s = store.lock().map_err(|_| "Store lock")?;
    s.save(config)?;
    Ok(s.state())
}
#[tauri::command]
fn apply_settings(store: State<Managed>) -> Result<AppState, String> {
    let mut s = store.lock().map_err(|_| "Store lock")?;
    let c = s.draft.clone();
    s.publish(c)?;
    Ok(s.state())
}
#[tauri::command]
fn discard_draft(store: State<Managed>) -> Result<AppState, String> {
    let mut s = store.lock().map_err(|_| "Store lock")?;
    s.discard()?;
    Ok(s.state())
}
#[tauri::command]
fn rollback(store: State<Managed>) -> Result<AppState, String> {
    let mut s = store.lock().map_err(|_| "Store lock")?;
    s.rollback_previous()?;
    Ok(s.state())
}
#[tauri::command]
fn set_paused(paused: bool, store: State<Managed>) -> Result<AppState, String> {
    let s = store.lock().map_err(|_| "Store lock")?;
    if !paused {
        if s.engine.recording.lock().unwrap().is_some() {
            return Err("Stop recording before resuming automatic matching".into());
        }
        quicker_platform::native_ready()?;
    }
    if !paused && !quicker_platform::engine_ready() {
        return Err("This platform input adapter is not ready; engine remains paused".into());
    }
    s.engine.paused.store(paused, Ordering::SeqCst);
    s.engine.generation.fetch_add(1, Ordering::SeqCst);
    if paused {
        s.engine.cancel.store(true, Ordering::SeqCst)
    }
    Ok(s.state())
}
#[derive(Serialize)]
struct Validation {
    valid: bool,
    errors: Vec<Diagnostic>,
}
#[tauri::command]
fn validate_config(config: Config) -> Validation {
    let errors = compile_config(&config).err().unwrap_or_default();
    Validation {
        valid: errors.is_empty(),
        errors,
    }
}
#[tauri::command]
fn test_rule(
    rule: Rule,
    input: String,
    store: State<Managed>,
) -> Result<serde_json::Value, String> {
    let globals = store
        .lock()
        .map_err(|_| "Store lock")?
        .draft
        .globals
        .clone();
    let terminator = rule
        .terminator
        .as_ref()
        .map(|t| &t.keys)
        .unwrap_or(&globals.terminators)
        .first()
        .cloned();
    let config = Config {
        rules: vec![rule.clone()],
        globals,
        ..Default::default()
    };
    let compiled = compile_config(&config).map_err(diagnostics)?;
    let ctx = ForegroundContext::default();
    let matched = compiled.match_input(
        &input,
        if rule.trigger_mode == TriggerMode::Immediate {
            None
        } else {
            terminator.as_deref()
        },
        &ctx,
    );
    serde_json::to_value(matched).map_err(|e| e.to_string())
}
#[tauri::command]
fn parse_source(dialect: Dialect, source: String) -> serde_json::Value {
    match parse_dialect(dialect, &source) {
        Ok(plan) => serde_json::json!({"plan":plan,"diagnostics":[]}),
        Err(errors) => serde_json::json!({"plan":null,"diagnostics":errors}),
    }
}
#[tauri::command]
fn cancel_execution(store: State<Managed>) -> bool {
    let s = store.lock().unwrap();
    s.engine.cancel.store(true, Ordering::SeqCst);
    true
}
#[tauri::command]
async fn execute_action(action: Action, app: tauri::AppHandle) -> Result<ExecutionResult, String> {
    let (engine, config, action, speed) = {
        let state = app.state::<Managed>();
        let s = state.lock().unwrap();
        if s.engine.busy.swap(true, Ordering::SeqCst) {
            return Err("An action is already running".into());
        }
        s.engine.cancel.store(false, Ordering::SeqCst);
        (
            s.engine.clone(),
            s.active.clone(),
            action.clone(),
            match &action {
                Action::Replay { speed, .. } => *speed,
                _ => 1.0,
            },
        )
    };
    let window = app.get_webview_window("main");
    if let Some(w) = &window {
        if let Err(e) = w.hide() {
            engine.busy.store(false, Ordering::SeqCst);
            return Err(e.to_string());
        }
    }
    let e = engine.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let target = quicker_platform::foreground();
        let generation = e.generation.load(Ordering::SeqCst);
        let mut context = VariableContext::default();
        context.clipboard = arboard::Clipboard::new()
            .ok()
            .and_then(|mut cb| cb.get_text().ok());
        let needs_selection = serde_json::to_string(&action)
            .unwrap_or_default()
            .contains("{selection}")
            || matches!(action, Action::Reusable { .. })
                && serde_json::to_string(&config)
                    .unwrap_or_default()
                    .contains("{selection}");
        if needs_selection {
            context.selection = quicker_platform::capture_selection().ok();
        }
        let plan = expand_reusable(&action, &config, &context).map_err(diagnostics)?;
        quicker_platform::execute(e, plan, target, generation, 0, None, speed)
    })
    .await;
    engine.busy.store(false, Ordering::SeqCst);
    if let Some(w) = window {
        let _ = w.show();
    }
    result.map_err(|e| e.to_string())?
}
#[tauri::command]
async fn get_foreground(app: tauri::AppHandle) -> Result<quicker_platform::Foreground, String> {
    if let Some(w) = app.get_webview_window("main") {
        w.hide().map_err(|e| e.to_string())?;
    }
    let result = tauri::async_runtime::spawn_blocking(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        quicker_platform::foreground()
    })
    .await
    .map_err(|e| e.to_string())?;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
    Ok(result)
}
#[tauri::command]
fn choose_file(kind: String) -> Option<Vec<String>> {
    let dialog = rfd::FileDialog::new();
    if kind == "files" {
        dialog.pick_files().map(|p| {
            p.into_iter()
                .map(|x| x.to_string_lossy().into_owned())
                .collect()
        })
    } else {
        dialog
            .pick_file()
            .map(|p| vec![p.to_string_lossy().into_owned()])
    }
}
#[tauri::command]
fn export_config(path: Option<String>, store: State<Managed>) -> Result<String, String> {
    let s = store.lock().map_err(|_| "Store lock")?;
    let target = path
        .map(PathBuf::from)
        .or_else(|| {
            rfd::FileDialog::new()
                .set_file_name("quicker-text-package.json")
                .save_file()
        })
        .ok_or("Export cancelled")?;
    let unresolved = s
        .unresolved
        .iter()
        .filter(|x| s.draft.rules.iter().any(|r| r.id == x.rule_id))
        .cloned()
        .collect();
    let package = export_package_with_unresolved(&s.draft, unresolved).map_err(diagnostics)?;
    fs::write(&target, package).map_err(|e| e.to_string())?;
    Ok(target.to_string_lossy().into_owned())
}
#[tauri::command]
fn import_config(
    path: String,
    strategy: String,
    store: State<Managed>,
) -> Result<serde_json::Value, String> {
    let source = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let imported = import_package(&source).map_err(diagnostics)?;
    let mut incoming = imported.config;
    let mut s = store.lock().map_err(|_| "Store lock")?;
    let mut draft = s.draft.clone();
    let mut report: Vec<String> = imported
        .items
        .iter()
        .map(|x| {
            format!(
                "{}: {:?} {}",
                x.rule_id,
                x.status,
                x.diagnostics
                    .iter()
                    .map(|d| d.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })
        .collect();
    for r in &mut incoming.rules {
        if matches!(
            r.action,
            Action::Quicker { .. }
                | Action::Reusable { .. }
                | Action::Open { .. }
                | Action::Steps { .. }
                | Action::Replay { .. }
                | Action::SendKeys { .. }
        ) {
            r.enabled = false;
            report.push(format!(
                "{}: imported side-effect action disabled pending review",
                r.id
            ));
        }
    }
    let conflicts: Vec<_> = incoming
        .rules
        .iter()
        .filter(|r| draft.rules.iter().any(|x| x.id == r.id))
        .map(|r| r.id.clone())
        .collect();
    report.push(format!(
        "{} conflicts: {}; strategy: {}",
        conflicts.len(),
        conflicts.join(", "),
        strategy
    ));
    if strategy == "keep_both" {
        draft = merge_import(
            &draft,
            &ImportReport {
                config: incoming.clone(),
                items: vec![],
                unresolved: vec![],
            },
            ConflictStrategy::KeepBoth,
        )
        .map_err(diagnostics)?;
    } else if strategy == "replace" {
        incoming.revision = s.active.revision;
        draft = incoming.clone();
    } else {
        for r in incoming.rules.iter().cloned() {
            if let Some(i) = draft.rules.iter().position(|x| x.id == r.id) {
                if strategy == "overwrite" {
                    draft.rules[i] = r
                } else {
                    report.push(format!("{}: conflicting id skipped", r.id));
                }
            } else {
                draft.rules.push(r)
            }
        }
    }
    let unresolved = merge_import_metadata(
        &strategy,
        &s.draft,
        &incoming,
        &draft,
        &s.unresolved,
        &imported.unresolved,
    );
    s.pending_import = Some((draft.clone(), unresolved));
    Ok(serde_json::json!({"config":draft,"report":report}))
}

/// Keep each original attached to the rule actually accepted by the import.
fn merge_import_metadata(
    strategy: &str,
    existing: &Config,
    incoming: &Config,
    merged: &Config,
    old_metadata: &[MigrationItem],
    incoming_metadata: &[MigrationItem],
) -> Vec<MigrationItem> {
    use std::collections::{BTreeMap, BTreeSet};
    let existing_ids: BTreeSet<_> = existing.rules.iter().map(|r| r.id.as_str()).collect();
    let incoming_ids: BTreeSet<_> = incoming.rules.iter().map(|r| r.id.as_str()).collect();
    let final_ids: BTreeSet<_> = merged.rules.iter().map(|r| r.id.as_str()).collect();
    let mut metadata = BTreeMap::new();
    if strategy != "replace" {
        for item in old_metadata {
            if existing_ids.contains(item.rule_id.as_str())
                && !(strategy == "overwrite" && incoming_ids.contains(item.rule_id.as_str()))
            {
                metadata.insert(item.rule_id.clone(), item.clone());
            }
        }
    }
    // KeepBoth appends every incoming rule in its original order. Using the
    // actual appended IDs also handles pre-existing and incoming suffix clashes.
    let appended_ids: BTreeMap<_, _> = if strategy == "keep_both" {
        incoming
            .rules
            .iter()
            .zip(merged.rules.iter().skip(existing.rules.len()))
            .map(|(original, appended)| (original.id.as_str(), appended.id.as_str()))
            .collect()
    } else {
        BTreeMap::new()
    };
    for item in incoming_metadata {
        if !incoming_ids.contains(item.rule_id.as_str()) {
            continue;
        }
        let mut item = item.clone();
        if strategy == "keep_both" {
            let Some(id) = appended_ids.get(item.rule_id.as_str()) else {
                continue;
            };
            item.rule_id = (*id).to_string();
        } else if strategy != "replace"
            && strategy != "overwrite"
            && existing_ids.contains(item.rule_id.as_str())
        {
            continue;
        }
        metadata.insert(item.rule_id.clone(), item);
    }
    metadata
        .into_values()
        .filter(|item| final_ids.contains(item.rule_id.as_str()))
        .collect()
}
#[tauri::command]
fn preview_file(source: ContentSource) -> Result<String, String> {
    read_content_source(&source).map_err(diagnostics)
}
#[tauri::command]
fn preview_image(path: String) -> Result<String, String> {
    use base64::Engine;
    let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
    if metadata.len() > 32_000_000 {
        return Err("Image exceeds 32 MB".into());
    }
    let image = image::open(&path).map_err(|e| e.to_string())?;
    let image = image.thumbnail(640, 480);
    let mut bytes = std::io::Cursor::new(vec![]);
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
    ))
}
fn autostart_enabled() -> bool {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
            .and_then(|r| r.get_value::<String, _>("QuickerText"))
            .is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        mac_login_path().map(|p| p.exists()).unwrap_or(false)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}
#[tauri::command]
fn get_cursor() -> serde_json::Value {
    quicker_platform::cursor()
}
#[tauri::command]
async fn capture_selection(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let w = app.get_webview_window("main");
    if let Some(w) = &w {
        w.hide().map_err(|e| e.to_string())?
    }
    let result = tauri::async_runtime::spawn_blocking(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        quicker_platform::capture_selection()
    })
    .await;
    if let Some(w) = w {
        let _ = w.show();
    }
    let text = result.map_err(|e| e.to_string())??;
    Ok(
        serde_json::json!({"text":text,"status":"success","message":"Selection read through accessibility; clipboard unchanged"}),
    )
}
#[tauri::command]
fn exit_app(app: tauri::AppHandle) {
    let state = app.state::<Managed>();
    let s = state.lock().unwrap();
    s.engine.cancel.store(true, Ordering::SeqCst);
    s.engine.paused.store(true, Ordering::SeqCst);
    app.exit(0)
}
#[tauri::command]
fn start_recording(store: State<Managed>) -> Result<(), String> {
    if store.lock().unwrap().engine.busy.load(Ordering::SeqCst) {
        return Err("Stop the current action before starting recording".into());
    }
    quicker_platform::start_recording(&store.lock().unwrap().engine)
}
#[tauri::command]
fn stop_recording(store: State<Managed>) -> Result<ActionPlan, String> {
    #[cfg(windows)]
    quicker_platform::finish_recording(&store.lock().unwrap().engine);
    quicker_platform::stop_recording(&store.lock().unwrap().engine)
}
#[tauri::command]
fn set_autostart(enabled: bool) -> Result<bool, String> {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        let run = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                KEY_SET_VALUE,
            )
            .map_err(|e| e.to_string())?;
        if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            run.set_value(
                "QuickerText",
                &format!("\"{}\" --background", exe.display()),
            )
            .map_err(|e| e.to_string())?
        } else {
            match run.delete_value("QuickerText") {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(enabled)
    }
    #[cfg(target_os = "macos")]
    {
        set_mac_autostart(enabled)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err("Login item support is unavailable on this platform".into())
    }
}
#[cfg(target_os = "macos")]
fn mac_login_path() -> Result<PathBuf, String> {
    let user_home = std::env::var_os("HOME").ok_or("macOS home directory is unavailable")?;
    Ok(PathBuf::from(user_home).join("Library/LaunchAgents/com.quickertext.desktop.plist"))
}
#[cfg(target_os = "macos")]
fn set_mac_autostart(enabled: bool) -> Result<bool, String> {
    let path = mac_login_path()?;
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let escaped = exe
            .to_string_lossy()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        let content=format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>com.quickertext.desktop</string><key>ProgramArguments</key><array><string>{escaped}</string><string>--background</string></array><key>RunAtLoad</key><true/></dict></plist>");
        fs::create_dir_all(path.parent().ok_or("Login item parent unavailable")?)
            .map_err(|e| e.to_string())?;
        fs::write(path, content).map_err(|e| e.to_string())?;
    } else if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(enabled)
}
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            // Release isolation is available only in the explicit input acceptance mode.
            let acceptance_dir = if cfg!(debug_assertions)
                || quicker_platform::acceptance_test_input_enabled()
            {
                std::env::var_os("QUICKER_TEXT_DATA_DIR")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            } else {
                None
            };
            let dir = match acceptance_dir {
                Some(dir) => dir,
                None => app.path().app_data_dir()?,
            };
            let store = Store::load(dir).map_err(std::io::Error::other)?;
            let engine = store.engine.clone();
            app.manage(Mutex::new(store));
            let handle = app.handle().clone();
            let watcher = engine.clone();
            std::thread::spawn(move || {
                use tauri_plugin_notification::NotificationExt;
                let mut seen = String::new();
                let mut last_tooltip = String::new();
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    let tooltip = if watcher.paused.load(Ordering::SeqCst) {
                        "Quicker Text · 已暂停"
                    } else if watcher.busy.load(Ordering::SeqCst) {
                        "Quicker Text · 执行中"
                    } else {
                        "Quicker Text · 监听中"
                    };
                    if tooltip != last_tooltip {
                        if let Some(tray) = handle.tray_by_id("quicker-text-tray") {
                            let _ = tray.set_tooltip(Some(tooltip));
                        }
                        last_tooltip = tooltip.into();
                    }
                    let last = watcher.last.lock().unwrap().clone();
                    if let Some(last) = last {
                        let stamp = format!("{}:{}", last.id, last.status);
                        if stamp == seen {
                            continue;
                        }
                        seen = stamp;
                        let notification = watcher
                            .config
                            .lock()
                            .unwrap()
                            .rules
                            .iter()
                            .find(|r| last.id.starts_with(&format!("{}:", r.id)))
                            .map(|r| r.notification.clone());
                        if let Some(n) = notification.filter(|n| n.enabled) {
                            let title = match last.status.as_str() {
                                "unknown" => "输入已发送 · 目标结果未知",
                                "cancelled" => "动作已停止",
                                _ => "动作执行失败",
                            };
                            let _ = handle
                                .notification()
                                .builder()
                                .title(title)
                                .body(if n.message.is_empty() {
                                    "请在文本指令管理中查看执行状态"
                                } else {
                                    &n.message
                                })
                                .show();
                        }
                    }
                }
            });
            if let Err(message) = quicker_platform::start_engine(engine.clone()) {
                *engine.last.lock().unwrap() = Some(ExecutionResult {
                    id: "engine".into(),
                    status: "blocked".into(),
                    message,
                    completed_steps: 0,
                    side_effects: false,
                });
            }
            let menu = tauri::menu::Menu::with_items(
                app,
                &[
                    &tauri::menu::MenuItem::with_id(
                        app,
                        "show",
                        "打开管理界面",
                        true,
                        None::<&str>,
                    )?,
                    &tauri::menu::MenuItem::with_id(
                        app,
                        "pause",
                        "暂停 / 恢复",
                        true,
                        None::<&str>,
                    )?,
                    &tauri::menu::MenuItem::with_id(app, "quit", "退出全部", true, None::<&str>)?,
                ],
            )?;
            tauri::tray::TrayIconBuilder::with_id("quicker-text-tray")
                .icon(app.default_window_icon().cloned().unwrap_or_else(|| {
                    tauri::image::Image::new_owned([27, 88, 162, 255].repeat(32 * 32), 32, 32)
                }))
                .tooltip("Quicker Text · 默认暂停")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "pause" => {
                        let state = app.state::<Managed>();
                        let s = state.lock().unwrap();
                        let old = s.engine.paused.load(Ordering::SeqCst);
                        if old
                            && (quicker_platform::native_ready().is_err()
                                || s.engine.recording.lock().unwrap().is_some())
                        {
                            return;
                        }
                        s.engine.paused.store(!old, Ordering::SeqCst);
                        s.engine.generation.fetch_add(1, Ordering::SeqCst);
                        let _ = app.emit("engine-status", s.state());
                    }
                    "quit" => {
                        let state = app.state::<Managed>();
                        let s = state.lock().unwrap();
                        s.engine.cancel.store(true, Ordering::SeqCst);
                        s.engine.paused.store(true, Ordering::SeqCst);
                        app.exit(0)
                    }
                    _ => {}
                })
                .build(app)?;
            if std::env::args().any(|x| x == "--background") {
                if let Some(w) = app.get_webview_window("main") {
                    w.hide()?;
                }
            }
            Ok(())
        })
        .on_window_event(|w, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = w.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            load_state,
            save_draft,
            apply_settings,
            discard_draft,
            rollback,
            set_paused,
            validate_config,
            test_rule,
            parse_source,
            cancel_execution,
            execute_action,
            get_foreground,
            choose_file,
            export_config,
            import_config,
            start_recording,
            stop_recording,
            set_autostart,
            preview_file,
            preview_image,
            get_cursor,
            capture_selection,
            exit_app
        ])
        .run(tauri::generate_context!())
        .expect("Quicker Text startup failed");
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metadata_config(ids: &[&str]) -> Config {
        Config {
            rules: ids
                .iter()
                .map(|id| Rule {
                    id: (*id).into(),
                    match_spec: MatchSpec {
                        kind: MatchKind::Literal,
                        value: format!("trigger-{id}"),
                    },
                    ..Rule::default()
                })
                .collect(),
            ..Config::default()
        }
    }
    fn original(id: &str, marker: &str) -> MigrationItem {
        MigrationItem {
            rule_id: id.into(),
            status: MigrationStatus::Compatible,
            diagnostics: vec![],
            original: serde_json::json!({"synthetic_source": marker, "timestamp": 123}),
        }
    }
    fn metadata_sources(items: Vec<MigrationItem>) -> std::collections::BTreeMap<String, String> {
        items
            .into_iter()
            .map(|item| {
                assert_eq!(item.status, MigrationStatus::Compatible);
                assert_eq!(item.original["timestamp"], 123);
                (
                    item.rule_id,
                    item.original["synthetic_source"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                )
            })
            .collect()
    }
    #[test]
    fn import_metadata_replace_merge_and_overwrite_follow_accepted_rules() {
        let existing = metadata_config(&["shared", "old", "without-raw"]);
        let incoming = metadata_config(&["shared", "new", "without-raw"]);
        let old = vec![
            original("shared", "old-shared"),
            original("old", "old"),
            original("without-raw", "obsolete"),
            original("deleted", "stale"),
        ];
        let imported = vec![
            original("shared", "new-shared"),
            original("new", "new"),
            original("missing", "stale"),
        ];
        let merged = metadata_config(&["shared", "old", "without-raw", "new"]);
        let replace = metadata_sources(merge_import_metadata(
            "replace", &existing, &incoming, &incoming, &old, &imported,
        ));
        assert_eq!(
            replace,
            [
                ("shared".into(), "new-shared".into()),
                ("new".into(), "new".into())
            ]
            .into()
        );
        let merge = metadata_sources(merge_import_metadata(
            "merge", &existing, &incoming, &merged, &old, &imported,
        ));
        assert_eq!(
            merge,
            [
                ("shared".into(), "old-shared".into()),
                ("old".into(), "old".into()),
                ("new".into(), "new".into()),
                ("without-raw".into(), "obsolete".into())
            ]
            .into()
        );
        let overwrite = metadata_sources(merge_import_metadata(
            "overwrite",
            &existing,
            &incoming,
            &merged,
            &old,
            &imported,
        ));
        assert_eq!(
            overwrite,
            [
                ("shared".into(), "new-shared".into()),
                ("old".into(), "old".into()),
                ("new".into(), "new".into())
            ]
            .into()
        );
    }
    #[test]
    fn keep_both_metadata_uses_actual_appended_ids_with_suffix_collisions() {
        let existing = metadata_config(&["shared", "shared-2"]);
        let incoming = metadata_config(&["shared", "shared-3", "shared-2"]);
        let merged = merge_import(
            &existing,
            &ImportReport {
                config: incoming.clone(),
                items: vec![],
                unresolved: vec![],
            },
            ConflictStrategy::KeepBoth,
        )
        .unwrap();
        assert_eq!(
            merged
                .rules
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["shared", "shared-2", "shared-3", "shared-3-2", "shared-2-2"]
        );
        let old = vec![
            original("shared", "old-base"),
            original("shared-2", "old-suffix"),
        ];
        let imported = vec![
            original("shared", "new-base"),
            original("shared-3", "new-three"),
            original("shared-2", "new-two"),
        ];
        let metadata = metadata_sources(merge_import_metadata(
            "keep_both",
            &existing,
            &incoming,
            &merged,
            &old,
            &imported,
        ));
        assert_eq!(
            metadata,
            [
                ("shared".into(), "old-base".into()),
                ("shared-2".into(), "old-suffix".into()),
                ("shared-3".into(), "new-base".into()),
                ("shared-3-2".into(), "new-three".into()),
                ("shared-2-2".into(), "new-two".into())
            ]
            .into()
        );
    }
    #[test]
    fn discard_import_restores_published_originals_after_reload() {
        let dir =
            std::env::temp_dir().join(format!("quicker-metadata-test-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone()).unwrap();
        let a = metadata_config(&["shared"]);
        store.pending_import = Some((a.clone(), vec![original("shared", "A")]));
        store.save(a).unwrap();
        store.publish(store.draft.clone()).unwrap();
        let mut b = metadata_config(&["shared"]);
        b.rules[0].description = "synthetic replacement".into();
        store.pending_import = Some((b.clone(), vec![original("shared", "B")]));
        store.save(b.clone()).unwrap();
        drop(store);
        let mut store = Store::load(dir.clone()).unwrap();
        assert_eq!(store.unresolved[0].original["synthetic_source"], "B");
        store.pending_import = Some((b, vec![original("shared", "pending")]));
        store.discard().unwrap();
        assert!(store.pending_import.is_none());
        assert_eq!(store.unresolved[0].original["synthetic_source"], "A");
        assert_eq!(store.draft, store.active);
        drop(store);
        let store = Store::load(dir.clone()).unwrap();
        assert_eq!(store.unresolved[0].original["synthetic_source"], "A");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rollback_restores_revision_originals_after_reload() {
        let dir =
            std::env::temp_dir().join(format!("quicker-metadata-test-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(dir.clone()).unwrap();
        for marker in ["A", "B"] {
            let mut config = metadata_config(&["shared"]);
            config.rules[0].description = marker.into();
            store.pending_import = Some((config.clone(), vec![original("shared", marker)]));
            store.save(config).unwrap();
            store.publish(store.draft.clone()).unwrap();
        }
        drop(store);
        let mut store = Store::load(dir.clone()).unwrap();
        store.rollback_previous().unwrap();
        assert_eq!(store.active.revision, 3);
        assert_eq!(store.active.rules[0].description, "A");
        assert_eq!(store.unresolved[0].original["synthetic_source"], "A");
        drop(store);
        let store = Store::load(dir.clone()).unwrap();
        assert_eq!(store.active_unresolved[0].original["synthetic_source"], "A");
        assert_eq!(
            store.previous_unresolved[0].original["synthetic_source"],
            "B"
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn legacy_metadata_is_snapshotted_before_replacing_draft() {
        let dir =
            std::env::temp_dir().join(format!("quicker-metadata-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let a = metadata_config(&["shared"]);
        atomic_json(&dir.join("active.json"), &a).unwrap();
        atomic_json(
            &dir.join("migration-unresolved.json"),
            &vec![original("shared", "legacy-A")],
        )
        .unwrap();
        let mut store = Store::load(dir.clone()).unwrap();
        store.pending_import = Some((a.clone(), vec![original("shared", "B")]));
        store.save(a).unwrap();
        drop(store);
        let mut store = Store::load(dir.clone()).unwrap();
        store.discard().unwrap();
        assert_eq!(store.unresolved[0].original["synthetic_source"], "legacy-A");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn persistent_revision_conflict() {
        let dir = std::env::temp_dir().join(format!("quicker-text-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone()).unwrap();
        s.publish(Config::default()).unwrap();
        assert_eq!(s.active.revision, 1);
        let mut altered = s.active.clone();
        altered.enabled = false;
        atomic_json(&dir.join("active.json"), &altered).unwrap();
        assert!(s.publish(Config::default()).is_err());
        assert_eq!(s.active.revision, 1);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_publish_preserves_active_and_pause() {
        let dir = std::env::temp_dir().join(format!("quicker-text-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone()).unwrap();
        s.publish(Config::default()).unwrap();
        let old = s.active.clone();
        s.engine.paused.store(false, Ordering::SeqCst);
        fs::remove_file(dir.join("draft.json")).unwrap();
        fs::create_dir(dir.join("draft.json")).unwrap();
        let mut next = old.clone();
        next.enabled = false;
        assert!(s.publish(next).is_err());
        assert_eq!(s.active, old);
        assert_eq!(read_config(&dir.join("active.json")).unwrap(), old);
        assert!(!s.engine.paused.load(Ordering::SeqCst));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rollback_revision_and_reload() {
        let dir = std::env::temp_dir().join(format!("quicker-text-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone()).unwrap();
        s.publish(Config::default()).unwrap();
        let mut next = s.active.clone();
        next.enabled = false;
        s.publish(next).unwrap();
        let previous = s.previous.clone().unwrap();
        s.publish(previous).unwrap();
        assert_eq!(s.active.revision, 3);
        assert!(s.active.enabled);
        drop(s);
        let s = Store::load(dir.clone()).unwrap();
        assert_eq!(s.active.revision, 3);
        assert!(s.engine.paused.load(Ordering::SeqCst));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn revision_overflow_is_error() {
        let dir = std::env::temp_dir().join(format!("quicker-text-test-{}", uuid::Uuid::new_v4()));
        let mut s = Store::load(dir.clone()).unwrap();
        s.active.revision = u64::MAX;
        assert!(s.publish(Config::default()).is_err());
        assert_eq!(s.active.revision, u64::MAX);
        assert!(s.engine.paused.load(Ordering::SeqCst));
        fs::remove_dir_all(dir).unwrap();
    }
}
