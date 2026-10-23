# 0.4 IPC 约定（前后端共同依据）

本文是 `docs/development-plan.md` §7.4（B-44、B-45）在 0.4 范围内的落地细化，供后端（`src-tauri`）与前端（`src/ipc/`）并行实现时对齐。两边都以本文为准；与计划冲突时以计划为准并回改本文。

Rust 侧的共享类型在 `crates/platform/src/health.rs`，JSON 形状由其 serde 派生决定；下文的 TypeScript 写法只是那些派生结果的手写镜像（0.5 起由 tauri-specta 生成，B-46）。

## 1. 通用约定

- 所有枚举值在 JSON 中都是 `snake_case` 字符串。
- 时间戳一律为 Unix 毫秒（`*_ms`，`number`）；耗时为微秒（`*_us`）。
- `u64` 在 JSON 中是 `number`。0.4 的取值范围（seq、修订号、毫秒时间戳）都远小于 2^53。
- `Option<T>` 为 `null`；带 `skip_serializing_if` 的字段可能缺省，前端按「缺省等同于 null 或空数组」处理。
- 后端可以新增字段与枚举值，前端必须容忍未知字段，并把未知枚举值按「其他」显示，不能抛错。

## 2. `CommandError`（B-45）

所有命令都返回 `Result<T, CommandError>`。Tauri 在 `invoke` 被拒绝时把错误值原样交给前端：

```json
{ "code": "safe_mode", "detail": { "reason": "crash_loop" } }
{ "code": "busy" }
{ "code": "internal", "detail": { "trace_id": "6f1c…" } }
```

单元变体（`busy`、`token_expired`、`cancelled`）没有 `detail` 字段。

| code | detail | 说明 |
|---|---|---|
| `validation` | `{ diagnostics: Diagnostic[] }` | `Diagnostic` 即 `quicker_domain::model::Diagnostic`：`{ code, message, path, span: { line, column, length }, severity: "error" \| "warning" }` |
| `rule_conflict` | `{ id, current }` | 0.5 使用，0.4 先定义 |
| `revision_conflict` | `{ current: number }` | |
| `safe_mode` | `{ reason: SafeModeReason }` | 安全模式下的写命令 |
| `io` | `{ op: IoOp, path: string \| null, os_code: number \| null }` | `path` 只给数据目录下的相对路径或文件名，不给用户目录全路径。`path` 为 `backup-pre-0.4` 表示升级前备份没有做成（计划 §9，R6）：这次写入没有执行，已有配置照常生效，下次写入会重试；同时会有 `backup_failed` 启动问题 |
| `not_found` | `{ kind: EntityKind, id }` | |
| `busy` | — | 执行器正忙 |
| `engine_unavailable` | `{ health: HealthState }` | |
| `unsupported` | `{ capability: string }` | 能力 id，见 §5 `get_capabilities` |
| `hotkey_conflict` | `{ combo }` | 0.5 使用 |
| `token_expired` | — | 0.5 使用 |
| `cancelled` | — | 用户主动取消的流程；前端不弹错误 |
| `internal` | `{ trace_id }` | 原始错误文本只写日志（`tracing::error!(trace_id, ...)`），不返回前端 |

- `SafeModeReason` = `crash_loop` \| `locked` \| `newer_schema` \| `no_valid_config`
- `IoOp` = `read` \| `write` \| `rename` \| `create_dir` \| `remove` \| `open` \| `dialog` \| `other`
- `EntityKind` = `rule` \| `group` \| `revision` \| `file` \| `history` \| `window`
- `HealthState` = `starting` \| `healthy` \| `degraded` \| `recovering` \| `failed`

错误里不带中文文案，前端按 `code` 和 `detail` 翻译（`src/ipc/errors.ts`）。收到无法识别的值（例如旧后端返回的纯字符串）时，归为 `internal`，`trace_id` 为空。

## 3. 事件（B-44）

广播事件的载荷顶层都有 `seq: number`，取自同一个全局单调递增计数器（`src-tauri/src/events.rs` 的 EventBus）。前端：

- 丢弃 `seq` 不大于已处理最大值的事件；
- 发现跳号（新 `seq` ≠ 上一个 + 1）或窗口重新获得焦点时，调用一次 `get_runtime_snapshot` 重新对齐；
- 订阅全部广播事件做 `seq` 记账，用不到的载荷直接丢弃。

