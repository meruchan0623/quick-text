use crate::*;

fn err(code: &str, message: &str, line: usize, column: usize) -> Diagnostic {
    Diagnostic::error(
        code,
        message,
        SourceSpan {
            line,
            column,
            length: 1,
        },
    )
}
fn parse_error(message: &str) -> String {
    message.into()
}
pub fn parse_key(value: &str) -> Result<Key, String> {
    let value = value.trim();
    if let Some(code) = value.strip_prefix('#') {
        let code = code
            .parse::<u16>()
            .map_err(|_| parse_error("Windows key code must be decimal"))?;
        if !(1..=255).contains(&code) {
            return Err(parse_error("Windows key code outside 1..255"));
        }
        return Ok(Key::WindowsCode { code });
    }
    let upper = value.to_ascii_uppercase();
    let valid = value.chars().count() == 1 && value.chars().all(|c| c.is_ascii_alphabetic())
        || upper.len() == 2 && upper.starts_with('D') && upper.as_bytes()[1].is_ascii_digit()
        || upper
            .strip_prefix('F')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=24).contains(&n))
        || [
            "CTRL",
            "CONTROL",
            "CONTROLKEY",
            "LCONTROLKEY",
            "RCONTROLKEY",
            "SHIFT",
            "SHIFTKEY",
            "LSHIFTKEY",
            "RSHIFTKEY",
            "ALT",
            "MENU",
            "LMENU",
            "RMENU",
            "CMD",
            "COMMAND",
            "META",
            "LWIN",
            "RWIN",
            "WIN",
            "ENTER",
            "RETURN",
            "SPACE",
            "TAB",
            "ESC",
            "ESCAPE",
            "BACK",
            "BACKSPACE",
            "BS",
            "LEFT",
            "RIGHT",
            "UP",
            "DOWN",
            "HOME",
            "END",
            "PGUP",
            "PGDN",
            "PRIOR",
            "NEXT",
            "DELETE",
            "DEL",
            "INSERT",
            "INS",
            "CAPSLOCK",
            "CAPITAL",
            "NUMLOCK",
            "SCROLLLOCK",
            "SCROLL",
            "PAUSE",
            "BREAK",
            "PRINTSCREEN",
            "PRTSC",
            "ADD",
            "SUBTRACT",
            "MULTIPLY",
            "DIVIDE",
            "DECIMAL",
            "APPS",
            "HELP",
            "CLEAR",
            "SELECT",
            "EXECUTE",
            "VOLUMEMUTE",
            "VOLUMEDOWN",
            "VOLUMEUP",
            "MEDIANEXTTRACK",
            "MEDIAPREVIOUSTRACK",
            "MEDIASTOP",
            "MEDIAPLAYPAUSE",
            "BROWSERBACK",
            "BROWSERFORWARD",
            "BROWSERREFRESH",
            "BROWSERSTOP",
            "BROWSERSEARCH",
            "BROWSERFAVORITES",
            "BROWSERHOME",
            "LAUNCHMAIL",
            "LAUNCHMEDISELECT",
            "LAUNCHAPPLICATION1",
            "LAUNCHAPPLICATION2",
            "OEMSEMICOLON",
            "OEMPLUS",
            "OEMCOMMA",
            "OEMMINUS",
            "OEMPERIOD",
            "OEMQUESTION",
            "OEMTILDE",
            "OEMOPENBRACKETS",
            "OEMPIPE",
            "OEMCLOSEBRACKETS",
            "OEMQUOTES",
            "OEMBACKSLASH",
        ]
        .contains(&upper.as_str())
        || upper
            .strip_prefix("NUMPAD")
            .is_some_and(|s| s.len() == 1 && s.as_bytes()[0].is_ascii_digit());
    if !valid {
        return Err(parse_error("Unknown key name; numeric keys require D0..D9"));
    }
    Ok(Key::named(value))
}
fn sendkeys_key(value: &str) -> Result<Key, String> {
    let k = parse_key(value)?;
    let upper = value.to_ascii_uppercase();
    if value.starts_with('#')
        || ["CMD", "COMMAND", "META", "WIN", "LWIN", "RWIN"].contains(&upper.as_str())
        || upper.starts_with("MEDIA")
        || upper.starts_with("VOLUME")
        || upper.starts_with("BROWSER")
        || upper.starts_with("LAUNCH")
        || upper
            .strip_prefix('F')
            .and_then(|s| s.parse::<u8>().ok())
            .is_some_and(|n| n > 16)
    {
        return Err(parse_error(
            "Key is unsupported in SendKeys B; use inputscript",
        ));
    }
    Ok(k)
}
struct BParser {
    chars: Vec<char>,
    pos: usize,
    steps: Vec<Step>,
    depth: usize,
}
impl BParser {
    fn span(&self, start: usize) -> SourceSpan {
        let prefix = &self.chars[..start];
        let line = 1 + prefix.iter().filter(|c| **c == '\n').count();
        let column = 1 + prefix.iter().rev().take_while(|c| **c != '\n').count();
        SourceSpan {
            line,
            column,
            length: self.pos - start,
        }
    }
    fn emit(&mut self, op: Operation, start: usize) -> Result<(), Diagnostic> {
        if self.steps.len() >= MAX_STEPS {
            return Err(Diagnostic::error(
                "step_limit",
                "Too many operations",
                self.span(start),
            ));
        }
        self.steps.push(Step::new(op, self.span(start)));
        Ok(())
    }
    fn atom(&mut self, mods: &[Key], start: usize) -> Result<(), Diagnostic> {
        let Some(c) = self.chars.get(self.pos).copied() else {
            return Err(Diagnostic::error(
                "dangling_modifier",
                "Modifier has no key",
                self.span(start),
            ));
        };
        self.pos += 1;
        if c == '(' {
            if self.depth >= 32 {
                return Err(Diagnostic::error(
                    "group_depth",
                    "SendKeys group nesting exceeds 32",
                    self.span(start),
                ));
            }
            self.depth += 1;
            self.sequence(mods, true)?;
            self.depth -= 1;
            return Ok(());
        }
        let mut repeat = 1;
        let op = if c == '{' {
            let mut body = String::new();
            if self.chars.get(self.pos) == Some(&'{') && self.chars.get(self.pos + 1) == Some(&'}')
            {
                self.pos += 2;
                Operation::Text {
                    text: "{".into(),
                    method: OutputMethod::Keys,
                }
            } else if self.chars.get(self.pos) == Some(&'}')
                && self.chars.get(self.pos + 1) == Some(&'}')
            {
                self.pos += 2;
                Operation::Text {
                    text: "}".into(),
                    method: OutputMethod::Keys,
                }
            } else {
                while let Some(c) = self.chars.get(self.pos).copied() {
                    self.pos += 1;
                    if c == '}' {
                        break;
                    }
                    body.push(c);
                }
                if self.chars.get(self.pos.wrapping_sub(1)) != Some(&'}') {
                    return Err(Diagnostic::error(
                        "unclosed_brace",
                        "Unclosed SendKeys brace",
                        self.span(start),
                    ));
                }
                let fields: Vec<_> = body.split_whitespace().collect();
                if fields.is_empty() || fields.len() > 2 {
                    return Err(Diagnostic::error(
                        "key_syntax",
                        "Expected {key} or {key count}",
                        self.span(start),
                    ));
                }
                if fields.len() == 2 {
                    repeat = fields[1].parse::<usize>().map_err(|_| {
                        Diagnostic::error(
                            "repeat",
                            "Repeat must be a positive integer",
                            self.span(start),
                        )
                    })?;
                    if repeat == 0 || repeat > MAX_STEPS {
                        return Err(Diagnostic::error(
                            "repeat",
                            "Repeat outside 1..10000",
                            self.span(start),
                        ));
                    }
                }
                if fields[0].chars().count() == 1 && "+^%~()[]".contains(fields[0]) {
                    if !mods.is_empty() {
                        return Err(Diagnostic::error(
                            "literal_modifier",
                            "Literal punctuation with modifiers is ambiguous; use structured text",
                            self.span(start),
                        ));
                    }
                    Operation::Text {
                        text: fields[0].into(),
                        method: OutputMethod::Keys,
                    }
                } else {
                    let key = sendkeys_key(fields[0])
                        .map_err(|m| Diagnostic::error("key_name", m, self.span(start)))?;
                    if mods.is_empty() {
                        Operation::KeyPress { key }
                    } else {
                        let mut keys = mods.to_vec();
                        keys.push(key);
                        Operation::Hotkey { keys }
                    }
                }
            }
        } else if c == '~' {
            if mods.is_empty() {
                Operation::KeyPress {
                    key: Key::named("Enter"),
                }
            } else {
                let mut keys = mods.to_vec();
                keys.push(Key::named("Enter"));
                Operation::Hotkey { keys }
            }
        } else if ")[]}".contains(c) {
            return Err(Diagnostic::error(
                "unexpected_symbol",
                "Escape SendKeys punctuation in braces",
                self.span(start),
            ));
        } else if mods.is_empty() {
            Operation::Text {
                text: c.to_string(),
                method: OutputMethod::Keys,
            }
        } else {
            let key = if c.is_ascii_digit() {
                Key::named(format!("D{c}"))
            } else if c.is_ascii_alphabetic() {
                Key::named(c.to_string())
            } else {
                return Err(Diagnostic::error(
                    "modified_character",
                    "Use a named key for modified non-ASCII/punctuation",
                    self.span(start),
                ));
            };
            let mut keys = mods.to_vec();
            keys.push(key);
            Operation::Hotkey { keys }
        };
        for _ in 0..repeat {
            self.emit(op.clone(), start)?;
        }
        Ok(())
    }
    fn sequence(&mut self, inherited: &[Key], group: bool) -> Result<(), Diagnostic> {
        while self.pos < self.chars.len() {
            if self.chars[self.pos] == ')' {
                if !group {
                    return Err(Diagnostic::error(
                        "unexpected_group_end",
                        "Unmatched closing group",
                        self.span(self.pos),
                    ));
                }
                self.pos += 1;
                return Ok(());
            }
            let start = self.pos;
            let mut mods = inherited.to_vec();
            while let Some(c) = self.chars.get(self.pos) {
                let name = match c {
                    '^' => "Ctrl",
                    '+' => "Shift",
                    '%' => "Alt",
                    _ => break,
                };
                self.pos += 1;
                let key = Key::named(name);
                if mods.contains(&key) {
                    return Err(Diagnostic::error(
                        "duplicate_modifier",
                        "Repeated modifier",
                        self.span(start),
                    ));
                }
                mods.push(key);
            }
            self.atom(&mods, start)?;
        }
        if group {
            return Err(Diagnostic::error(
                "unclosed_group",
                "Unclosed SendKeys group",
                self.span(self.pos),
            ));
        }
        Ok(())
    }
}
pub fn parse_sendkeys(source: &str) -> Result<ActionPlan, Vec<Diagnostic>> {
    if source.len() > 1_048_576 {
        return Err(vec![err("source_limit", "Source exceeds 1 MiB", 1, 1)]);
    }
    let mut parser = BParser {
        chars: source.chars().collect(),
        pos: 0,
        steps: vec![],
        depth: 0,
    };
    parser.sequence(&[], false).map_err(|e| vec![e])?;
    // Merge adjacent literal characters without altering spans of nonliteral keys.
    let mut merged: Vec<Step> = vec![];
    for step in parser.steps {
        if let Operation::Text { text, .. } = &step.operation {
            if let Some(Step {
                operation: Operation::Text { text: last, .. },
                span,
                ..
            }) = merged.last_mut()
            {
                last.push_str(text);
                span.length += step.span.length;
                continue;
            }
        }
        merged.push(step);
    }
    Ok(ActionPlan {
        version: SCHEMA_VERSION,
        dialect: Dialect::SendKeys,
        source: Some(source.into()),
        steps: merged,
    })
}
fn button(value: &str) -> Result<MouseButton, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "left" => Ok(MouseButton::Left),
        "right" => Ok(MouseButton::Right),
        "middle" => Ok(MouseButton::Middle),
        "x1" => Ok(MouseButton::X1),
        "x2" => Ok(MouseButton::X2),
        _ => Err(parse_error("Unknown mouse button")),
    }
}
fn coordinate(value: &str) -> Result<(f64, bool), String> {
    let percent = value.trim().ends_with('%');
    let value = value
        .trim()
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|_| parse_error("Invalid coordinate"))?;
    if !value.is_finite() {
        return Err(parse_error("Coordinate must be finite"));
    }
    if percent && !(0.0..=100.0).contains(&value) {
        return Err(parse_error("Percent coordinate outside 0..100"));
    }
    Ok((value, percent))
}
fn input2(value: &str) -> Result<String, String> {
    let mut result = String::new();
    let mut c = value.chars();
    while let Some(x) = c.next() {
        if x != '\\' {
            result.push(x);
            continue;
        }
        match c.next() {
            Some('t') => result.push('\t'),
            Some('r') => {
                if c.next() != Some('\\') || c.next() != Some('n') {
                    return Err(parse_error("Only paired \\r\\n newline is verified"));
                }
                result.push_str("\r\n");
            }
            Some('\\') => {
                return Err(parse_error(
                    "Literal backslash escape is not verified; use input or structured text",
                ))
            }
            _ => {
                return Err(parse_error(
                    "Unknown input2 escape; use input or structured text",
                ))
            }
        }
    }
    Ok(result)
}
fn command(cmd: &str, arg: &str) -> Result<Vec<Operation>, String> {
    let trim = arg.trim();
    Ok(match cmd {
        "input" => vec![Operation::Text {
            text: arg.into(),
            method: OutputMethod::Keys,
        }],
        "input2" => vec![Operation::Text {
            text: input2(arg)?,
            method: OutputMethod::Keys,
        }],
        "paste" => vec![Operation::Text {
            text: arg.into(),
            method: OutputMethod::Paste,
        }],
        "sendkeys" => parse_sendkeys(arg)
            .map_err(|e| e[0].message.clone())?
            .steps
            .into_iter()
            .map(|s| s.operation)
            .collect(),
        "delay" => {
            let ms = trim
                .parse::<u64>()
                .map_err(|_| parse_error("Delay must be nonnegative milliseconds"))?;
            if ms > MAX_DELAY_MS {
                return Err(parse_error("Delay exceeds one hour"));
            }
            vec![Operation::Delay { ms }]
        }
        "keydown" => vec![Operation::KeyDown {
            key: parse_key(trim)?,
        }],
        "keyup" => vec![Operation::KeyUp {
            key: parse_key(trim)?,
        }],
        "keypress" => vec![Operation::KeyPress {
            key: parse_key(trim)?,
        }],
        "hotkey" => {
            let keys: Vec<_> = trim.split('+').map(parse_key).collect::<Result<_, _>>()?;
            if keys.len() < 2 || keys.len() > 8 {
                return Err(parse_error("Hotkey requires 2..8 keys"));
            }
            vec![Operation::Hotkey { keys }]
        }
        "moveto" | "move" => {
            let parts: Vec<_> = trim.split(',').collect();
            if parts.len() != 2 {
                return Err(parse_error("Mouse move expects x,y"));
            }
            let (x, xp) = coordinate(parts[0])?;
            let (y, yp) = coordinate(parts[1])?;
            if xp != yp || cmd == "move" && xp {
                return Err(parse_error("Use both percentages only for absolute moveto"));
            }
            vec![Operation::MouseMove {
                x,
                y,
                relative: cmd == "move",
                percent: xp,
            }]
        }
        "click" | "dbclick" | "down" | "up" => vec![Operation::MouseButton {
            button: button(trim)?,
            state: match cmd {
                "down" => ButtonState::Down,
                "up" => ButtonState::Up,
                "dbclick" => ButtonState::DoubleClick,
                _ => ButtonState::Click,
            },
        }],
        "wheel" | "wheeldelta" | "hwheel" | "hwheeldelta" => vec![Operation::Wheel {
            axis: if cmd.starts_with('h') {
                WheelAxis::Horizontal
            } else {
                WheelAxis::Vertical
            },
            amount: trim
                .parse()
                .map_err(|_| parse_error("Wheel amount must be an integer"))?,
            unit: if cmd.ends_with("delta") {
                WheelUnit::Delta
            } else {
                WheelUnit::Clicks
            },
        }],
        "pastefile" => {
            let paths: Vec<_> = trim.split(';').map(|s| s.trim().to_string()).collect();
            if paths.iter().any(String::is_empty) {
                return Err(parse_error("File list contains empty path"));
            }
            vec![Operation::PasteFiles { paths }]
        }
        "pasteimage" => {
            if trim.is_empty() || trim.contains(';') {
                return Err(parse_error("pasteimage requires one image path"));
            }
            vec![Operation::PasteImage { path: trim.into() }]
        }
        _ => return Err(parse_error("Unknown inputscript command")),
    })
}
pub fn parse_inputscript(source: &str) -> Result<ActionPlan, Vec<Diagnostic>> {
    if source.len() > 1_048_576 {
        return Err(vec![err("source_limit", "Source exceeds 1 MiB", 1, 1)]);
    }
    let mut steps = vec![];
    let mut diagnostics = vec![];
    for (i, line) in source.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with("//") {
            continue;
        }
        let mut col = 0;
        for fragment in line.split(";;") {
            let leading = fragment.chars().count() - fragment.trim_start().chars().count();
            let trimmed = fragment.trim_start();
            let span = SourceSpan {
                line: i + 1,
                column: col + leading + 1,
                length: trimmed.chars().count(),
            };
            if let Some((cmd, arg)) = trimmed.split_once(':') {
                match command(cmd.trim().to_ascii_lowercase().as_str(), arg) {
                    Ok(ops) => {
                        for op in ops {
                            steps.push(Step::new(op, span.clone()));
                        }
                    }
                    Err(message) => {
                        diagnostics.push(Diagnostic::error("inputscript", message, span))
                    }
                }
            } else {
                diagnostics.push(Diagnostic::error(
                    "ambiguous_separator",
                    "Expected command:parameter; literal ;; must use structured text",
                    span,
                ));
            }
            col += fragment.chars().count() + 2;
        }
    }
    if steps.len() > MAX_STEPS {
        diagnostics.push(err("step_limit", "Too many operations", 1, 1));
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    Ok(ActionPlan {
        version: SCHEMA_VERSION,
        dialect: Dialect::Inputscript,
        source: Some(source.into()),
        steps,
    })
}
pub fn parse_replay(source: &str) -> Result<ActionPlan, Vec<Diagnostic>> {
    if source.len() > 1_048_576 {
        return Err(vec![err("source_limit", "Source exceeds 1 MiB", 1, 1)]);
    }
    let mut steps = vec![];
    let mut diagnostics = vec![];
    for (i, line) in source.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let span = SourceSpan {
            line: i + 1,
            column: 1,
            length: line.chars().count(),
        };
        let result = (|| -> Result<Vec<Operation>, String> {
            let mut fields: Vec<_> = if line.contains(';') {
                line.split(';').map(str::trim).collect()
            } else {
                line.split('\t').map(str::trim).collect()
            };
            if fields.len() == 4 && fields.last() == Some(&"") {
                fields.pop();
            }
            if fields.len() != 3 {
                return Err(parse_error("Replay expects delay; opcode; arguments"));
            }
            let ms = fields[0]
                .parse::<u64>()
                .map_err(|_| parse_error("Replay delay must be nonnegative integer"))?;
            if ms > MAX_DELAY_MS {
                return Err(parse_error("Replay delay exceeds one hour"));
            }
            let mut operations = vec![Operation::Delay { ms }];
            match fields[1] {
                "KD" => operations.push(Operation::KeyDown {
                    key: parse_key(fields[2])?,
                }),
                "KU" => operations.push(Operation::KeyUp {
                    key: parse_key(fields[2])?,
                }),
                "KP" => operations.push(Operation::KeyPress {
                    key: parse_key(fields[2])?,
                }),
                "DL" => {
                    if !fields[2].is_empty() && fields[2] != "None,0,0,0" {
                        return Err(parse_error(
                            "DL argument boundary unverified; use empty arguments",
                        ));
                    }
                }
                opcode if ["MV", "MVD", "MD", "MU", "MC", "MH", "MW"].contains(&opcode) => {
                    let args: Vec<_> = fields[2].split(',').map(str::trim).collect();
                    if args.len() != 4 {
                        return Err(parse_error("Mouse replay expects button,x,y,clicks"));
                    }
                    let x = if args[1].is_empty() || args[1] == "-99999" {
                        None
                    } else {
                        Some(coordinate(args[1])?)
                    };
                    let y = if args[2].is_empty() || args[2] == "-99999" {
                        None
                    } else {
                        Some(coordinate(args[2])?)
                    };
                    if x.is_some() != y.is_some() {
                        return Err(parse_error("Partial replay coordinates are ambiguous"));
                    }
                    if x.is_some_and(|x| x.1) || y.is_some_and(|y| y.1) {
                        return Err(parse_error("Replay uses absolute pixels, not percentages"));
                    }
                    let amount = if args[3].is_empty() {
                        0
                    } else {
                        args[3]
                            .parse::<i32>()
                            .map_err(|_| parse_error("Replay clicks must be an integer"))?
                    };
                    if let (Some((x, _)), Some((y, _))) = (x, y) {
                        operations.push(Operation::MouseMove {
                            x,
                            y,
                            relative: opcode == "MVD",
                            percent: false,
                        });
                    }
                    match opcode {
                        "MD" | "MU" | "MC" => operations.push(Operation::MouseButton {
                            button: button(args[0])?,
                            state: match opcode {
                                "MD" => ButtonState::Down,
                                "MU" => ButtonState::Up,
                                _ => ButtonState::Click,
                            },
                        }),
                        "MW" | "MH" => operations.push(Operation::Wheel {
                            axis: if opcode == "MW" {
                                WheelAxis::Vertical
                            } else {
                                WheelAxis::Horizontal
                            },
                            amount,
                            unit: WheelUnit::Clicks,
                        }),
                        _ => {
                            if x.is_none() {
                                return Err(parse_error("Move event requires coordinates"));
                            }
                        }
                    }
                }
                _ => return Err(parse_error("Unknown replay opcode")),
            }
            Ok(operations)
        })();
        match result {
            Ok(ops) => {
                for op in ops {
                    steps.push(Step::new(op, span.clone()));
                }
            }
            Err(message) => diagnostics.push(Diagnostic::error("replay", message, span)),
        }
    }
    if steps.len() > MAX_STEPS {
        diagnostics.push(err("step_limit", "Too many replay operations", 1, 1));
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    Ok(ActionPlan {
        version: SCHEMA_VERSION,
        dialect: Dialect::Replay,
        source: Some(source.into()),
        steps,
    })
}
pub fn parse_dialect(dialect: Dialect, source: &str) -> Result<ActionPlan, Vec<Diagnostic>> {
    match dialect {
        Dialect::SendKeys => parse_sendkeys(source),
        Dialect::Inputscript => parse_inputscript(source),
        Dialect::Replay => parse_replay(source),
        Dialect::Structured => serde_json::from_str(source)
            .map_err(|e| vec![err("json_plan", &e.to_string(), e.line(), e.column())]),
    }
}
/// Scale replay waits only. Coordinates and wheel amounts remain unchanged.
pub fn scale_replay(plan: &ActionPlan, speed: f64) -> Result<ActionPlan, Vec<Diagnostic>> {
    if !speed.is_finite() || speed <= 0.0 {
        return Err(vec![err(
            "replay_speed",
            "Replay speed must be positive and finite",
            1,
            1,
        )]);
    }
    let mut plan = plan.clone();
    for step in &mut plan.steps {
        if let Operation::Delay { ms } = &mut step.operation {
            let wait = *ms as f64 / speed;
            if !wait.is_finite() || wait > MAX_DELAY_MS as f64 {
                return Err(vec![Diagnostic::error(
                    "replay_delay",
                    "Scaled delay exceeds one hour",
                    step.span.clone(),
                )]);
            }
            *ms = wait.round() as u64;
        }
    }
    Ok(plan)
}

/// Canonical identity for validation/ownership; physical bindings do not depend on imported alias spelling.
pub fn canonical_key(key: &Key) -> Key {
    match key {
        Key::Named { name } => {
            let upper = name.to_ascii_uppercase();
            let canonical = match upper.as_str() {
                "CTRL" | "CONTROL" | "CONTROLKEY" => "CTRL",
                "SHIFTKEY" | "SHIFT" => "SHIFT",
                "ALT" | "MENU" => "ALT",
                "CMD" | "COMMAND" | "META" => "CMD",
                "ENTER" | "RETURN" => "ENTER",
                "ESC" | "ESCAPE" => "ESC",
                "BACK" | "BACKSPACE" | "BS" => "BACKSPACE",
                "DEL" | "DELETE" => "DELETE",
                "INS" | "INSERT" => "INSERT",
                "PGUP" | "PRIOR" => "PGUP",
                "PGDN" | "NEXT" => "PGDN",
                "CAPSLOCK" | "CAPITAL" => "CAPSLOCK",
                "SCROLLLOCK" | "SCROLL" => "SCROLLLOCK",
                _ => &upper,
            };
            Key::named(canonical)
        }
        _ => key.clone(),
    }
}
