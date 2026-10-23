use super::*;
use std::io::{Cursor, Read, Write};
fn filled(cells: &[(&str, &str, &str)], formula: bool) -> Vec<u8> {
    let bytes = template().unwrap();
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).unwrap();
        let name = file.name().unwrap().into_owned();
        let mut data = String::new();
        file.read_to_string(&mut data).unwrap();
        let sheet = match name.as_str() {
            "xl/worksheets/sheet2.xml" => "指令",
            "xl/worksheets/sheet3.xml" => "操作步骤",
            "xl/worksheets/sheet4.xml" => "调用参数",
            "xl/worksheets/sheet5.xml" => "捕获参数",
            _ => "",
        };
        if !sheet.is_empty() {
            let mut rows = std::collections::BTreeMap::<String, Vec<(&str, &str)>>::new();
            for (s, coord, text) in cells {
                if *s == sheet {
                    let row = coord
                        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
                        .to_string();
                    rows.entry(row).or_default().push((coord, text));
                }
            }
            for (row, items) in rows {
                let start = format!("<row r=\"{row}\"");
                if let Some(pos) = data.find(&start) {
                    if let Some(end) = data[pos..].find("</row>") {
                        data.replace_range(pos..pos + end + 6, "");
                    }
                }
                let content=items.iter().map(|(coord,text)|{if formula {format!("<c r=\"{coord}\"><f>1+1</f><v>2</v></c>")}else{format!("<c r=\"{coord}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",text.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;"))}}).collect::<String>();
                data = data.replace(
                    "</sheetData>",
                    &format!("<row r=\"{row}\">{content}</row></sheetData>"),
                );
            }
        }
        writer
            .start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
        writer.write_all(data.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}
fn parse_bytes(bytes: Vec<u8>) -> Result<Vec<quicker_domain::Rule>, Vec<Diagnostic>> {
    let path = std::env::temp_dir().join(format!("quicktext-excel-{}.xlsx", uuid::Uuid::new_v4()));
    std::fs::write(&path, bytes).unwrap();
    let out = read(&path);
    std::fs::remove_file(path).unwrap();
    out
}
#[test]
fn excel_template_generation() {
    let bytes = template().unwrap();
    if let Ok(path) = std::env::var("QUICKTEXT_EXCEL_TEST_OUTPUT") {
        std::fs::write(path, &bytes).unwrap();
    }
    let mut book: calamine::Xlsx<_> = calamine::Reader::new(Cursor::new(bytes.clone())).unwrap();
    use calamine::Reader;
    assert!(book.sheet_names().len() >= 8);
    assert_eq!(
        book.worksheet_range("指令").unwrap().get_value((0, 1)),
        Some(&calamine::Data::String("缩写或正则*".into()))
    );
    assert!(parse_bytes(bytes).unwrap_err()[0]
        .message
        .contains("没有可导入"));
}
#[test]
fn excel_text_roundtrip_preserves_text() {
    let rules = parse_bytes(filled(
        &[
            ("指令", "A2", "001"),
            ("指令", "B2", "0007"),
            ("指令", "D2", "=您好+世界\n第二行"),
            ("指令", "E2", "客服"),
        ],
        false,
    ))
    .unwrap();
    assert_eq!(rules[0].match_spec.value, "0007");
    assert_eq!(rules[0].group, "客服");
    match &rules[0].action {
        quicker_domain::Action::Text {
            source: quicker_domain::ContentSource::Inline { value },
            ..
        } => assert_eq!(value, "=您好+世界\n第二行"),
        _ => panic!("wrong action"),
    }
}
#[test]
fn excel_steps_reusable_parameters_and_capture() {
    let rules = parse_bytes(filled(
        &[
            ("指令", "A2", "base"),
            ("指令", "B2", "a"),
            ("指令", "C2", "不执行操作"),
            ("指令", "A3", "flow"),
            ("指令", "B3", "(f)"),
            ("指令", "H3", "正则表达式"),
            ("指令", "C3", "操作步骤"),
            ("操作步骤", "A2", "flow"),
            ("操作步骤", "B2", "1"),
            ("操作步骤", "C2", "复用指令"),
            ("操作步骤", "T2", "base"),
            ("操作步骤", "U2", "指定程序"),
            ("操作步骤", "V2", "notepad.exe"),
            ("调用参数", "A2", "flow"),
            ("调用参数", "B2", "1"),
            ("调用参数", "C2", "name"),
            ("调用参数", "D2", "文本"),
            ("调用参数", "E2", "001"),
            ("指令", "P3", "捕获模板"),
            ("捕获参数", "A2", "flow"),
            ("捕获参数", "B2", "name"),
            ("捕获参数", "C2", "1"),
        ],
        false,
    ))
    .unwrap();
    match &rules[1].action {
        quicker_domain::Action::Steps { plan } => {
            assert_eq!(plan.steps.len(), 1);
            match &plan.steps[0].operation {
                quicker_domain::Operation::Reusable {
                    action_id,
                    parameters,
                } => {
                    assert_eq!(action_id, &rules[0].id);
                    assert_eq!(parameters["name"], "001")
                }
                _ => panic!("wrong operation"),
            }
        }
        _ => panic!("wrong action"),
    };
    assert!(
        matches!(&rules[1].parameter,quicker_domain::ParameterSource::CaptureTemplate{template} if template=="name={$1}")
    );
}
#[test]
fn excel_rejects_formula_duplicates_and_unknown_values() {
    for (cells, formula, needle) in [
        (vec![("指令", "B2", "test")], true, "公式"),
        (
            vec![
                ("指令", "A2", "id"),
                ("指令", "B2", "a"),
                ("指令", "A3", "id"),
                ("指令", "B3", "b"),
            ],
            false,
            "编号重复",
        ),
        (
            vec![("指令", "B2", "a"), ("指令", "C2", "未知")],
            false,
            "未知选项",
        ),
        (
            vec![
                ("操作步骤", "A2", "missing"),
                ("操作步骤", "B2", "1"),
                ("操作步骤", "C2", "等待"),
            ],
            false,
            "没有可导入",
        ),
    ] {
        let error = parse_bytes(filled(&cells, formula)).unwrap_err();
        assert!(error[0].message.contains(needle), "{}", error[0].message);
    }
}
#[test]
fn excel_read_external_workbook() {
    if let Ok(path) = std::env::var("QUICKTEXT_EXCEL_TEST_INPUT") {
        let rules = read(std::path::Path::new(&path)).unwrap();
        assert!(rules.len() >= 2);
        if let Ok(output) = std::env::var("QUICKTEXT_EXCEL_TEST_JSON_OUTPUT") {
            std::fs::write(output, serde_json::to_vec_pretty(&rules).unwrap()).unwrap();
        }
        println!("外部Excel已解析：{}条指令", rules.len());
    }
}
#[test]
fn excel_copyable_examples_compile_after_roundtrip() {
    use calamine::Reader;
    let mut book: calamine::Xlsx<_> =
        calamine::Reader::new(Cursor::new(template().unwrap())).unwrap();
    let mut owned = Vec::<(String, String, String)>::new();
    for (source, dest) in [("可复制指令示例", "指令"), ("可复制步骤示例", "操作步骤")]
    {
        let range = book.worksheet_range(source).unwrap();
        for (r, c, value) in range.cells() {
            if r == 0 {
                continue;
            }
            let text = match value {
                calamine::Data::String(text) => Some(text.clone()),
                calamine::Data::Float(number) => Some(number.to_string()),
                calamine::Data::Int(number) => Some(number.to_string()),
                _ => None,
            };
            if let Some(text) = text {
                let mut n = c + 1;
                let mut col = String::new();
                while n > 0 {
                    n -= 1;
                    col.insert(0, (b'A' + (n % 26) as u8) as char);
                    n /= 26;
                }
                owned.push((dest.into(), format!("{col}{}", r + 1), text));
            }
        }
    }
    let refs = owned
        .iter()
        .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
        .collect::<Vec<_>>();
    let rules = parse_bytes(filled(&refs, false)).unwrap();
    assert_eq!(rules.len(), 8);
    let config = quicker_domain::Config {
        rules,
        ..Default::default()
    };
    let result = quicker_domain::compile_config(&config);
    assert!(result.is_ok(), "{:?}", result.err());
}
#[test]
fn excel_domain_errors_include_sheet_row_and_field() {
    let errors = parse_bytes(filled(
        &[
            ("指令", "B8", "["),
            ("指令", "H8", "正则表达式"),
            ("指令", "G8", "否"),
        ],
        false,
    ))
    .unwrap_err();
    assert!(
        errors[0].message.contains("指令")
            && errors[0].message.contains("第8行")
            && errors[0].message.contains("缩写或正则")
    );
    let errors = parse_bytes(filled(
        &[
            ("指令", "A2", "f"),
            ("指令", "B2", "flow"),
            ("指令", "C2", "操作步骤"),
            ("操作步骤", "A7", "f"),
            ("操作步骤", "B7", "1"),
            ("操作步骤", "C7", "按一下键"),
            ("操作步骤", "E7", "unknown-key"),
        ],
        false,
    ))
    .unwrap_err();
    assert!(errors[0].message.contains("操作步骤") && errors[0].message.contains("第7行"));
}

#[test]
fn excel_text_and_key_combination_accepts_new_and_old_labels() {
    let rules = parse_bytes(filled(
        &[
            ("指令", "B2", "fm"),
            ("指令", "C2", "文字与按键组合"),
            ("指令", "D2", "小王{Tab}138{Enter 3}"),
            ("指令", "B3", "old"),
            ("指令", "C3", "按键命令"),
            ("指令", "D3", "^a"),
        ],
        false,
    ))
    .unwrap();
    for rule in &rules {
        assert!(matches!(
            rule.action,
            quicker_domain::Action::SendKeys { .. }
        ));
        let plan = quicker_domain::action_to_plan(&rule.action, &Default::default()).unwrap();
        assert!(!plan.steps.is_empty());
    }
    let plan = quicker_domain::action_to_plan(&rules[0].action, &Default::default()).unwrap();
    assert_eq!(plan.steps.len(), 6);
}