| 事件 | 载荷 | 频率控制 |
|---|---|---|
| `engine-health` | `{ seq, state: HealthState, components: ComponentHealth[] }` | 状态变化时立即发送；状态不变时最多每 5 秒重发一次。细则：`state` 变化立即发送；`state` 不变、只有 `components` 变化时，距上次发送满 5 秒立即发送，否则只保留最新值，到 5 秒时发出（其间变回上次发出的值则不发）；没有变化不重发（无心跳） |
| `engine-state` | `{ seq, listening: boolean, paused_reasons: PauseReason[], busy: boolean, current_process: string \| null }` | 100ms 内的多次变化合并为一次。细则：距上次发送满 100ms 的变化立即发送，否则只保留最新值，在上次发送后 100ms 发出（其间变回上次发出的值则不发） |
| `execution-result` | `{ seq, entry: HistoryEntry, suppressed: number }` | ≤ 10 条/秒；超出的条目不单独推送，下一条推送的 `suppressed` 记被合并的条数，前端据此显示「另有 N 条」，需要明细时调用 `list_history`。细则：1 秒滑动窗口；窗口满时只保留最新一条，窗口一有空位就发出（或被随后的新条目带走），`suppressed` 是被它合并、没有单独推送的条数 |
| `startup-problem` | `{ seq, kind: string, severity: "info" \| "warning" \| "error", args: object }` | 每个问题一次；同时可通过 `startup_problems` 命令取全集。只推送加载之后才出现的问题（SafeMode(Locked) 重试成功后新出现的问题、`autostart_repaired`、`self_test_failed`、`log_level_elevated`）；加载时已有的问题不推送，由 `load_state.startup_problems` 提供。问题消失（例如 Locked 重试成功后移除 `safe_mode`）不发事件，前端在 `store-state` 的 `safe_mode` 变为 `null` 时（或 `engine-state` 的 `paused_reasons` 不再含 `safe_mode` 时）可以重新调用 `startup_problems` |
| `store-state` | `{ seq, revision: number, safe_mode: SafeModeReason \| null }` | 不限速、不合并。当前生效修订号或安全模式变化时发送一次，不论原因（保存并应用、回滚、导入发布、SafeMode(Locked) 重试成功、其他后端路径）；两者同时变化（如 Locked 重试成功：新修订号与 `safe_mode: null`）是同一条事件；值与上一条 `store-state`（或下方发送规则所说的基线）相同时不重发。初值不推送，从 `get_runtime_snapshot` / `load_state` 取 |
| `engine-status` | 旧字符串 `"running"` \| `"paused"` \| `"listening"` | 0.4–0.5 保留为别名，不带 `seq`，新代码不要订阅。某条 `engine-state` 使这个字符串变化时紧随其后发送 |

发送规则（`src-tauri/src/events.rs`）：

- 只有 EventBus 的一个线程发送事件，`seq` 在发送时分配，所以事件按 `seq` 顺序到达。第一条事件的 `seq` 是 1。
- 引擎分配历史 `seq` 与把条目交给 EventBus 在同一把锁内完成，所以 `execution-result` 所带条目的 `entry.seq` 随事件 `seq` 严格递增（被合并的条目只是不单独出现）。
- 启动时的初始 Health / State / `revision` / `safe_mode` 都不推送，前端从 `load_state`（或 `get_runtime_snapshot`）取初值。`revision`、`safe_mode` 的初值在启动时（前端能调用命令之前）直接写进 EventBus 的记录作为基线，不占 `seq`；之后每次变化都以更大的 `seq` 推送一条 `store-state`，与基线相同的值不推送。
- 合并与限速只影响推送；托盘图标的提示、「暂停监听 / 恢复监听」菜单项和执行通知看到每一次变化。Windows 的执行通知在限速之前逐条取历史条目，按条目自己的 `rule_id`、`outcome` 判断（规则开了通知才发），每个历史 `seq` 一次（记住最近 256 个已通知的 `seq`；比已通知的更小但没见过的 `seq` 照样通知，晚到超过这个窗口的条目不再通知，以免重复），所以被 `suppressed` 合并的条目、被执行器超时围栏结束的执行也各有通知；macOS 没有执行历史，仍轮询最后一次结果。
- 合并中、尚未发出的值不会出现在任何事件里，也不在 `get_runtime_snapshot` 里，直到它以更大的 `seq` 发出。

