# 原版 Quicker 文本指令导入兼容性

更新日期：2026-10-27。适用于原版「文本指令」导出的 PascalCase JSON 数组，可带 UTF-8 BOM。这里的“可转换”表示结构和已核实语义可以映射，不表示已完成目标程序、键盘/输入法或跨平台验收。

导入流程是预览、确认后直接保存并生效；取消预览不修改配置。普通文字沿用原启用状态；快捷键、SendKeys、多步骤、回放、打开目标和常用功能默认停用，审阅后才能手动启用。需要补全或不兼容的条目也会停用。每条原始 JSON，包括未知字段、时间戳、动作正文和参数，都保留在迁移元数据中，并可随原生配置包导出、重新导入。

## 已核实的动作类型

下列数字来自本机 Quicker **1.44.24.0** 的 `Quicker.Common.QuickActions.QuickActionType` 反射结果，不按下拉列表顺序猜测，也不把相似名称直接当成等价操作。

| 数值 / 原版枚举 | 处理方式 | 启用前需要做什么 |
|---|---|---|
| 0 / None | 映射为不执行动作，保留删除缩写/通知设置 | 沿用原禁用状态 |
| 1 / Keystroke | 将 `修饰键位掩码\|Windows虚拟键码` 转为快捷键 | 审阅；Windows 以外平台需要重录并验证键位 |
| 2 / SendKeys | 使用现有按键 B 解析器；普通固定语法可转换 | 审阅；动态变量有行为差异，见下文 |
| 3 / PasteText | 转为粘贴纯文本，保留支持的动态正文变量 | 不支持的模板仍停用并报告 |
| 4 / PasteHtml | HTML 原文转入“粘贴带格式的文字” | **停用待补全**：需要填写纯文本回退，尚无与原版 HTML→纯文本完全一致的实现 |
| 5 / RunOrOpen | 固定 http(s) 网址或绝对文件/程序路径可转换 | 审阅；带原始参数串、环境/动态变量、脚本、解释器、快捷方式等情况需手动处理 |
| 6 / InputText | 转为键入纯文本 | 不支持的模板仍停用并报告 |
| 7 / InputScript | 用现有 inputscript 白名单解析为步骤 | 审阅；动态生成步骤/从 FILE 读取脚本需改写 |
| 11 / QuickerOperation | 按已核实的原版操作标识精确映射白名单 | 审阅；未覆盖操作有具体分类原因，见下表 |
| 12 / QuickerAction | 保留原始动作及参数，停用 | 依赖原版私有动作库/运行时；在原版运行或手动重建，不会调用私有动作 |
| 13 / PlayKeyMouseData | 用现有 playrecord 解析器转为回放，原版默认速度 `1.0` | 审阅屏幕坐标、按键和鼠标状态；平台适配仍需验证 |
| 21 / StartCircleMenu | 保留并停用 | 依赖 Quicker 轮盘/场景；在原版使用或改成明确单个操作 |
| 22 / StartMouseGesture | 保留并停用 | 依赖原版手势运行时；手动重建手势结果 |
| 23 / ShowPanel | 保留并停用 | 本程序没有 Quicker 场景面板 |
| 24 / QuickCapture | 保留并停用 | 截图模式和输出约定不等价；可重建为系统截图快捷键 |
| 25 / MoveWindow | 保留并停用 | 窗口目标/布局规则没有等价适配器；手动设置系统快捷键 |
| 100 / InheritGlobal | 保留并停用 | 文本指令导出不含被继承的全局动作；在原版改为明确动作后重新导出 |
| 其他数字 | 保留并停用，报告实际数字 | 没有已核实的映射；补充版本和格式证据或手动选动作 |

## 快捷键与常用功能

`Hotkey.ToData()` 的格式为 `{ModifierKeys整数}|{VirtualKeyCode整数}`。已核实 WPF 修饰位：1=Alt、2=Control、4=Shift、8=Windows，键码范围 1–255。例如 `2|65` 是 Ctrl+A；`Ctrl+A` 这样的显示文字不是该原生导出格式。未知位、缺失字段或越界键码不猜测转换。

`QuickOperationItem` 静态表与对应处理函数提供了以下原版标识：

| 原版 Data | 本程序动作 |
|---|---|
| `windows_volume_up` / `windows_volume_down` / `windows_volume_mute` | 音量加 / 音量减 / 静音 |
| `windows_media_next_track` / `windows_media_prev_track` / `windows_media_play_pause` | 下一曲 / 上一曲 / 播放暂停 |
| `windows_browser_back` / `windows_browser_forward` | 浏览后退 / 前进 |
| `operation_copy` / `operation_paste` | 复制 / 粘贴 |
| `windows_media_stop` / `windows_browser_refresh` | MEDIASTOP / BROWSERREFRESH 按键；报告 Windows 专用差异，macOS 不能据此视为兼容 |
| `windows_window_close_tab` | Ctrl+W；报告 macOS 需要重录并验证 |

其他常用功能逐项分类报告：输入法需要等价输入法适配器；窗口最小化/最大化/透明度/置顶等需要窗口适配器；亮度依赖设备；原版面板、搜索、轮盘、最近动作等依赖私有状态；复制或粘贴分支、剪贴板菜单、运行/搜索选区不能简化为单个按键。**关闭窗口也不直接替换成 Alt+F4**：本机实现含按前台程序分支的处理及窗口消息，不等价于固定组合键。可在报告指引下手动重建。

