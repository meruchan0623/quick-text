# Quicktext

用于 Windows / macOS 的文本指令管理桌面应用。Tauri 2 + React 提供管理、参数设置和高级动作编辑，Rust 负责规则与三种脚本解析。Windows 输入检测与注入纳入 espanso v2.4.1 的原生组件，不需要另装 Espanso。规则、导入、界面与动作仍由本项目管理。源码依据见 [espanso 核对记录](docs/espanso-source-audit.md)。

当前版本为 **0.3.1**，提供 Windows x64 绿色版，解压后运行 `quicktext.exe`。需要 Windows 的 WebView2 运行环境。绿色版表示免安装，配置仍保存在当前用户的应用数据目录，不会放在 exe 旁边。0.3.1 移除规则的允许进程限制；macOS 未迁移到 espanso，尚无实机运行、完整应用链接、签名或公证验证。

## Windows 绿色版与第一条指令

1. 退出旧版 Quicktext（托盘菜单中的“退出全部”），解压新绿色版，再启动 `quicktext.exe`。关闭窗口只会隐藏到托盘；首次使用默认暂停。
2. 添加一条测试指令，例如缩写 `thx`、立即执行、操作“粘贴纯文本”、正文 `Thanks very much.`。点击“保存”后立即生效，无需另行应用；如果当前暂停，请恢复监听。编辑、删除、批量启停和参数保存同样在确认成功后直接生效。
3. 先在记事本等普通输入框试用。“粘贴纯文本”一次输出整段文字；“键入纯文本”一个字一个字输入。两个操作可以在下拉框中直接选择，来回切换会保留正文。
4. 指令不再需要设置“生效进程”，Windows 和 Mac 均已移除允许列表。旧配置中的程序限制在加载时自动忽略，例如原先只允许 `notepad.exe` 的规则，现在也会在其他符合触发条件的程序中参与匹配。
5. Windows 使用 espanso RawInput 检测字符，对最近的输入做规则匹配，随后退格并输出。支持立即、空格和分号触发；Tab、Enter 触发不可用。RawInput 不能在目标程序收到这两个按键前阻止它们。正文不可读取不会阻止触发；这不等于所有聊天软件已经实测通过。
6. 黑名单、暂停、本程序窗口及已识别的密码/只读控件检查仍然有效。中文布局下的英文模式可输入字母、数字、标点缩写；中文选字时数字或空格可能用于选择候选词，建议切到英文模式后使用。
7. 输出报告 `unknown` 表示系统已接受输入，目标程序是否收到尚未确认。新链路不会仅因无法读取正文而停止后续动作；焦点变化、继续按键、暂停或取消仍会中断执行。

本次允许进程移除与验证结果见 [0.3.1 更新记录](docs/windows-0.3.1-notes.md)。输入输出重构的历史依据见 [0.3.0 重构记录](docs/windows-0.3.0-refactor.md)。旧版修复说明属于历史记录，不代表当前实现或当前验收状态。

测试动作会在目标窗口真实输入。不要把浏览器中的管理界面预览当作原生输入验证。普通权限应用不能操作管理员权限窗口。粘贴文字、HTML、文件或图片后，软件仍可能无法确认接收结果；已经发生的输入、点击或程序启动无法自动撤销。

## JSON / Excel 配置导入导出

管理页点击「导出」保存 JSON，或点击旁边的下拉箭头选择「导出 JSON 文件」「导出 Excel 文件」。两种格式均导出当前配置，导出前会保存参数页尚未保存的修改，并支持通过「导入」重新导回。Excel 中可阅读「填写说明」和「填写示例」，再在「指令」修改或添加缩写、动作、正文等字段；高级列可展开，支持正则、参数、结束键、步骤目标程序、文件、图片、HTML、按键、打开目标、常用功能与高级动作 JSON。

导出的简单文本动作可直接修改正文；复杂动作保存在「高级动作JSON」列。全局参数在「全局设置」页，超长内容通过「长内容」页分段保存；请保留隐藏的「原始配置」页，以便保留扩展字段与未识别指令。修改已有指令时保留其编号；同一次导入不要同时删除旧编号并新增编号，以免产生改名歧义。