类型细节（与 `health.rs` 一致）：

```ts
type Component = 'executor' | 'keyboard_hook' | 'mouse_hook' | 'raw_input' | 'supervisor'
               | 'focus_events' | 'clipboard' | 'protection';
interface ComponentHealth { component: Component; state: HealthState; reason: ReasonCode | null; since_ms: number }
type PauseReason = 'manual' | 'timed' | 'fullscreen' | 'recording' | 'safe_mode' | 'engine_down' | 'self_test';
type TriggerKind = 'immediate' | 'terminator' | 'manual';
type Outcome = 'succeeded' | 'unknown' | 'failed' | 'skipped' | 'cancelled' | 'dropped_busy';
interface StageTimings { queue_us: number | null; resolve_us: number | null; erase_us: number | null;
                         output_us: number | null; restore_us: number | null; total_us: number | null }
interface HistoryEntry { seq: number; at_ms: number; rule_id: string; trigger: TriggerKind;
                         outcome: Outcome; reason: ReasonCode; process: string | null;
                         completed_steps: number; side_effects: boolean; skipped_formats?: number[];
                         timings: StageTimings }
```

`ReasonCode` 的完整列表以 `crates/platform/src/health.rs` 为准（`ok`、`dropped_busy`、`cancelled`、`paused`、`engine_down`、`focus_changed`、`stale_input`、`protected_control`、`modifier_held`、`inject_partial`、`clipboard_busy`、`clipboard_changed`、`clipboard_format_skipped`、`clipboard_too_large`、`resolve_failed`、`preflight_failed`、`executor_panic`、`executor_timeout`、`hook_missing`、`raw_input_missing`、`hook_install_failed`、`unsupported`、`internal`）。

`HistoryEntry.seq` 是执行历史自己的序号（`list_history(since_seq)` 用它），与事件的全局 `seq` 不是同一个计数器。

`paused_reasons` 中只有 `manual`、`timed`、`fullscreen`、`recording`、`safe_mode` 是用户可见的暂停（Health 映射表的「已暂停」行）；`engine_down`、`self_test` 由 Health 行（故障、恢复中、正在启动）表达。显示文案按计划 §7.2 Health 映射表，Rust 侧的同一逻辑在 `StatusText::resolve`。

### `startup-problem` 的 kind（0.4）

| kind | severity | args |
|---|---|---|
| `safe_mode` | error | `{ reason: SafeModeReason }` |
| `recovered` | warning | `{ from: "last_good" \| "revision" \| "previous", revision: number \| null }` |
| `config_quarantined` | warning | `{ file, quarantined_as }`（文件名，不含目录） |
| `rules_skipped` | warning | `{ count, ids: string[] }`（宽容读取或逐条编译失败的规则） |
| `terminator_unsupported` | warning | `{ keys: string[] }`（旧配置中的 tab、enter） |
| `autostart_repaired` | info | `{}`（自启项路径与当前程序不符，已修正） |
| `self_test_failed` | warning | `{ checks: ("raw_input" \| "hook_heartbeat" \| "foreground" \| "clipboard")[] }`：启动自检未通过的项（原因为 `unsupported` 的项不计入），不改变暂停状态：引擎保持暂停，启动期间用户已手动恢复的，保持用户的选择（是否在监听仍由其余暂停原因决定，以 `engine-state` 为准），界面提供「恢复监听」。见 §5「自检」 |
| `log_level_elevated` | info | `{ level: "error" \| "warn" \| "debug" \| "trace", until_ms: number }`：启动时从 prefs 恢复了仍在有效期内的临时日志级别，`until_ms` 为到期时间（Unix 毫秒） |
| `store_meta_reset` | warning | `{}`（`store-meta.json` 无法解析，已重建） |
| `backup_failed` | warning | `{ backup: "backup-pre-0.4" }`：0.4 第一次写入 0.3.1 会读的文件前要做的备份没有做成（计划 §9，R6）。保存、放弃草稿、发布、回滚都会以 `io`（`path` 为 `backup-pre-0.4`）拒绝，已有配置照常生效，每次写入都会重试；备份做成后从 `startup_problems` 中移除。建议文案：「升级前备份失败，暂时不能保存，已有配置照常生效」 |

后端可以新增 kind；前端对未知 kind 显示 `kind` 原文加通用文案。

## 4. 现有命令在 0.4 的变化

