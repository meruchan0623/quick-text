> 2026-10-15 更新：本文保留 0.2.x 的历史实现/验收记录。Windows 0.3.0 已改为直接集成 espanso 输入组件，当前行为与验收状态以 [0.3.0 重构记录](windows-0.3.0-refactor.md) 为准。

# 平台支持与已知差异

更新于 2026-10-14。这里描述代码路径和验证边界，不把编译或按钮存在写成实机通过。默认暂停；`native_ready()` / `input_status()` 从平台权限及实际 hook 状态判断是否可恢复。

| 能力 | Windows x64 | macOS Apple Silicon | 验证边界 |
|---|---|---|---|
| 单引擎监听/结束键消费 | 单一低级键鼠 hook → 共享 Rust matcher；0.2.0仅即时触发，结束键透传不执行 | 单一 session CGEventTap → 同一 matcher，可返回 null 消费结束键 | Windows GUI草稿/发布子用例已观察；真实键盘触发未完成。Mac无实机证据 |
| ASCII 触发/规则范围 | 原生Edit/RichEdit对照路径；新增 UIAutomation TextPattern2 观察可靠文本/折叠光标与composition的控件路径；进程名。浏览器若未提供可靠provider则拒绝 | CGEvent Unicode字符 + 仅ABC/U.S.输入源ID白名单；真实bundle id；dead-key/IME布局拒绝 | 本机只读枚举NotepadTextBox→RichEditD2DPT，不能视作hook消费通过。导航、鼠标、物理组合键、外部注入、超时、焦点或revision变化清/拒绝旧输入 |
| IME | Active、Unknown或过期composition不执行；可靠TextPattern2 inactive已上屏即时匹配，IME提交还须匹配已观察候选且正文未变，不自动清候选 | TIS input-source type 非 keyboard-layout 时拒绝 | 规则 null/缺省继承全局，旧显式 escape/backspace 保留。Windows特定provider路径仍未实机验收；Mac中文commit未实现，不能宣称通用支持 |
| 焦点/选中文本 | 原生前台和 UI Automation 只读选区 | NSWorkspace bundle id + AXFocusedWindow/AXFocusedUIElement，仅AXTextArea/AXTextField，AXSelectedText读取 | Mac AX拿不到焦点、非文本角色或安全控件时id=0并拒绝。AX选区不修改剪贴板；未暴露选区的应用明确失败 |
| Unicode 键入 | SendInput Unicode | CGEventKeyboardSetUnicodeString | Unicode/emoji/换行及应用消费需实测；结果为 unknown，不冒充目标消费成功 |
| 文本/HTML 粘贴 | 原生多格式剪贴板 | NSPasteboard public.utf8-plain-text / public.html | 保留文本fallback；不自动改成其他动作方法；消费仍未知 |
| 文件对象/图片 | CF_HDROP / 真图像格式 | 每文件一个 public.file-url item / 完整解码后 public.png | 不是粘贴路径字符串。Mac图片上限32 MB输入/1600万像素；实际目标类型兼容未验 |
| 剪贴板恢复 | native generation 归属检查 | changeCount归属检查+自有payload marker核验、全部items/types/data快照 | 异常计数/marker不再认作本任务generation；promised/private格式无法取出或过大时删除前拒绝。Mac快照64 MB/256 items上限；NSPasteboard无CAS，检查与clear/write之间竞态及目标延迟读取尚未验，不能宣称T08零覆盖已通过 |
| 键盘/修饰键 | Windows键名/显式数字值 | 显式Mac keycode映射；Ctrl仍为Ctrl，platform_primary为Cmd | 未知Windows键值、Win、F21-F24等不安全等价项预检拒绝。不是把Windows整数当Mac keycode |
| 鼠标/多屏坐标 | Windows虚拟桌面/DPI | Quartz全局display points，保留负坐标；主屏百分比 | 不自动缩放原录制坐标。Retina/多屏和自然滚动需实机验证 |
| 滚轮 | clicks或1/120-click delta | CGEvent line单位clicks；vertical/horizontal | 原Windows delta拒绝，用户可改clicks；不将delta偷偷取整成click |
| 媒体/音量/导航/亮度 | 已有按键映射；部分系统动作需额外adapter | NSEvent system-defined + Apple NX_KEYTYPE；Cmd+[ / Cmd+]导航 | 音量/静音/播放暂停/上一曲/下一曲已实现发送；亮度仅可处理该事件的硬件有效。未知媒体消费者和硬件无成功保证 |
| 任意窗口/IME切换/私有动作 | 未完成所有候选 | 未完成所有候选 | 明确需专门adapter、原清单核对与迁移；这仍是计划缺口，不改称已交付 |
| 录制/回放/取消 | 用户显式启停；自注入过滤 | 显式启停；键鼠/媒体及间隔，左右Cmd均记录为可移植Cmd；Ctrl+Shift+Esc停止收集并排除停止组合；停止时补IR持键释放 | 未知硬件键不静默省略，会使录制失败；有长度/超时界限。补IR释放不向用户当前按键发送释放。自然滚动、全部11类opcode实际回放未验 |
| 目标安全/清理 | generation/revision/focus、物理键冲突检查 | generation/revision + AX窗口/控件，物理键/鼠标冲突检查 | 本任务持键才清理，冲突不无条件释放用户按键。权限丢失不能绕过保护做cleanup，需提示用户释放；任何副作用仍记入报告 |
| 跨窗宏 | declared目标策略已进入IR | declared bundle id校验；执行期间焦点变化清matcher、由live focus guard检查locked/declared，不再将其单独当物理输入 | unexpected物理输入仍失效旧generation。目标是否已完成切换不可假定，需要显式等待及声明；T19声明转换实机未验 |
| 启动/托盘/退出/登录项 | 用户会话、托盘、Run登录启动实现 | GUI可运行；原生tap权限后台重试，Secure Input暂停，退出poll清理tap；用户级LaunchAgent显式开启下次登录运行background | Mac登录项/关窗/退出必须在本机验收；完整app链接与打包未验；Developer ID和公证凭据缺失 |
| 分发 | Windows NSIS开发包 | 源码 + adapter目标类型检查 | 没有Mac机器、签名/公证凭据；Intel Mac/WinARM未验。Linux没有native adapter |

