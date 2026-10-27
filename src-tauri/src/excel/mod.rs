//! Native, bounded XLSX interchange. Parsing never executes an action.
mod config;
mod parse;
mod template;
#[cfg(test)]
mod tests;
pub use config::{export_config, read_config};
#[cfg(test)]
pub use parse::read;
use quicker_domain::{Diagnostic, SourceSpan};
pub use template::template;
pub const TEMPLATE_VERSION: &str = "quicktext-excel-1";
pub const INPUT_ROWS: u32 = 500;
pub const MAX_ROWS: usize = 10_000;
pub const RULE_HEADERS: &[&str] = &[
    "指令编号",
    "缩写或正则*",
    "动作类型",
    "正文或目标",
    "分组",
    "说明",
    "启用",
    "匹配方式",
    "忽略大小写",
    "触发方式",
    "结束键",
    "吞掉结束键",
    "输入法清除",
    "通知",
    "通知内容",
    "参数来源",
    "固定参数或捕获模板",
    "文字来源",
    "编码",
    "备用纯文本",
    "按键",
    "启动参数（每行一项）",
    "启动文件夹",
    "复用编号或UUID",
    "Quicker参数",
    "允许命令变量（每行一项）",
    "重放倍速",
    "高级动作JSON",
];
pub const STEP_HEADERS: &[&str] = &[
    "指令编号*",
    "顺序*",
    "操作类型*",
    "正文或目标",
    "按键",
    "毫秒",
    "X",
    "Y",
    "相对坐标",
    "百分比坐标",
    "鼠标键",
    "鼠标状态",
    "滚轮方向",
    "滚轮数量",
    "滚轮单位",
    "文件（每行一项）",
    "备用纯文本",
    "启动参数（每行一项）",
    "启动文件夹",
    "复用编号或UUID",
    "目标窗口",
    "允许程序",
    "高级操作JSON",
];
pub const PARAM_HEADERS: &[&str] = &[
    "指令编号*",
    "步骤顺序（空为主动作）",
    "参数名*",
    "值类型",
    "值",
];
pub const CAPTURE_HEADERS: &[&str] = &["指令编号*", "参数名*", "捕获组序号*"];
pub const ACTIONS: &[(&str, &str)] = &[
    ("不执行操作", "none"),
    ("粘贴文本", "text_paste"),
    ("逐字输入", "text_keys"),
    ("富文本", "html"),
    ("组合键", "hotkey"),
    ("文字与按键组合", "send_keys"),
    ("操作步骤", "steps"),
    ("重放", "replay"),
    ("打开", "open"),
    ("常用功能", "common"),
    ("复用指令", "reusable"),
    ("Quicker动作", "quicker"),
];
/// Older template labels still accepted on import.
pub const ACTION_ALIASES: &[(&str, &str)] = &[("按键命令", "send_keys")];
pub const OPERATIONS: &[(&str, &str)] = &[
    ("粘贴文本", "text_paste"),
    ("逐字输入", "text_keys"),
    ("按住键", "key_down"),
    ("松开键", "key_up"),
    ("按一下键", "key_press"),
    ("组合键", "hotkey"),
    ("等待", "delay"),
    ("移动鼠标", "mouse_move"),
    ("鼠标按键", "mouse_button"),
    ("滚动", "wheel"),
    ("粘贴文件", "paste_files"),
    ("粘贴图片", "paste_image"),
    ("富文本", "html"),
    ("打开", "open"),
    ("常用功能", "common"),
    ("复用指令", "reusable"),
];
pub const COMMON: &[(&str, &str)] = &[
    ("浏览器后退", "browser_back"),
    ("浏览器前进", "browser_forward"),
    ("锁定屏幕", "lock_screen"),
    ("复制", "copy"),
    ("剪切", "cut"),
    ("粘贴", "paste"),
    ("全选", "select_all"),
    ("撤销", "undo"),
    ("重做", "redo"),
    ("音量加", "volume_up"),
    ("音量减", "volume_down"),
    ("静音", "volume_mute"),
    ("播放暂停", "media_play_pause"),
    ("上一曲", "media_previous"),
    ("下一曲", "media_next"),
];
fn error(sheet: &str, row: usize, field: &str, message: &str) -> Diagnostic {
    Diagnostic::error(
        "excel_invalid",
        format!("工作表「{sheet}」第{row}行「{field}」：{message}"),
        SourceSpan {
            line: row,
            column: 0,
            length: 0,
        },
    )
    .at(format!("{sheet}!{row}.{field}"))
}