| 命令 | 变化 |
|---|---|
| 全部 | 错误类型改为 `CommandError` |
| `load_state` | 返回值在原字段之外新增 `health: HealthSnapshot`、`engine_state: EngineState`（不含 `seq`）、`startup_problems: StartupProblem[]`、`safe_mode: SafeModeReason \| null`；`engine_status` 字符串保留。`health`、`engine_state` 是调用时运行时的实时值（不是 EventBus 最后发出的值，需要 `seq` 基准时用 `get_runtime_snapshot`）；`capabilities` 与 `get_capabilities` 同源，按这份 `health` 的组件降级 |
| `load_state.auto_listen_on_start` | 新增 `boolean`：`prefs.json` 的 `app.auto_listen_on_start`（启动自检通过后是否自动开始监听），prefs 读不出时为 `false`。由 `set_auto_listen` 修改 |
| `load_state.platform` | 取 `std::env::consts::OS`：`"windows"`、`"macos"`，或没有输入适配器的平台名（如 `"linux"`）。0.3 在 macOS 以外一律返回 `"windows"`；Windows 与 macOS 上的值不变 |
| `set_paused(paused)` | 只置位或清除 `manual`；其他暂停原因仍在时，返回的状态仍是暂停。恢复（`paused: false`）在安全模式下返回 `safe_mode`；Health 不是 `healthy` / `degraded` 时返回 `engine_unavailable { health }`；录制中恢复不再返回 `busy`，而是清除 `manual` 后仍因 `recording` 保持暂停。托盘菜单项走同一逻辑：监听中显示「暂停监听」，否则显示「恢复监听」；托盘恢复被拒绝时发系统通知说明原因（安全模式、引擎启动中、恢复中、已停止） |
| `start_recording`、`stop_recording` | 录制期间置位 `recording` 暂停原因，录制结束即清除；结束后引擎回到其他原因决定的状态（录制前已手动暂停的仍暂停，原来在监听的恢复监听） |
| `load_state.paused`、`engine_status` | 由运行时推导：`paused` = 不在监听（任何暂停原因，或 Health 不接受恢复）；`engine_status` 为 `running`（执行中）、`paused` 或 `listening` |
| `save_draft(config)` | 先做校验，存在 error 级诊断时返回 `validation`（B-42） |
| `export_config` | 用户取消保存对话框时返回 `null`（原来是错误） |
| `choose_file`、`import_config` | 对话框按扩展名过滤；取消返回 `null` |
| 安全模式下的写命令 | 返回 `safe_mode`；导入和打开数据目录除外 |

## 5. 0.4 新增命令