## 文件、参数与动态模板

- `FILE:` 大小写不敏感，ActionType 2/3/6 会保留路径并转换为文件来源，初始编码选择 UTF-8，**仍标记待处理并停用**。原版 `.NET ReadAllText` 会识别 BOM，本程序需明确编码。用户必须核对绝对路径、UTF-8/UTF-16/GBK 编码并预览后启用；导入不会探测或读取文件。路径里的 `%NAME%`、剪贴板或日期占位符需改为实际路径。
- RunOrOpen 原版把 Data 第一行作为目标、第二行作为原始 Windows 参数字符串。本程序使用参数数组，不猜测 Windows 引号/转义；有参数串时保留原始 JSON 并要求用户在编辑器逐项填写。固定网址的百分号编码（例如 `%E4%BD%A0`）会保留，不会误当环境变量。相对路径、命令、脚本、解释器、`.lnk`/`.url` 等不会自动转换为可运行目标。
- 原版 SendKeys 在展开剪贴板、选区或日期之后，直接把结果交给 SendWait；本程序默认转义动态值，让它们按字面输出。涉及这些变量时，明确报告行为差异、标记待改写并停用。用户可在“文字与按键组合”中对可信剪贴板/选区显式选择语法来源，或改成独立文字和按键步骤。导入不会自动授予变量执行按键语法的权限。
- 原版 inputscript 支持先展开变量或读取 FILE 后再解析步骤；本程序不会自动生成这种动态脚本。相关条目停用，提示改为固定步骤、独立文本指令或外部文字来源。固定输入使用现有 21 命令白名单；未知命令、分隔符错误和按下/释放不成对等均给出定位与原因。
- 纯文本 `{cliptext}`、`{selection}` 和已支持的 `{0:数字日期格式}` 沿用现有正文渲染；日期区域格式（如月份全名）、未定义参数等会停用并说明替代方式。正则参数继续支持完整匹配、第一捕获组和 URL 编码的 `{$n}` 捕获模板；冲突来源或缺失捕获组需手动修正。

## 字段与触发方式

分组、标题、大小写、原禁用状态、IME 删除方式、参数来源及通知保留。字段类型错误会列出具体字段名，不把字符串 `"false"` 猜成布尔值。未知字段完整保留并停用，不能断言新字段不影响行为；请在迁移原数据中核对其含义。重复 Id 会拒绝整次导入，明确提示当前配置未改变。

`TriggerKey=0` 为即时触发，null/缺省继承全局结束键；32/186 对应空格/分号。9/13 会保留 Tab/Enter 映射，但本次导入保守停用并解释 **Windows RawInput 不支持 Tab/Enter 触发**，提示改为空格、分号或即时触发；这不代表对其他平台触发能力做了判定。其他键码以及与 `IsDirectInputTrigger` 冲突的记录需人工处理。正则即时触发、.NET 环视/反向引用、可匹配空串的正则也需改写。

**进程绑定有明确差异**：本程序已移除进程范围，`BindingProcessName` 仅保留作迁移元数据。报告会警告导入后可在所有程序触发、生效范围比原版更广。不会重新引入已移除的 scope；用户应检查缩写冲突与适用场景，必要时停用。

## 证据与验证边界

仓库依据：`work/plan-research/quicker-textcommand.md` 的动作、inputscript、playrecord、动态变量与 FILE 语义；`work/plan-research/codebase-backend.md` 的现有模型与解析器说明。

本次仅对程序二进制做只读反射/IL 检查，没有调用原版动作、运行截图/窗口/键盘操作，也没有读取 Quicker 私有数据库：

- `Quicker.Common.QuickActions.QuickActionType`：枚举名称与数字。
- `Quicker.View.Hotkeys.Hotkey.ToData()` 与字符串构造器，以及 `ModifierKeys`：键码格式与位值。
- `Quicker.Domain.QuickActions.QuickActionRunner`：SendKeys 展开顺序、HtmlToPlainText 回退、原始参数字符串拆行、playrecord 速度 1、FILE 大小写与编码读取路径。
- `Quicker.View.TextCommands.QuickOperationItem` 静态初始化与对应处理函数：精确操作标识和常用按键；关闭窗口的特殊分支据此未映射。
- 本机 `Quicker.Common.dll` SHA-256：`07D2B1A2C554B602047D96FBE33031FD77778B1D501D3614D5F8EA9BE7A49152`；`Quicker.exe`：`DBA23CDDE86D3BAC3ADDD54F7EC91C55957FA7C930DBD1014D4CC091EBF76EB1`。

自动验证使用 `cargo test -p quicker-domain`。合成用例覆盖新增类型、逐项不兼容原因、URL 编码、脚本/参数拒绝、跨平台提示、FILE/HTML 待补全、动态 SendKeys 差异、中文诊断和原始记录往返。另将用户现有迁移元数据中的 **53 条原版记录**临时提取到本机临时目录，运行已有的显式样本测试：53 条保留，50 条粘贴文字、1 条键入文字、2 条 SendKeys；51 条文字启用、2 条自动化停用，原始记录导出/再导入一致。正文未输出到日志或检入仓库。

这些结果验证导入、静态校验和元数据保留，不替代真实键盘/IME、目标程序粘贴、文件实际编码、Windows 与 macOS 设备或整包安装验收。未对原版私有类型声称运行兼容。
