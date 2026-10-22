# Espanso Windows 输入输出源码审计

审计日期：2026-10-15。本文区分上游已核实事实、集成建议和待验收行为。本文不是本项目新版已完成或真实输入法已通过验收的声明；本次实现、构建和设备测试结果应以对应代码及测试报告为准。

## 固定来源

- 官方仓库：<https://github.com/espanso/espanso>。
- 版本：`v2.4.1`。
- 完整 commit：`a87d1723f47a59e38d370e2f0c636d66d568b8f6`，已通过远端 tag 查询与本地 checkout 核实。
- 本次参考 checkout：`work/espanso-reference`，研究完成时 HEAD 为上述 commit，工作树干净。该目录是研究副本，不应随便携包打包。
- 关键许可：源文件头为 Federico Terzi 的版权声明及 `GPL-3.0-or-later`；根 `LICENSE` 为 GPLv3 全文。

以下行号均针对这个固定 commit，不能套用到未来分支。

## 检测链路事实

| 事实 | 固定源码证据 | 对本项目的含义 |
| --- | --- | --- |
| 检测通过隐藏窗口接收 `WM_INPUT`，不是 `WH_KEYBOARD_LL` | [detect/native.cpp:68–124](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/src/win32/native.cpp#L68-L124) | 不能把“复用 Espanso 检测”写成沿用旧低级 hook；后台检测不依赖目标控件提供完整正文 |
| `hDevice == 0` 被标为无已知硬件来源 | [detect/native.cpp:124](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/src/win32/native.cpp#L124) | 递归抑制依赖 bridge 过滤；native 仍会回调这些事件 |
| 按缓存间隔从前台窗口线程读取 HKL | [detect/native.cpp:130–154](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/src/win32/native.cpp#L130-L154) | 切换布局与切换前台窗口可能短暂使用缓存；默认间隔是 2000ms，不是每键重新获取 |
| 字符由 `GetKeyboardState`、`ToUnicodeEx` 和 HKL 转换；flags 为 `5` | [detect/native.cpp:156–191](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/src/win32/native.cpp#L156-L191) | 输出是键对应的 Unicode 值，不是从目标控件读取的最终提交文本 |
| Alt 且未按 Ctrl 时清空字符；AltGr 保留；scan/E0 区分部分左右 modifier | [detect/native.cpp:177–220](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/src/win32/native.cpp#L177-L220) | 不宜照搬旧美式键位映射；需保留布局字符及修饰键语义 |
| callback 在窗口过程内直接调用，返回 `void` | [detect/native.cpp:270–273](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/src/win32/native.cpp#L270-L273)、[detect/native.h](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/src/win32/native.h) | 回调应短时入队，不能在其中等待 UIA、渲染或注入；不能返回“消费 Enter/Tab” |
| 键盘注册 `RIDEV_NOLEGACY | RIDEV_INPUTSINK`，鼠标注册 `RIDEV_INPUTSINK` | [detect/native.cpp:347–360](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/src/win32/native.cpp#L347-L360) | 这不是对其他进程的终止键拦截能力 |
| 初始化创建窗口，eventloop 使用 `GetMessage/DispatchMessage` | [detect/native.cpp:283–395](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/src/win32/native.cpp#L283-L395) | 初始化、消息泵和窗口销毁应由同一个持久接收线程管理 |

上游 Rust API 为 `Source::initialize(&mut self)`、`Source::eventloop(&self, Box<dyn Fn(InputEvent)>)`，`get_source(SourceCreationOptions)` 返回 `Box<dyn Source>`。Windows 默认 `win32_exclude_orphan_events=true`、`win32_keyboard_layout_cache_interval=2000`，见 [detect/lib.rs:38–96](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/src/lib.rs#L38-L96)。

上游在 Rust callback 中才过滤 orphan 事件，并结合 `espanso-info` 的展开事件计数处理自身输入，见 [detect/win32/mod.rs:192–212](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/src/win32/mod.rs#L192-L212) 及 [info/lib.rs:130–148](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-info/src/lib.rs#L130-L148)。只复制 native 文件不会自动获得这层保护。

## 输入法边界与原链路缺陷

必须分别描述“当前键盘布局”“物理按键字符”“正在组合的候选串”和“已提交正文”。中文 HKL 或输入法开启状态不等于正在 composition。`Unknown` 也不能改写为已确定的 `Active` 或 `Inactive`。

这个 Espanso Windows detector 没有 IMM/TSF composition 查询，不证明 ToUnicodeEx 字符已经提交到目标控件。迁移目标是让布局转换和物理键检测独立工作，避免 UIA provider 读取失败、正文不可读、TextPattern 缺失成为普通快捷词的硬性拒绝条件；不能把旧的 ASCII-only 或仅连续英文字母门槛换个函数名继续保留。布局支持范围应来自转换结果与真实测试，而不是中文布局黑名单。

审计时旧 `windows_physical.rs` 的缓冲入口只接纳 ASCII 可见字符/空格，另有只允许 ASCII 字母的原始按键兼容路径；旧观察链路还围绕 UIA 文本、caret、composition 证据维护删除授权。这样的设计把可访问性可读性与普通键盘快捷词耦合，不能仅因单元测试通过而认为跨控件可靠。上述是迁移前问题定位；并行实现期间文件可能变化，不能据此判断最终新版仍有同样缺陷。

边界也不能反向取消：检测到了快捷词不等于确认了目标控件最终内容。输入法候选、焦点变更、用户继续输入、删除数量、目标对 Unicode/粘贴的响应仍须独立验收。需要明确报告已验证、失败、未测试及阻塞，不把源码移植或编译成功写成中文输入法通过。

RawInput 不消费目标按键，因此即时触发和空格后触发可以按输入后替换设计；Enter/Tab 可能先造成发送、换行或焦点移动。没有独立完成消费机制和验证前，不能宣称自动支持 Enter/Tab 终止触发。

## 注入、退格与时序

| 行为 | 固定源码证据 | 建议 |
| --- | --- | --- |
| 字符串以 UTF-16 单元生成 Unicode down/up，单批 `SendInput` | [inject/native.cpp:42–58](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-inject/src/win32/native.cpp#L42-L58) | 可复用为 Keys 文本输出基础；不等同于逐物理字母键，也不等同于剪贴板粘贴 |
| 序列逐键 down/up，组合全部 down 后逆序 up | [inject/native.cpp:60–109](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-inject/src/win32/native.cpp#L60-L109) | 复用按键组合和退格基础，保持组合释放顺序 |
| 延迟变体每个事件后 `Sleep` | [inject/native.cpp:111–162](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-inject/src/win32/native.cpp#L111-L162) | 延迟留给执行线程；不能阻塞检测消息泵 |
| 注入前 modifier 等待在 engine 内，100ms 轮询、3秒超时 | [delay_modifiers.rs:27–70](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-engine/src/process/middleware/delay_modifiers.rs#L27-L70) | 仅复制 native 不会自动等待物理 modifier 释放；本项目应自行选择并验证超时策略 |
| 换行由上层拆 CRLF/LF，行间注入 Enter | [event_injector.rs:50–81](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso/src/cli/worker/engine/dispatch/executor/event_injector.rs#L50-L81) | 直接调用字符串 FFI 不包含换行适配；必须保留本项目宏和文本动作语义 |
| trigger 按 Unicode scalar 数生成 Backspace | [action.rs:119–149](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-engine/src/process/middleware/action.rs#L119-L149) | 不是控件读回证明；emoji、组合字符、IME 候选与不同控件需实际测试 |

上游 `Injector` 暴露 `send_string`、`send_keys`、`send_key_combination`，Windows 的 `InjectionOptions.delay` 默认 0，见 [inject/lib.rs:37–84](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-inject/src/lib.rs#L37-L84)。但 Windows `send_string` 忽略传入 options，原 native 函数均返回 void、没有检查 `SendInput` 结果。若补上发送数检查或分批节流，应作为本项目修改记录，不能称为原封不动的上游行为。

## 剪贴板与性能

上游 clipboard adapter 默认使用 Ctrl+V，支持自定义组合，见 [clipboard_injector.rs:65–104](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso/src/cli/worker/engine/dispatch/executor/clipboard_injector.rs#L65-L104)。其 restore guard 只备份 `Option<String>`，等待固定时长后 set_text，见 [同文件:181–218](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso/src/cli/worker/engine/dispatch/executor/clipboard_injector.rs#L181-L218)。Windows native 写入时先 EmptyClipboard，见 [clipboard/native.cpp:92–114](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-clipboard/src/win32/native.cpp#L92-L114)。

因此建议仅复用 detect/inject，保留本项目剪贴板多格式备份、恢复和并发修改保护；不为追求源码相似度降级到仅恢复纯文本。Paste 和 Keys 两种用户可见动作应保持独立，不能为修复一条路径而静默互换。

上游默认 pre-paste 为100ms、组合事件间隔10ms、恢复延迟300ms，自动 clipboard 阈值100字符，见 [config/default.rs:20–23](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-config/src/config/default.rs#L20-L23)。这些是策略参数，不是测得的性能保证。detect 每个 RawInput 分配数据 vector，键盘事件再分配256字节状态 vector；没有从源码推导 p95 的依据。真正延迟应分别测量检测、排队、删除、注入和目标控件最终响应。

## 最小移植范围与自有能力复用

建议 vendor `espanso-detect/src/win32/native.cpp/native.h` 与 `espanso-inject/src/win32/native.cpp/native.h`，再以 Rust 薄 bridge 转成本项目事件和动作。不启动 Espanso 服务，不增加第二套匹配器，不搬 YAML 配置或整个 engine。

最小 native 构建只需 C++ 编译环境、`cc` build dependency 和 `user32`；GNU target 另需 `stdc++`，见 [detect/build.rs:21–37](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-detect/build.rs#L21-L37) 与 [inject/build.rs:21–37](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/espanso-inject/build.rs#L21-L37)。引入完整 crate 则会额外带入 `espanso-info`、Rust 字符串转换、热键和跨平台依赖；当前 Windows 薄桥接无需为此引入整个 workspace。

本项目 `crates/domain` 的规则模型、编译匹配、Quicker 导入、宏解析和 typed actions 应继续作为业务语义来源；现有编辑器、规则存储和宏执行队列与新输入后端对接。Espanso 负责成熟的 Windows 键盘事件/注入基础，本项目继续负责规则语义、启停/范围控制、渲染失败处理、焦点复核、宏调度和剪贴板事务。这里描述接线边界，不声明并行实现已完成这些接线。

native 保存 Rust 上下文裸指针，bridge 应保证稳定地址和足够生命周期，避免 panic 越过 FFI。上游固定注册窗口类但 destroy 不注销类，进程内反复初始化需要处理 class already exists，或维持单个线程；退出和 DestroyWindow 应在窗口所属线程处理。原 `GetMessage` 循环也没有分别处理错误返回。这些小范围修正应明确登记为移植补丁。

## GPL 组合交付

[上游 LICENSE](https://github.com/espanso/espanso/blob/a87d1723f47a59e38d370e2f0c636d66d568b8f6/LICENSE) 和每个 native 文件的版权/许可头必须保留。vendor 清单应记录仓库、tag、完整 commit、上游路径、修改说明与日期，并在第三方声明中提供来源。

本项目审计时 workspace 使用 MIT 元数据。静态链接 GPL native 后，不能仅加署名仍把组合交付描述为纯 MIT。应协调 package/workspace 许可元数据、第三方声明、完整 GPL 文本以及二进制对应源码提供方式，使组合交付符合 GPL；自有原始 MIT 代码的许可说明可保留。对应源码应覆盖实际分发版本及构建所需脚本、本地补丁，不能仅链接未修改的上游仓库而遗漏自己的改动。

本次研究只核对源码和来源，没有执行客户软件输入、发送消息或进行完整设备验收。原 `docs/espanso-poc.md` 记录的是此前编译评估与当时 fallback 决策；后续若已实际接入 vendor，需由最终实现同步更新该历史说明，避免把“当时未链接 Espanso”误读为当前分发状态。