| 命令 | 参数 | 返回 |
|---|---|---|
| `get_runtime_snapshot` | — | `{ seq, health: HealthSnapshot, state: EngineState, revision: number, history_seq: number, safe_mode: SafeModeReason \| null }`；`seq` 是生成快照时的全局事件序号，前端以它为新的基准。细则见下方「快照与事件」 |
| `get_health` | — | `HealthSnapshot` = `{ state, components }`：运行时的实时值，可能比最后一条 `engine-health` 新（合并中的值） |
| `repair_engine` | — | `HealthSnapshot`：立即返回当前快照；`recovering` 和结果通过 `engine-health` 推送。Failed 已锁存（supervisor 的 watchdog 判定卡死）时返回 `failed`，锁存要等 supervisor 受理这次修复才解除。安全模式下也可调用：重建不会恢复监听（`safe_mode`、`manual` 仍在） |
| `startup_problems` | — | `StartupProblem[]`（不含 `seq`） |
| `get_capabilities` | — | `Capability[]`，`Capability = { id: string, status: "available" \| "unavailable" \| "experimental" \| "degraded", reason: string \| null, available: boolean }`；`reason` 是原因码，`status` 为 `available` 时恰好为 `null`；`available` 保留给旧界面（`status` 不是 `unavailable` 即为真）。id 与 reason 的取值见下方「能力表」 |
| `export_diagnostics` | `{ redaction: "summary" \| "full" }` | `string \| null`：弹出保存对话框，返回写入的 zip 路径；取消返回 `null` |
| `set_log_level` | `{ level: "error" \| "warn" \| "info" \| "debug" \| "trace", ttl_hours: number \| null }` | `{ level, until_ms: number \| null }`；`ttl_hours` 为空表示恢复默认级别 |
| `open_log_dir`、`open_data_dir` | — | `null` |
| `self_test` | — | `{ passed: boolean, checks: { name: "raw_input" \| "hook_heartbeat" \| "foreground" \| "clipboard", passed: boolean, reason: ReasonCode \| null, elapsed_ms: number }[] }`；`checks` 恒为这四项、按此顺序；`reason` 在 `passed` 为真时为 `null`。细则见下方「自检」 |
| `set_auto_listen` | `{ enabled: boolean }` | `boolean`：写入 `prefs.json` 的 `app.auto_listen_on_start`，返回写入后的值。与 `set_autostart` 对称，但安全模式下也允许（prefs 与安全模式保护的配置、修订、导出分开存放；无法读取的 prefs 文件不会被覆盖，此时返回错误，通常是 `io`）。只影响下次启动，不改变当前的监听状态 |
| `log_frontend_error` | `{ message: string, stack: string \| null, source: string \| null }` | `null`；后端截断到 8 KiB 后写日志 |
| `report_metric` | `{ name: string, value: number }` | `null`；只接受白名单内的名称（0.4：`ui_first_paint_ms`、`ui_event_lag_ms`），其他名称忽略 |
| `list_history` | `{ since_seq: number \| null }` | `HistoryEntry[]`，按 `seq` 升序，只含 `seq > since_seq` 的条目；`null` 返回全部（内存中最多 200 条、30 分钟内） |
| `clear_history` | — | `null`；清空内存中的执行历史，不发事件。历史 `seq` 不归零，之后的条目接着编号。安全模式下也可调用（不写数据或设置） |
| `explain_last_miss` | — | `{ seq, at_ms, rule_id: string, outcome: Outcome, reason: ReasonCode } \| null`：0.4 只基于执行历史，取最近一条不是 `succeeded` 的记录（F-52 的完整版在 0.5） |
| `debug_inject_fault` | `{ point: FaultPoint, param: number }` | `null`；只在 `fault-injection` feature 下注册，release 构建不存在（命令和符号都没有）。`param` 对 `executor_hang`、`clipboard_busy` 是毫秒数，其他点忽略；每次调用写一条 `fault.injected` 日志 |

`FaultPoint` 的取值见 `crates/platform/src/fault.rs`：`executor_panic`、`executor_hang`、`hook_panic`、`hook_unhook`、`rawinput_close`、`rawinput_unregister`、`clipboard_busy`、`store_write_fail`、`config_corrupt`、`store_locked`、`autostart_stale`、`session_end`。

### 快照与事件（`get_runtime_snapshot`）

- `seq`、`health`、`state`、`revision`、`safe_mode` 是 EventBus 同一时刻的一份拷贝：`seq` 是最后发出的事件序号，`health`、`state` 是截至这个 `seq` 最后一条 `engine-health`、`engine-state` 的值，`revision`、`safe_mode` 是截至这个 `seq` 最后一条 `store-state` 的值（还没有任何事件时 `seq` 为 0，`health`、`state` 是 EventBus 启动时的值；还没有 `store-state` 时 `revision`、`safe_mode` 是基线）。合并中尚未发出的值不在快照里，它们之后以更大的 `seq` 发出。
- 快照里的 `revision`、`safe_mode` 总是来自这份拷贝：启动时（前端能调用命令之前）存储把当时的值写进 EventBus 的记录作为基线，之后的每次变化都是一条 `store-state`。它们和 `load_state` 的 `revision` / `safe_mode`、写命令返回的 `AppState.revision` 是同一个量（存储当前生效的修订号与安全模式），只是截至快照的 `seq`。唯一的例外是 EventBus 线程没能启动（之后不会有任何事件）：这时快照的 `seq` 为 0，其余字段都是实时值。
- `history_seq` 在这份拷贝之后读取，所以至少和它一样新。快照没有包含的变化，一定以大于快照 `seq` 的事件到达；`seq` 不大于快照的事件可以直接丢弃，不会漏掉变化。例如快照之后 SafeMode(Locked) 重试成功：前端收到一条 `seq` 大于快照的 `store-state`，带新的 `revision` 和 `safe_mode: null`。
- `history_seq` 是读取时已分配的最大历史序号。`seq` 不大于快照的 `execution-result` 所带的条目都满足 `entry.seq ≤ history_seq`；`entry.seq > history_seq` 的条目都是快照之后记录的，会以更大的 `seq` 推送（或计入某条推送的 `suppressed`）。快照之前的明细用 `list_history(null)` 取，之后的条目跟随事件；被合并的条目用 `list_history(history_seq)` 补齐。
- 与 `load_state` 的区别：`load_state` 给实时值、不带 `seq`，适合首屏；对齐事件流用 `get_runtime_snapshot`。

