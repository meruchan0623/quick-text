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
#[derive(Clone, Serialize, Deserialize)]
pub struct MigrationItem {
    pub rule_id: String,
    pub status: MigrationStatus,
    pub diagnostics: Vec<Diagnostic>,
    pub original: serde_json::Value,
}
/// The original imported entry carries the abbreviation and phrase, so it is redacted (B-09).
impl std::fmt::Debug for MigrationItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigrationItem")
            .field("rule_id", &self.rule_id)
            .field("status", &self.status)
            .field("diagnostics", &self.diagnostics)
            .field("original", &RedactedValue(&self.original))
            .finish()
    }
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
    // Stored configuration keeps unknown fields (B-43); imports stay strict (plan §9).
    if !config.extra.is_empty() || !config.globals.extra.is_empty() {
        return Err(vec![Diagnostic::error(
            "config_schema",
            "Configuration has incompatible fields; source remains untouched",
            SourceSpan::default(),
        )]);
    }
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
        let parsed = serde_json::from_value::<Rule>(original.clone())
            .ok()
            .filter(|rule| rule.extra.is_empty());
        let mut rule = match parsed {
            Some(rule) => rule,
            None => {
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
            Action::Hotkey { .. }
                | Action::SendKeys { .. }
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

// These encodings are verified from Quicker 1.44.24.0, not inferred from UI order.
// Hotkey.ToData uses "{WPF ModifierKeys integer}|{Windows VirtualKeyCode integer}".
fn quicker_hotkey(data: &str) -> Result<Action, Diagnostic> {
    let invalid = || {
        quicker_diagnostic("quicker_hotkey", "快捷键数据应为“修饰键位掩码|Windows 键码”（例如 2|65 表示 Ctrl+A）；请在“按快捷键”中重新录入。原数据已保留。")
    };
    let (modifiers, code) = data.split_once('|').ok_or_else(invalid)?;
    let modifiers = modifiers.trim().parse::<u8>().map_err(|_| invalid())?;
    let code = code.trim().parse::<u16>().map_err(|_| invalid())?;
    if modifiers > 15 || !(1..=255).contains(&code) {
        return Err(invalid());
    }
    let mut keys = vec![];
    for (flag, name) in [(2, "CTRL"), (1, "ALT"), (4, "SHIFT"), (8, "LWIN")] {
        if modifiers & flag != 0 {
            keys.push(Key::named(name));
        }
    }
    keys.push(Key::WindowsCode { code });
    Ok(Action::Hotkey { keys })
}

fn quicker_content(data: &str, diagnostics: &mut Vec<Diagnostic>) -> ContentSource {
    // Quicker uses OrdinalIgnoreCase and .NET ReadAllText (BOM detection).
    // Import never reads the file or assumes that it exists on this machine.
    if data
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("FILE:"))
    {
        let path = data[5..].trim();
        diagnostics.push(quicker_diagnostic("quicker_file_source", "已转换为外部文件来源并停用：原版会自动识别 BOM，本程序需要明确编码。请确认文件绝对路径、选择 UTF-8/UTF-16/GBK 编码并预览后启用；导入时不会读取文件。路径中的环境变量或动态占位符需要改为实际路径。"));
        ContentSource::File {
            path: path.into(),
            encoding: Encoding::Utf8,
        }
    } else {
        ContentSource::inline(data)
    }
}

fn quicker_open(data: &str) -> Result<Action, Diagnostic> {
    // Quicker separates a target and a raw Windows argument string with LF.
    // Our args are an argv array; guessing a shell/Windows quoting dialect is unsafe.
    let mut lines = data.split('\n');
    let target = lines.next().unwrap_or_default().trim();
    if lines.any(|line| !line.trim().is_empty()) {
        return Err(quicker_diagnostic("quicker_open_arguments", "“打开或运行”含原始命令行参数；本程序使用逐项参数列表，不能可靠自动拆分 Windows 引号与转义。请在“打开网页、文件或程序”中分别填写目标和每个参数，原始命令行已保留。"));
    }
    let lower = target.to_ascii_lowercase();
    let web_scheme = lower.starts_with("https://") || lower.starts_with("http://");
    let environment_variable = !web_scheme
        && regex::Regex::new(r"%[A-Za-z_][A-Za-z0-9_()]*%")
            .is_ok_and(|pattern| pattern.is_match(target));
    if environment_variable || target.contains(['{', '}', '\0', '\u{200b}', '\u{feff}']) {
        return Err(quicker_diagnostic("quicker_open_dynamic", "打开目标含环境变量、动态占位符或隐藏字符，无法直接沿用原版的展开规则。请改为固定网址或绝对路径，再预览并启用。"));
    }
    let is_web = web_scheme
        && target.split_once("://").is_some_and(|(_, rest)| {
            !rest.is_empty() && !rest.starts_with('/') && !rest.chars().any(char::is_whitespace)
        });
    let is_absolute = target.starts_with('/')
        || target.starts_with("\\\\")
        || (target.as_bytes().get(1) == Some(&b':')
            && target
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
            && matches!(target.as_bytes().get(2), Some(b'\\' | b'/')));
    let file_name = lower.rsplit(['/', '\\']).next().unwrap_or_default();
    let interpreter = [
        "cmd",
        "powershell",
        "pwsh",
        "python",
        "python3",
        "node",
        "wscript",
        "cscript",
        "bash",
        "sh",
        "mshta",
        "rundll32",
        "regsvr32",
    ]
    .iter()
    .any(|name| file_name == *name || file_name == format!("{name}.exe"));
    let script = [
        ".bat", ".cmd", ".ps1", ".psm1", ".vbs", ".vbe", ".js", ".jse", ".wsf", ".wsh", ".hta",
        ".py", ".sh", ".reg", ".lnk", ".url",
    ]
    .iter()
    .any(|extension| file_name.ends_with(extension));
    if !is_web && (!is_absolute || interpreter || script) {
        return Err(quicker_diagnostic("quicker_open_target", "此打开目标是相对路径、命令、脚本、快捷方式或命令解释器，无法作为固定文档/程序目标安全转换。请改为明确的 http(s) 网址或绝对文件/程序路径；任意脚本执行不在兼容范围内。"));
    }
    Ok(Action::Open {
        target: target.into(),
        args: vec![],
        working_directory: None,
    })
}

fn quicker_common(data: &str) -> Result<Action, Diagnostic> {
    // QuickOperationItem's static table provides these exact keys and labels.
    let function = match data {
        "windows_volume_up" => "volume_up",
        "windows_volume_down" => "volume_down",
        "windows_volume_mute" => "volume_mute",
        "windows_media_next_track" => "media_next",
        "windows_media_prev_track" => "media_previous",
        "windows_media_play_pause" => "media_play_pause",
        "windows_browser_back" => "browser_back",
        "windows_browser_forward" => "browser_forward",
        "operation_copy" => "copy",
        "operation_paste" => "paste",
        "windows_media_stop" => {
            return Ok(Action::Hotkey {
                keys: vec![Key::named("MEDIASTOP")],
            })
        }
        "windows_browser_refresh" => {
            return Ok(Action::Hotkey {
                keys: vec![Key::named("BROWSERREFRESH")],
            })
        }
        "windows_window_close_tab" => {
            return Ok(Action::Hotkey {
                keys: vec![Key::named("CTRL"), Key::named("W")],
            })
        }
        _ => {
            let message = if data.starts_with("windows_ime_") {
                "此常用功能用于切换输入法；当前没有与原版等价的输入法适配器。请改用明确的系统快捷键并在目标输入法中验证。"
            } else if data.starts_with("windows_window_") || data.starts_with("windows_alpha_") {
                "此常用功能会改变前台窗口状态/透明度；当前缺少等价窗口适配器。请手动选择可验证的快捷键替代。"
            } else if data.starts_with("monitor_brightness_") {
                "此常用功能调整显示器亮度；当前没有可跨设备等价执行的适配器。请手动设置设备支持的快捷键。"
            } else if data.starts_with("quicker_")
                || [
                    "showConfigWindow",
                    "showExeSettingWindow",
                    "showCircleMenu",
                    "toggle_lock_panel",
                ]
                .contains(&data)
            {
                "此常用功能依赖 Quicker 的面板、场景、搜索或动作运行状态，本程序没有对应私有运行时。请保留在原版使用，或手动改写为本程序已有操作。"
            } else if data.starts_with("operation_")
                || ["run_selected_text", "search_selected_text"].contains(&data)
            {
                "此常用功能依赖 Quicker 的选择区、剪贴板菜单或复制/粘贴分支状态，不能简化为单个按键。请用明确的复制、粘贴或打开操作重建。"
            } else {
                "此常用功能标识未在已核实的 Quicker 操作表中找到，不能按名称猜测执行。请在编辑器选择等价的常用操作，或提供原版类型与格式证据。"
            };
            return Err(quicker_diagnostic("quicker_common", message));
        }
    };
    Ok(Action::Common {
        function: function.into(),
    })
}

fn quicker_action(command: &QuickerCommand, diagnostics: &mut Vec<Diagnostic>) -> Action {
    let data = &command.data;
    let result = match command.action_type {
        0 => Ok(Action::None),
        1 => quicker_hotkey(data),
        2 => {
            if data.contains("{cliptext}") || data.contains("{selection}") || data.contains("{0") {
                diagnostics.push(quicker_diagnostic("quicker_sendkeys_dynamic", "原版先展开剪贴板、选中文字或日期，再将展开结果作为 SendKeys 按键语法；本程序默认把动态值转义为字面文字，行为可能不同。请核对后在“文字与按键组合”的语法来源中明确选择可信变量，或改为独立文字/按键步骤，再启用。"));
            }
            Ok(Action::SendKeys { source: quicker_content(data, diagnostics), syntax_variables: vec![] })
        }
        3 | 6 => Ok(Action::Text { method: if command.action_type == 3 { OutputMethod::Paste } else { OutputMethod::Keys }, source: quicker_content(data, diagnostics) }),
        4 => {
            diagnostics.push(quicker_diagnostic("quicker_html_plain_text", "已保留 HTML 到“粘贴带格式的文字”，但原版的 HTML 转纯文本规则没有等价实现。请补全纯文本回退、核对格式后启用；暂不自动启用，避免在纯文本输入框丢失内容。"));
            Ok(Action::Html { html: data.clone(), plain_text: String::new() })
        }
        5 => quicker_open(data),
        7 | 13 => {
            if command.action_type == 7 && (data.contains("{cliptext}") || data.contains("{selection}") || data.contains("{0:") || data.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("FILE:"))) {
                Err(quicker_diagnostic("quicker_script_dynamic", "原版会在解析多步骤输入之前展开动态变量或读取 FILE 文件；本程序的步骤计划不会执行这种动态脚本生成。请把内容改为固定步骤，文件正文改用“外部文件来源”，日期或剪贴板文字改用独立文本指令。"))
            } else {
                let parsed = if command.action_type == 7 { parse_inputscript(data) } else { parse_replay(data) };
                match parsed {
                    Ok(plan) => Ok(if command.action_type == 7 { Action::Steps { plan } } else { Action::Replay { plan, speed: 1.0 } }),
                    Err(errors) => { diagnostics.extend(errors); Ok(Action::None) }
                }
            }
        }
        11 => quicker_common(data),
        12 => Err(quicker_diagnostic("quicker_private_action", "“运行 Quicker 动作”依赖原版动作库、动作 ID 和私有运行时，不能在本程序执行。原始动作和参数已保留；请在原版运行，或手动重建为文本、快捷键、多步骤输入或本程序可复用动作。")),
        21 => Err(quicker_diagnostic("quicker_circle_menu", "此类型启动 Quicker 轮盘菜单，依赖原版菜单与场景配置；请保留在原版使用，或改写为明确的单个操作。")),
        22 => Err(quicker_diagnostic("quicker_mouse_gesture", "此类型启动 Quicker 鼠标手势，依赖原版手势配置与运行时；请在原版使用，或把手势结果重建为本程序的步骤。")),
        23 => Err(quicker_diagnostic("quicker_panel", "此类型显示 Quicker 面板，依赖原版场景和面板状态；本程序没有对应面板，请在原版使用。")),
        24 => Err(quicker_diagnostic("quicker_capture", "此类型调用 Quicker 快速截图；截图模式和输出约定无法等价导入。请使用系统截图快捷键重建并验证。")),
        25 => Err(quicker_diagnostic("quicker_move_window", "此类型使用 Quicker 窗口移动功能，依赖原版窗口目标和布局规则；当前没有等价适配器，请手动配置系统窗口快捷键。")),
        100 => Err(quicker_diagnostic("quicker_inherit_global", "此类型继承 Quicker 的全局操作，但文本指令导出没有携带被继承的全局动作。请在原版改为明确动作后重新导出，或在本程序手动选择动作。")),
        _ => Err(quicker_diagnostic("quicker_action_type", &format!("动作类型 {} 没有已核实的 Quicker 数值类型映射。原始数据已保留并停用；请提供原版版本及类型格式，或在编辑器手动选择等价动作。", command.action_type))),
    };
    match result {
        Ok(action) => action,
        Err(error) => {
            diagnostics.push(error);
            Action::None
        }
    }
}