「操作步骤」「调用参数」「捕获参数」通过指令编号关联主表；步骤调用参数还需填步骤顺序。示例页不会导入。单元格文字保留前导零与换行；请粘贴为值，公式会被拒绝。Excel 不能直接录制键鼠、拾取屏幕坐标或选择文件，路径与坐标须填写；录制、拾取和文件选择仍在软件界面中操作。文件与图片的路径须在实际使用指令的机器上存在，模板不打包这些文件。

填写后点击「导入」选择 `.xlsx`，按重复编号策略合并，核对中文预览报告，再确认导入；确认成功后直接保存并生效，取消预览不修改当前配置。填写固定指令编号后，可修改表格并重复导入，选择「用文件中的内容更新」更新编号相同的指令并保留其他指令；默认「保留现有指令，只添加新的」不覆盖相同编号。新导出的 Excel 和 JSON 配置文件选择「替换全部指令」时恢复文件中的规则与全局设置；其他策略保留当前全局设置。旧版空白 Excel 模板仍可导入，仅导入规则并保留当前全局设置。带副作用的动作自动禁用，需审阅内容后手动启用。旧版模板已在 Windows Excel 16 检查打开、下拉验证、冻结行列，以及 11 条合成指令填写另存后由生产解析器读回；WPS、macOS Excel 和真实输入执行尚未验收，这些历史检查不代表新导出格式或原生应用全链路已验收。

## macOS 开发与权限

在本机 Mac 安装 Xcode Command Line Tools、Rust stable 和 Node.js，运行 `npm ci`、`npm run tauri dev`；本机验证构建运行 `npm run tauri -- build --no-bundle`（只生成程序，不生成安装包）。macOS 的 app/dmg 安装包目前没有发布流程：第三方声明和依赖源码包只覆盖 Windows x64 发布，打包钩子 `scripts/check-bundle-preconditions.ps1` 会在 macOS 上拒绝打包，要发布 Mac 安装包须先把这条流程扩展到 macOS。应先在 Apple Silicon Mac 验证；Intel Mac 另行编译和验收。仓库 Windows 环境成功执行了 `cargo check -p quicker-platform --target aarch64-apple-darwin`，该命令不链接 macOS frameworks，也不代表完整 Tauri 应用或安装包可运行。

此前还尝试了 `cargo check -p quicktext --target aarch64-apple-darwin`，在 `objc2-exception-helper v0.1.1` 编译 Objective-C `try_catch.m` 时失败：`failed to find tool "cc": program not found`。当前 Windows 环境没有 Apple Objective-C 编译/SDK工具链；需在用户本机 Mac 完成这个完整应用检查。

首次运行在“系统设置 → 隐私与安全性”中给实际安装的 Quicktext 应用授予“辅助功能”和“输入监控”。开发运行可能需要给当前可执行文件或宿主终端授权；以实际系统提示为准。授权后可能需重启，再手动恢复。权限不足时 GUI 保持可用、引擎保持暂停并显示原因；不会绕过 Secure Input。更换应用路径或签名后重新核对授权。

选择 **ABC 或 U.S.** 输入源（支持 `com.apple.keylayout.ABC` / `com.apple.keylayout.US`），再在 TextEdit 的纯文本窗口测试 `qtzzx`。ABC Extended、U.S. International等dead-key布局和IME拒绝，避免把组合中的ASCII误当已经上屏。安全输入、不可识别的窗口/控件或本应用自身均拒绝触发。Ctrl 导入仍为 Ctrl；新动作可用平台常用修饰键，在 Mac 映射 Cmd。Windows `#键值` 只走显式映射，未知值在删除缩写前报错。

Mac adapter 已实现 CGEventTap、CGEvent、AX 焦点/选区、NSPasteboard 文本/HTML/file-URL/PNG、多屏 Quartz 坐标、键鼠及媒体/音量/亮度事件。不同应用和硬件可能不消费这些事件，成功发送返回“未知”。Windows 1/120-click 滚轮 delta 没有经过验证的 Mac 等价映射，预检拒绝并提示使用 clicks。

Mac自动目标也保守限定为AX可观察的 `AXTextArea` / `AXTextField`，安全字段和其他角色拒绝。任意浏览器编辑区、特殊应用控件并未因此获得通用支持。

macOS 分发的 Developer ID、签名和公证凭据未提供；尚未生成已签名/公证的 Mac 安装包。登录启动使用本应用用户级 LaunchAgent，显式开启后下次登录运行 `--background`，关闭只移除本应用plist；用户自己的 Mac 仍需核对登录项权限和实际启动。详细边界见 [平台支持](docs/platform-support.md)。

