use super::*;
use rust_xlsxwriter::{
    ConditionalFormatFormula, DataValidation, DataValidationRule, Format, Table, TableColumn,
    TableStyle, Workbook, XlsxError,
};
pub fn template() -> Result<Vec<u8>, Vec<Diagnostic>> {
    build()
        .and_then(|mut book| book.save_to_buffer())
        .map_err(|_| vec![error("填写说明", 1, "文件", "无法生成 Excel 模板")])
}
fn numeric_column(header: &str) -> bool {
    matches!(
        header,
        "顺序*"
            | "步骤顺序（空为主动作）"
            | "毫秒"
            | "捕获组序号*"
            | "X"
            | "Y"
            | "滚轮数量"
            | "重放倍速"
    )
}
pub(super) fn build() -> Result<Workbook, XlsxError> {
    let mut book = Workbook::new();
    book.define_name("RuleIds", "='指令'!$A$2:$A$501")?;
    let title = Format::new()
        .set_bold()
        .set_font_size(18)
        .set_font_color("#174B52");
    let wrap = Format::new()
        .set_text_wrap()
        .set_align(rust_xlsxwriter::FormatAlign::Top);
    let ws = book.add_worksheet();
    ws.set_name("填写说明")?;
    ws.set_column_width(0, 110)?;
    for (i,s) in [TEMPLATE_VERSION,"QuickText · 批量创建指令", "① 先看「填写示例」，再到「指令」从第2行填写。至少填写缩写；默认动作是粘贴文本。", "② 指令编号可自定义，例如 welcome。没有子表或复用关联时可留空，导入自动生成 UUID。", "③ 下拉菜单可直接选择中文；空白采用默认值。500行带筛选和输入提示；可继续增加行，导入每页最多10000行。", "④ 高级列已折叠，点击表头上方 + 展开。操作步骤、调用参数、捕获参数按指令编号关联。", "⑤ 只导入指令/操作步骤/调用参数/捕获参数；示例永远不会导入。全空模板会提示错误，不会清空配置。", "⑥ 文本按原样保留，包括前导0、换行、=和+。请勿输入Excel公式，粘贴时选择仅值；公式将被拒绝。", "动作正文：文本/文字与按键组合填正文；富文本填HTML并填备用纯文本；打开填目标；常用功能填复制/剪切/粘贴/全选/撤销/重做/音量加/音量减/静音/播放暂停/上一曲/下一曲。", "按键：组合键用 + 分隔，例如 Ctrl+C；$primary 为平台主键；vk:65 为Windows虚拟键码(1到255)；其他名称为普通命名按键。", "【文字与按键组合】正文从左到右执行：普通文字原样输入，花括号里的是按键。例：小王{Tab}13800000000{Enter} = 输入姓名→按Tab→输入电话→按Enter。", "  连按：{Enter 3} 连按3次回车（花括号内空格后写次数，1到10000）；也可写 {Enter}{Enter}{Enter}。常用键：{Enter} {Tab} {Esc} {Backspace} {Delete} {Left} {Right} {Up} {Down} {Home} {End} {F1}到{F16}。", "  同时按：^ 代表 Ctrl，+ 代表 Shift，% 代表 Alt，写在键前面。例：^a = Ctrl+A，+{Tab} = Shift+Tab，%{F4} = Alt+F4，^(ac) = 按住Ctrl依次按A和C。", "  特殊符号：+ ^ % ~ ( ) { } [ ] 有特殊含义，要原样输出请加花括号，如 {+} {%} {(} {{}；~ 单独写等于按 Enter。正文为文字与按键组合时，Excel里的 + 不会被当成公式。", "  区别：动作类型「组合键」是同时按下几个键（填在「按键」列，如 Ctrl+C），不能连按同一个键；要连按或在文字中间按键，请用「文字与按键组合」。Win键、媒体键、F17–F24 请改用操作步骤。", "文件来源：文字来源选读取文件，正文填路径。编码UTF-8/UTF-16LE/UTF-16BE/GBK。文件内容只在运行指令时读取。", "结束键：继承全局 / 空格 / 分号 / 空格和分号 / Tab（旧设置） / Enter（旧设置）。Windows不支持Tab/Enter结束键。", "步骤：顺序填正整数，同一指令不得重复。文本、按键、等待、鼠标、文件、图片、HTML、打开、常用、复用都有独立列。", "鼠标：X/Y为数值，相对/百分比选是或否；鼠标键与状态、滚轮方向/数量/单位均有下拉。目标窗口默认锁定，可选指定程序并填写允许程序。", "调用参数：主动作步骤顺序留空；步骤调用填对应顺序。值类型支持文本/数字/布尔/空值/JSON；文本原样保留，布尔填 true/false。", "捕获参数：参数来源选捕获模板，逐行填参数名和捕获组序号，例如 name/1。可用高级原始捕获模板串覆盖，但不能同时填写。", "高级动作JSON用于完整Action对象（含plan/version/dialect/source等）；高级操作JSON用于完整Operation对象。不能与该动作其它正文/步骤/参数同时使用。", "Excel模板不支持录制键鼠、拾取窗口或选择本地文件；可填写路径和坐标，需要录制时请回到App。", "限制：仅xlsx，文件最大16MiB，ZIP展开总计64MiB，最多256个条目，每页10000数据行；不读取外部链接、不计算公式。导入只是预览，不会执行动作。"].iter().enumerate(){ws.write_string_with_format(i as u32,0,*s,if i==1 {&title}else{&wrap})?;ws.set_row_height(i as u32,if i<2 {30.0}else{42.0})?;}
    for (name, headers) in [
        ("指令", RULE_HEADERS),
        ("操作步骤", STEP_HEADERS),
        ("调用参数", PARAM_HEADERS),
        ("捕获参数", CAPTURE_HEADERS),
    ] {
        let ws = book.add_worksheet();
        ws.set_name(name)?;
        ws.set_freeze_panes(1, 3.min(headers.len() as u16))?;
        let text = Format::new().set_num_format("@").set_text_wrap();
        let number = Format::new().set_num_format("General").set_text_wrap();
        for (col, h) in headers.iter().enumerate() {
            ws.set_column_width(col as u16, if col == 3 { 34.0 } else { 20.0 })?;
            ws.set_column_format(col as u16, if numeric_column(h) { &number } else { &text })?;
            ws.write_string(0, col as u16, *h)?;
        }
        let columns: Vec<_> = headers
            .iter()
            .map(|h| TableColumn::new().set_header(*h))
            .collect();
        ws.add_table(
            0,
            0,
            INPUT_ROWS,
            (headers.len() - 1) as u16,
            &Table::new()
                .set_columns(&columns)
                .set_style(TableStyle::Medium2),
        )?;
        if name == "指令" {
            ws.group_columns_collapsed(6, (headers.len() - 1) as u16)?;
        }
        if name == "操作步骤" {
            ws.group_columns_collapsed(6, (headers.len() - 1) as u16)?;
        }
        for (col, h) in headers.iter().enumerate() {
            let values: Vec<&str> = match *h {
                "动作类型" => ACTIONS.iter().map(|p| p.0).collect(),
                "操作类型*" => OPERATIONS.iter().map(|p| p.0).collect(),
                "启用" | "忽略大小写" | "吞掉结束键" | "通知" | "相对坐标" | "百分比坐标" =>
                {
                    vec!["是", "否"]
                }
                "匹配方式" => vec!["普通缩写", "正则表达式"],
                "触发方式" => vec!["结束键触发", "立即触发"],
                "输入法清除" => vec!["继承全局", "Escape", "Backspace"],
                "参数来源" => vec!["匹配内容", "固定参数", "第一个捕获组", "捕获模板", "空值"],
                "文字来源" => vec!["直接填写", "读取文件"],
                "编码" => vec!["UTF-8", "UTF-16LE", "UTF-16BE", "GBK"],
                "结束键" => vec![
                    "继承全局",
                    "空格",
                    "分号",
                    "空格和分号",
                    "Tab（旧设置）",
                    "Enter（旧设置）",
                ],
                "鼠标键" => vec!["左键", "右键", "中键", "X1", "X2"],
                "鼠标状态" => vec!["单击", "双击", "按住", "松开"],
                "滚轮方向" => vec!["垂直", "水平"],
                "滚轮单位" => vec!["格数", "原始增量"],
                "目标窗口" => vec!["锁定开始窗口", "指定程序"],
                "值类型" => vec!["文本", "数字", "布尔", "空值", "JSON"],
                _ => vec![],
            };
            let prompt = if *h == "指令编号*" {
                "从指令页编号中选择父指令；请先在指令页填写编号。"
            } else if *h == "触发方式" {
                "普通缩写默认立即触发；正则默认结束键触发。"
            } else if *h == "正文或目标" {
                "文本填正文；文字与按键组合如 你好{Enter 3}、^a；打开填网址/路径；Quicker填动作ID；详见填写说明。"
            } else if *h == "按键" {
                "示例 Ctrl+C、$primary+V、vk:65；组合键用+分隔。"
            } else if *h == "参数来源" {
                "默认匹配内容；捕获模板可配合捕获参数页逐行填写。"
            } else if *h == "分组" {
                "空白默认分组为“默认”。"
            } else if *h == "启用" {
                "默认是；选择否会保存为禁用指令。"
            } else if *h == "动作类型" {
                "默认粘贴文本；选择动作后填写对应正文、路径或子表。"
            } else if *h == "文字来源" {
                "默认直接填写；读取文件时正文填完整路径，默认UTF-8。"
            } else if h.contains("每行") {
                "在一个单元格内用换行分隔，每行一项，保留顺序。"
            } else if h.contains("JSON") {
                "高级选项，填写合法JSON；普通用户请使用前面的专用列。"
            } else {
                "空白采用默认值；请参阅填写说明。以*标记的字段必填。"
            };
            let mut dv = DataValidation::new()
                .set_input_title(*h)?
                .set_input_message(prompt)?
                .set_error_title("请选择有效值")?
                .set_error_message("请使用下拉菜单中的选项。")?;
            if !values.is_empty() {
                dv = dv.allow_list_strings(&values)?;
            }
            if *h == "指令编号*" || *h == "复用编号或UUID" {
                dv = dv.allow_list_formula("=RuleIds".into());
                if *h == "复用编号或UUID" {
                    dv = dv
                        .show_error_message(false)
                        .set_input_message("可选同表编号，或直接输入已有指令UUID。")?;
                }
            }
            dv = match *h {
                "顺序*" | "步骤顺序（空为主动作）" => dv
                    .allow_whole_number(DataValidationRule::Between(1, 10_000))
                    .set_input_message("步骤顺序填1到10000整数；调用主动作时留空。")?
                    .set_error_message("请输入1到10000的整数。")?,
                "毫秒" => dv
                    .allow_whole_number(DataValidationRule::Between(0, 3_600_000))
                    .set_input_message("默认0，单位毫秒；1000表示等待1秒，最大3600000。")?
                    .set_error_message("请输入0到3600000的整数毫秒。")?,
                "捕获组序号*" => dv
                    .allow_whole_number(DataValidationRule::Between(1, 99))
                    .set_input_message("必填，1到99整数；1表示正则第一个括号捕获组。")?
                    .set_error_message("请输入1到99的捕获组序号。")?,
                "重放倍速" => dv
                    .allow_decimal_number(DataValidationRule::GreaterThan(0.0))
                    .set_input_message("默认1；2为两倍速，0.5为半速，必须大于0。")?
                    .set_error_message("倍速必须大于0。")?,
                "X" | "Y" => dv
                    .allow_decimal_number(DataValidationRule::Between(
                        -2_147_483_648.0,
                        2_147_483_647.0,
                    ))
                    .set_input_message("默认0；坐标可为小数，相对和百分比开关见后列。")?
                    .set_error_message("请输入有效坐标数值。")?,
                "滚轮数量" => dv
                    .allow_whole_number(DataValidationRule::Between(i32::MIN, i32::MAX))
                    .set_input_message("必填整数；正负表示相反方向，例如-1。")?
                    .set_error_message("请输入32位范围内的整数。")?,
                _ => dv,
            };
            ws.add_data_validation(1, col as u16, INPUT_ROWS, col as u16, &dv)?;
        }
        let red = Format::new().set_background_color("#FCE4D6");
        let formula = if name == "指令" {
            "=AND(COUNTA($A2:$AB2)>0,OR($B2=\"\",AND($A2<>\"\",COUNTIF($A$2:$A$501,$A2)>1)))"
        } else if name == "操作步骤" || name == "捕获参数" {
            "=AND(COUNTA($A2:$E2)>0,OR($A2=\"\",$B2=\"\",$C2=\"\"))"
        } else {
            "=AND(COUNTA($A2:$E2)>0,$A2=\"\")"
        };
        ws.add_conditional_format(
            1,
            0,
            INPUT_ROWS,
            (headers.len() - 1) as u16,
            &ConditionalFormatFormula::new()
                .set_rule(formula)
                .set_format(&red),
        )?;
    }
    let ws = book.add_worksheet();
    ws.set_name("填写示例")?;
    ws.set_column_width(0, 26)?;
    ws.set_column_width(1, 95)?;
    for (i,(a,b)) in [("简单文本","指令：编号 hello；缩写 nh；动作 粘贴文本；正文 您好，欢迎咨询。"),("文件文本","指令：编号 file；缩写 ft；动作 逐字输入；正文 C:\\模板.txt；文字来源 读取文件；编码 UTF-8"),("步骤","指令：编号 flow；缩写 fl；动作 操作步骤。操作步骤：flow/1/粘贴文本/您好；flow/2/等待，毫秒填1000。"),("复用","指令：编号 reuse；缩写 re；动作 复用指令；复用编号或UUID填 hello。调用参数：reuse/空/name/文本/小王。"),("捕获","指令：编号 cap；正则 ^订单(\\d+)$；匹配方式 正则表达式；参数来源 捕获模板。捕获参数：cap/order/1。"),("滚轮","操作步骤：flow/3/滚动；滚轮方向 垂直；滚轮数量 -1；滚轮单位 格数。"),("按键","操作步骤：flow/4/组合键；按键 $primary+C。"),("文字与按键组合","指令：编号 form；缩写 fm；动作 文字与按键组合；正文 小王{Tab}13800000000{Enter}。连按三次回车写 {Enter 3}；Ctrl+A 写 ^a。"),("高级计划","高级动作JSON示例：{\"type\":\"steps\",\"plan\":{\"version\":1,\"dialect\":\"structured\",\"source\":null,\"steps\":[]}}")].iter().enumerate(){ws.write_string(i as u32,0,*a)?;ws.write_string_with_format(i as u32,1,*b,&wrap)?;ws.set_row_height(i as u32,50.0)?;}
    let ws = book.add_worksheet();
    ws.set_name("选项")?;
    ws.write_string(0, 0, TEMPLATE_VERSION)?;
    ws.set_hidden(true);
    let ws = book.add_worksheet();
    ws.set_name("可复制指令示例")?;
    ws.set_freeze_panes(1, 3)?;
    let text = Format::new().set_num_format("@").set_text_wrap();
    let number = Format::new().set_num_format("General").set_text_wrap();
    for (c, h) in RULE_HEADERS.iter().enumerate() {
        ws.write_string(0, c as u16, *h)?;
        ws.set_column_width(c as u16, 22)?;
        ws.set_column_format(c as u16, if numeric_column(h) { &number } else { &text })?;
    }
    let examples: &[&[(usize, &str)]] = &[
        &[
            (0, "hello"),
            (1, "nh"),
            (2, "粘贴文本"),
            (3, "您好，欢迎咨询。"),
            (13, "是"),
            (14, "已输入欢迎语"),
        ],
        &[
            (0, "web"),
            (1, "www"),
            (2, "打开"),
            (3, "https://example.com"),
        ],
        &[(0, "copy"), (1, "cc"), (2, "组合键"), (20, "$primary+C")],
        &[
            (0, "send"),
            (1, "sk"),
            (2, "文字与按键组合"),
            (3, "小王{Tab}13800000000{Enter}"),
            (25, "cliptext"),
        ],
        &[(0, "flow"), (1, "fl"), (2, "操作步骤")],
        &[(0, "replay"), (1, "rp"), (2, "重放"), (26, "1")],
        &[(0, "reuse"), (1, "re"), (2, "复用指令"), (23, "hello")],
        &[
            (0, "regex"),
            (1, "^订单(\\d+)$"),
            (2, "粘贴文本"),
            (3, "已收到订单"),
            (7, "正则表达式"),
            (15, "捕获模板"),
            (16, "order={$1}"),
        ],
    ];
    for (r, values) in examples.iter().enumerate() {
        for (c, s) in *values {
            if numeric_column(RULE_HEADERS[*c]) {
                ws.write_number(
                    (r + 1) as u32,
                    *c as u16,
                    s.parse::<f64>().expect("numeric example"),
                )?;
            } else {
                ws.write_string((r + 1) as u32, *c as u16, *s)?;
            }
        }
    }
    let ws = book.add_worksheet();
    ws.set_name("可复制步骤示例")?;
    ws.set_freeze_panes(1, 3)?;
    for (c, h) in STEP_HEADERS.iter().enumerate() {
        ws.write_string(0, c as u16, *h)?;
        ws.set_column_width(c as u16, 22)?;
        ws.set_column_format(c as u16, if numeric_column(h) { &number } else { &text })?;
    }
    for (r, values) in [
        &[(0, "flow"), (1, "1"), (2, "粘贴文本"), (3, "您好")][..],
        &[(0, "flow"), (1, "2"), (2, "等待"), (5, "1000")][..],
        &[(0, "replay"), (1, "1"), (2, "按一下键"), (4, "Tab")][..],
    ]
    .iter()
    .enumerate()
    {
        for (c, s) in *values {
            if numeric_column(STEP_HEADERS[*c]) {
                ws.write_number(
                    (r + 1) as u32,
                    *c as u16,
                    s.parse::<f64>().expect("numeric example"),
                )?;
            } else {
                ws.write_string((r + 1) as u32, *c as u16, *s)?;
            }
        }
    }
    Ok(book)
}