fn quicker_schema_diagnostic(original: &serde_json::Value) -> Diagnostic {
    let Some(object) = original.as_object() else {
        return quicker_diagnostic(
            "quicker_schema",
            "此条目不是指令对象；已保留原数据并停用，请从原版重新导出文本指令 JSON 数组。",
        );
    };
    let mut invalid = vec![];
    for name in ["Id", "CmdText", "Data"] {
        if !object.get(name).is_some_and(serde_json::Value::is_string) {
            invalid.push(name);
        }
    }
    if !object
        .get("ActionType")
        .and_then(serde_json::Value::as_i64)
        .is_some_and(|n| i32::try_from(n).is_ok())
    {
        invalid.push("ActionType");
    }
    for name in ["BindingProcessName", "Title", "Group", "ParamData"] {
        if object.get(name).is_some_and(|v| !v.is_string()) {
            invalid.push(name);
        }
    }
    for name in [
        "UseRegex",
        "IgnoreCase",
        "IsDisabled",
        "UseBackspaceWhenImeOpen",
        "ExtractFirstMatchGroup",
        "IsDirectInputTrigger",
    ] {
        if object
            .get(name)
            .is_some_and(|v| !v.is_boolean() && !(name == "IsDirectInputTrigger" && v.is_null()))
        {
            invalid.push(name);
        }
    }
    for name in ["Message", "LastUpdateTimeUtc"] {
        if object
            .get(name)
            .is_some_and(|v| !v.is_null() && !v.is_string())
        {
            invalid.push(name);
        }
    }
    if object
        .get("TriggerKey")
        .is_some_and(|v| !v.is_null() && !v.as_i64().is_some_and(|n| i32::try_from(n).is_ok()))
    {
        invalid.push("TriggerKey");
    }
    quicker_diagnostic("quicker_schema", &format!("字段缺失或类型错误：{}。文本字段必须为字符串，开关必须为布尔值，动作/结束键为整数；请修正这些字段或从原版重新导出。原始数据已保留。", invalid.join("、")))
}

