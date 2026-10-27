//! Current-configuration workbooks keep a native snapshot alongside editable
//! template columns. Only edited fields replace their original values, so
//! fields this version cannot display survive a round trip.
use super::*;
use calamine::{Data, Reader, Xlsx};
use quicker_domain::{Config, ImportReport, Rule};
use rust_xlsxwriter::{Format, Workbook, XlsxError};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read, Seek},
    path::Path,
};

const VERSION: &str = "quicktext-config-excel-1";
const BACKUP: &str = "原始配置";
const SETTINGS: &str = "全局设置";
const LONG: &str = "长内容";
const CHUNK: usize = 15_000; // At most 30k UTF-16 units, including astral characters.

fn fail(sheet: &str, message: &str) -> Diagnostic {
    error(sheet, 1, "配置", message)
}
fn chunks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(CHUNK)
        .map(|chunk| chunk.iter().collect())
        .collect()
}
fn write_cell(
    book: &mut Workbook,
    sheet: &str,
    row: u32,
    col: u16,
    value: &str,
    long: &mut BTreeMap<String, String>,
) -> Result<(), XlsxError> {
    let value = if value.encode_utf16().count() > 30_000 {
        let key = format!("长内容:{}", uuid::Uuid::new_v4());
        long.insert(key.clone(), value.into());
        key
    } else {
        value.into()
    };
    book.worksheet_from_name(sheet)?
        .write_string(row, col, &value)?;
    Ok(())
}
fn fields(config: &Value) -> BTreeMap<String, Value> {
    let mut fields = BTreeMap::new();
    if let Some(object) = config.as_object() {
        for (key, value) in object {
            if key == "rules" {
                continue;
            }
            if key == "globals" {
                if let Some(globals) = value.as_object() {
                    for (key, value) in globals {
                        fields.insert(format!("/globals/{}", escape(key)), value.clone());
                    }
                }
            } else {
                fields.insert(format!("/{}", escape(key)), value.clone());
            }
        }
    }
    fields
}
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

