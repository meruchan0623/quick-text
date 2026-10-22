use crate::*;
use chrono::{DateTime, Datelike, FixedOffset, Timelike};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct VariableContext {
    pub clipboard: Option<String>,
    pub selection: Option<String>,
    pub now: DateTime<FixedOffset>,
    pub syntax_variables: BTreeSet<String>,
    pub parameters: BTreeMap<String, serde_json::Value>,
}
impl Default for VariableContext {
    fn default() -> Self {
        Self {
            clipboard: None,
            selection: None,
            now: chrono::Local::now().fixed_offset(),
            syntax_variables: BTreeSet::new(),
            parameters: BTreeMap::new(),
        }
    }
}
fn content_error(code: &str, message: &str) -> Vec<Diagnostic> {
    vec![Diagnostic::error(code, message, SourceSpan::default())]
}
/// A deliberately explicit .NET subset. Unsupported locale tokens fail instead of silently changing format.
pub fn format_dotnet_date(format: &str, now: &DateTime<FixedOffset>) -> Result<String, String> {
    let chars: Vec<char> = format.chars().collect();
    let mut i = 0;
    let mut out = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != quote {
                i += 1;
            }
            if i == chars.len() {
                return Err("Unclosed date literal".into());
            }
            out.extend(chars[start..i].iter());
            i += 1;
            continue;
        }
        if c == '\\' {
            i += 1;
            if i == chars.len() {
                return Err("Dangling date escape".into());
            }
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if c == '%' {
            i += 1;
            if i == chars.len() {
                return Err("Dangling date format marker".into());
            }
            let single = chars[i].to_string();
            out.push_str(&format_dotnet_date(&single, now)?);
            i += 1;
            continue;
        }
        if !c.is_ascii_alphabetic() {
            out.push(c);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i] == c {
            i += 1;
        }
        let count = i - start;
        let value=match(c,count){
   ('y',1)=>format!("{}",now.year()%100),('y',2)=>format!("{:02}",now.year()%100),('y',3..=5)=>format!("{:0width$}",now.year(),width=count),
   ('M',1)=>now.month().to_string(),('M',2)=>format!("{:02}",now.month()),
   ('d',1)=>now.day().to_string(),('d',2)=>format!("{:02}",now.day()),
   ('H',1)=>now.hour().to_string(),('H',2)=>format!("{:02}",now.hour()),
   ('h',1)=>{let h=now.hour()%12;format!("{}",if h==0{12}else{h})},('h',2)=>{let h=now.hour()%12;format!("{:02}",if h==0{12}else{h})},
   ('m',1)=>now.minute().to_string(),('m',2)=>format!("{:02}",now.minute()),('s',1)=>now.second().to_string(),('s',2)=>format!("{:02}",now.second()),
   ('f',1..=7)=>format!("{:09}",now.nanosecond())[..count].to_string(),('F',1..=7)=>format!("{:09}",now.nanosecond())[..count].trim_end_matches('0').to_string(),
   ('t',1)=>if now.hour()<12{"A"}else{"P"}.into(),('t',2)=>if now.hour()<12{"AM"}else{"PM"}.into(),
   ('z',1..=3)=>{let seconds=now.offset().local_minus_utc();let sign=if seconds<0{'-'}else{'+'};let h=seconds.unsigned_abs()/3600;let m=seconds.unsigned_abs()%3600/60;match count{1=>format!("{sign}{h}"),2=>format!("{sign}{h:02}"),_=>format!("{sign}{h:02}:{m:02}")}},
   ('K',1)=>now.format("%:z").to_string(),
   _=>return Err("Unsupported .NET date token or locale-dependent format; choose numeric fields or quoted literal".into()),
  };
        out.push_str(&value);
    }
    Ok(out)
}
pub fn escape_sendkeys_literal(value: &str) -> String {
    let mut out = String::new();
    for c in value.chars() {
        match c {
            '{' => out.push_str("{{}"),
            '}' => out.push_str("{}}"),
            '+' | '^' | '%' | '~' | '(' | ')' | '[' | ']' => {
                out.push('{');
                out.push(c);
                out.push('}');
            }
            _ => out.push(c),
        }
    }
    out
}
/// Only recognized placeholders are interpolated. Dynamic values are never reparsed as scripts.
pub fn render_template(
    value: &str,
    context: &VariableContext,
    sendkeys: bool,
) -> Result<String, Vec<Diagnostic>> {
    let mut result = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            result.push_str(&rest[start..]);
            return Ok(result);
        };
        let token = &after[..end];
        let dynamic = match token {
            "cliptext" => Some(context.clipboard.clone().ok_or_else(|| {
                content_error("clipboard_unavailable", "Clipboard text is unavailable")
            })?),
            "selection" => Some(context.selection.clone().ok_or_else(|| {
                content_error(
                    "selection_unavailable",
                    "Selection must be explicitly captured",
                )
            })?),
            token if token.starts_with("param:") => Some(parameter_text(
                context.parameters.get(&token[6..]).ok_or_else(|| {
                    content_error(
                        "parameter_unavailable",
                        "Reusable action parameter is unavailable",
                    )
                })?,
            )?),
            token if token.starts_with("0:") => {
                Some(format_dotnet_date(&token[2..], &context.now).map_err(|_| {
                    content_error(
                        "date_format",
                        "Unsupported .NET date format; use numeric date fields",
                    )
                })?)
            }
            _ => None,
        };
        if let Some(dynamic) = dynamic {
            if sendkeys && !context.syntax_variables.contains(token) {
                result.push_str(&escape_sendkeys_literal(&dynamic));
            } else {
                result.push_str(&dynamic);
            }
        } else {
            result.push_str(&rest[start..start + end + 2]);
        }
        rest = &after[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}