// Localize only the Quicker report: native package diagnostics retain their API contract.
fn localize_quicker_diagnostic(diagnostic: &mut Diagnostic) {
    if diagnostic
        .message
        .chars()
        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    {
        return;
    }
    let message = match diagnostic.code.as_str() {
        "import_confirmation" => "已转换动作，但导入的自动化默认停用。请检查目标、按键/步骤和运行环境，预览后手动启用。",
        "match_empty" => "缩写不能为空；请填写要输入的缩写后启用。",
        "match_limit" => "缩写超过 8192 字节限制；请缩短缩写或简化正则表达式。",
        "immediate_regex" => "当前正则表达式不能使用即时触发；请改为空格或分号结束触发。",
        "regex_unsupported" => "正则表达式无效或使用了 Rust 不支持的 .NET 语法（如环视、反向引用）；请改写为受支持的正则，或改为普通缩写。",
        "empty_match" => "正则表达式可以匹配空输入，可能误触发；请改成至少匹配一个字符的表达式。",
        "capture_missing" => "参数引用的正则捕获组不存在；请补全捕获组，或修改参数来源。",
        "capture_template" => "捕获组参数模板格式无效；请在参数来源中使用已存在的 {$1}、{$2} 等捕获组。",
        "terminator" => "结束键配置不能为空；请选择空格、分号或继承全局设置。",
        "date_format" => "日期模板使用了未支持或依赖语言区域的 .NET 格式；请改用 yyyy、MM、dd、HH、mm、ss 等数字格式。",
        "parameter_unavailable" => "正文引用了未定义的可复用动作参数；原版导出没有对应参数定义，请补全参数或改为固定文本。",
        "file_path" => "外部文件路径为空或含 NUL 字符；请选择有效的文件绝对路径并确认编码。",
        "inputscript" => return localize_quicker_parser_error(diagnostic, "多步骤输入"),
        "replay" => return localize_quicker_parser_error(diagnostic, "键鼠回放"),
        "ambiguous_separator" => "多步骤输入应使用“命令:参数”；正文中的字面量 ;; 无法区分于步骤分隔符，请改用结构化文本步骤。",
        "source_limit" => "动作文本超过 1 MiB 解析限制；请拆分为较小的动作。",
        "step_limit" => "动作超过 10000 步限制；请减少重复次数或拆分动作。",
        "key_pair" => "步骤中的按下/释放按键不成对，或结束时仍有按键按住；请补全释放步骤。",
        "button_pair" => "步骤中的鼠标按下/释放不成对；请补全释放步骤。",
        "key" | "key_name" | "key_code" => "按键名称或 Windows 键码不受支持；请在按键编辑器重新录入。",
        "hotkey" | "key_duplicate" => "快捷键为空、超过 8 个按键或含重复键；请重新录入快捷键。",
        "delay_limit" | "replay_delay" => "单步等待超过一小时或超出回放限值；请缩短等待时间。",
        "open_target" => "打开目标或参数为空/含无效 NUL 字符；请填写明确的目标与逐项参数。",
        "action_id" => "动作编号不能为空；请指定已有动作或选择其他动作类型。",
        "common_function" => "常用功能标识为空；请重新选择受支持的常用操作。",
        "unclosed_group" | "unclosed_key" | "unexpected_group" | "unexpected_close" | "dangling_modifier" | "duplicate_modifier" | "repeat" | "empty_key" => "SendKeys 按键组合、括号或重复次数格式无效；请对照按键 B 语法修正，或改为结构化按键步骤。",
        _ => {
            diagnostic.message = format!("此条目未通过本程序校验（{}），已停用且原始数据已保留。请检查报告定位的字段或步骤，在编辑器修正后重新预览。解析器详情：{}", diagnostic.code, diagnostic.message);
            return;
        }
    };
    diagnostic.message = message.into();
}

