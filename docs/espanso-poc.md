> 2026-10-15 更新：本文保留 0.2.x 的历史实现/验收记录。Windows 0.3.0 已改为直接集成 espanso 输入组件，当前行为与验收状态以 [0.3.0 重构记录](windows-0.3.0-refactor.md) 为准。

# 输入后端 PoC 与工程路线

2026-10-14，在 Windows x64 以 Cargo 1.95 / Rust 1.95 执行。

实测仓库：`https://github.com/espanso/espanso`，tag `v2.4.1`，commit `a87d1723f47a59e38d370e2f0c636d66d568b8f6`。

命令：`git clone --depth 1 --branch v2.4.1 --filter=blob:none https://github.com/espanso/espanso.git C:\Develop\quicker-text-poc-espanso`；随后 `cargo check -p espanso-detect`。

结果：`Finished dev profile [unoptimized + debuginfo] target(s) in 1m 38s`。工具链可编译原生 C++ 检测模块。因此不存在“Espanso 无法编译”的结论。

源码证据：`espanso-detect/src/win32/native.cpp` 的 93 行走 `WM_INPUT`，360 行注册 `RegisterRawInputDevices`，272–273 行才将事件通知 Rust，callback 不返回按键抑制决策。RawInput 检测本身不能在 Tab/Enter 送达目标前消费事件；这正是原计划预见的适配缺口。

当前工程决策采用计划允许的独立 Rust fallback：替换检测层为单一低级 hook（可消费事件），以同一 immutable compiled domain snapshot 匹配、同一队列执行。没有启动 Espanso 服务，没有双监听或双匹配，也没有将 Espanso 源码链接进交付。

此决策不是声称 fork 无法实现。若保留 fork，需要同步替换 RawInput 检测、bridge 本项目 typed IR、改剪贴板多格式备份及 generation、改 render failure 和删除事务；现有检测模块的直接复用收益减少，而 GPL fork 维护、C++ ABI 和升级补丁仍存在。当前独立后端集中这些改动在 `crates/platform`，降低首次交付的集成边界；代价是自行负责布局/IME/macOS 适配。Espanso 其他成熟平台能力没有因此自动获得，未验收项明确记录。

本 PoC 是编译和源码集成评估，不是对 Espanso 的系统输入验收，也不是本应用全部原生行为通过的证明。