pub fn read_content_source(source: &ContentSource) -> Result<String, Vec<Diagnostic>> {
    match source {
        ContentSource::Inline { value } => Ok(value.clone()),
        ContentSource::File { path, encoding } => {
            let metadata = std::fs::metadata(path.as_str()).map_err(|_| {
                content_error("file_missing", "Content file is missing or unreadable")
            })?;
            if !metadata.is_file() || metadata.len() > 16_777_216 {
                return Err(content_error(
                    "file_size",
                    "Content source must be a file no larger than 16 MiB",
                ));
            }
            let bytes = std::fs::read(path.as_str())
                .map_err(|_| content_error("file_read", "Could not read content source"))?;
            match encoding {
                Encoding::Utf8 => String::from_utf8(
                    bytes
                        .strip_prefix(&[0xef, 0xbb, 0xbf])
                        .unwrap_or(&bytes)
                        .to_vec(),
                )
                .map_err(|_| content_error("file_encoding", "File is not valid UTF-8")),
                Encoding::Utf16Le | Encoding::Utf16Be => {
                    let bom = if *encoding == Encoding::Utf16Le {
                        [0xff, 0xfe]
                    } else {
                        [0xfe, 0xff]
                    };
                    let bytes = bytes.strip_prefix(&bom).unwrap_or(&bytes);
                    if bytes.len() % 2 != 0 {
                        return Err(content_error(
                            "file_encoding",
                            "UTF-16 file has an incomplete code unit",
                        ));
                    }
                    let units: Vec<_> = bytes
                        .chunks_exact(2)
                        .map(|b| {
                            if *encoding == Encoding::Utf16Le {
                                u16::from_le_bytes([b[0], b[1]])
                            } else {
                                u16::from_be_bytes([b[0], b[1]])
                            }
                        })
                        .collect();
                    String::from_utf16(&units).map_err(|_| {
                        content_error(
                            "file_encoding",
                            "File has invalid UTF-16 surrogate sequence",
                        )
                    })
                }
                Encoding::Gbk => encoding_rs::GBK
                    .decode_without_bom_handling_and_without_replacement(&bytes)
                    .map(|v| v.into_owned())
                    .ok_or_else(|| content_error("file_encoding", "File is not valid GBK")),
            }
        }
    }
}
pub fn legacy_content_source(value: &str) -> ContentSource {
    if let Some(path) = value.strip_prefix("FILE:") {
        ContentSource::File {
            path: path.trim().into(),
            encoding: Encoding::Utf8,
        }
    } else {
        ContentSource::inline(value)
    }
}
/// Syntax-valued dynamic input must be explicitly selected for this B action.
pub fn validate_syntax_variables(names: &[String]) -> Vec<Diagnostic> {
    let mut seen = BTreeSet::new();
    names
        .iter()
        .filter_map(|name| {
            let valid = matches!(name.as_str(), "cliptext" | "selection")
                || name
                    .strip_prefix("param:")
                    .is_some_and(|n| !n.is_empty() && !n.contains(['{', '}']));
            if !valid || !seen.insert(name) {
                Some(
                    Diagnostic::error(
                        "syntax_variable",
                        "Syntax source must be unique cliptext, selection, or param:name",
                        SourceSpan::default(),
                    )
                    .at("syntax_variables"),
                )
            } else {
                None
            }
        })
        .collect()
}
pub fn action_to_plan(
    action: &Action,
    context: &VariableContext,
) -> Result<ActionPlan, Vec<Diagnostic>> {
    let mut plan = ActionPlan::default();
    match action{
 Action::None=>{},Action::Text{method,source}=>plan.steps.push(Step::new(Operation::Text{text:render_template(&read_content_source(source)?,context,false)?,method:*method},SourceSpan::default())),
 Action::SendKeys{source,syntax_variables}=>{let errors=validate_syntax_variables(syntax_variables);if !errors.is_empty(){return Err(errors);}let mut scoped=context.clone();scoped.syntax_variables.extend(syntax_variables.iter().cloned());plan=parse_sendkeys(&render_template(&read_content_source(source)?,&scoped,true)?)?;},
 Action::Html{html,plain_text}=>plan.steps.push(Step::new(Operation::Html{html:render_template(html,context,false)?,plain_text:render_template(plain_text,context,false)?},SourceSpan::default())),
 Action::Hotkey{keys}=>plan.steps.push(Step::new(Operation::Hotkey{keys:keys.clone()},SourceSpan::default())),
 Action::Steps{plan:source}=>plan=source.clone(),Action::Replay{plan:source,speed}=>plan=scale_replay(source,*speed)?,
 Action::Open{target,args,working_directory}=>plan.steps.push(Step::new(Operation::Open{target:target.clone(),args:args.clone(),working_directory:working_directory.clone()},SourceSpan::default())),
 Action::Common{function}=>plan.steps.push(Step::new(Operation::Common{function:function.clone()},SourceSpan::default())),
 Action::Reusable{action_id,parameters}=>plan.steps.push(Step::new(Operation::Reusable{action_id:action_id.clone(),parameters:parameters.clone()},SourceSpan::default())),
 Action::Quicker{..}=>return Err(content_error("quicker_bridge","Quicker-specific actions require an explicitly configured Windows bridge or portable replacement")),
 }
    let errors = validate_plan(&plan);
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(plan)
}
/// Reads static inputs and resolves trigger-time variables before any trigger erasure.
pub fn preflight_action(
    action: &Action,
    context: &VariableContext,
) -> Result<ActionPlan, Vec<Diagnostic>> {
    let mut plan = action_to_plan(action, context)?;
    for step in &mut plan.steps {
        match &mut step.operation {
            Operation::Text { text, .. }
                if matches!(action, Action::Steps { .. } | Action::Replay { .. }) =>
            {
                *text = render_template(text, context, false)?
            }
            Operation::Html { html, plain_text }
                if matches!(action, Action::Steps { .. } | Action::Replay { .. }) =>
            {
                *html = render_template(html, context, false)?;
                *plain_text = render_template(plain_text, context, false)?;
            }
            Operation::PasteFiles { paths } => {
                for path in paths {
                    if !Path::new(path).is_file() {
                        return Err(content_error(
                            "file_missing",
                            "A clipboard file object is missing",
                        ));
                    }
                }
            }
            Operation::PasteImage { path } => {
                let metadata = std::fs::metadata(path.as_str())
                    .map_err(|_| content_error("image_missing", "Image source is missing"))?;
                if metadata.len() > 32_000_000 {
                    return Err(content_error("image_size", "Image exceeds 32 MB"));
                }
                let bytes = std::fs::read(path.as_str())
                    .map_err(|_| content_error("image_read", "Image source cannot be read"))?;
                let known = bytes.starts_with(b"\x89PNG\r\n\x1a\n")
                    || bytes.starts_with(&[0xff, 0xd8, 0xff])
                    || bytes.starts_with(b"GIF87a")
                    || bytes.starts_with(b"GIF89a")
                    || bytes.starts_with(b"BM")
                    || bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP");
                if !known {
                    return Err(content_error("image_format","Unsupported image signature; decoder must verify complete image before erasure"));
                }
            }
            _ => {}
        }
    }
    Ok(plan)
}