fn localize_quicker_parser_error(diagnostic: &mut Diagnostic, action: &str) {
    let reason = match diagnostic.message.as_str() {
        "Unknown inputscript command" => {
            "包含未支持的命令；请改为已支持的按键、文本、鼠标或等待步骤"
        }
        "Unknown replay opcode" => {
            "包含未支持的操作码；请在原版重新导出标准 playrecord，或手动重建步骤"
        }
        "Replay expects delay; opcode; arguments" => {
            "每行应为“毫秒延迟;操作码;参数;”；请修正分隔符与字段数"
        }
        "Replay delay must be nonnegative integer" | "Delay must be nonnegative milliseconds" => {
            "等待时间必须为非负整数毫秒；请修正等待时间"
        }
        "Replay delay exceeds one hour" => "单步延迟超过一小时；请缩短等待时间",
        "Replay clicks must be an integer" | "Wheel amount must be an integer" => {
            "点击次数或滚轮量必须为整数；请修正该行参数"
        }
        "Mouse replay expects button,x,y,clicks" => "鼠标参数应为“按键,X,Y,点击次数”；请补全字段",
        "Partial replay coordinates are ambiguous" => {
            "鼠标只指定了一个坐标；请同时指定 X/Y，或均使用 -99999 表示不移动"
        }
        "Replay uses absolute pixels, not percentages" => {
            "键鼠回放不支持百分比坐标；请使用当前屏幕的绝对像素坐标"
        }
        "DL argument boundary unverified; use empty arguments" => {
            "等待操作的参数格式未核实；请使用空参数的 DL 行"
        }
        _ => {
            diagnostic.message = format!("{action}无法直接转换：参数格式或按键语义无法通过校验，请检查报告定位的行并用结构化步骤编辑器重新录入。解析器详情：{}。原始动作文本已保留。", diagnostic.message);
            return;
        }
    };
    diagnostic.message = format!("{action}无法直接转换：{reason}。原始动作文本已保留。");
}

