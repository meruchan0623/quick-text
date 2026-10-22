use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Component, Path};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub format: String,
    pub version: u32,
    pub config: Config,
    #[serde(default)]
    pub unresolved: Vec<MigrationItem>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationItem {
    pub rule_id: String,
    pub status: MigrationStatus,
    pub diagnostics: Vec<Diagnostic>,
    pub original: serde_json::Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStatus {
    Compatible,
    NeedsRewrite,
    NotPortable,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub config: Config,
    pub items: Vec<MigrationItem>,
    pub unresolved: Vec<MigrationItem>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictStrategy {
    Reject,
    Replace,
    KeepBoth,
}
pub fn export_package(config: &Config) -> Result<String, Vec<Diagnostic>> {
    export_package_with_unresolved(config, vec![])
}
pub fn export_package_with_unresolved(
    config: &Config,
    unresolved: Vec<MigrationItem>,
) -> Result<String, Vec<Diagnostic>> {
    compile_config(config)?;
    let package = Package {
        format: "quicker-text".into(),
        version: SCHEMA_VERSION,
        config: config.clone(),
        unresolved,
    };
    serde_json::to_string_pretty(&package).map_err(|_| {
        vec![Diagnostic::error(
            "export",
            "Package could not be serialized",
            SourceSpan::default(),
        )]
    })
}
pub fn import_package(source: &str) -> Result<ImportReport, Vec<Diagnostic>> {
    if source.len() > 16_777_216 {
        return Err(vec![Diagnostic::error(
            "package_size",
            "Package exceeds 16 MiB",
            SourceSpan::default(),
        )]);
    }
    let value: serde_json::Value =
        serde_json::from_str(source.strip_prefix('\u{feff}').unwrap_or(source)).map_err(|e| {
            vec![Diagnostic::error(
                "package_json",
                "Package JSON is invalid",
                SourceSpan {
                    line: e.line(),
                    column: e.column(),
                    length: 1,
                },
            )]
        })?;
    if let Some(entries) = value.as_array() {
        return import_quicker_commands(entries);
    }
    import_native_value(value)
}

fn import_native_value(value: serde_json::Value) -> Result<ImportReport, Vec<Diagnostic>> {
    let (mut config_value, mut unresolved) = if value.get("format").is_some() {
        if value.get("format").and_then(|v| v.as_str()) != Some("quicker-text")
            || value.get("version").and_then(|v| v.as_u64()) != Some(SCHEMA_VERSION as u64)
        {
            return Err(vec![Diagnostic::error(
                "package_version",
                "Package format/version is unsupported",
                SourceSpan::default(),
            )]);
        }
        if value.as_object().is_some_and(|o| {
            o.keys()
                .any(|k| !["format", "version", "config", "unresolved"].contains(&k.as_str()))
        }) {
            return Err(vec![Diagnostic::error(
                "package_schema",
                "Unknown package metadata cannot be silently discarded",
                SourceSpan::default(),
            )]);
        }
        let unresolved = serde_json::from_value(
            value
                .get("unresolved")
                .cloned()
                .unwrap_or(serde_json::json!([])),
        )
        .map_err(|_| {
            vec![Diagnostic::error(
                "package_schema",
                "Unresolved migration entries are invalid",
                SourceSpan::default(),
            )]
        })?;
        (
            value.get("config").cloned().ok_or_else(|| {
                vec![Diagnostic::error(
                    "package_schema",
                    "Package configuration is missing",
                    SourceSpan::default(),
                )]
            })?,
            unresolved,
        )
    } else {
        (value, vec![])
    };
    let raw_rules = config_value
        .get_mut("rules")
        .map(std::mem::take)
        .unwrap_or(serde_json::json!([]));
    if let Some(obj) = config_value.as_object_mut() {
        obj.insert("rules".into(), serde_json::json!([]));
    }
    let mut config: Config = serde_json::from_value(config_value).map_err(|_| {
        vec![Diagnostic::error(
            "config_schema",
            "Configuration has incompatible fields; source remains untouched",
            SourceSpan::default(),
        )]
    })?;
    if config.version != SCHEMA_VERSION {
        return Err(vec![Diagnostic::error(
            "schema_version",
            "Future/unknown schema cannot be migrated automatically",
            SourceSpan::default(),
        )]);
    }
    let raw_rules = raw_rules.as_array().ok_or_else(|| {
        vec![Diagnostic::error(
            "config_schema",
            "Rules must be an array",
            SourceSpan::default(),
        )]
    })?;
    let mut items = vec![];
    let mut ids = BTreeSet::new();
    for (index, original) in raw_rules.iter().enumerate() {
        let id = original
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("unresolved-{}", index + 1));
        if !ids.insert(id.clone()) {
            return Err(vec![Diagnostic::error(
                "rule_id",
                "Imported rule IDs must be unique",
                SourceSpan::default(),
            )]);
        }
        let mut rule: Rule = match serde_json::from_value(original.clone()) {
            Ok(rule) => rule,
            Err(_) => {
                let item=MigrationItem{rule_id:id.clone(),status:MigrationStatus::NeedsRewrite,diagnostics:vec![Diagnostic::error("rule_schema","Unknown rule fields/action are preserved in unresolved entries; rewrite explicitly",SourceSpan::default())],original:original.clone()};
                unresolved.push(item.clone());
                items.push(item);
                config.rules.push(Rule {
                    id,
                    enabled: false,
                    description: "Imported entry needs explicit rewriting".into(),
                    match_spec: MatchSpec {
                        kind: MatchKind::Literal,
                        value: original
                            .get("match")
                            .and_then(|m| m.get("value"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .into(),
                    },
                    ..Rule::default()
                });
                continue;
            }
        };
        let mut single = config.clone();
        let mut candidate = rule.clone();
        candidate.enabled = true;
        single.rules = vec![candidate];
        let mut diagnostics = compile_config(&single).err().unwrap_or_default();
        let not_portable = matches!(rule.action, Action::Quicker { .. });
        let status = if not_portable {
            MigrationStatus::NotPortable
        } else if diagnostics.is_empty() {
            MigrationStatus::Compatible
        } else {
            MigrationStatus::NeedsRewrite
        };
        let needs_confirmation = matches!(
            rule.action,
            Action::SendKeys { .. }
                | Action::Steps { .. }
                | Action::Replay { .. }
                | Action::Open { .. }
                | Action::Reusable { .. }
                | Action::Common { .. }
                | Action::Quicker { .. }
        );
        if status != MigrationStatus::Compatible || needs_confirmation {
            rule.enabled = false;
        }
        if needs_confirmation {
            let mut warning = Diagnostic::error(
                "import_confirmation",
                "Imported automation is disabled until explicitly reviewed and enabled",
                SourceSpan::default(),
            );
            warning.severity = Severity::Warning;
            diagnostics.push(warning);
        }
        if original.get("scope").is_some() {
            let mut warning = Diagnostic::error(
                "legacy_program_scope_ignored",
                "Program restrictions were removed; legacy scope is ignored and omitted on save",
                SourceSpan::default(),
            );
            warning.severity = Severity::Warning;
            diagnostics.push(warning);
        }
        items.push(MigrationItem {
            rule_id: id,
            status,
            diagnostics,
            original: original.clone(),
        });
        config.rules.push(rule);
    }
    Ok(ImportReport {
        config,
        items,
        unresolved,
    })
}
// PascalCase and numeric action values are the native TextCommand JSON contract.
// Type checks are strict; additional fields are retained but require review.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QuickerCommand {
    id: String,
    cmd_text: String,
    action_type: i32,
    data: String,
    #[serde(default)]
    use_regex: bool,
    #[serde(default)]
    binding_process_name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    is_disabled: bool,
    #[serde(default)]
    use_backspace_when_ime_open: bool,
    #[serde(default)]
    param_data: String,
    #[serde(default)]
    extract_first_match_group: bool,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    trigger_key: Option<i32>,
    #[serde(default)]
    is_direct_input_trigger: Option<bool>,
    #[serde(default, rename = "LastUpdateTimeUtc")]
    _last_update_time_utc: Option<String>,
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, serde_json::Value>,
}

fn quicker_diagnostic(code: &str, message: &str) -> Diagnostic {
    Diagnostic::error(code, message, SourceSpan::default())
}

fn import_quicker_commands(entries: &[serde_json::Value]) -> Result<ImportReport, Vec<Diagnostic>> {
    let mut config = Config::default();
    let mut conversion_diagnostics = vec![];
    let mut ids = BTreeSet::new();
    for (index, original) in entries.iter().enumerate() {
        let id = original
            .get("Id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("quicker-unresolved-{}", index + 1));
        if !ids.insert(id.clone()) {
            return Err(vec![quicker_diagnostic(
                "rule_id",
                "Imported rule IDs must be unique",
            )]);
        }
        let mut diagnostics = vec![];
        let mut rule = Rule {
            id,
            enabled: false,
            ..Rule::default()
        };
        match serde_json::from_value::<QuickerCommand>(original.clone()) {
            Err(_) => diagnostics.push(quicker_diagnostic("quicker_schema", "Malformed Quicker text command is preserved; field types must match the native JSON contract")),
            Ok(command) => {
                if command.id.is_empty() {
                    diagnostics.push(quicker_diagnostic("quicker_id", "Quicker command ID must not be empty"));
                }
                // Process bindings are intentionally ignored; originals remain migration metadata.
                let _ = command.binding_process_name;
                rule.enabled = !command.is_disabled;
                rule.group = command.group;
                rule.description = command.title;
                rule.ignore_case = command.ignore_case;
                rule.match_spec = MatchSpec {
                    kind: if command.use_regex { MatchKind::Regex } else { MatchKind::Literal },
                    value: command.cmd_text,
                };
                rule.ime_erasure = Some(if command.use_backspace_when_ime_open { ImeErasure::Backspace } else { ImeErasure::Escape });
                rule.notification = Notification {
                    enabled: command.message.as_ref().is_some_and(|m| !m.is_empty()),
                    message: command.message.unwrap_or_default(),
                };
                // Verified against Quicker.Common.TextCommand.IsDirectInputTrigger:
                // zero means immediate; null means no per-command trigger key.
                rule.trigger_mode = if command.trigger_key == Some(0) { TriggerMode::Immediate } else { TriggerMode::Terminator };
                if let Some(key) = command.trigger_key.filter(|key| *key != 0) {
                    let name = match key { 9 => Some("tab"), 13 => Some("enter"), 32 => Some("space"), 186 => Some("semicolon"), _ => None };
                    if let Some(name) = name {
                        rule.terminator = Some(Terminator { keys: vec![name.into()], consume: true });
                    } else {
                        diagnostics.push(quicker_diagnostic("quicker_trigger_key", "Unverified Quicker trigger-key value requires explicit mapping"));
                    }
                }
                if command.is_direct_input_trigger.is_some_and(|direct| direct != (command.trigger_key == Some(0))) {
                    diagnostics.push(quicker_diagnostic("quicker_trigger_key", "Quicker computed trigger mode conflicts with TriggerKey"));
                }
                rule.parameter = match migrate_parameter(command.use_regex, command.extract_first_match_group, Some(&command.param_data), None) {
                    Ok(parameter) => parameter,
                    Err(_) => {
                        diagnostics.push(quicker_diagnostic("quicker_parameter", "Quicker parameter conventions conflict; select an explicit source"));
                        ParameterSource::DefaultMatch
                    }
                };
                // Values verified by reflection of the installed QuickActionType enum.
                if matches!(command.action_type, 2 | 3 | 6) && command.data.starts_with("FILE:") {
                    diagnostics.push(quicker_diagnostic("quicker_file_source", "Quicker FILE: content requires explicit file-source and encoding review"));
                }
                rule.action = match command.action_type {
                    0 => Action::None,
                    2 => Action::SendKeys { source: ContentSource::inline(command.data), syntax_variables: vec![] },
                    3 | 6 => Action::Text { method: if command.action_type == 3 { OutputMethod::Paste } else { OutputMethod::Keys }, source: ContentSource::inline(command.data) },
                    7 => match parse_inputscript(&command.data) {
                        Ok(plan) => Action::Steps { plan },
                        Err(errors) => { diagnostics.extend(errors); Action::None }
                    },
                    _ => {
                        diagnostics.push(quicker_diagnostic("quicker_action_type", "Unsupported Quicker action type is preserved and requires explicit rewriting"));
                        Action::None
                    }
                };
                if let Action::Text {
                    source: ContentSource::Inline { value },
                    ..
                } = &rule.action
                {
                    // Check static template syntax without reading the user's
                    // clipboard/selection or changing the retained source text.
                    let context = VariableContext {
                        clipboard: Some(String::new()),
                        selection: Some(String::new()),
                        ..VariableContext::default()
                    };
                    if let Err(errors) = render_template(value, &context, false) {
                        diagnostics.extend(errors);
                    }
                }
                if !command.extra.is_empty() {
                    diagnostics.push(quicker_diagnostic("quicker_unknown_fields", "Unknown Quicker fields are preserved and require explicit review"));
                }
            }
        }
        if !diagnostics.is_empty() {
            rule.enabled = false;
        }
        config.rules.push(rule);
        conversion_diagnostics.push(diagnostics);
    }
    // Share native validation and the automation review policy, without array dispatch.
    let value = serde_json::to_value(config).map_err(|_| {
        vec![quicker_diagnostic(
            "quicker_schema",
            "Quicker conversion could not be serialized",
        )]
    })?;
    let mut report = import_native_value(value)?;
    for (index, item) in report.items.iter_mut().enumerate() {
        let diagnostics = &conversion_diagnostics[index];
        if !diagnostics.is_empty() {
            item.status = MigrationStatus::NeedsRewrite;
            item.diagnostics.extend(diagnostics.iter().cloned());
            report.config.rules[index].enabled = false;
        }
        if entries[index]
            .get("BindingProcessName")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty())
        {
            let mut warning = quicker_diagnostic("quicker_process_binding_ignored", "Program restrictions were removed; BindingProcessName is retained only as migration metadata");
            warning.severity = Severity::Warning;
            item.diagnostics.push(warning);
        }
        item.original = entries[index].clone();
    }
    // The retention channel also stores compatible originals: timestamps and all
    // native metadata survive package export; status identifies actual failures.
    report.unresolved = report.items.clone();
    Ok(report)
}

pub fn merge_import(
    existing: &Config,
    report: &ImportReport,
    strategy: ConflictStrategy,
) -> Result<Config, Vec<Diagnostic>> {
    let mut next = existing.clone();
    let mut ids: BTreeSet<_> = existing.rules.iter().map(|r| r.id.clone()).collect();
    for rule in &report.config.rules {
        if let Some(index) = next.rules.iter().position(|r| r.id == rule.id) {
            match strategy {
                ConflictStrategy::Reject => {
                    return Err(vec![Diagnostic::error(
                        "import_conflict",
                        "Rule ID already exists; choose a conflict strategy",
                        SourceSpan::default(),
                    )])
                }
                ConflictStrategy::Replace => next.rules[index] = rule.clone(),
                ConflictStrategy::KeepBoth => {
                    let mut rule = rule.clone();
                    let base = rule.id.clone();
                    let mut n = 2;
                    while ids.contains(&rule.id) {
                        rule.id = format!("{base}-{n}");
                        n += 1;
                    }
                    ids.insert(rule.id.clone());
                    next.rules.push(rule);
                }
            }
        } else {
            ids.insert(rule.id.clone());
            next.rules.push(rule.clone());
        }
    }
    compile_config(&next)?;
    Ok(next)
}
/// Parses documented parameter conventions, with explicit origin choice resolving conflicts.
pub fn migrate_parameter(
    regex: bool,
    first_capture: bool,
    value: Option<&str>,
    explicit: Option<ParameterSource>,
) -> Result<ParameterSource, String> {
    if let Some(explicit) = explicit {
        return Ok(explicit);
    }
    if value == Some("*NULL*") {
        return Ok(ParameterSource::Null);
    }
    if let Some(value) = value.filter(|s| s.contains("{$")) {
        return Ok(ParameterSource::CaptureTemplate {
            template: value.into(),
        });
    }
    if first_capture {
        if !regex {
            return Err("First capture requires regex".into());
        }
        if value.is_some_and(|s| !s.is_empty()) {
            return Err(
                "Legacy fixed and first-capture parameters conflict; select an explicit source"
                    .into(),
            );
        }
        return Ok(ParameterSource::FirstCapture);
    }
    match value {
        Some(value) if !value.is_empty() => Ok(ParameterSource::Fixed {
            value: value.into(),
        }),
        _ => Ok(ParameterSource::DefaultMatch),
    }
}
/// Asset paths in portable packages may never be absolute, drive-qualified or escape the managed root.
pub fn validate_asset_path(value: &str) -> Result<(), String> {
    let normalized = value.replace('\\', "/");
    let path = Path::new(&normalized);
    if normalized.contains(':')
        || path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || normalized.is_empty()
    {
        return Err("Asset path must stay within the package root".into());
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EspansoExport {
    pub yaml: String,
    pub unsupported_rule_ids: Vec<String>,
    pub global_conflicts: Vec<String>,
}
/// Interoperable literal/text subset only; never writes or modifies Espanso's unmanaged files.
pub fn export_espanso_subset(config: &Config) -> EspansoExport {
    let mut yaml = String::from("matches:\n");
    let mut unsupported_rule_ids = vec![];
    for rule in config.rules.iter().filter(|r| r.enabled) {
        let compatible = rule.match_spec.kind == MatchKind::Literal
            && !rule.ignore_case
            && rule.terminator.is_none()
            && rule.trigger_mode == TriggerMode::Immediate;
        if let Action::Text {
            method: _,
            source: ContentSource::Inline { value },
        } = &rule.action
        {
            if compatible
                && !value.contains("{cliptext}")
                && !value.contains("{selection}")
                && !value.contains("{0:")
            {
                yaml.push_str(&format!(
                    "  - trigger: {}\n    replace: {}\n",
                    serde_json::to_string(&rule.match_spec.value).unwrap(),
                    serde_json::to_string(value).unwrap()
                ));
                continue;
            }
        }
        unsupported_rule_ids.push(rule.id.clone());
    }
    EspansoExport{yaml,unsupported_rule_ids,global_conflicts:vec!["word_separators, terminators, blacklist and clipboard policy require separate managed configuration; unmanaged default.yml is unchanged".into()]}
}