### 自检（`self_test`、启动自检）

| 检查项 | 含义（Windows） | 失败原因 |
|---|---|---|
| `raw_input` | `raw_input` 组件健康，检测窗口存在，进程的键盘 RawInput 注册指向它 | 组件给出的原因，否则 `raw_input_missing` |
| `hook_heartbeat` | `keyboard_hook`、`mouse_hook` 组件健康（已安装），钩子线程在 1.5 秒内应答一条消息，且自上次物理按键以来没有投递缺口（RawInput 看到、钩子没看到的物理按键，哪怕只有一个、远不到交叉校验判定丢失的数量）。钩子被静默移除后若还没有任何物理按键，无法与正常的钩子区分，此项通过；之后的按键由交叉校验发现并重装 | 组件给出的原因（如 `hook_install_failed`），否则 `hook_missing` |
| `foreground` | 存在前台窗口，能取到它的进程 id 和焦点控件（不读标题） | `focus_changed`（安全桌面、锁屏、切换中） |
| `clipboard` | 剪贴板能打开，随即关闭（不读不写，序号与所有者不变） | `clipboard_busy` |

- 每项在自己的线程上运行，超时 2.5 秒（探测本身在 1.5 秒内放弃）；超时的项失败，`elapsed_ms` 为超时值，原因依次为 `raw_input_missing`、`hook_missing`、`internal`、`clipboard_busy`；探测 panic 为 `internal`。一项卡住不影响其他项，整次调用最多约 2.5 秒。
- 运行期间置位 `self_test` 暂停原因，结束即清除（它由 Health 行表达，不是用户可见的暂停）。同一时间只运行一次，后来的调用等前一次结束。不持有 store 锁，安全模式下也可调用，不恢复监听、不写任何数据。
- Windows 以外的平台四项都以 `unsupported` 失败。
- 启动流程：引擎启动后最多等 30 秒让 Health 进入 `healthy`（超时也照常自检），然后运行同样的四项检查：
  - 有未通过的项（不计原因为 `unsupported` 的项，所以 macOS 不受影响）：推送 `startup-problem` `self_test_failed { checks }` 并发系统通知，不改变暂停状态：引擎保持暂停，用户可从托盘或界面「恢复监听」。启动期间用户已手动恢复的，保持用户的选择（不重新置位 `manual`），只报告问题；通知改为陈述用户的操作（「你已在启动期间手动恢复监听，监听可能无法正常工作」），不声称引擎正在监听：`recording` 等其余暂停原因仍可能让它暂停，实际状态以 `engine-state` 为准。安全模式、关闭自动监听时同样报告。
  - 全部通过：仅当 `prefs.json` 的 `app.auto_listen_on_start` 为真、不在安全模式、且用户在启动等待期间没有手动暂停或恢复过时，清除 `manual` 开始监听；恢复被拒绝（Health 已不接受恢复）时发系统通知说明原因。其余情况保持暂停。
  - 上面的决定在 store 锁内一次读完所需状态后做出，用户的暂停 / 恢复和 SafeMode(Locked) 重试要么整个在它之前，要么整个在它之后。SafeMode(Locked) 重试在决定之前成功的，按上面的规则照常决定（条件满足就自动开始监听）；在决定之后才成功的，不补做自动监听，`manual` 仍在，需用户手动恢复。

### 能力表（`get_capabilities`）

`load_state.capabilities`、`get_capabilities` 和诊断包 `health.json` 的 `capabilities` 是同一张表（`src-tauri/src/app_state.rs` 的 `capabilities`，即 `capabilities_for(当前 Health 的组件)`）。先看平台适配器能否提供，再按依赖组件的实时健康降级：依赖组件 `degraded` 时能力为 `degraded`；`starting`、`recovering`、`failed` 时为 `unavailable`；取最差的一个，同等严重时保留先出现的原因；健康的组件不会抬高适配器给出的状态。