fn parameter_text(value: &serde_json::Value) -> Result<String, Vec<Diagnostic>> {
    match value {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Bool(v) => Ok(v.to_string()),
        serde_json::Value::Number(v) => Ok(v.to_string()),
        serde_json::Value::Null => Ok(String::new()),
        _ => Err(content_error(
            "parameter_type",
            "Parameter must be a scalar string, number, boolean or null",
        )),
    }
}
/// Why `action_id` does not resolve to a rule that is on at runtime.
fn unavailable(config: &Config, action_id: &str) -> Vec<Diagnostic> {
    match config.rules.iter().find(|r| r.id == action_id) {
        Some(rule) if !rule.enabled => content_error(
            "action_disabled",
            "Referenced action is disabled; explicitly enable it before execution",
        ),
        Some(_) => content_error(
            "action_skipped",
            "Referenced action did not compile and is off at runtime",
        ),
        None if config
            .unreadable_rules
            .iter()
            .any(|entry| entry.id() == Some(action_id)) =>
        {
            content_error(
                "action_unreadable",
                "Referenced action could not be read by this version and is off at runtime",
            )
        }
        None => content_error("action_unknown", "Reusable action ID is not registered"),
    }
}
/// Resolve portable action IDs to rules that are on at runtime, preserving typed parameters
/// and preventing cycles. A referenced rule must be enabled and compile (B-43: a rule that
/// does not compile, or an entry this version cannot read, never runs, not even by
/// reference). [`CompiledConfig::expand_reusable`] resolves against the compiled rules
/// directly instead of compiling each referenced rule again.
pub fn expand_reusable(
    action: &Action,
    config: &Config,
    context: &VariableContext,
) -> Result<ActionPlan, Vec<Diagnostic>> {
    expand(
        action,
        &|action_id: &str| match config.rules.iter().find(|r| r.id == action_id) {
            Some(rule) if rule_compiles(rule) => Ok(rule),
            _ => Err(unavailable(config, action_id)),
        },
        context,
        &mut vec![],
    )
}
impl CompiledConfig {
    /// [`expand_reusable`] against the rules that compiled: references to disabled rules,
    /// rules that did not compile and unreadable entries are refused, however deeply nested.
    pub fn expand_reusable(
        &self,
        action: &Action,
        context: &VariableContext,
    ) -> Result<ActionPlan, Vec<Diagnostic>> {
        expand(
            action,
            &|action_id: &str| {
                self.rule(action_id)
                    .ok_or_else(|| unavailable(&self.config, action_id))
            },
            context,
            &mut vec![],
        )
    }
}
fn expand<'a>(
    action: &Action,
    resolve: &dyn Fn(&str) -> Result<&'a Rule, Vec<Diagnostic>>,
    context: &VariableContext,
    stack: &mut Vec<String>,
) -> Result<ActionPlan, Vec<Diagnostic>> {
    if stack.len() >= 16 {
        return Err(content_error(
            "action_depth",
            "Reusable action nesting exceeds 16",
        ));
    }
    if let Action::Reusable {
        action_id,
        parameters,
    } = action
    {
        if stack.contains(action_id) {
            return Err(content_error(
                "action_cycle",
                "Reusable action reference contains a cycle",
            ));
        }
        let rule = resolve(action_id)?;
        let mut child = context.clone();
        for (name, value) in parameters {
            parameter_text(value)?;
            child.parameters.insert(name.clone(), value.clone());
        }
        stack.push(action_id.clone());
        let result = expand(&rule.action, resolve, &child, stack);
        stack.pop();
        return result;
    }
    let mut plan = preflight_action(action, context)?;
    let mut steps = vec![];
    for step in plan.steps {
        if let Operation::Reusable {
            action_id,
            parameters,
        } = &step.operation
        {
            let nested = expand(
                &Action::Reusable {
                    action_id: action_id.clone(),
                    parameters: parameters.clone(),
                },
                resolve,
                context,
                stack,
            )?;
            for mut nested_step in nested.steps {
                if nested_step.target == TargetPolicy::Locked {
                    nested_step.target = step.target.clone();
                }
                steps.push(nested_step);
            }
        } else {
            steps.push(step);
        }
        if steps.len() > MAX_STEPS {
            return Err(content_error(
                "step_limit",
                "Expanded action exceeds 10000 steps",
            ));
        }
    }
    plan.steps = steps;
    let diagnostics = validate_plan(&plan);
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    Ok(plan)
}
