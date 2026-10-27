use super::*;
use calamine::{Data, Reader, Xlsx};
use quicker_domain::Rule;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Seek},
    path::Path,
};
type E = Diagnostic;
struct Row {
    sheet: String,
    n: usize,
    cells: BTreeMap<String, String>,
    used: BTreeSet<String>,
}
impl Row {
    fn get(&mut self, k: &str) -> String {
        self.used.insert(k.into());
        self.cells.get(k).cloned().unwrap_or_default()
    }
    fn err(&self, k: &str, msg: &str) -> E {
        error(&self.sheet, self.n, k, msg)
    }
    fn choice(&mut self, k: &str, default: &str, choices: &[(&str, &str)]) -> Result<String, E> {
        let s = self.get(k);
        if s.is_empty() {
            return Ok(default.into());
        }
        choices
            .iter()
            .find(|(a, b)| s == *a || s == *b)
            .map(|p| p.1.to_string())
            .ok_or_else(|| self.err(k, "未知选项，请使用下拉菜单"))
    }
    fn boolean(&mut self, k: &str, default: bool) -> Result<bool, E> {
        Ok(self.choice(
            k,
            if default { "true" } else { "false" },
            &[("是", "true"), ("否", "false")],
        )? == "true")
    }
    fn number(&mut self, k: &str, default: f64) -> Result<f64, E> {
        let s = self.get(k);
        if s.is_empty() {
            return Ok(default);
        }
        s.parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .ok_or_else(|| self.err(k, "必须填写有限数值"))
    }
    fn uint(&mut self, k: &str, default: u64) -> Result<u64, E> {
        let s = self.get(k);
        if s.is_empty() {
            return Ok(default);
        }
        s.parse().map_err(|_| self.err(k, "必须填写非负整数"))
    }
    fn required(&mut self, k: &str) -> Result<String, E> {
        let s = self.get(k);
        if s.trim().is_empty() {
            Err(self.err(k, "必填项不能为空"))
        } else {
            Ok(s)
        }
    }
    fn finish(&self) -> Result<(), E> {
        if let Some((k, _)) = self
            .cells
            .iter()
            .find(|(k, v)| !v.is_empty() && !self.used.contains(*k))
        {
            Err(self.err(k, "此字段不适用于当前类型，不能忽略；请清空或选择正确类型"))
        } else {
            Ok(())
        }
    }
}
#[cfg(test)]
pub fn read(path: &Path) -> Result<Vec<Rule>, Vec<Diagnostic>> {
    let mut book = open_book(path).map_err(|e| vec![e])?;
    read_rules(&mut book, false).map_err(|e| vec![e])
}
pub(super) fn open_book(path: &Path) -> Result<Xlsx<std::io::BufReader<File>>, E> {
    let mut file = File::open(path).map_err(|_| error("文件", 1, "文件", "无法读取文件"))?;
    check_zip(
        file.try_clone()
            .map_err(|_| error("文件", 1, "文件", "无法读取文件"))?,
    )?;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|_| error("文件", 1, "文件", "无法读取文件"))?;
    let mut book = Xlsx::new(std::io::BufReader::new(file))
        .map_err(|_| error("文件", 1, "文件", "不是有效的xlsx文件"))?;
    for name in book.sheet_names().to_owned() {
        let formulas = book
            .worksheet_formula(&name)
            .map_err(|_| error(&name, 1, "公式", "无法检查公式"))?;
        if let Some(((r, c), _)) = formulas
            .cells()
            .filter(|(_, _, s)| !s.is_empty())
            .map(|(r, c, s)| ((r, c), s))
            .next()
        {
            let (sr, sc) = formulas.start().unwrap_or((0, 0));
            return Err(error(
                &name,
                r + sr as usize + 1,
                &format!("第{}列", c + sc as usize + 1),
                "禁止导入公式，请粘贴为值",
            ));
        }
    }
    Ok(book)
}
pub(super) fn read_rules<RS: Read + Seek>(
    book: &mut Xlsx<RS>,
    full_config: bool,
) -> Result<Vec<Rule>, E> {
    let version = book
        .worksheet_range("选项")
        .map_err(|_| error("选项", 1, "版本", "缺少模板版本信息"))?;
    if version.get_value((0, 0)) != Some(&Data::String(TEMPLATE_VERSION.into())) {
        return Err(error("选项", 1, "版本", "模板版本不兼容，请重新导出模板"));
    }
    let mut rules = rows(book, "指令", RULE_HEADERS)?;
    let mut steps = rows(book, "操作步骤", STEP_HEADERS)?;
    let mut params = rows(book, "调用参数", PARAM_HEADERS)?;
    let mut captures = rows(book, "捕获参数", CAPTURE_HEADERS)?;
    if rules.is_empty() && !full_config {
        return Err(error("指令", 2, "缩写或正则*", "模板中没有可导入的指令"));
    }
    let mut ids = BTreeMap::new();
    let mut keys = vec![];
    for row in &mut rules {
        let key = row.get("指令编号");
        if !key.is_empty() && ids.contains_key(&key) {
            return Err(row.err("指令编号", "编号重复"));
        }
        let id = if !key.is_empty() {
            key.clone()
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        if !key.is_empty() {
            ids.insert(key.clone(), id.clone());
        }
        keys.push((key, id));
    }
    let mut step_map: BTreeMap<String, BTreeMap<u64, (Value, usize)>> = BTreeMap::new();
    let mut parameter_map: BTreeMap<(String, u64), BTreeMap<String, Value>> = BTreeMap::new();
    for row in &mut params {
        let parent = row.required("指令编号*")?;
        check_parent(row, &parent, &ids)?;
        let order = row.uint("步骤顺序（空为主动作）", 0)?;
        let name = row.required("参数名*")?;
        let kind = row.choice(
            "值类型",
            "string",
            &[
                ("文本", "string"),
                ("数字", "number"),
                ("布尔", "boolean"),
                ("空值", "null"),
                ("JSON", "json"),
            ],
        )?;
        let raw = row.get("值");
        let value = match kind.as_str() {
            "string" => json!(raw),
            "null" => {
                if !raw.is_empty() {
                    return Err(row.err("值", "空值类型请保持值为空"));
                }
                Value::Null
            }
            "number" => {
                let v: Value =
                    serde_json::from_str(&raw).map_err(|_| row.err("值", "必须填写JSON数字"))?;
                if !v.is_number() {
                    return Err(row.err("值", "必须填写数字"));
                }
                v
            }
            "boolean" => match raw.as_str() {
                "true" | "是" => json!(true),
                "false" | "否" => json!(false),
                _ => return Err(row.err("值", "布尔值必须为true或false")),
            },
            _ => serde_json::from_str(&raw).map_err(|_| row.err("值", "JSON格式错误"))?,
        };
        if value.is_array() || value.is_object() {
            return Err(row.err(
                "值",
                "复用参数仅支持文本、数字、布尔或空值，不能使用数组或对象",
            ));
        }
        validate_numbers(&value)
            .map_err(|_| row.err("值", "整数超出JavaScript安全范围，请改用文本保存"))?;
        let map = parameter_map.entry((parent, order)).or_default();
        if map.insert(name, value).is_some() {
            return Err(row.err("参数名*", "同一调用参数名重复"));
        }
        row.finish()?;
    }
    let mut capture_map: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
    for row in &mut captures {
        let parent = row.required("指令编号*")?;
        check_parent(row, &parent, &ids)?;
        let name = row.required("参数名*")?;
        if name.contains(['&', '=', '$', '{', '}']) {
            return Err(row.err("参数名*", "参数名不能包含 & = $ { }"));
        }
        let n = row.uint("捕获组序号*", 0)?;
        if n == 0 || n > 99 {
            return Err(row.err("捕获组序号*", "捕获组序号必须为1到99"));
        }
        let entries = capture_map.entry(parent).or_default();
        if entries.iter().any(|p| p.0 == name) {
            return Err(row.err("参数名*", "参数名重复"));
        }
        entries.push((name, n));
        row.finish()?;
    }
    for row in &mut steps {
        let parent = row.required("指令编号*")?;
        check_parent(row, &parent, &ids)?;
        let order = row.uint("顺序*", 0)?;
        if order == 0 || order > 10_000 {
            return Err(row.err("顺序*", "顺序必须为1到10000整数"));
        }
        let operation = operation(row, &ids, &mut parameter_map, &parent, order)?;
        validate_parameters(&operation)
            .map_err(|_| row.err("高级操作JSON", "复用参数仅支持文本、数字、布尔或空值"))?;
        validate_numbers(&operation)
            .map_err(|_| row.err("高级操作JSON", "整数超出JavaScript安全范围"))?;
        let policy = row.choice(
            "目标窗口",
            "locked",
            &[("锁定开始窗口", "locked"), ("指定程序", "declared")],
        )?;
        let target = if policy == "declared" {
            json!({"type":"declared","process":row.required("允许程序")?})
        } else {
            json!({"type":"locked"})
        };
        let value = json!({"operation":operation,"target":target});
        let map = step_map.entry(parent).or_default();
        if map.insert(order, (value, row.n)).is_some() {
            return Err(row.err("顺序*", "同一指令的步骤顺序重复"));
        }
        row.finish()?;
    }
    let mut result = vec![];
    for (row, (key, id)) in rules.iter_mut().zip(keys) {
        let match_value = row.required("缩写或正则*")?;
        let kind = row.choice(
            "匹配方式",
            "literal",
            &[("普通缩写", "literal"), ("正则表达式", "regex")],
        )?;
        let parameter_kind = row.choice(
            "参数来源",
            "default_match",
            &[
                ("匹配内容", "default_match"),
                ("固定参数", "fixed"),
                ("第一个捕获组", "first_capture"),
                ("捕获模板", "capture_template"),
                ("空值", "null"),
            ],
        )?;
        let parameter = match parameter_kind.as_str() {
            "fixed" => json!({"type":"fixed","value":row.get("固定参数或捕获模板")}),
            "capture_template" => {
                let raw = row.get("固定参数或捕获模板");
                let mapped = capture_map.remove(&key).unwrap_or_default();
                if !raw.is_empty() && !mapped.is_empty() {
                    return Err(row.err("固定参数或捕获模板", "不能同时填写捕获参数子表和原始模板"));
                }
                let template = if mapped.is_empty() {
                    raw
                } else {
                    mapped
                        .iter()
                        .map(|(k, n)| format!("{k}={{${n}}}"))
                        .collect::<Vec<_>>()
                        .join("&")
                };
                json!({"type":"capture_template","template":template})
            }
            _ => json!({"type":parameter_kind}),
        };
        let terminator_key = row.choice(
            "结束键",
            "inherit",
            &[
                ("继承全局", "inherit"),
                ("空格", "space"),
                ("分号", "semicolon"),
                ("空格和分号", "both"),
                ("Tab（旧设置）", "tab"),
                ("Enter（旧设置）", "enter"),
            ],
        )?;
        let consume = row.boolean("吞掉结束键", true)?;
        let terminator = if terminator_key == "inherit" {
            if row.cells.get("吞掉结束键").is_some_and(|s| !s.is_empty()) {
                return Err(row.err("吞掉结束键", "继承全局时请留空；先选择具体结束键"));
            }
            Value::Null
        } else {
            let keys = if terminator_key == "both" {
                vec!["space", "semicolon"]
            } else {
                vec![terminator_key.as_str()]
            };
            json!({"keys":keys,"consume":consume})
        };
        let ime = row.choice(
            "输入法清除",
            "inherit",
            &[
                ("继承全局", "inherit"),
                ("Escape", "escape"),
                ("Backspace", "backspace"),
            ],
        )?;
        let action = action(row, &ids, &mut step_map, &mut parameter_map, &key)?;
        if !full_config {
            validate_parameters(&action)
                .map_err(|_| row.err("高级动作JSON", "复用参数仅支持文本、数字、布尔或空值"))?;
        }
        let group = row.get("分组");
        let trigger_default = if kind == "regex" || !cfg!(windows) {
            "terminator"
        } else {
            "immediate"
        };
        let value = json!({"id":id,"enabled":row.boolean("启用",true)?,"group":if group.is_empty(){"默认"}else{&group},"description":row.get("说明"),"ignore_case":row.boolean("忽略大小写",false)?,"match":{"kind":kind,"value":match_value},"trigger_mode":row.choice("触发方式",trigger_default,&[("结束键触发","terminator"),("立即触发","immediate")])?,"terminator":terminator,"ime_erasure":if ime=="inherit"{Value::Null}else{json!(ime)},"parameter":parameter,"notification":{"enabled":row.boolean("通知",false)?,"message":row.get("通知内容")},"action":action});
        if !full_config {
            validate_numbers(&value).map_err(|_| {
                row.err("高级动作JSON", "整数超出JavaScript安全范围，请改用文本保存")
            })?;
        }
        row.finish()?;
        let rule: Rule = serde_json::from_value(value)
            .map_err(|_| row.err("动作类型", "字段类型或高级JSON与当前版本不兼容"))?;
        if !full_config {
            validate_rule(&rule, row, &steps, &key)?;
        }
        result.push(rule);
    }
    if let Some((_, (_, n))) = step_map.values().flat_map(|v| v.iter()).next() {
        return Err(error(
            "操作步骤",
            *n,
            "指令编号*",
            "父指令不是操作步骤或重放，存在多余子行",
        ));
    }
    if !parameter_map.is_empty() {
        return Err(error(
            "调用参数",
            2,
            "指令编号*",
            "存在没有对应复用动作的参数子行",
        ));
    }
    if !capture_map.is_empty() {
        return Err(error(
            "捕获参数",
            2,
            "指令编号*",
            "父指令未选择捕获模板，存在多余子行",
        ));
    }
    Ok(result)
}
pub(super) fn check_generated(bytes: &[u8]) -> Result<(), E> {
    check_zip(std::io::Cursor::new(bytes))
}
fn check_zip(mut file: impl Read + Seek) -> Result<(), E> {
    let fail = || {
        error(
            "文件",
            1,
            "大小",
            "文件过大或压缩结构不安全（16MiB文件/64MiB展开/256条目）",
        )
    };
    if file.seek(std::io::SeekFrom::End(0)).map_err(|_| fail())? > 16 * 1024 * 1024 {
        return Err(fail());
    }
    file.seek(std::io::SeekFrom::Start(0)).map_err(|_| fail())?;
    let mut zip = zip::ZipArchive::new(&mut file).map_err(|_| fail())?;
    if zip.len() > 256 {
        return Err(fail());
    }
    let mut total = 0u64;
    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(|_| fail())?;
        total = total.checked_add(entry.size()).ok_or_else(fail)?;
        if total > 64 * 1024 * 1024 {
            return Err(fail());
        }
    }
    // Fully bounded decompression catches false size headers before calamine sees them.
    let mut actual = 0u64;
    let mut buffer = [0u8; 8192];
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|_| fail())?;
        loop {
            let n = entry.read(&mut buffer).map_err(|_| fail())?;
            if n == 0 {
                break;
            }
            actual += n as u64;
            if actual > 64 * 1024 * 1024 {
                return Err(fail());
            }
        }
    }
    // Inspect every worksheet before calamine allocates a dense Range. Even an
    // otherwise tiny XLSX may claim XFD1048576 through a sparse cell/dimension.
    let mut dense_total = 0usize;
    validate_root_relationships(
        zip.by_name("_rels/.rels")
            .map_err(|_| error("文件", 1, "根关系", "缺少xlsx根关系"))?,
    )?;
    let labels = worksheet_labels(&mut zip)?;
    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(|_| fail())?;
        let entry_name = entry.name().map_err(|_| fail())?.into_owned();
        if entry_name.ends_with(".xml") || entry_name.ends_with(".rels") {
            let label = labels
                .get(&entry_name)
                .map(String::as_str)
                .unwrap_or("工作表XML");
            inspect_xml(entry, &mut dense_total, label)?;
        }
    }
    Ok(())
}
fn validate_root_relationships(input: impl Read) -> Result<(), E> {
    use quick_xml::events::Event;
    let fail = || {
        error(
            "文件",
            1,
            "根关系",
            "必须存在唯一的标准内部工作簿关系，目标仅支持xl/workbook.xml",
        )
    };
    let mut reader = quick_xml::Reader::from_reader(std::io::BufReader::new(input));
    let mut buffer = Vec::new();
    let mut count = 0;
    loop {
        match reader.read_event_into(&mut buffer).map_err(|_| fail())? {
            Event::Start(tag) | Event::Empty(tag)
                if tag.local_name().as_ref() == b"Relationship" =>
            {
                let mut attrs = BTreeMap::new();
                for attr in tag.attributes() {
                    let attr = attr.map_err(|_| fail())?;
                    let key = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
                    let value = attr
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map_err(|_| fail())?
                        .into_owned();
                    if attrs.insert(key, value).is_some() {
                        return Err(fail());
                    }
                }
                if attrs
                    .get("Type")
                    .is_some_and(|v| v.ends_with("officeDocument"))
                {
                    count += 1;
                    if count!=1 || attrs.get("Type").map(String::as_str)!=Some("http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument")
                        || !matches!(attrs.get("Target").map(String::as_str),Some("xl/workbook.xml"|"/xl/workbook.xml"))
                        || attrs.get("TargetMode").is_some_and(|v|v!="Internal") {
                        return Err(fail());
                    }
                }
            }
            Event::DocType(_) => return Err(fail()),
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if count != 1 {
        return Err(fail());
    }
    Ok(())
}
fn worksheet_labels<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Result<BTreeMap<String, String>, E> {
    let mut ids = BTreeMap::new();
    let mut parts = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for filename in ["xl/workbook.xml", "xl/_rels/workbook.xml.rels"] {
        let mut xml = String::new();
        zip.by_name(filename)
            .map_err(|_| error("文件", 1, "结构", "缺少xlsx工作簿结构"))?
            .read_to_string(&mut xml)
            .map_err(|_| error("文件", 1, "结构", "工作簿结构不是有效UTF-8"))?;
        let mut reader = quick_xml::Reader::from_str(&xml);
        loop {
            match reader
                .read_event()
                .map_err(|_| error("文件", 1, "结构", "工作簿XML格式错误"))?
            {
                quick_xml::events::Event::Start(tag) | quick_xml::events::Event::Empty(tag) => {
                    let local = tag.local_name();
                    if local.as_ref() != b"sheet" && local.as_ref() != b"Relationship" {
                        continue;
                    }
                    let mut attrs = BTreeMap::new();
                    for a in tag.attributes() {
                        let a = a.map_err(|_| error("文件", 1, "结构", "属性格式错误"))?;
                        let key = String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned();
                        let value = a
                            .decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                reader.decoder(),
                            )
                            .map_err(|_| error("文件", 1, "结构", "属性格式错误"))?
                            .into_owned();
                        if attrs.insert(key, value).is_some() {
                            return Err(error(
                                "文件",
                                1,
                                "工作表关系",
                                "关联属性重复或存在命名空间冲突",
                            ));
                        }
                    }
                    if local.as_ref() == b"sheet" {
                        if let (Some(id), Some(name)) = (attrs.get("id"), attrs.get("name")) {
                            if ids.insert(id.clone(), name.clone()).is_some() {
                                return Err(error("文件", 1, "工作表关系", "工作表关联重复"));
                            }
                        } else {
                            return Err(error("文件", 1, "工作表关系", "工作表缺少名称或关联编号"));
                        }
                    } else if let Some(id) = attrs.get("Id").filter(|id| ids.contains_key(*id)) {
                        let path = validated_sheet_part(&attrs)?;
                        if !seen.insert(id.clone()) || parts.insert(path, id.clone()).is_some() {
                            return Err(error("文件", 1, "工作表关系", "工作表关联重复"));
                        }
                    }
                }
                quick_xml::events::Event::Eof => break,
                _ => {}
            }
        }
    }
    if seen.len() != ids.len() {
        return Err(error(
            "文件",
            1,
            "工作表关系",
            "工作表缺少对应的有效XML关联",
        ));
    }
    Ok(parts
        .into_iter()
        .filter_map(|(path, id)| ids.get(&id).map(|name| (path, name.clone())))
        .collect())
}
fn validated_sheet_part(attrs: &BTreeMap<String, String>) -> Result<String, E> {
    let fail = || {
        error(
            "文件",
            1,
            "工作表关系",
            "只支持标准xlsx普通工作表及XML路径，不支持图表页、对话页或外部关系",
        )
    };
    if attrs.get("Type").map(String::as_str)
        != Some("http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet")
        || attrs.get("TargetMode").is_some_and(|v| v != "Internal")
    {
        return Err(fail());
    }
    let target = attrs.get("Target").ok_or_else(fail)?;
    let standard = target
        .strip_prefix("worksheets/sheet")
        .or_else(|| target.strip_prefix("/xl/worksheets/sheet"));
    if !standard
        .and_then(|s| s.strip_suffix(".xml"))
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(fail());
    }
    Ok(if target.starts_with('/') {
        target.trim_start_matches('/').into()
    } else {
        format!("xl/{target}")
    })
}
fn validate_rule(rule: &Rule, row: &Row, steps: &[Row], parent: &str) -> Result<(), E> {
    let mut active = rule.clone();
    active.enabled = true;
    let config = quicker_domain::Config {
        rules: vec![active],
        ..Default::default()
    };
    if let Err(diags) = quicker_domain::compile_config(&config) {
        if let Some(diag) = diags.first() {
            let (field, message) = match diag.code.as_str() {
                "regex_unsupported" => ("缩写或正则*", "正则表达式无效或使用了不支持的语法"),
                "empty_match" => ("缩写或正则*", "正则表达式不能匹配空内容"),
                "capture_missing" | "capture_template" => {
                    ("参数来源", "捕获参数模板无效或引用不存在的捕获组")
                }
                "key" | "key_name" | "key_code" | "key_pair" => {
                    ("按键", "按键名称、编码或按住/松开配对无效")
                }
                "coordinate" => ("X/Y", "坐标无效；百分比必须为0到100且不能同时使用相对坐标"),
                "target" => ("允许程序", "指定目标窗口时必须填写有效程序"),
                "file_path" | "image_path" => {
                    ("正文或目标", "文件或图片路径无效，不能留空或包含空字符")
                }
                _ => ("动作类型", "动作或参数设置无效，请核对所选类型及必填参数"),
            };
            if let Some(index) = diag
                .path
                .split("steps[")
                .nth(1)
                .and_then(|s| s.split(']').next())
                .and_then(|s| s.parse::<usize>().ok())
            {
                let mut origins = steps
                    .iter()
                    .filter(|s| s.cells.get("指令编号*").is_some_and(|p| p == parent))
                    .collect::<Vec<_>>();
                origins.sort_by_key(|s| {
                    s.cells
                        .get("顺序*")
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(0)
                });
                if let Some(origin) = origins.get(index) {
                    return Err(origin.err(field, message));
                }
            }
            return Err(row.err(field, message));
        }
    }
    Ok(())
}
fn inspect_xml(input: impl Read, dense_total: &mut usize, sheet: &str) -> Result<(), E> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(std::io::BufReader::new(input));
    let mut buffer = Vec::new();
    let mut max_row = 0;
    let mut max_col = 0;
    let mut count = 0usize;
    let mut current_row = 1usize;
    let fail = || {
        error(
            "文件",
            1,
            "工作表范围",
            "XML结构异常或工作表超过10001行、128列、100000个单元格及总计200万网格限制",
        )
    };
    loop {
        match reader.read_event_into(&mut buffer).map_err(|_| fail())? {
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                let local = tag.local_name();
                let name = local.as_ref();
                if name == b"Relationship" {
                    let attrs = tag
                        .attributes()
                        .map(|a| a.map_err(|_| fail()))
                        .collect::<Result<Vec<_>, _>>()?;
                    let is_sheet = attrs
                        .iter()
                        .any(|a| a.key.as_ref() == b"Type" && a.value.ends_with(b"/worksheet"));
                    if is_sheet {
                        let target = attrs
                            .iter()
                            .find(|a| a.key.as_ref() == b"Target")
                            .ok_or_else(fail)?;
                        let text = std::str::from_utf8(&target.value).map_err(|_| fail())?;
                        let standard = text
                            .strip_prefix("worksheets/sheet")
                            .or_else(|| text.strip_prefix("/xl/worksheets/sheet"));
                        if !standard
                            .and_then(|s| s.strip_suffix(".xml"))
                            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                        {
                            return Err(fail());
                        }
                    }
                }
                if name == b"f" {
                    return Err(error(
                        sheet,
                        current_row,
                        "公式",
                        "禁止导入公式，请粘贴为值",
                    ));
                }
                if [b"dimension".as_slice(), b"c", b"row"].contains(&name) {
                    let mut coordinate_found = false;
                    if name == b"c" {
                        count += 1;
                        if count > 100_000 {
                            return Err(fail());
                        }
                    }
                    for attr in tag.attributes() {
                        let attr = attr.map_err(|_| fail())?;
                        if attr.key.local_name().as_ref()
                            == if name == b"dimension" {
                                b"ref".as_slice()
                            } else {
                                b"r".as_slice()
                            }
                        {
                            coordinate_found = true;
                            let text = std::str::from_utf8(&attr.value).map_err(|_| fail())?;
                            for coord in text.split(':') {
                                if name == b"row" {
                                    let r: usize = coord.parse().map_err(|_| fail())?;
                                    if r == 0 || r > MAX_ROWS + 1 {
                                        return Err(fail());
                                    }
                                    max_row = max_row.max(r);
                                    current_row = r;
                                } else {
                                    let mut col = 0usize;
                                    let mut index = 0;
                                    for b in coord.bytes().take_while(|b| b.is_ascii_uppercase()) {
                                        col = col
                                            .checked_mul(26)
                                            .and_then(|n| n.checked_add((b - b'A' + 1) as usize))
                                            .ok_or_else(fail)?;
                                        index += 1;
                                    }
                                    let r: usize = coord[index..].parse().map_err(|_| fail())?;
                                    if col == 0 || col > 128 || r == 0 || r > MAX_ROWS + 1 {
                                        return Err(fail());
                                    }
                                    max_row = max_row.max(r);
                                    max_col = max_col.max(col);
                                    current_row = r;
                                }
                            }
                        }
                    }
                    if !coordinate_found {
                        return Err(fail());
                    }
                }
            }
            Event::DocType(_) => return Err(fail()),
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    *dense_total += max_row * max_col;
    if *dense_total > 2_000_000 {
        return Err(fail());
    }
    Ok(())
}
fn validate_parameters(value: &Value) -> Result<(), ()> {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("reusable") {
                if let Some(Value::Object(params)) = map.get("parameters") {
                    if params.values().any(|v| v.is_object() || v.is_array()) {
                        return Err(());
                    }
                }
            }
            for child in map.values() {
                validate_parameters(child)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                validate_parameters(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn validate_numbers(value: &Value) -> Result<(), ()> {
    match value {
        Value::Number(n) => {
            let f = n.as_f64().ok_or(())?;
            if !f.is_finite() || (f.fract() == 0.0 && f.abs() > 9_007_199_254_740_991.0) {
                return Err(());
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                validate_numbers(v)?;
            }
        }
        Value::Array(values) => {
            for v in values {
                validate_numbers(v)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn rows<RS: Read + Seek>(
    book: &mut Xlsx<RS>,
    sheet: &str,
    headers: &[&str],
) -> Result<Vec<Row>, E> {
    let long_values = super::config::long_values(book)?;
    let range = book
        .worksheet_range(sheet)
        .map_err(|_| error(sheet, 1, "表头", "缺少工作表或无法读取"))?;
    if range.height() > MAX_ROWS + 1 || range.width() > 128 {
        return Err(error(sheet, 1, "范围", "超过10000数据行或128列限制"));
    }
    let mut positions = vec![];
    for h in headers {
        let matches: Vec<_> = (0..range.width())
            .filter(|c| range.get_value((0, *c as u32)) == Some(&Data::String((*h).into())))
            .collect();
        if matches.len() != 1 {
            return Err(error(sheet, 1, h, "缺少列或表头重复；请使用原始模板表头"));
        }
        positions.push(matches[0]);
    }
    let mut out = vec![];
    for r in 1..=range.end().map_or(0, |p| p.0 as usize) {
        let mut cells = BTreeMap::new();
        for (c, h) in positions.iter().zip(headers) {
            let v = match range.get_value((r as u32, *c as u32)) {
                None | Some(Data::Empty) => String::new(),
                Some(Data::String(s)) => long_values.get(s).cloned().unwrap_or_else(|| s.clone()),
                Some(Data::Int(n)) => n.to_string(),
                Some(Data::Float(n)) if n.is_finite() => n.to_string(),
                Some(Data::Bool(b)) => if *b { "是" } else { "否" }.into(),
                _ => {
                    return Err(error(
                        sheet,
                        r + 1,
                        h,
                        "不支持日期、错误或此单元格类型，请填写文本/数值",
                    ))
                }
            };
            cells.insert((*h).into(), v);
        }
        for c in 0..range.width() {
            if !positions.contains(&c)
                && range
                    .get_value((r as u32, c as u32))
                    .is_some_and(|v| v != &Data::Empty)
            {
                return Err(error(
                    sheet,
                    r + 1,
                    "额外列",
                    "发现无法识别的数据列，请勿新增字段",
                ));
            }
        }
        if cells.values().any(|v| !v.trim().is_empty()) {
            out.push(Row {
                sheet: sheet.into(),
                n: r + 1,
                cells,
                used: BTreeSet::new(),
            });
        }
    }
    Ok(out)
}
fn lines(s: String) -> Vec<String> {
    if s.is_empty() {
        vec![]
    } else {
        s.replace("\r\n", "\n")
            .split('\n')
            .map(str::to_string)
            .collect()
    }
}
fn keys(row: &mut Row) -> Result<Value, E> {
    let text = row.required("按键")?;
    let mut result = vec![];
    for key in text.split('+') {
        result.push(key_value(row, key.trim())?)
    }
    Ok(json!(result))
}
fn key_value(row: &Row, key: &str) -> Result<Value, E> {
    if key == "$primary" {
        Ok(json!({"type":"platform_primary"}))
    } else if let Some(raw) = key.strip_prefix("vk:") {
        let code: u16 = raw
            .parse()
            .map_err(|_| row.err("按键", "vk:后必须为1到255整数"))?;
        if !(1..=255).contains(&code) {
            return Err(row.err("按键", "虚拟键码必须为1到255"));
        }
        Ok(json!({"type":"windows_code","code":code}))
    } else if key.is_empty() {
        Err(row.err("按键", "按键名称不能为空"))
    } else {
        Ok(json!({"type":"named","name":key}))
    }
}
fn reference(row: &mut Row, ids: &BTreeMap<String, String>) -> Result<String, E> {
    let target = row.required("复用编号或UUID")?;
    ids.get(&target)
        .cloned()
        .or_else(|| uuid::Uuid::parse_str(&target).ok().map(|id| id.to_string()))
        .ok_or_else(|| row.err("复用编号或UUID", "找不到同表编号，也不是有效现有UUID"))
}
fn check_parent(row: &Row, parent: &str, ids: &BTreeMap<String, String>) -> Result<(), E> {
    if ids.contains_key(parent) {
        Ok(())
    } else {
        Err(row.err("指令编号*", "找不到指令页中的父编号"))
    }
}
fn common(row: &mut Row) -> Result<String, E> {
    row.choice("正文或目标", "", COMMON).and_then(|s| {
        if s.is_empty() {
            Err(row.err("正文或目标", "请选择常用功能"))
        } else {
            Ok(s)
        }
    })
}
fn open(row: &mut Row) -> Result<Value, E> {
    let cwd = row.get("启动文件夹");
    Ok(
        json!({"type":"open","target":row.required("正文或目标")?,"args":lines(row.get("启动参数（每行一项）")),"working_directory":if cwd.is_empty(){Value::Null}else{json!(cwd)}}),
    )
}
fn source(row: &mut Row) -> Result<Value, E> {
    let kind = row.choice(
        "文字来源",
        "inline",
        &[("直接填写", "inline"), ("读取文件", "file")],
    )?;
    if kind == "file" {
        Ok(
            json!({"type":"file","path":row.required("正文或目标")?,"encoding":row.choice("编码","utf8",&[("UTF-8","utf8"),("UTF-16LE","utf16_le"),("UTF-16BE","utf16_be"),("GBK","gbk")])?}),
        )
    } else {
        Ok(json!({"type":"inline","value":row.get("正文或目标")}))
    }
}
fn action(
    row: &mut Row,
    ids: &BTreeMap<String, String>,
    steps: &mut BTreeMap<String, BTreeMap<u64, (Value, usize)>>,
    params: &mut BTreeMap<(String, u64), BTreeMap<String, Value>>,
    parent: &str,
) -> Result<Value, E> {
    let raw = row.get("高级动作JSON");
    if !raw.is_empty() {
        return serde_json::from_str(&raw).map_err(|_| row.err("高级动作JSON", "JSON格式错误"));
    }
    let kind = row.choice(
        "动作类型",
        "text_paste",
        &[ACTIONS, ACTION_ALIASES].concat(),
    )?;
    Ok(match kind.as_str() {
        "none" => json!({"type":"none"}),
        "text_keys" | "text_paste" => {
            json!({"type":"text","method":if kind=="text_keys"{"keys"}else{"paste"},"source":source(row)?})
        }
        "html" => {
            json!({"type":"html","html":row.get("正文或目标"),"plain_text":row.get("备用纯文本")})
        }
        "hotkey" => json!({"type":"hotkey","keys":keys(row)?}),
        "send_keys" => {
            json!({"type":"send_keys","source":source(row)?,"syntax_variables":lines(row.get("允许命令变量（每行一项）"))})
        }
        "open" => open(row)?,
        "common" => json!({"type":"common","function":common(row)?}),
        "quicker" => {
            json!({"type":"quicker","action_id":row.required("正文或目标")?,"parameter":row.get("Quicker参数")})
        }
        "reusable" => {
            json!({"type":"reusable","action_id":reference(row,ids)?,"parameters":params.remove(&(parent.into(),0)).unwrap_or_default()})
        }
        "steps" | "replay" => {
            let list: Vec<_> = steps
                .remove(parent)
                .unwrap_or_default()
                .into_values()
                .map(|p| p.0)
                .collect();
            let mut value = json!({"type":kind,"plan":{"version":1,"dialect":if kind=="replay"{"replay"}else{"structured"},"steps":list}});
            if kind == "replay" {
                let speed = row.number("重放倍速", 1.0)?;
                if speed <= 0.0 {
                    return Err(row.err("重放倍速", "倍速必须大于0"));
                }
                value["speed"] = json!(speed)
            }
            value
        }
        _ => unreachable!(),
    })
}
fn operation(
    row: &mut Row,
    ids: &BTreeMap<String, String>,
    params: &mut BTreeMap<(String, u64), BTreeMap<String, Value>>,
    parent: &str,
    order: u64,
) -> Result<Value, E> {
    let raw = row.get("高级操作JSON");
    if !raw.is_empty() {
        return serde_json::from_str(&raw).map_err(|_| row.err("高级操作JSON", "JSON格式错误"));
    }
    let kind = row.choice("操作类型*", "", OPERATIONS)?;
    Ok(match kind.as_str() {
        "text_paste" | "text_keys" => {
            json!({"type":"text","text":row.get("正文或目标"),"method":if kind=="text_keys"{"keys"}else{"paste"}})
        }
        "key_down" | "key_up" | "key_press" => {
            let raw = row.required("按键")?;
            json!({"type":kind,"key":key_value(row,&raw)?})
        }
        "hotkey" => json!({"type":"hotkey","keys":keys(row)?}),
        "delay" => {
            let n = row.uint("毫秒", 0)?;
            if n > quicker_domain::MAX_DELAY_MS {
                return Err(row.err("毫秒", "等待不能超过3600000毫秒"));
            }
            json!({"type":"delay","ms":n})
        }
        "mouse_move" => {
            json!({"type":"mouse_move","x":row.number("X",0.0)?,"y":row.number("Y",0.0)?,"relative":row.boolean("相对坐标",false)?,"percent":row.boolean("百分比坐标",false)?})
        }
        "mouse_button" => {
            json!({"type":"mouse_button","button":row.choice("鼠标键","left",&[("左键","left"),("右键","right"),("中键","middle"),("X1","x1"),("X2","x2")])?,"state":row.choice("鼠标状态","click",&[("单击","click"),("双击","double_click"),("按住","down"),("松开","up")])?})
        }
        "wheel" => {
            let amount = row
                .required("滚轮数量")?
                .parse::<i32>()
                .map_err(|_| row.err("滚轮数量", "必须为32位整数"))?;
            json!({"type":"wheel","axis":row.choice("滚轮方向","vertical",&[("垂直","vertical"),("水平","horizontal")])?,"amount":amount,"unit":row.choice("滚轮单位","clicks",&[("格数","clicks"),("原始增量","delta")])?})
        }
        "paste_files" => {
            json!({"type":"paste_files","paths":lines(row.required("文件（每行一项）")?)})
        }
        "paste_image" => json!({"type":"paste_image","path":row.required("正文或目标")?}),
        "html" => {
            json!({"type":"html","html":row.get("正文或目标"),"plain_text":row.get("备用纯文本")})
        }
        "open" => open(row)?,
        "common" => json!({"type":"common","function":common(row)?}),
        "reusable" => {
            json!({"type":"reusable","action_id":reference(row,ids)?,"parameters":params.remove(&(parent.into(),order)).unwrap_or_default()})
        }
        _ => return Err(row.err("操作类型*", "请选择操作类型")),
    })
}
#[cfg(test)]
mod security_tests {
    use super::*;
    #[test]
    fn excel_root_workbook_relationships_are_strictly_validated() {
        let typ =
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
        for target in ["xl/workbook.xml", "/xl/workbook.xml", "xl/workbook&#46;xml"] {
            let xml = format!(
                "<Relationships><Relationship Type=\"{typ}\" Target=\"{target}\"/></Relationships>"
            );
            assert!(validate_root_relationships(xml.as_bytes()).is_ok());
        }
        for body in [
            String::new(),
            format!("<Relationship Type=\"{typ}\" Target=\"alt/workbook.xml\"/>"),
            format!("<Relationship Type=\"{typ}\" Target=\"xl/../alt/workbook.xml\"/>"),
            format!("<Relationship Type=\"{typ}\" Target=\"xl/workbook.xml\" TargetMode=\"External\"/>"),
            "<Relationship Type=\"officeDocument\" Target=\"xl/workbook.xml\"/>".into(),
            "<Relationship Type=\"http://evil/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>".into(),
            format!("<Relationship Type=\"{typ}\" Target=\"xl/workbook.xml\"/><Relationship Type=\"{typ}\" Target=\"xl/workbook.xml\"/>"),
            "<Relationship Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDoc&#117;ment\" Target=\"alt/workbook&#46;xml\"/>".into(),
        ] {
            let xml=format!("<Relationships>{body}</Relationships>");
            assert!(validate_root_relationships(xml.as_bytes()).is_err());
        }
    }
    #[test]
    fn excel_xml_preflight_rejects_sparse_shared_formula_and_implicit_coordinates() {
        for xml in [
            "<worksheet><dimension ref=\"A1:XFD1048576\"/></worksheet>",
            "<worksheet><sheetData><row r=\"1\"><c r=\"A1\"><f t=\"shared\" si=\"1000000000\"/></c></row></sheetData></worksheet>",
            "<worksheet><sheetData><row><c r=\"A1\"/></row></sheetData></worksheet>",
            "<worksheet><sheetData><row r=\"1\"><c/></row></sheetData></worksheet>",
            "<foo><sheetData><row r=\"1\"><c r=\"XFD1048576\"/></row></sheetData></foo>",
            "<Relationships><Relationship Type=\"a/worksheet\" Target=\"worksheets/sheet1.dat\"/></Relationships>",
            "<Relationships><Relationship Type=\"a/worksheet\" Target=\"worksheets/sheet1.XML\"/></Relationships>",
        ] {
            assert!(inspect_xml(xml.as_bytes(), &mut 0, "指令").is_err());
        }
    }
    #[test]
    fn excel_parameter_and_number_limits() {
        assert!(validate_parameters(&json!({"type":"reusable","parameters":{"v":[]}})).is_err());
        assert!(validate_parameters(&json!({"type":"steps","plan":{"steps":[{"operation":{"type":"reusable","parameters":{"v":{}}}}]}})).is_err());
        assert!(validate_numbers(&json!(9007199254740992u64)).is_err());
        assert!(validate_numbers(&json!(-9007199254740992i64)).is_err());
        assert!(validate_numbers(&json!(9007199254740991u64)).is_ok());
    }
    #[test]
    fn excel_referenced_sheet_relations_are_strictly_validated() {
        use std::io::{Cursor, Write};
        let workbook = r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="指令" r:id="rId1"/></sheets></workbook>"#;
        for relation in [
            r#"<Relationship Id="rId1" Type="worksheet" Target="worksheets/sheet1.dat"/>"#,
            r#"<Relationship Id="rId1" Type="a/chartsheet" Target="worksheets/sheet1.dat"/>"#,
            r#"<Relationship Id="rId1" Type="a/dialogsheet" Target="worksheets/sheet1.dat"/>"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/work&#115;heet" Target="worksheets/sheet1&#x2E;dat"/>"#,
            r#"<Relationship Id="rId1" Target="worksheets/sheet1.xml"/>"#,
            r#"<Relationship xmlns:x="urn:test" Id="rId1" Type="a/chartsheet" x:Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/evil.dat" x:Target="worksheets/sheet1.xml"/>"#,
            r#"<Relationship Id="other" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>"#,
        ] {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (name, xml) in [
                ("xl/workbook.xml", workbook.to_string()),
                (
                    "xl/_rels/workbook.xml.rels",
                    format!("<Relationships>{relation}</Relationships>"),
                ),
            ] {
                writer
                    .start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(xml.as_bytes()).unwrap();
            }
            let bytes = writer.finish().unwrap().into_inner();
            let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
            assert!(worksheet_labels(&mut zip).is_err());
        }
        let attrs = BTreeMap::from([
            (
                "Type".into(),
                "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet"
                    .into(),
            ),
            ("Target".into(), "worksheets/sheet1.xml".into()),
        ]);
        assert_eq!(
            validated_sheet_part(&attrs).unwrap(),
            "xl/worksheets/sheet1.xml"
        );
    }
}