macOS FFI使用系统ApplicationServices、CoreFoundation、Carbon、AppKit及Objective-C runtime，无新增第三方native依赖。CGPoint/CGRect按`repr(C)`布局，NSInteger/NSUInteger按指针宽度，NSEvent借用CGEvent先CFRetain再由发送函数释放。类型检查不会验证真实framework符号链接、权限、NSPasteboard目标读取或CGEvent消费，需要Mac实机验证。

完整应用Apple目标检查具体阻碍：`cargo check -p quicker-text --target aarch64-apple-darwin`在`objc2-exception-helper v0.1.1`的Objective-C `try_catch.m`构建阶段报`failed to find tool "cc": program not found`；Windows没有Apple SDK/Objective-C工具链，不能把platform crate通过扩写成整app通过。

API依据：[Apple Secure Input技术说明](https://developer.apple.com/library/archive/technotes/tn2150/_index.html)、[NSPasteboard](https://developer.apple.com/documentation/appkit/nspasteboard)、[NSEvent system-defined事件](https://developer.apple.com/documentation/appkit/nsevent/otherevent(with:location:modifierflags:timestamp:windownumber:context:subtype:data1:data2:))、[Apple IOHIDSystem特殊键定义](https://github.com/apple-oss-distributions/IOHIDFamily/blob/main/IOHIDSystem/IOKit/hidsystem/ev_keymap.h)。

本轮新增的浏览器/中文provider路径与匿名测试页说明见 [浏览器与IME手测矩阵](browser-ime-manual-tests.md)。当前目标状态展示provider、目标进程、composition、拒绝原因及采样时间；不显示正文。0.2.0 的构建不代替该矩阵验收。

Windows 0.2.0 手测与开发包限制见 [即时规则首测](windows-0.2.0-test-notes.md)。keys（包括手动动作测试）逐字符读回可能增加延迟；未测p95。目标自定义Backspace可能已有首步副作用；读回只停止后续操作，不能证明绝无误删或自动撤销。


Windows 0.2.0 keys（包括手动动作测试）需要可靠UIAutomation provider，按 Unicode 字符发送（emoji 的 UTF-16 代理对同批），每字符前后核对新鲜目标与预期正文前缀，并有界等待消费；正文或光标改变即停止后续。所有路径的paste／HTML／文件／图片发送后返回 unknown 并停止剩余步骤，不证明目标已接收。手动测试不能跳过键入provider检查；自动替换中需要核对正文的快捷键及非导航按键发送后也会unknown停止后续；手动独立快捷键没有该自动替换限制。复杂多步骤不作为完整交付／实机验收通过。



