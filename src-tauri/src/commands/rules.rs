//! Rule editing helpers without side effects: validation, trial matching,
//! script parsing and content previews.
use crate::{app_state::Managed, error::diagnostics};
use quicker_domain::*;
use serde::Serialize;
use std::fs;
use tauri::State;

#[derive(Serialize)]
pub struct Validation {
    valid: bool,
    errors: Vec<Diagnostic>,
}
#[tauri::command]
pub fn validate_config(config: Config) -> Validation {
    let errors = compile_config(&config).err().unwrap_or_default();
    Validation {
        valid: errors.is_empty(),
        errors,
    }
}
#[tauri::command]
pub fn test_rule(
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
pub fn parse_source(dialect: Dialect, source: String) -> serde_json::Value {
    match parse_dialect(dialect, &source) {
        Ok(plan) => serde_json::json!({"plan":plan,"diagnostics":[]}),
        Err(errors) => serde_json::json!({"plan":null,"diagnostics":errors}),
    }
}
#[tauri::command]
pub fn preview_file(source: ContentSource) -> Result<String, String> {
    read_content_source(&source).map_err(diagnostics)
}
#[tauri::command]
pub fn preview_image(path: String) -> Result<String, String> {
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