fn import_quicker_commands(entries: &[serde_json::Value]) -> Result<ImportReport, Vec<Diagnostic>> {
    let mut config = Config::default();
    let mut conversion_diagnostics = vec![];
    let mut ids = BTreeSet::new();
    for (index, original) in entries.iter().enumerate() {
        let id = original
            .get("Id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("quicker-unresolved-{}", index + 1));
        if !ids.insert(id.clone()) {
            return Err(vec![quicker_diagnostic(
                "rule_id",
                "原版导出包含重复指令 Id，整次导入已拒绝，当前配置未改变。请先为重复条目设置不同编号后重试。",
            )]);
        }
        let mut diagnostics = vec![];
        let mut rule = Rule {
            id,
            enabled: false,
            ..Rule::default()
        };
        match serde_json::from_value::<QuickerCommand>(original.clone()) {
            Err(_) => diagnostics.push(quicker_schema_diagnostic(original)),
            Ok(command) => {
                if command.id.trim().is_empty() {
                    diagnostics.push(quicker_diagnostic(
                        "quicker_id",
                        "指令 Id 不能为空；已保留原条目，请填写唯一编号后重新导入。",
                    ));
                }
                rule.action = quicker_action(&command, &mut diagnostics);
                // Process bindings are intentionally ignored; originals remain migration metadata.
                let _ = command.binding_process_name;
                rule.enabled = !command.is_disabled;
                rule.group = command.group;
                rule.description = command.title;
                rule.ignore_case = command.ignore_case;
                rule.match_spec = MatchSpec {
                    kind: if command.use_regex {
                        MatchKind::Regex
                    } else {
                        MatchKind::Literal
                    },
                    value: command.cmd_text,
                };
                rule.ime_erasure = Some(if command.use_backspace_when_ime_open {
                    ImeErasure::Backspace
                } else {
                    ImeErasure::Escape
                });
                rule.notification = Notification {
                    enabled: command.message.as_ref().is_some_and(|m| !m.is_empty()),
                    message: command.message.unwrap_or_default(),
                };
                // Verified against Quicker.Common.TextCommand.IsDirectInputTrigger:
                // zero means immediate; null means no per-command trigger key.
                rule.trigger_mode = if command.trigger_key == Some(0) {
                    TriggerMode::Immediate
                } else {
                    TriggerMode::Terminator
                };
                if let Some(key) = command.trigger_key.filter(|key| *key != 0) {
                    let name = match key {
                        9 => Some("tab"),
                        13 => Some("enter"),
                        32 => Some("space"),
                        186 => Some("semicolon"),
                        _ => None,
                    };
                    if let Some(name) = name {
                        rule.terminator = Some(Terminator {
                            keys: vec![name.into()],
                            consume: true,
                        });
                    } else {
                        diagnostics.push(quicker_diagnostic("quicker_trigger_key", &format!("结束键数值 {key} 尚无已核实映射；请改为即时触发、空格或分号后再启用。")));
                    }
                    if matches!(key, 9 | 13) {
                        diagnostics.push(quicker_diagnostic("quicker_trigger_unavailable", "已保留 Tab/Enter 结束键，但当前 Windows 输入引擎不能以 Tab/Enter 触发。请改为即时触发、空格或分号；确认后再启用。"));
                    }
                }
                if command
                    .is_direct_input_trigger
                    .is_some_and(|direct| direct != (command.trigger_key == Some(0)))
                {
                    diagnostics.push(quicker_diagnostic("quicker_trigger_key", "IsDirectInputTrigger 与 TriggerKey 的计算结果冲突；请在原版确认触发方式后重新导出，或手动设置触发方式。"));
                }
                rule.parameter = match migrate_parameter(
                    command.use_regex,
                    command.extract_first_match_group,
                    Some(&command.param_data),
                    None,
                ) {
                    Ok(parameter) => parameter,
                    Err(_) => {
                        diagnostics.push(quicker_diagnostic("quicker_parameter", "参数设置冲突：提取第一捕获组需要正则表达式，且不能同时指定另一种参数来源。请在参数设置中选择唯一来源。"));
                        ParameterSource::DefaultMatch
                    }
                };
                if let Action::Text {
                    source: ContentSource::Inline { value },
                    ..
                }
                | Action::SendKeys {
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
                    diagnostics.push(quicker_diagnostic("quicker_unknown_fields", "导出包含当前版本未识别的字段，其行为可能影响执行；这些字段和原始值已完整保留，指令已停用。请对照迁移原数据逐项核对，确认不影响行为后手动重建或启用。"));
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
            "Quicker 转换结果无法序列化；请保留源文件并报告此错误。",
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
            let mut warning = quicker_diagnostic("quicker_process_binding_ignored", "本程序已移除进程范围限制：原 BindingProcessName 仅保存在迁移元数据中，导入指令可在所有程序触发，生效范围比原版更广。请核对缩写冲突与适用场景，必要时停用该指令。");
            warning.severity = Severity::Warning;
            item.diagnostics.push(warning);
        }
        if matches!(&report.config.rules[index].action, Action::Hotkey { keys } if keys.iter().any(|key| matches!(key, Key::WindowsCode { .. })))
        {
            item.diagnostics.push(Diagnostic::warning("quicker_windows_keycode", "快捷键保留了原版 Windows 虚拟键码；Windows 以外平台可能没有对应键位。请在目标平台重新录入快捷键并验证后启用。", SourceSpan::default()));
        }
        if entries[index]
            .get("ActionType")
            .and_then(serde_json::Value::as_i64)
            == Some(11)
            && matches!(
                entries[index]
                    .get("Data")
                    .and_then(serde_json::Value::as_str),
                Some("windows_media_stop" | "windows_browser_refresh" | "windows_window_close_tab")
            )
        {
            item.diagnostics.push(Diagnostic::warning("quicker_windows_operation", "此原版常用功能已转换为 Windows 按键：媒体停止、浏览器刷新或 Ctrl+W。macOS 的键位/适配能力不同，不能视为等价兼容；请在目标平台重录受支持的快捷键并验证。", SourceSpan::default()));
        }
        item.original = entries[index].clone();
        for diagnostic in &mut item.diagnostics {
            localize_quicker_diagnostic(diagnostic);
        }
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
#[derive(Clone, Serialize, Deserialize)]
pub struct EspansoExport {
    pub yaml: String,
    pub unsupported_rule_ids: Vec<String>,
    pub global_conflicts: Vec<String>,
}
/// The exported YAML holds the triggers and replacements, so it is redacted (B-09).
impl std::fmt::Debug for EspansoExport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EspansoExport")
            .field("yaml", &Redacted(&self.yaml))
            .field("unsupported_rule_ids", &self.unsupported_rule_ids)
            .field("global_conflicts", &self.global_conflicts)
            .finish()
    }
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