## 开发与配置

在仓库根目录运行 `npm ci`、`npm run tauri dev`。自动检查使用 `npm run build` 和 `cargo test --workspace`；最终通过数量以本轮检查输出为准。这些检查不证明目标程序实际收到输入。本机自用的 exe 用 `npm run tauri -- build --no-bundle`（或先 `npm run build` 再 `cargo build -p quicktext --release`）构建；`target\release\quicktext.exe` 只是中间产物，不能直接分发。要分发的构建（安装包、绿色包）必须同时带第三方声明和依赖源码包：安装包用 `./scripts/build-windows.ps1 -DependencySourcesZip <Source-deps.zip>`，绿色包见下一节；缺少依赖源码包时两者都直接失败。直接运行 `npm run tauri build`（会打安装包）同样被 `scripts/check-bundle-preconditions.ps1` 拒绝。

配置在 Tauri 当前用户应用数据目录，以 `active.json` 为当前生效配置，`revisions/` 保存历史版本及对应原始迁移元数据。保存先校验，再一次提交到磁盘和监听引擎；版本冲突或保存失败时继续使用旧配置。没有待应用草稿，也没有独立的“应用设置”操作。导出使用已保存配置；历史恢复会创建新版本并恢复对应原始元数据。升级前通过 GUI 导出本地包并保留旧版程序。旧版未确认的文件仅保全，不自动启用。详见[保存即生效](docs/direct-save.md)。不要直接读取原 Quicker 的加密数据库。

SendKeys B、inputscript、replay 是三个独立格式；导入保留 dialect、行列和不支持报告。高级源码不是主入口。原 Quicker 私有动作、账户、私有云和任意脚本没有直接运行承诺；需明确映射为自有动作。

导入接受 Quicker「文本指令」导出的 JSON 数组（可带 UTF-8 BOM），无需手工转换。普通文本、已核实的快捷键、SendKeys、inputscript、固定打开目标、常用操作和录制回放有对应转换；HTML/外部文件及不能直接兼容的内容保留原始数据并说明处理方法。自动化动作导入后须审阅再启用，确认导入不会直接执行动作。未知字段、错误类型、不可兼容语法会停用，重复 ID 拒绝整个导入。原进程绑定仅保留在迁移元数据中，不再限制触发范围。逐项支持、平台差异和未支持原因见[原版 Quicker 导入兼容说明](docs/quicker-import-compatibility.md)。

完整 F/G/A/T 状态、已取得证据与用户手工步骤见 [验收记录](docs/acceptance.md)。

新规则默认继承全局 IME 删除方式，旧规则显式设置保留。运行状态页显示当前目标观察方式、进程、组合状态及拒绝原因，不显示正文；管理窗口获焦也会改变目标。匿名浏览器测试页位于 `scripts/fixtures/browser-input.html`，使用真实键盘按 [浏览器与中文IME手测矩阵](docs/browser-ime-manual-tests.md) 验证。

Windows 输出检查焦点、配置版本和执行代次，使用带本程序标记的 espanso 注入实现，防止输出再次触发规则。剪贴板继续使用本项目的多格式备份和变更保护。此实现不逐字符读取目标正文，不保证目标的自定义退格、页面脚本移动光标或输入法选词行为；复杂多步骤与 p95 延迟尚未完成完整实机验收。

自动验收使用独立配置与显式双门槛启动模式，详见 [Windows 验收模式](docs/windows-acceptance-mode.md)。正常启动使用真实 RawInput；自动注入验收通过不代表实物键盘入口或中文 IME 已验收。

## 许可与绿色包构建

