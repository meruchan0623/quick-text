//! Offline, privacy-preserving probe; writes private artifacts only to the supplied directory.
use quicker_domain::*;
use serde_json::{json, Value};
use std::{env, fs, path::PathBuf};

fn expected_sendkeys(data: &str) -> Result<(String, usize), String> {
    let (text, left) = if let Some(text) = data.strip_suffix("{LEFT 3}") {
        (text, 3)
    } else if let Some(text) = data.strip_suffix("{LEFT 12}") {
        (text, 12)
    } else {
        return Err("SendKeys is outside the supported text + LEFT 3/12 subset".into());
    };
    if text.chars().any(|c| "{}+^%~()[]".contains(c)) {
        return Err("SendKeys text contains unsupported syntax".into());
    }
    Ok((text.into(), left))
}

fn run() -> Result<(), String> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: check_quicker_export INPUT_JSON OUTPUT_DIRECTORY".into());
    }
    let source = fs::read_to_string(&args[0]).map_err(|_| "cannot read input")?;
    let raw: Value = serde_json::from_str(source.trim_start_matches('\u{feff}'))
        .map_err(|_| "invalid input JSON")?;
    let entries = raw.as_array().ok_or("input must be a Quicker array")?;
    let mut report = import_package(&source).map_err(|_| "import_package rejected input")?;
    if report.config.rules.len() != entries.len() || report.items.len() != entries.len() {
        return Err("imported rule/item count mismatch".into());
    }
    compile_config(&report.config).map_err(|_| "imported configuration does not compile")?;
    let initially_enabled = report.config.rules.iter().filter(|r| r.enabled).count();
    let mut expected = Vec::new();
    let mut passed = 0;
    let mut text_count = 0;
    let mut sendkeys_count = 0;
    for (index, original) in entries.iter().enumerate() {
        let rule = report.config.rules[index].clone();
        let item = &report.items[index];
        let action_type = original["ActionType"]
            .as_u64()
            .ok_or("missing ActionType")?;
        let data = original["Data"].as_str().ok_or("missing Data")?;
        let mut diagnostics = serde_json::to_value(&item.diagnostics)
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        let expectation = match action_type {
            3 | 6 => {
                text_count += 1;
                Ok((data.to_owned(), 0))
            }
            2 => {
                sendkeys_count += 1;
                expected_sendkeys(data)
            }
            _ => Err("unsupported action type".into()),
        };
        let mut single = report.config.clone();
        single.rules = vec![rule.clone()];
        single.rules[0].enabled = true;
        let compilation = compile_config(&single);
        let expansion = expand_reusable(&rule.action, &single, &VariableContext::default());
        let (expected_text, expected_left) = match &expectation {
            Ok(pair) => pair.clone(),
            Err(message) => {
                diagnostics.push(json!({"code":"probe_subset", "message":message}));
                (String::new(), 0)
            }
        };
        let identity_ok = original["Id"].as_str() == Some(rule.id.as_str())
            && original["CmdText"].as_str() == Some(rule.match_spec.value.as_str());
        let plan_ok = expansion.as_ref().is_ok_and(|plan| {
            let mut text = String::new();
            let mut left = 0;
            for step in &plan.steps {
                match &step.operation {
                    Operation::Text {
                        text: chunk,
                        method,
                    } if left == 0 => {
                        let expected_method = if action_type == 3 {
                            OutputMethod::Paste
                        } else {
                            OutputMethod::Keys
                        };
                        if *method != expected_method {
                            return false;
                        }
                        text.push_str(chunk);
                    }
                    Operation::KeyPress {
                        key: Key::Named { name },
                    } if action_type == 2 && name == "LEFT" => left += 1,
                    _ => return false,
                }
            }
            text == expected_text
                && left == expected_left
                && (action_type == 2 || plan.steps.len() == 1)
        });
        let ok = expectation.is_ok()
            && identity_ok
            && compilation.is_ok()
            && plan_ok
            && item.status == MigrationStatus::Compatible;
        if !ok {
            diagnostics.push(json!({"code":"probe_validation", "message":"identity, compile, expansion, migration, or raw text comparison failed"}));
        } else {
            passed += 1;
            // This is the isolated generated configuration, never the user's active config.
            report.config.rules[index].enabled = true;
        }
        expected.push(json!({
            "id":rule.id, "trigger":rule.match_spec.value, "type":action_type,
            "expected_text":expected_text, "expected_left":expected_left,
            "original_disabled":original["IsDisabled"].as_bool().unwrap_or(false),
            "import_default_disabled":!rule.enabled,
            "passed":ok,
            "steps":expansion.as_ref().ok().map(|p| &p.steps), "diagnostics":diagnostics
        }));
    }
    compile_config(&report.config).map_err(|_| "generated configuration does not compile")?;
    let output = PathBuf::from(&args[1]);
    fs::create_dir_all(&output).map_err(|_| "cannot create output directory")?;
    for (name, value) in [
        ("active.json", serde_json::to_value(&report.config).unwrap()),
        (
            "migration-unresolved.json",
            serde_json::to_value(&report.unresolved).unwrap(),
        ),
        ("expected.json", Value::Array(expected)),
    ] {
        fs::write(
            output.join(name),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .map_err(|_| "cannot write output artifact")?;
    }
    println!(
        "total={} passed={} failed={} text={} sendkeys={} import_enabled={} isolated_enabled={}",
        entries.len(),
        passed,
        entries.len() - passed,
        text_count,
        sendkeys_count,
        initially_enabled,
        report.config.rules.iter().filter(|r| r.enabled).count()
    );
    if passed != entries.len() {
        return Err("some entries did not pass; see expected.json".into());
    }
    Ok(())
}

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
