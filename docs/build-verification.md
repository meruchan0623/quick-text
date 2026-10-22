> 2026-10-15 更新：本文保留 0.2.x 的历史实现/验收记录。Windows 0.3.0 已改为直接集成 espanso 输入组件，当前行为与验收状态以 [0.3.0 重构记录](windows-0.3.0-refactor.md) 为准。

# 构建验证记录

2026-10-14，Windows x64；Cargo 1.95.0，Rust 1.95.0，npm 11.6.2。

以下为0.1.0历史构建证据：

- `npm run build`：TypeScript 检查及 Vite 7.3.7 生产资源构建。
- `cargo check -p quicker-text`：Tauri 2 桌面后端与 Windows adapter 编译检查。
- `cargo test --workspace`：78 项通过，0 失败。包括 71 项规则／三 dialect／动态值／导入／事务模型，3 项 Windows 静态预检及剪贴板格式，4 项本地配置持久发布／故障／冲突／回滚／revision overflow。
- `npm run tauri -- build --bundles nsis`：已生成 Windows release exe 和 NSIS x64 安装包。安装包未数字签名；本会话未执行安装或升级迁移验收。

本地配置持久化故障测试真实制造 `draft.json` 为目录，验证发布失败保持旧 `active.json` 和有效 revision，后台恢复原暂停状态；外部修改 `active.json` 会使发布拒绝；历史有效版本保存在 `revisions/N.json`，失败发布不覆盖旧历史。

本机只读窗口枚举确认现代 Notepad 包含 `NotepadTextBox`／`RichEditD2DPT` 控件，适合后续安全路径手测。该控件证据不等于物理键盘 hook 已通过。

此前桌面操作验证了保存草稿 v0 → 应用 v1 → 暂停／恢复界面状态；其他系统输入验收未完成。电脑工具使用外部模拟按键，受到原生 hook 的注入过滤，不能替代物理键盘验收。后续桌面测试按用户指示停止。完整差距见 acceptance.md 与 platform-support.md。

macOS Rust platform-only 交叉检查由平台适配任务单独记录；Windows 环境无法证明完整 macOS Tauri 应用编译、权限交互、签名、公证或实机输入结果。

## 0.2.0 前端增量验证

- `npm run build` 通过：TypeScript 与 Vite，36 modules，最终资源构建 829ms；该耗时是资源构建时间，不是输入延迟。
- 匿名浏览器 fixture 的内联脚本通过 Node 语法检查；package/package-lock根版本与Tauri版本均为0.2.0。
- React Doctor `--scope changed` 无法运行：当前仓库没有HEAD基准（工具报 `Diff base branch "HEAD" does not exist`）。改用全量检查，86/100，剩余2 warning：App既有React函数复杂度、CaptureTemplate既有prop后state调整。新RuntimeStatus复杂度警告已拆分消除；无法据此宣称差异分数未回归。
- 此记录不包含本轮Rust或安装包构建结果，也不包含浏览器操作/真实键盘/中文IME通过证据；由主任务完成独立审查和统一打包后补充。

## 0.2.0 初次集成检查（2026-10-14；最终票据／输出修复前）

本轮在 Windows x64 运行并通过：

- `cargo test --workspace`：98 项通过，0 失败，0 忽略。domain 86 项（动态来源 6、输入会话 15、规则语义 65）、Windows platform 8 项、配置持久化后端 4 项；另有 0 项单元／文档测试的空套件。
- `npm run build`：TypeScript 检查及 Vite 7.3.7 生产构建通过，36 modules，资源构建 725ms；该数字不是输入延迟。
- `cargo check -p quicker-platform --target aarch64-apple-darwin`：通过 Apple Silicon 平台模块类型检查；不包含完整 Tauri 链接、Mac 安装包或实机权限／输入验证。
- 根 package.json／package-lock.json、workspace、Tauri 配置及 platform 为 0.2.0。domain 已统一继承 workspace 版本 0.2.0；配置与导出包 SCHEMA_VERSION 仍为 1。旧 B 动态值默认 literal、缺省／null IME 设置继承、旧显式 IME 字符串保留覆盖的测试通过。

本轮仅执行命令行集成验证，没有浏览器、桌面、物理键盘、中文输入法或安装升级实测。用户自行验收请使用 `browser-ime-manual-tests.md`、`platform-support.md` 和 `acceptance.md`；安全拒绝不等于目标功能支持通过。最终开发包等待独立审查协调后生成。


本轮UI最终边界提示后 
pm run build 通过（36 modules，639ms）。React Doctor changed因仓库无HEAD基准无法运行；full备用86/100，仍为App复杂度与CaptureTemplate既有state/prop警告，与先前full结果一致。差异扫描未完成，不能宣称changed零回归。



## 0.2.0 最终冻结与交付检查

独立对抗评审已闭环，主代理确认源码冻结后允许生成开发包。本轮最终命令行检查：

- `cargo test --workspace`：110 项通过（domain 86、Windows platform 20、backend 4），0 失败／忽略。平台增量包括物理输入票据、候选→提交证据、结束键透传、Shift释放不续期、目标脚本移光标后的输出停止、emoji整对UTF-16发送及剪贴板副作用记录。
- `npm run build`：TypeScript／Vite通过，36 modules，836ms；不是输入延迟。
- `cargo check -p quicker-platform --target aarch64-apple-darwin`：通过；domain／platform均为0.2.0，配置schema仍1。
- 最后剪贴板恢复备份修复由Windows作者及独立评审核对：恢复成功才清除备份；失败保留以供重试。不是剪贴板竞态或目标消费实机验收。

本轮无浏览器、桌面、物理键盘、中文IME、安装升级或Mac实机验收；即使对抗模型测试通过，也不能宣称通用浏览器／IME支持、绝无误删或p95达标。Windows版本仅即时已上屏自动触发，完整限制见 `windows-0.2.0-test-notes.md`。旧0.1.0交付保留，新包独立命名并另存SHA256及可读构建日志。现有MIT根LICENSE与domain GPL metadata均原样保留，未作重授权。


最终 `npm run tauri -- build --bundles nsis` 成功：release优化编译53.86s，beforeBuild资源构建661ms，生成0.2.0 Windows x64 exe及NSIS安装包（约2.78 MiB）。未运行安装器、未安装／升级验收，未签名。打包日志另存UTF8交付目录；源码白名单与拷贝文件SHA256将在交付前独立核对。