Windows 交付静态纳入 GPL-3.0-or-later 的 espanso 组件，整个项目（含自研代码）按 GPL-3.0-or-later 授权与分发，完整许可见 [LICENSE](LICENSE)。上游版权、完整许可和本地补丁清单见 [第三方声明](THIRD_PARTY_NOTICES.md)。每个发布（安装包、单文件 exe、绿色包）都附带对应的完整源码，由两个压缩包合在一起组成：`Quicktext-<版本>-Source.zip`（绿色包为其 `source/` 目录）包括修改后的原生组件、`Cargo.lock`、`package-lock.json` 与构建、打包脚本；`Quicktext-<版本>-Source-deps.zip` 是编进程序的 Rust crate（`cargo vendor --locked`）和前端运行时 npm 包（注册表的原始 tarball）的源码，附离线重建步骤。Rust 与前端依赖按各自的许可证使用，它们的版权与许可证原文汇总在 `THIRD-PARTY-LICENSES.txt`：安装包和绿色包里各有一份，发布附件里也有。这个文件由 `scripts/generate-third-party-licenses.ps1` 按实际分发的依赖生成，仓库里只放占位文件；任何依赖缺许可证文字时生成失败，打包随之失败。Rust 依赖的许可证由 `deny.toml` 对照与 GPL-3.0-or-later 兼容的清单检查，发布前的核对项见 [发布清单](docs/release-checklist.md)。也可从版本库的发布标签获取源码。已发布的 0.3.1 的许可证说明与源码获取方式见 [docs/licensing-0.3.1.md](docs/licensing-0.3.1.md)。

绿色包用 PowerShell 7 在仓库根目录构建，依赖源码包是必需的，没有它不会产出任何可分发的文件：

1. `npm ci`、`cargo fetch --locked`。构建需要 Rust、Node.js、MSVC C++ 工具和 Windows SDK。
2. `./scripts/package-dependency-sources.ps1 -OutputDirectory <依赖包目录>`：生成 `Quicktext-<版本>-Source-deps.zip`（需要网络，crates.io 和 npm 注册表（npm 包用 `npm pack` 取，命中 npm 缓存就不联网），约一分钟，约 100 MB；写完后脚本用同一个检查函数把成品再验一遍）。
3. `./scripts/generate-third-party-licenses.ps1`：生成 `THIRD-PARTY-LICENSES.txt`，输出写到 `src-tauri/resources/`，提交前用 `git checkout -- src-tauri/resources/THIRD-PARTY-LICENSES.txt` 还原占位文件。
4. `npm run build`、`cargo build -p quicktext --release`。
5. `./scripts/package-portable.ps1 -OutputDirectory <新输出目录> -DependencySourcesZip <依赖包目录>\Quicktext-<版本>-Source-deps.zip`。

`-DependencySourcesZip` 是必填参数，不带它脚本直接失败。脚本在写入任何文件之前先检查：声明文件不是占位文件，且与本树的锁文件一致；依赖源码包的文件名与版本正确，包内条目名规范（不重名，没有仅大小写不同的别名，也没有 `\`、`..`、Windows 保留设备名等在 Windows 上不能原样解压的名字，先于任何哈希检查，整个包一次判定），清单里的 `Cargo.lock` 与 `package-lock.json` 哈希与本树一致，每个 crate 都在包里、校验和与 `Cargo.lock` 对得上，并且它的 `.cargo-checksum.json` 列出的每个文件（源码、`build.rs` 等）都在包里、SHA256 一致；每个 npm 包都以注册表原始 tarball 的形式在包里，哈希等于 `package-lock.json` 的 `integrity`。这些检查防的是不小心发出不完整、过期或不匹配的包，不防有人同时改文件和校验文件。任何一项不符都报错退出，输出目录里不留 ZIP（中途失败时本次生成的文件会被清理）。通过后，依赖源码包被复制到绿色包 ZIP 旁边，同一输出目录，文件名 `Quicktext-<版本>-Source-deps.zip`（约 100 MB，放进约 5 MB 的绿色包会让下载变得很大，所以作为附属文件，与安装包发布时 `Source.zip` 和 `Source-deps.zip` 并列的方式一致）；ZIP 里的 `DEPENDENCY-SOURCES.txt` 写明它的文件名与 SHA256。分发绿色包时这两个文件必须一起提供。脚本还会逐文件校验 ZIP 与源目录的 SHA256，不覆盖已有交付包。复制 exe 之前，脚本核对 `dist/` 就是这个 exe 嵌入的前端（构建时记在 exe 里的摘要），再扫描其中没有 mock 后端，所以第 4 步之后不要单独重建 `dist/`（需要 Node.js）。安装包用 `./scripts/build-windows.ps1 -DependencySourcesZip <...>`（同样必填，同样的检查）。正式发布由 `release` 工作流完成这些步骤，见 [发布清单](docs/release-checklist.md)。