pub fn export_config(snapshot: &str) -> Result<Vec<u8>, Vec<Diagnostic>> {
    build(snapshot).map_err(|diagnostic| vec![diagnostic])
}
fn build(snapshot: &str) -> Result<Vec<u8>, Diagnostic> {
    if snapshot.len() > 16_777_216 {
        return Err(fail(BACKUP, "配置超过16MiB，无法生成可重新导入的Excel"));
    }
    let package: Value =
        serde_json::from_str(snapshot).map_err(|_| fail(BACKUP, "配置快照格式错误"))?;
    let config: Config = serde_json::from_value(package["config"].clone())
        .map_err(|_| fail(BACKUP, "配置快照字段错误"))?;
    if config.rules.len() > MAX_ROWS {
        return Err(fail("指令", "超过10000条指令限制"));
    }
    let mut book = template::build().map_err(|_| fail("文件", "无法生成Excel配置"))?;
    let mut long = BTreeMap::new();
    let xerr = |_| fail("文件", "无法生成Excel配置，原目标文件未修改");
    book.worksheet_from_name("填写说明")
        .map_err(xerr)?
        .write_string(1, 0, "QuickText · 当前配置导出")
        .map_err(xerr)?;
    book.worksheet_from_name("填写说明").map_err(xerr)?.write_string(2, 0, "此工作簿包含当前已保存配置。编辑「指令」和「全局设置」后重新导入；完整替换会恢复全局设置，其它策略保留当前全局设置。").map_err(xerr)?;
    book.worksheet_from_name("填写说明").map_err(xerr)?.write_string(3, 0, "复杂动作保存在「高级动作JSON」列；超过单元格长度的内容用「长内容:编号」引用「长内容」页的连续JSON分段。请保留「原始配置」页，以保留未知字段和未识别指令。").map_err(xerr)?;
    for (index, rule) in config.rules.iter().enumerate() {
        let mut cells = BTreeMap::<&str, String>::new();
        cells.insert("指令编号", rule.id.clone());
        cells.insert(
            "缩写或正则*",
            if rule.match_spec.value.is_empty() {
                "（原配置为空；保留此值则不修改）".into()
            } else {
                rule.match_spec.value.clone()
            },
        );
        cells.insert("分组", rule.group.clone());
        cells.insert("说明", rule.description.clone());
        cells.insert("启用", if rule.enabled { "是" } else { "否" }.into());
        cells.insert(
            "匹配方式",
            serde_json::to_value(rule.match_spec.kind)
                .map_err(|_| fail("指令", "匹配方式无法导出"))?
                .as_str()
                .unwrap_or("literal")
                .into(),
        );
        cells.insert(
            "忽略大小写",
            if rule.ignore_case { "是" } else { "否" }.into(),
        );
        cells.insert(
            "触发方式",
            serde_json::to_value(rule.trigger_mode)
                .map_err(|_| fail("指令", "触发方式无法导出"))?
                .as_str()
                .unwrap_or("terminator")
                .into(),
        );
        if let Some(terminator) = &rule.terminator {
            let key = match terminator.keys.as_slice() {
                [one] if ["space", "semicolon", "tab", "enter"].contains(&one.as_str()) => {
                    one.as_str()
                }
                _ => "both",
            };
            cells.insert("结束键", key.into());
            cells.insert(
                "吞掉结束键",
                if terminator.consume { "是" } else { "否" }.into(),
            );
        }
        if let Some(ime) = rule.ime_erasure {
            cells.insert(
                "输入法清除",
                serde_json::to_value(ime)
                    .map_err(|_| fail("指令", "输入法设置无法导出"))?
                    .as_str()
                    .unwrap_or("inherit")
                    .into(),
            );
        }
        cells.insert(
            "通知",
            if rule.notification.enabled {
                "是"
            } else {
                "否"
            }
            .into(),
        );
        cells.insert("通知内容", rule.notification.message.clone());
        let parameter =
            serde_json::to_value(&rule.parameter).map_err(|_| fail("指令", "参数无法导出"))?;
        cells.insert(
            "参数来源",
            parameter["type"].as_str().unwrap_or("default_match").into(),
        );
        if let Some(value) = parameter
            .get("value")
            .or_else(|| parameter.get("template"))
            .and_then(Value::as_str)
        {
            cells.insert("固定参数或捕获模板", value.into());
        }
        let action =
            serde_json::to_value(&rule.action).map_err(|_| fail("指令", "动作无法导出"))?;
        if action["type"] == "text"
            && !(action["source"]["type"] == "file"
                && action["source"]["path"].as_str().unwrap_or("").is_empty())
        {
            cells.insert(
                "动作类型",
                if action["method"] == "keys" {
                    "text_keys"
                } else {
                    "text_paste"
                }
                .into(),
            );
            let source = &action["source"];
            cells.insert(
                "文字来源",
                source["type"].as_str().unwrap_or("inline").into(),
            );
            cells.insert(
                "正文或目标",
                source
                    .get("value")
                    .or_else(|| source.get("path"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
            );
            if let Some(encoding) = source.get("encoding").and_then(Value::as_str) {
                cells.insert("编码", encoding.into());
            }
        } else if action["type"] == "none" {
            cells.insert("动作类型", "none".into());
        } else {
            cells.insert("高级动作JSON", action.to_string());
        }
        for (header, value) in cells {
            let column = RULE_HEADERS
                .iter()
                .position(|name| *name == header)
                .ok_or_else(|| fail("指令", "导出列错误"))? as u16;
            write_cell(
                &mut book,
                "指令",
                index as u32 + 1,
                column,
                &value,
                &mut long,
            )
            .map_err(xerr)?;
        }
    }
    let format = Format::new().set_text_wrap().set_num_format("@");
    let ws = book.add_worksheet();
    ws.set_name(SETTINGS).map_err(xerr)?;
    ws.set_freeze_panes(1, 1).map_err(xerr)?;
    ws.set_column_width(0, 38.0).map_err(xerr)?;
    ws.set_column_width(1, 80.0).map_err(xerr)?;
    ws.set_column_format(1, &format).map_err(xerr)?;
    ws.write_string(0, 0, "配置字段").map_err(xerr)?;
    ws.write_string(0, 1, "JSON值").map_err(xerr)?;
    let settings = fields(&package["config"]);
    if settings.len() > MAX_ROWS {
        return Err(fail(SETTINGS, "设置字段超过10000行限制"));
    }
    for (index, (key, value)) in settings.iter().enumerate() {
        write_cell(&mut book, SETTINGS, index as u32 + 1, 0, key, &mut long).map_err(xerr)?;
        write_cell(
            &mut book,
            SETTINGS,
            index as u32 + 1,
            1,
            &value.to_string(),
            &mut long,
        )
        .map_err(xerr)?;
    }
    write_long(&mut book, &long).map_err(xerr)?;
    // Parse our own visible representation once. Comparing against this baseline
    // prevents defaults/legacy values from changing untouched original fields.
    let bytes = book.save_to_buffer().map_err(xerr)?;
    let mut parsed =
        Xlsx::new(Cursor::new(bytes)).map_err(|_| fail("文件", "无法核对导出的Excel"))?;
    let baseline = parse::read_rules(&mut parsed, true)?;
    let metadata =
        json!({"package":package,"baseline":baseline,"long_keys":long.keys().collect::<Vec<_>>()})
            .to_string();
    let parts = chunks(&metadata);
    if parts.len() > MAX_ROWS {
        return Err(fail(BACKUP, "配置数据超过10000分段限制"));
    }
    let ws = book.add_worksheet();
    ws.set_name(BACKUP).map_err(xerr)?;
    ws.set_hidden(true);
    ws.write_string(0, 0, VERSION).map_err(xerr)?;
    for (index, part) in parts.iter().enumerate() {
        ws.write_string(index as u32 + 1, 0, part).map_err(xerr)?;
    }
    let bytes = book.save_to_buffer().map_err(xerr)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(fail("文件", "导出文件超过16MiB导入限制"));
    }
    // Apply exactly the same ZIP/XML resource limits before any target is written.
    parse::check_generated(&bytes)?;
    Ok(bytes)
}

fn write_long(book: &mut Workbook, values: &BTreeMap<String, String>) -> Result<(), XlsxError> {
    let ws = book.add_worksheet();
    ws.set_name(LONG)?;
    ws.set_freeze_panes(1, 2)?;
    ws.set_column_width(0, 52.0)?;
    ws.set_column_width(2, 90.0)?;
    for (column, heading) in ["引用", "分段序号", "JSON文本分段"].iter().enumerate() {
        ws.write_string(0, column as u16, *heading)?;
    }
    let mut row = 1;
    for (key, value) in values {
        for (part, text) in chunks(&json!(value).to_string()).iter().enumerate() {
            ws.write_string(row, 0, key)?;
            ws.write_string(row, 1, (part + 1).to_string())?;
            ws.write_string(row, 2, text)?;
            row += 1;
        }
    }
    Ok(())
}

pub(super) fn long_values<RS: Read + Seek>(
    book: &mut Xlsx<RS>,
) -> Result<BTreeMap<String, String>, Diagnostic> {
    if !book.sheet_names().iter().any(|name| name == LONG) {
        return Ok(BTreeMap::new());
    }
    let range = book
        .worksheet_range(LONG)
        .map_err(|_| fail(LONG, "无法读取长内容"))?;
    let mut parts: BTreeMap<String, BTreeMap<usize, String>> = BTreeMap::new();
    for row in range.rows().skip(1) {
        if row.iter().all(|value| value == &Data::Empty) {
            continue;
        }
        let key = string(row.first(), LONG)?;
        let number = string(row.get(1), LONG)?
            .parse::<usize>()
            .map_err(|_| fail(LONG, "分段序号必须为正整数"))?;
        if number == 0 || number > MAX_ROWS || row.iter().skip(3).any(|value| value != &Data::Empty)
        {
            return Err(fail(LONG, "长内容分段结构错误"));
        }
        if parts
            .entry(key)
            .or_default()
            .insert(number, string(row.get(2), LONG)?)
            .is_some()
        {
            return Err(fail(LONG, "分段序号重复"));
        }
    }
    let mut values = BTreeMap::new();
    for (key, parts) in parts {
        if parts.keys().copied().ne(1..=parts.len()) {
            return Err(fail(LONG, "长内容分段缺失"));
        }
        let value: String = serde_json::from_str(&parts.into_values().collect::<String>())
            .map_err(|_| fail(LONG, "长内容必须拼接成合法JSON文本字符串"))?;
        values.insert(key, value);
    }
    Ok(values)
}
fn string(value: Option<&Data>, sheet: &str) -> Result<String, Diagnostic> {
    match value {
        Some(Data::String(value)) => Ok(value.clone()),
        _ => Err(fail(sheet, "配置单元格必须为文本；请勿删除或改变分段结构")),
    }
}

/// `bool` distinguishes old rules-only templates from complete configurations.
pub fn read_config(path: &Path) -> Result<(ImportReport, bool), Vec<Diagnostic>> {
    read_inner(path).map_err(|diagnostic| vec![diagnostic])
}
fn read_inner(path: &Path) -> Result<(ImportReport, bool), Diagnostic> {
    let mut book = parse::open_book(path)?;
    if !book.sheet_names().iter().any(|name| name == BACKUP) {
        if book
            .sheet_names()
            .iter()
            .any(|name| name == SETTINGS || name == LONG)
        {
            return Err(fail(
                BACKUP,
                "当前配置Excel缺少原始配置页，无法安全保留全局设置和未知字段；请重新导出",
            ));
        }
        let rules = parse::read_rules(&mut book, false)?;
        return Ok((
            ImportReport {
                config: Config {
                    rules,
                    ..Config::default()
                },
                items: vec![],
                unresolved: vec![],
            },
            false,
        ));
    }
    let range = book
        .worksheet_range(BACKUP)
        .map_err(|_| fail(BACKUP, "无法读取原始配置"))?;
    if range.get_value((0, 0)) != Some(&Data::String(VERSION.into())) {
        return Err(fail(BACKUP, "配置Excel版本不兼容"));
    }
    let mut source = String::new();
    for row in range.rows().skip(1) {
        if row.iter().skip(1).any(|value| value != &Data::Empty) {
            return Err(fail(BACKUP, "原始配置含多余列"));
        }
        source.push_str(&string(row.first(), BACKUP)?);
        if source.len() > 64 * 1024 * 1024 {
            return Err(fail(BACKUP, "原始配置过大"));
        }
    }
    let metadata: Value =
        serde_json::from_str(&source).map_err(|_| fail(BACKUP, "原始配置格式错误"))?;
    let expected_long: Vec<String> = serde_json::from_value(metadata["long_keys"].clone())
        .map_err(|_| fail(BACKUP, "缺少长内容索引"))?;
    let long = long_values(&mut book)?;
    if expected_long.iter().any(|key| !long.contains_key(key)) {
        return Err(fail(
            LONG,
            "长内容引用缺失；请保留全部分段，避免丢失原始内容",
        ));
    }
    let mut package = metadata
        .get("package")
        .cloned()
        .ok_or_else(|| fail(BACKUP, "缺少配置包"))?;
    let baseline: Vec<Rule> = serde_json::from_value(metadata["baseline"].clone())
        .map_err(|_| fail(BACKUP, "缺少指令基线"))?;
    let baseline: BTreeMap<_, _> = baseline
        .into_iter()
        .map(|rule| (rule.id.clone(), rule))
        .collect();
    let mut config: Config = serde_json::from_value(package["config"].clone())
        .map_err(|_| fail(BACKUP, "原始配置字段错误"))?;
    let originals: BTreeMap<_, _> = config
        .rules
        .iter()
        .map(|rule| (rule.id.clone(), rule.clone()))
        .collect();
    let current = parse::read_rules(&mut book, true)?;
    let current_ids: BTreeSet<_> = current.iter().map(|rule| rule.id.as_str()).collect();
    if baseline.keys().any(|id| !current_ids.contains(id.as_str()))
        && current.iter().any(|rule| !baseline.contains_key(&rule.id))
    {
        return Err(fail("指令", "不能同时删除原编号并添加新编号；为保留未知字段及迁移数据，请在应用中修改编号，或分两次导入删除与新增"));
    }
    let mut rules = vec![];
    for rule in current {
        let value = if let (Some(original), Some(base)) =
            (originals.get(&rule.id), baseline.get(&rule.id))
        {
            let mut value =
                serde_json::to_value(original).map_err(|_| fail(BACKUP, "指令格式错误"))?;
            apply_diff(
                &mut value,
                &serde_json::to_value(base).map_err(|_| fail(BACKUP, "基线格式错误"))?,
                &serde_json::to_value(&rule).map_err(|_| fail("指令", "指令格式错误"))?,
            );
            value
        } else {
            serde_json::to_value(&rule).map_err(|_| fail("指令", "指令格式错误"))?
        };
        rules.push(
            serde_json::from_value(value).map_err(|_| fail("指令", "修改后的指令字段不兼容"))?,
        );
    }
    config.rules = rules;
    package["config"] = serde_json::to_value(config).map_err(|_| fail(BACKUP, "无法组合配置"))?;
    let range = book
        .worksheet_range(SETTINGS)
        .map_err(|_| fail(SETTINGS, "缺少全局设置工作表"))?;
    if range.get_value((0, 0)) != Some(&Data::String("配置字段".into()))
        || range.get_value((0, 1)) != Some(&Data::String("JSON值".into()))
    {
        return Err(fail(SETTINGS, "设置表头错误"));
    }
    let expected = fields(&package["config"]);
    let mut seen = BTreeSet::new();
    for row in range.rows().skip(1) {
        if row.iter().all(|value| value == &Data::Empty) {
            continue;
        }
        let key = string(row.first(), SETTINGS)?;
        let key = long.get(&key).unwrap_or(&key);
        if !expected.contains_key(key)
            || !seen.insert(key.clone())
            || row.iter().skip(2).any(|value| value != &Data::Empty)
        {
            return Err(fail(SETTINGS, "设置字段重复或无法识别；请保留原字段名"));
        }
        let text = string(row.get(1), SETTINGS)?;
        let text = long.get(&text).unwrap_or(&text);
        let value: Value = serde_json::from_str(text)
            .map_err(|_| fail(SETTINGS, "设置值必须为合法JSON；文本请保留双引号"))?;
        *package["config"]
            .pointer_mut(key)
            .ok_or_else(|| fail(SETTINGS, "找不到设置字段"))? = value;
    }
    if seen.len() != expected.len() {
        return Err(fail(SETTINGS, "设置字段缺失；请保留所有设置行"));
    }
    let report = crate::config_package::import(&package.to_string()).map_err(|errors| {
        errors
            .into_iter()
            .next()
            .unwrap_or_else(|| fail(BACKUP, "配置导入失败"))
    })?;
    Ok((report, true))
}

fn apply_diff(original: &mut Value, baseline: &Value, current: &Value) {
    if baseline == current {
        return;
    }
    if let (Some(original), Some(base), Some(current)) = (
        original.as_object_mut(),
        baseline.as_object(),
        current.as_object(),
    ) {
        for (key, value) in current {
            if let Some(base) = base.get(key) {
                apply_diff(original.entry(key).or_insert(Value::Null), base, value);
            } else {
                original.insert(key.clone(), value.clone());
            }
        }
        for key in base.keys().filter(|key| !current.contains_key(*key)) {
            original.remove(key);
        }
    } else {
        *original = current.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quicker_domain::*;
    use std::io::Write;

    fn rule(id: &str, action: Action) -> Rule {
        Rule {
            id: id.into(),
            match_spec: MatchSpec {
                value: format!("000-{id}"),
                ..MatchSpec::default()
            },
            action,
            ..Rule::default()
        }
    }
    fn read_bytes(bytes: &[u8]) -> Result<(ImportReport, bool), Vec<Diagnostic>> {
        let path =
            std::env::temp_dir().join(format!("quicktext-config-{}.xlsx", uuid::Uuid::new_v4()));
        std::fs::write(&path, bytes).unwrap();
        let result = read_config(&path);
        std::fs::remove_file(&path).unwrap();
        result
    }
    fn edit(bytes: &[u8], sheet: &str, coord: &str, value: &str, formula: bool) -> Vec<u8> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..archive.len() {
            let mut file = archive.by_index(index).unwrap();
            let name = file.name().unwrap().to_string();
            let mut data = Vec::new();
            file.read_to_end(&mut data).unwrap();
            if name == sheet {
                let mut xml = String::from_utf8(data).unwrap();
                let start = xml.find(&format!("<c r=\"{coord}\"")).unwrap();
                let end = start + xml[start..].find("</c>").unwrap() + 4;
                let cell = if formula {
                    format!("<c r=\"{coord}\"><f>1+1</f><v>2</v></c>")
                } else {
                    format!("<c r=\"{coord}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>", value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"))
                };
                xml.replace_range(start..end, &cell);
                data = xml.into_bytes();
            }
            writer
                .start_file(
                    name,
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(&data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn excel_current_config_roundtrip_preserves_complete_native_data() {
        let long = "=0007+中文\r\n🎉\u{000b}_x000A_".repeat(5000);
        let mut config = Config {
            enabled: false,
            revision: 37,
            ..Config::default()
        };
        config.globals.word_separators = ",中\n".into();
        config.extra.insert(
            "future/root~key".into(),
            json!({"arbitrary":[true,9007199254740993u64]}),
        );
        config
            .globals
            .extra
            .insert("future_setting".into(), json!({"text":"值"}));
        let mut text = rule(
            "text",
            Action::Text {
                method: OutputMethod::Paste,
                source: ContentSource::Inline { value: long },
            },
        );
        text.extra
            .insert("future_rule".into(), json!({"unknown":[1,2]}));
        text.notification.message = "\n原样保留".into();
        text.parameter = ParameterSource::Fixed {
            value: "0001=+".into(),
        };
        config.rules.push(text);
        let mut step = Step::new(
            Operation::Reusable {
                action_id: "text".into(),
                parameters: [("n".into(), json!(3))].into(),
            },
            SourceSpan {
                line: 17,
                column: 9,
                length: 3,
            },
        );
        step.target = TargetPolicy::Declared {
            process: "notepad.exe".into(),
        };
        let plan = ActionPlan {
            source: Some("原始脚本\r\n".into()),
            dialect: Dialect::Inputscript,
            steps: vec![step],
            ..ActionPlan::default()
        };
        let mut replay = rule("replay", Action::Replay { plan, speed: 1.5 });
        replay.enabled = false;
        config.rules.push(replay);
        let mut legacy = rule(
            "legacy",
            Action::Text {
                method: OutputMethod::Keys,
                source: ContentSource::File {
                    path: "".into(),
                    encoding: Encoding::Gbk,
                },
            },
        );
        legacy.enabled = false;
        legacy.match_spec.value.clear();
        legacy.terminator = Some(Terminator {
            keys: vec!["semicolon".into(), "space".into(), "space".into()],
            consume: false,
        });
        config.rules.push(legacy);
        for (index, raw) in [
            json!({"id":"","action":{"type":"future"}}),
            json!({"id":"future","action":{"type":"future"}}),
            json!({"id":"future","action":{"type":"another_future"}}),
            json!("raw future entry"),
        ]
        .into_iter()
        .enumerate()
        {
            config.unreadable_rules.push(UnreadableRule {
                index: index * 2,
                raw,
            });
        }
        let unresolved = vec![MigrationItem {
            rule_id: "future".into(),
            status: MigrationStatus::NeedsRewrite,
            diagnostics: vec![],
            original: json!({"Original":"原始数据"}),
        }];
        let snapshot = export_package_with_unresolved(&config, unresolved.clone()).unwrap();
        let bytes = export_config(&snapshot).unwrap();
        let (actual, full) = read_bytes(&bytes).unwrap();
        assert!(full);
        assert_eq!(
            serde_json::to_value(actual.config).unwrap(),
            serde_json::to_value(config).unwrap()
        );
        assert_eq!(
            serde_json::to_value(actual.unresolved).unwrap(),
            serde_json::to_value(unresolved).unwrap()
        );
    }

    #[test]
    fn excel_current_empty_configuration_is_importable() {
        let config = Config::default();
        let (report, full) =
            read_bytes(&export_config(&export_package(&config).unwrap()).unwrap()).unwrap();
        assert!(full);
        assert_eq!(report.config, config);
    }

    #[test]
    fn excel_edits_apply_to_readable_fields_and_keep_unknown_extensions() {
        let mut rule = rule(
            "a",
            Action::Text {
                method: OutputMethod::Paste,
                source: ContentSource::Inline {
                    value: "before".into(),
                },
            },
        );
        rule.extra.insert("future".into(), json!({"keep":true}));
        let config = Config {
            rules: vec![rule],
            ..Config::default()
        };
        let bytes = export_config(&export_package(&config).unwrap()).unwrap();
        let bytes = edit(
            &bytes,
            "xl/worksheets/sheet2.xml",
            "D2",
            "=after\n0001",
            false,
        );
        let bytes = edit(&bytes, "xl/worksheets/sheet2.xml", "B2", "changed", false);
        let actual = read_bytes(&bytes).unwrap().0.config;
        assert_eq!(actual.rules[0].extra, config.rules[0].extra);
        assert_eq!(actual.rules[0].match_spec.value, "changed");
        assert!(
            matches!(&actual.rules[0].action, Action::Text { source: ContentSource::Inline { value }, .. } if value == "=after\n0001")
        );
        let bytes = edit(&bytes, "xl/worksheets/sheet2.xml", "A2", "new-id", false);
        assert!(read_bytes(&bytes).unwrap_err()[0]
            .message
            .contains("不能同时删除原编号"));
    }

    #[test]
    fn excel_edits_restore_global_settings_and_complex_action_changes() {
        let mut complex = rule(
            "a",
            Action::Replay {
                plan: ActionPlan::default(),
                speed: 1.5,
            },
        );
        complex.enabled = false;
        complex.extra.insert("future".into(), json!("保留"));
        let config = Config {
            rules: vec![complex],
            ..Config::default()
        };
        let bytes = export_config(&export_package(&config).unwrap()).unwrap();
        let mut book = Xlsx::new(Cursor::new(&bytes)).unwrap();
        let settings_sheet = format!(
            "xl/worksheets/sheet{}.xml",
            book.sheet_names()
                .iter()
                .position(|name| name == SETTINGS)
                .unwrap()
                + 1
        );
        let settings = book.worksheet_range(SETTINGS).unwrap();
        assert_eq!(
            settings.get_value((1, 0)),
            Some(&Data::String("/enabled".into()))
        );
        let bytes = edit(&bytes, &settings_sheet, "B2", "false", false);
        let bytes = edit(&bytes, "xl/worksheets/sheet2.xml", "AB2", &json!({"type":"open","target":"https://example.com","args":["0001"],"working_directory":null}).to_string(), false);
        let actual = read_bytes(&bytes).unwrap().0.config;
        assert!(!actual.enabled);
        assert_eq!(actual.rules[0].extra, config.rules[0].extra);
        assert!(
            matches!(&actual.rules[0].action, Action::Open { target, .. } if target == "https://example.com")
        );
        assert!(!actual.rules[0].enabled);
    }

    #[test]
    fn excel_current_config_retains_formula_and_sparse_xml_guards() {
        let config = Config {
            rules: vec![rule("a", Action::None)],
            ..Config::default()
        };
        let bytes = export_config(&export_package(&config).unwrap()).unwrap();
        let bytes = edit(&bytes, "xl/worksheets/sheet2.xml", "B2", "ignored", true);
        assert!(read_bytes(&bytes).is_err());
    }

    #[test]
    fn excel_current_config_missing_backup_is_not_imported_as_rules_only() {
        let mut book = template::build().unwrap();
        book.add_worksheet().set_name(SETTINGS).unwrap();
        let error = read_bytes(&book.save_to_buffer().unwrap()).unwrap_err();
        assert!(error[0].message.contains("缺少原始配置页"));
    }
}
