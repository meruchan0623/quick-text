//! Rule editing helpers without side effects: validation, trial matching,
//! script parsing and content previews.
use crate::{
    app_state::Managed,
    error::{CommandError, IoOp},
};
use quicker_domain::*;
use serde::Serialize;
use std::{fs, path::Path};
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
) -> Result<serde_json::Value, CommandError> {
    let globals = store.lock().draft.globals.clone();
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
    let compiled = compile_config(&config).map_err(CommandError::validation)?;
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
    serde_json::to_value(matched).map_err(CommandError::internal)
}
#[tauri::command]
pub fn parse_source(dialect: Dialect, source: String) -> serde_json::Value {
    match parse_dialect(dialect, &source) {
        Ok(plan) => serde_json::json!({"plan":plan,"diagnostics":[]}),
        Err(errors) => serde_json::json!({"plan":null,"diagnostics":errors}),
    }
}
#[tauri::command]
pub fn preview_file(source: ContentSource) -> Result<String, CommandError> {
    read_content_source(&source).map_err(CommandError::validation)
}
#[tauri::command]
pub fn preview_image(path: String) -> Result<String, CommandError> {
    use base64::Engine;
    let file = Path::new(&path);
    let metadata = fs::metadata(file).map_err(|e| CommandError::io(IoOp::Read, Some(file), &e))?;
    if metadata.len() > 32_000_000 {
        return Err(CommandError::invalid_file(
            "image_too_large",
            "Image exceeds 32 MB",
            file,
        ));
    }
    let image = image::open(file).map_err(|e| match e {
        image::ImageError::IoError(e) => CommandError::io(IoOp::Read, Some(file), &e),
        e => CommandError::invalid_file("image_decode", e.to_string(), file),
    })?;
    let image = image.thumbnail(640, 480);
    let mut bytes = std::io::Cursor::new(vec![]);
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(CommandError::internal)?;
    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
    ))
}
