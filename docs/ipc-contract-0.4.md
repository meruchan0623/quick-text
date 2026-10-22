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
| `io` | `{ op: IoOp, path: string \| null, os_code: number \| null }` | `path` 只给数据目录下的相对路径或文件名，不给用户目录全路径 |
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
| `engine-health` | `{ seq, state: HealthState, components: ComponentHealth[] }` | 状态变化时立即发送；状态不变时最多每 5 秒重发一次 |
| `engine-state` | `{ seq, listening: boolean, paused_reasons: PauseReason[], busy: boolean, current_process: string \| null }` | 100ms 内的多次变化合并为一次 |
| `execution-result` | `{ seq, entry: HistoryEntry, suppressed: number }` | ≤ 10 条/秒；超出的条目不单独推送，下一条推送的 `suppressed` 记被合并的条数，前端据此显示「另有 N 条」，需要明细时调用 `list_history` |
| `startup-problem` | `{ seq, kind: string, severity: "info" \| "warning" \| "error", args: object }` | 每个问题一次；同时可通过 `startup_problems` 命令取全集 |
| `engine-status` | 旧字符串 `"running"` \| `"paused"` \| `"listening"` | 0.4–0.5 保留为别名，不带 `seq`，新代码不要订阅 |

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
| `self_test_failed` | warning | `{ checks: string[] }`（未通过的自检项名称） |
| `log_level_elevated` | info | `{ level, until_ms }` |
| `store_meta_reset` | warning | `{}`（`store-meta.json` 无法解析，已重建） |

后端可以新增 kind；前端对未知 kind 显示 `kind` 原文加通用文案。

## 4. 现有命令在 0.4 的变化

| 命令 | 变化 |
|---|---|
| 全部 | 错误类型改为 `CommandError` |
| `load_state` | 返回值在原字段之外新增 `health: HealthSnapshot`、`engine_state: EngineState`（不含 `seq`）、`startup_problems: StartupProblem[]`、`safe_mode: SafeModeReason \| null`；`engine_status` 字符串保留 |
| `set_paused(paused)` | 只置位或清除 `manual`；其他暂停原因仍在时，返回的状态仍是暂停 |
| `save_draft(config)` | 先做校验，存在 error 级诊断时返回 `validation`（B-42） |
| `export_config` | 用户取消保存对话框时返回 `null`（原来是错误） |
| `choose_file`、`import_config` | 对话框按扩展名过滤；取消返回 `null` |
| 安全模式下的写命令 | 返回 `safe_mode`；导入和打开数据目录除外 |

## 5. 0.4 新增命令

| 命令 | 参数 | 返回 |
|---|---|---|
| `get_runtime_snapshot` | — | `{ seq, health: HealthSnapshot, state: EngineState, revision: number, history_seq: number, safe_mode: SafeModeReason \| null }`；`seq` 是生成快照时的全局事件序号，前端以它为新的基准 |
| `get_health` | — | `HealthSnapshot` = `{ state, components }` |
| `repair_engine` | — | `HealthSnapshot`（触发重建后立即返回，通常为 `recovering`；结果通过 `engine-health` 推送） |
| `startup_problems` | — | `StartupProblem[]`（不含 `seq`） |
| `get_capabilities` | — | `Capability[]`，`Capability = { id: string, status: "available" \| "unavailable" \| "experimental" \| "degraded", reason: string \| null, available: boolean }`；`reason` 是原因码，`available` 保留给旧界面（`status` 不是 `unavailable` 即为真） |
| `export_diagnostics` | `{ redaction: "summary" \| "full" }` | `string \| null`：弹出保存对话框，返回写入的 zip 路径；取消返回 `null` |
| `set_log_level` | `{ level: "error" \| "warn" \| "info" \| "debug" \| "trace", ttl_hours: number \| null }` | `{ level, until_ms: number \| null }`；`ttl_hours` 为空表示恢复默认级别 |
| `open_log_dir`、`open_data_dir` | — | `null` |
| `self_test` | — | `{ passed: boolean, checks: { name: "raw_input" \| "hook_heartbeat" \| "foreground" \| "clipboard", passed: boolean, reason: ReasonCode \| null, elapsed_ms: number }[] }` |
| `log_frontend_error` | `{ message: string, stack: string \| null, source: string \| null }` | `null`；后端截断到 8 KiB 后写日志 |
| `report_metric` | `{ name: string, value: number }` | `null`；只接受白名单内的名称（0.4：`ui_first_paint_ms`、`ui_event_lag_ms`），其他名称忽略 |
| `list_history` | `{ since_seq: number \| null }` | `HistoryEntry[]`，按 `seq` 升序，只含 `seq > since_seq` 的条目 |
| `clear_history` | — | `null` |
| `explain_last_miss` | — | `{ seq, at_ms, rule_id: string, outcome: Outcome, reason: ReasonCode } \| null`：0.4 只基于执行历史，取最近一条不是 `succeeded` 的记录（F-52 的完整版在 0.5） |
| `debug_inject_fault` | `{ point: FaultPoint, param: number }` | `null`；只在 `fault-injection` feature 下注册，release 构建不存在 |

`FaultPoint` 的取值见 `crates/platform/src/fault.rs`。

## 6. 跨 IPC 的整数范围

前端用 `JSON.parse` 把 IPC 的 JSON 变成 JavaScript 数值（IEEE 754 双精度）。绝对值超过 2^53−1 的整数在解析时就被舍入到最近的可表示值，前端代码拿到的已经不是原值，也无从还原。

- 跨 IPC 的所有整数（命令参数、返回值、事件载荷，以及配置里的规则和未知字段）必须在 ±(2^53−1) 以内，即 `Number.MIN_SAFE_INTEGER` 到 `Number.MAX_SAFE_INTEGER`。
- 以后新增的 64 位标识符一律以字符串传输，不用 `number`。
- 现有的 64 位整数按构造都在范围内：`revision` 是后端计数器；`span.line`、`span.column`、`span.length` 受源码长度约束；`delay.ms` 不超过 `MAX_DELAY_MS`（3 600 000）。seq 与毫秒时间戳见 §1。
- 应用自身不会产生超出范围的值，只有手改配置才会出现；规则中这样的值被舍入后，对后端同样不合法，或语义等价。前端在只读列表里给无法读取的规则（B-43）标出原因：「含超出可安全保存范围的数值，保存其他内容时该数值可能被改写。」不因此阻止保存。检测是可靠的：任何超过 2^53−1 的原值解析后都不小于 2^53（或成为 `Infinity`），所以不会漏报；2^53 本身、`1e300` 这类恰好能还原的值也会被标出。
- 前端对无法读取的规则和本版本不认识的字段的承诺是：保存时把收到的值放回原位提交，在本节的数值范围内值不变。它不承诺逐字节保留配置文件原文；数值写法（例如 `1.0` 写回为 `1`）、键顺序和空白由序列化决定。