| id | 依赖组件 | 平台 |
|---|---|---|
| `keyboard_hook` | `keyboard_hook`、`raw_input`（macOS 只有 `keyboard_hook`） | 全部；Windows 还需要 RawInput |
| `keys` | `executor` | 全部 |
| `paste`、`html`、`file_clipboard`、`image_clipboard` | `executor`、`clipboard` | 全部 |
| `mouse` | `executor` | 全部 |
| `recording` | `keyboard_hook`、`mouse_hook` | 全部 |
| `foreground` | `focus_events` | 全部 |
| `open` | `executor` | 全部 |
| `ime_committed_text` | `keyboard_hook`、`raw_input` | Windows；macOS 恒为 `unavailable` / `not_implemented` |
| `quicker_private_action` | — | 恒为 `unavailable` / `not_implemented` |
| `wheel_delta` | — | **仅 macOS 列出**，恒为 `unavailable` / `not_implemented` |
| `media_system_actions` | `executor` | **仅 macOS 列出** |
| `window_ime_actions` | — | **仅 macOS 列出**，恒为 `unavailable` / `not_implemented` |

Windows 与不受支持的平台列出前 12 个 id（同样的顺序），macOS 列出全部 15 个。前端按 id 查找，不依赖顺序和个数；对未知 id 显示 id 原文。

`reason` 是下面两类之一（裸字符串）：

- 平台原因：`experimental_platform`（macOS 适配器就绪但未经实机验收，ADR-f；`status` 为 `experimental`）、`permission_missing`（macOS 辅助功能、输入监控或合成输入权限未授予）、`not_implemented`（这个平台没有适配器提供该能力）。
- `ReasonCode`（§3 的完整列表）：适配器本身给出的 `unsupported`（不受支持的平台，全部能力）、`hook_missing`（Windows 钩子未安装）、`raw_input_missing`（Windows RawInput 未注册）、`engine_down`（macOS 权限已授予但事件 tap 未运行）；或依赖组件报告的原因（组件未给原因时按组件取默认值：`executor` → `engine_down`，`keyboard_hook`、`mouse_hook`、`focus_events` → `hook_missing`，`raw_input` → `raw_input_missing`，`clipboard` → `clipboard_busy`），例如 `executor_panic`、`executor_timeout`、`clipboard_changed`、`hook_install_failed`。

## 6. 跨 IPC 的整数范围

前端用 `JSON.parse` 把 IPC 的 JSON 变成 JavaScript 数值（IEEE 754 双精度）。绝对值超过 2^53−1 的整数在解析时就被舍入到最近的可表示值，前端代码拿到的已经不是原值，也无从还原。

- 跨 IPC 的所有整数（命令参数、返回值、事件载荷，以及配置里的规则和未知字段）必须在 ±(2^53−1) 以内，即 `Number.MIN_SAFE_INTEGER` 到 `Number.MAX_SAFE_INTEGER`。
- 以后新增的 64 位标识符一律以字符串传输，不用 `number`。
- 现有的 64 位整数按构造都在范围内：`revision` 是后端计数器；`span.line`、`span.column`、`span.length` 受源码长度约束；`delay.ms` 不超过 `MAX_DELAY_MS`（3 600 000）。seq 与毫秒时间戳见 §1。
- 应用自身不会产生超出范围的值，只有手改配置才会出现；规则中这样的值被舍入后，对后端同样不合法，或语义等价。前端的处理如下，都不阻止保存：
  - 规则里任何位置出现超范围数值（已知字段、操作和步骤的 `reusable.parameters`、规则层未知字段，以及更深层的任意 JSON 值），整条规则判为无法读取（B-43），只读列表标出原因：「含超出可安全保存范围的数值，保存其他内容时该数值可能被改写。」这样编辑器不会把舍入后的值当成真值继续编辑。
  - 配置层和 `globals` 层出现超范围数值时，配置照常读取，设置页给出一行提示：「配置中含超出可安全保存范围的数值，保存时可能被改写。」
  - 检测是可靠的：任何超过 2^53−1 的原值解析后都不小于 2^53（或成为 `Infinity`），所以不会漏报；2^53 本身、`1e300` 这类恰好能还原的值也会被标出。
- 前端对无法读取的规则和本版本不认识的字段的承诺是：保存时把收到的值放回原位提交，在本节的数值范围内值不变。它不承诺逐字节保留配置文件原文；数值写法（例如 `1.0` 写回为 `1`）、键顺序和空白由序列化决定。
