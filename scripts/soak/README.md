# 浸泡监视脚本 `soak.ps1`

## 用途

开发计划 L6 的浸泡监视工具，用于 0.4 实机验收的 S-27（2 小时浸泡）和 PF-05：

- 按固定间隔采样 Quicktext 进程的工作集、Private Bytes、句柄、线程、GDI/USER 对象和 CPU 时间；
- 只读当天日志的新增部分，统计健康变化、输入源丢失与重新挂接、执行器 panic、熔断、故障注入和 ERROR 级日志；
- 结束时（或按 Ctrl+C 时）写出汇总：内存起止、峰值和增长（对照 S-27 的 20 MB 参考线），句柄和线程增长，各事件次数，进程重启次数，「静默失效」可疑项和需要人工复核的项。

通过标准见 `docs/development-plan.md` 的浸泡一节和 `docs/performance-baseline.md`：句柄和线程不单调增长、不出现静默失效、结束时健康为「正常」。内存增长 0.4 只记录，不判失败；1.0 要求小于 20 MB。

## 前提

- **触发词由人工打字，或用实体按键工具（例如硬件宏键盘、按键机器人）输入。** 外部模拟输入（SendInput、`[System.Windows.Forms.SendKeys]`、AutoHotkey 之类）会让 Quicktext 的检测缓冲失效（不回归清单第 4 条），**外部模拟输入不能代表实体键盘路径**，用它跑出来的结果不能算浸泡通过。
- 脚本本身不产生任何输入，不装钩子。它只查询进程信息、以共享只读方式打开日志文件，不写数据目录，不改任何设置，不需要管理员权限。
- 汇总和 `events.csv` 里只有事件名、少量白名单字段的值、计数和时间戳，不抄日志原文。日志本来就不含短语正文、缩写原文和剪贴板内容；白名单字段的值也只允许字母、数字和 `_ . -`，其他一律记为「(其他)」。
- 需要 PowerShell 7，或 Windows PowerShell 5.1。

## 用法

在仓库根目录执行：

```powershell
# PowerShell 7，默认 2 小时、每 10 秒采样一次
pwsh -NoProfile -File scripts/soak/soak.ps1

# Windows PowerShell 5.1；-ExecutionPolicy Bypass 只影响这一个进程，不改系统设置
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\soak\soak.ps1

# 半小时、每 5 秒采样，指定输出目录
pwsh -NoProfile -File scripts/soak/soak.ps1 -Hours 0.5 -IntervalSeconds 5 -OutputDirectory D:\soak\round1

# 验收模式下用了别的数据目录
pwsh -NoProfile -File scripts/soak/soak.ps1 -DataDirectory D:\qt-acceptance-data
```

建议的流程：

1. 先启动 Quicktext（release 构建），等健康页显示「正常」；
2. 每一轮用一个新的输出目录启动脚本（已有上一轮结果的目录会被拒绝）；
3. 人工打字或用实体按键工具按计划触发；每 30 分钟内至少看一次界面的执行历史并记下成功、失败和「未执行」的条数（执行历史只在内存里，最多 200 条、30 分钟）；
4. 时间到后脚本自动写出汇总；提前结束按 Ctrl+C，已有数据照样汇总。直接关掉终端窗口不会写最终汇总，但 `soak.csv` 和 `events.csv` 每次采样都已落盘，`summary.txt` 每 10 分钟刷新一次检查点；
5. 以 `summary.txt` 为这一轮的记录，连同执行历史的记录一起填进验收清单。

脚本启动前进程不在运行也可以：它会等进程出现。监视期间进程退出、重启都会记录。

## 参数

| 参数 | 默认值 | 说明 |
|---|---|---|
| `-Hours` | `2` | 监视时长（小时），0.01 到 168 |
| `-IntervalSeconds` | `10` | 采样间隔（秒），1 到 3600 |
| `-OutputDirectory` | `%TEMP%\qt-soak-<开始时间>` | 输出目录。不能在数据目录里；里面已有 `soak.csv`、`events.csv` 或 `summary.txt` 时拒绝运行 |
| `-ProcessName` | `quicktext` | 进程名，可带或不带 `.exe` |
| `-DataDirectory` | `%APPDATA%\com.quicktext.desktop` | 数据目录，日志在其下 `logs\`。release 版只在验收模式下使用 `QUICKER_TEXT_DATA_DIR`，脚本不读这个变量，需要时用本参数指定 |
| `-UnhealthySuspectSeconds` | `60` | 健康处于「正在启动 / 恢复中 / 故障」、或输入源丢失未重新挂接，持续多久算可疑 |
| `-PauseSuspectSeconds` | `300` | 暂停（手动暂停、安全模式、启动时未自动监听等）持续多久算可疑 |
| `-ActiveWindowMinutes` | `10` | 连续输入活动多长的时段列入人工复核 |

## 输出

三个文件都是 UTF-8（带 BOM），可直接用 Excel 打开。

### `soak.csv`（每次采样一行）

| 列 | 含义 |
|---|---|
| `TimeLocal` / `TimeUtc` | 采样时间 |
| `ProcessExists` | 进程是否存在 |
| `ProcessId` | 跟踪的 PID（同名进程有多个时优先上次的 PID，否则取最早启动的） |
| `PidChanged` | 和上一次采样相比 PID 是否变了（进程重启） |
| `ProcessCount` | 同名进程个数（单实例程序正常为 0 或 1） |
| `WorkingSetBytes` | 工作集 |
| `PrivateBytes` | Private Bytes（`PrivateMemorySize64`），内存增长以它为准 |
| `Handles` / `Threads` | 句柄数、线程数 |
| `GdiObjects` / `UserObjects` | GDI、USER 对象数（`GetGuiResources`） |
| `CpuMsDelta` / `CpuPercent` | 和上一次采样之间的 CPU 时间增量（毫秒）及占全部逻辑处理器的百分比；PID 变化后的第一次为空 |
| `InputIdleSec` | 系统上一次输入距今的秒数（`GetLastInputInfo`，不区分键盘鼠标，外部注入的输入也算） |
| `HealthFromLog` | 从日志推断的健康：`starting` / `healthy` / `degraded` / `recovering` / `failed` / `exited` / `notstarted` / `unknown` |
| `ListeningFromLog` | 从日志推断的监听状态：`listening` / `paused` / `unknown` |
| `SafeModeFromLog` | 是否在安全模式 |
| `InputLostFromLog` | 丢失后尚未重新挂接的输入源，多个用 `+` 连接 |
| `NewLogLines` / `NewEvents` / `NewErrors` | 这次采样读到的新增日志行、带事件名的行、ERROR 级行 |
| `LogFileFound` | 当天（UTC）的日志文件是否存在 |

### `events.csv`（每个日志事件一行）

`TimeLocal`、`TimeUtc`、`Level`、`Event`、`Detail`。`Detail` 只含下表的白名单字段，没有日志原文。

### `summary.txt`

运行信息、内存（按最后一个进程实例统计：起始、结束、最低、峰值、增长、S-27 参考线、四段中位数趋势、线性斜率）、句柄 / 线程 / GDI / USER、进程（PID、重启次数、不存在的时段、启动与退出次数）、健康（起止状态、各状态累计时长、转换列表）、监听与安全模式与输入源、关键事件计数（含 0 次）、ERROR 级日志、全部事件、「静默失效」可疑项、日志读取统计。

## 统计的事件（以代码为准）

事件名和字段取自代码里的 `tracing` 调用（`event = "…"`）和 `src-tauri/src/diagnostics/logging.rs` 的事件表。日志格式是 `tracing_subscriber` 的默认格式：`<UTC 时间> <级别> <线程> <target>: <消息> event="x.y" 字段=值 …`。

| 事件 | 代码出处 | 汇总里拆分的字段 | 用途 |
|---|---|---|---|
| `app.start` | `src-tauri/src/main.rs` | `version`、`debug_build` | 进程启动；健康 → 正在启动 |
| `app.exit` | `src-tauri/src/main.rs` | | 正常退出；健康 → 已退出 |
| `app.panic` | `src-tauri/src/diagnostics/panic.rs` | | 进程内 panic |
| `self_test.startup` | `src-tauri/src/startup.rs` | `health`、`outcome` | 启动自检结果；健康和监听状态的起点 |
| `self_test.finished` | `crates/platform/src/self_test.rs` | `passed` | 一次自检结束 |
| `supervisor.recovering` | `crates/platform/src/supervisor.rs` | | 健康 → 恢复中 |
| `supervisor.recovered` | `crates/platform/src/supervisor.rs` | `state` | 恢复结束，健康 → `state` |
| `supervisor.repair` | `crates/platform/src/supervisor.rs` | | 「修复引擎」；健康 → 恢复中 |
| `supervisor.circuit_open` | `crates/platform/src/supervisor.rs` | `cause` | 熔断；健康 → 故障 |
| `runtime.failed_latched` | `crates/platform/src/runtime.rs` | `reason` | 故障锁定；健康 → 故障 |
| `supervisor.unsupervised` | `crates/platform/src/supervisor_wiring.rs` | | 健康 → 故障 |
| `supervisor.engine_unavailable` | `crates/platform/src/supervisor_wiring.rs` | | 健康 → 故障 |
| `supervisor.driver_hung` | `crates/platform/src/supervisor.rs` | | 健康 → 故障 |
| `supervisor.attempt_failed` / `attempt_hung` / `panic` / `respawn` | `crates/platform/src/supervisor.rs` | | 计数 |
| `input.check` | `crates/platform/src/windows/input.rs` | `source`、`result` | `result` 为 `lost` / `window_missing` / `unregistered` 时记为输入源丢失 |
| `input.reattach` | `crates/platform/src/windows/input.rs` | `component`、`ok` | `ok=true` 结束同一组件的丢失；`ok=false` 记为重新挂接失败 |
| `input.refused` | `crates/platform/src/windows/input.rs` | `reason` | `hook_missing` 列为可疑 |
| `input.queue_full` | `crates/platform/src/windows/input.rs` | `source` | 输入记录被丢弃，列为可疑 |
| `hook.panic` / `rawinput.panic` / `input.consumer_panic` | `crates/platform/src/windows/input.rs` | | 计数 |
| `executor.panic` | `crates/platform/src/windows/executor.rs` | `stage`、`kind` | 计数 |
| `executor.stalled` / `fenced` / `unwound` / `send_refused` / `clipboard_busy` / `rebuilt` | `crates/platform/src/windows/executor.rs` | | 计数 |
| `executor.manual_send_refused` | `crates/platform/src/windows/executor.rs` | `reason` | WARN。手动执行的输入没有发送，`reason` 为 `send_lock_busy` / `release_owed` / `fenced` / `unclaimed`；计数（随后的结果见下两行） |
| `command.execute_refused` | `src-tauri/src/commands/runtime.rs`（`jobs_exhausted`） | `reason`、`limit`、`health` | WARN。手动执行（「测试动作（真实执行）」「单步测试（真实执行）」）开始前被拒：之前的 4 个（`MAX_MANUAL_JOBS`）手动执行任务的线程都还没结束；`reason` 只有 `jobs_exhausted`。界面在健康为正常 / 降级时显示「上一个动作仍在执行」，否则显示「输入引擎暂时不可用」。列为可疑 |
| `command.execute_failed` | `src-tauri/src/commands/runtime.rs`（`stalled` / `run_failed` / `not_started`） | `category`、`reason`、`health`、`thread`、`kind` | 手动执行失败，按 `category` 分：`stalled`（WARN，任务卡住被隔离或被发送互斥拒绝，执行器停用到重建为止；带返回给界面的 `health`）列为可疑；`run_failed`（ERROR，带执行的原因码）和 `spawn_failed`（ERROR，`qt-manual-run` / `qt-manual-watch` 线程没起来，带 `thread` 和 OS 错误的 `kind`）按 ERROR 级列为可疑。两者另带的 `trace_id`、`os_code` 不进汇总 |
| `clipboard.lease_refused` | `crates/platform/src/windows/clipboard.rs` | | 计数 |
| `rawinput.dropped_busy` | `crates/platform/src/windows/rawinput.rs` | | 执行器忙丢弃（debug 级，默认日志级别下不出现） |
| `runtime.events_dropped` | `crates/platform/src/runtime.rs` | | 执行事件推送丢失，列为可疑 |
| `fault.injected` | `src-tauri/src/commands/engine.rs`、`crates/platform/src/windows/input.rs`、`crates/platform/src/windows/session.rs` | `point` | 故障注入 |
| `fault.config_corrupt` | `src-tauri/src/store/load.rs` | | 故障注入损坏配置 |
| `store.safe_mode` | `src-tauri/src/store/mod.rs` | `reason` | 进入安全模式（同时暂停） |
| `store.safe_mode_exit` | `src-tauri/src/store/mod.rs` | | 离开安全模式（手动暂停保留） |
| `store.safe_mode_import` | `src-tauri/src/store/revisions.rs` | | 安全模式下导入 |
| `engine.manual_pause` | `src-tauri/src/store/mod.rs` | `paused` | 手动暂停 / 继续 |
| `session.end` | `crates/platform/src/windows/session.rs` | `source` | 会话结束 |

其余带 `event` 字段的日志也会按名称计数，ERROR 级日志全部按事件名计数（没有事件名的记为「(无 event 字段)」）。

**日志里没有的：** 触发词展开的每次执行的成功或失败、手动执行的成功、钩子路径上的执行器忙丢弃、正常 ↔ 降级的切换。这些只能看界面的执行历史和健康页。手动执行（「测试动作（真实执行）」「单步测试（真实执行）」）的失败和被拒记在上表的 `command.execute_failed` / `command.execute_refused`；校验不通过（动作展不开）不写日志，界面直接显示诊断。

## 「静默失效」的判定方法

日志不记触发词的每次执行，脚本也看不到界面的执行历史，所以「打了触发词却没有展开」不能由脚本直接判定。脚本把能从日志和进程看出的「引擎可能没在工作」的迹象列为**自动可疑项**，把需要对照执行历史的列为**人工复核项**。

**A. 自动可疑项**（任一出现即列出）：

1. 健康（日志推断）处于正在启动 / 恢复中 / 故障，连续不少于 `-UnhealthySuspectSeconds`，或到结束仍未恢复；
2. 结束时健康为正在启动 / 恢复中 / 故障 / 已退出 / 未运行；
3. 进入过安全模式；
4. 输入源丢失后在阈值内没有重新挂接（同一组件 `input.reattach ok=true`，或 `supervisor.recovered state=Healthy`），或出现 `input.reattach ok=false`；
5. 出现 `input.refused reason=hook_missing`、`input.queue_full`、`runtime.events_dropped`、`command.execute_refused`、`command.execute_failed category=stalled`（手动执行的 `run_failed` / `spawn_failed` 是 ERROR 级，归第 8 条）；
6. 进程重启、进程消失、启动前没有 `app.exit`、`app.panic`；结束时进程不在运行，且最后一次 `app.start` 之后没有 `app.exit`；整轮没有看到进程。脚本启动时的进程检查也算「看到过」：进程在第一次采样之前就没了，同样记为进程消失；
7. 暂停持续不少于 `-PauseSuspectSeconds`（手动暂停、安全模式、启动时未自动监听、启动自检未通过等，原因一并列出）；
8. 出现 ERROR 级日志；进程在运行但日志目录不存在；日志读取或采样出错；日志文件被截断或替换后从头重读；进程在运行而当天日志文件超过宽限仍不存在；整轮没有读到任何日志文件；启动时回放日志出错（见下文「日志的读法」）。

**B. 人工复核项：**

1. 有输入活动的采样（`InputIdleSec` 小于采样间隔）落在上面的不可用时段里：这段时间打的触发词不会展开，若是有意暂停或注入故障可忽略；
2. 连续输入活动不少于 `-ActiveWindowMinutes` 的时段：对照执行历史，确认每个触发都有记录；
3. 结束健康为「降级」或「未知」：日志里看不全，以健康页为准；
4. 每轮固定要人工看的：执行成功 / 失败 / 未执行的条数，结束时健康页的实际状态，每次触发是否真的产生了输出。

健康状态的推断规则：`app.start` → 正在启动；`self_test.startup` 的 `health`；`supervisor.recovering`、`supervisor.repair` → 恢复中；`supervisor.recovered` 的 `state`；`supervisor.circuit_open`、`runtime.failed_latched`、`supervisor.unsupervised`、`supervisor.engine_unavailable`、`supervisor.driver_hung` → 故障；`app.exit` 或采样时进程不存在 → 已退出。脚本开始时会回放昨天和今天（UTC）的日志，只用来确定起始状态，不计入统计；回放时读文件出错会隔 0.5 秒重试一次，仍失败就计入「启动时回放出错」，起始状态的依据后面标「基线不完整」，并列为 A 类可疑项（第 8 条）。今天的文件回放失败时，监视仍从脚本启动时的文件末尾读起，启动前的历史不会被当成新增。

## 日志的读法

- 每个日志文件记一个游标：读到的字节偏移、文件身份（创建时间，UTC）、偏移前 64 字节的指纹。只读完整的行，最后一行没写完时留到下次。
- 应用只追加日志。所以下次读时出现下面任一情况，就认为这个路径上已经不是原来那个文件，游标回到 0 从头重读，计入「日志从头重读」并列为 A 类可疑项（第 8 条），汇总的「日志读取」一节按原因分别计数：
  - 文件比偏移短（被截断）——「文件变短」；
  - 创建时间变了（删掉后重建，或被别的文件改名覆盖）——「文件被替换」；
  - 偏移前 64 字节和上次不同（两次读取之间被截断又长回偏移以上，或被替换但 NTFS 的文件名隧道保留了原创建时间）——「偏移前的内容变了」。
- 从头重读的代价：截断或替换之前、上次读取之后写进旧文件的事件会漏读；新文件里和旧文件相同的开头会再计一次。所以出现这一项时，事件计数和状态推断只作参考。
- 文件开头的 UTF-8 BOM（`EF BB BF`）只在偏移 0 处跳过，包括从头重读的时候；不跳过的话第一行的时间戳对不上行首格式，第一条事件会被当成续行丢掉。应用自己（`tracing_appender`）不写 BOM，用编辑器另存过的文件才会有。
- **当天日志文件缺失的宽限。** 进程在运行、日志目录存在、当天（UTC）的日志文件却不存在时，脚本按连续的采样记一段「缺失」，超过宽限就在控制台提示一次，并在汇总里列为 A 类可疑项（第 8 条）；宽限内文件出现了就只计数，不列。宽限从文件「应该出现」的时刻算：
  - 进程在当前 UTC 日启动：从进程启动算 **120 秒**。`tracing_appender` 在日志初始化时就建当天的文件（`src-tauri/src/diagnostics/logging.rs` 的 `file_writer`），正常情况下进程一起来文件就在；
  - 进程在 UTC 零点之前就在运行：从 UTC 零点算 **60 分钟**。零点之后，新一天的文件要等应用写第一行日志时才建，而应用没有定时日志，一直空闲时可能很久都不写；所以这一项出现时先想想这段时间应用是否真的写过日志。想确认时可在「运行状态与恢复」里点一次「导出诊断包」，它会写一条 `diagnostics.export`；
  - 读不到进程启动时间：从第一次发现缺失算，按 60 分钟。
- **整轮没有读到日志。** 回放和监视期间一次都没能读到日志文件（不存在或读取出错）时，列为 A 类可疑项：健康、监听和事件都无从推断。

宽限值是脚本里的常量 `$LogStartupGraceSeconds`、`$LogRolloverGraceMinutes`，不是参数。

## 局限

- 日志按 UTC 日期分文件（`logs\quicktext.YYYY-MM-DD.log`），应用在 UTC 零点之后第一次写日志时才换文件；脚本跨日时先读完旧文件再读新文件，并在之后的每次采样里继续检查旧文件的尾部。
- 健康和监听状态是从日志推断的，「正常 ↔ 降级」和「正在启动 → 正常」不写日志；以界面为准。
- 输入活动只说明「有人在操作」，不说明打了触发词，也不区分键盘和鼠标。
- 内存统计以最后一个进程实例为准；进程重启过时另列全程峰值。
- WebView2 子进程（`msedgewebview2`）不在统计范围内。

## 回归用例 `soak.Tests.ps1`

不依赖 Pester。它用 `[System.Management.Automation.Language.Parser]::ParseFile` 解析 `soak.ps1`，只加载其中的常量和函数（最后一个函数定义及之前的顶层语句；这一段里出现常量和函数以外的语句会直接报错退出），不跑设置和主循环。每个用例先调用 `Initialize-SoakState` 重置状态，在 `%TEMP%\qt-soak-tests-<随机>\` 下自己的目录里写合成日志，再直接调用脚本里的函数，检查计数、游标和汇总里的自动可疑项。

它不启动应用，不查询或结束任何进程，不读写 `%APPDATA%` 下的数据目录（临时目录落在 `%APPDATA%` 里时拒绝运行），不装钩子，不产生输入；结束时删除自己的临时目录。

```powershell
pwsh -NoProfile -File scripts/soak/soak.Tests.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\soak\soak.Tests.ps1
```

每个用例输出一行 `PASS` / `FAIL`（失败的断言缩进列在下面），最后一行 `RESULT passed=<n> failed=<n>`；全部通过时退出码为 0，否则为失败的断言数。改过 `soak.ps1` 后两种 PowerShell 都跑一遍，并做一次语法检查：

```powershell
$t = $null; $e = $null
[void][System.Management.Automation.Language.Parser]::ParseFile((Resolve-Path scripts/soak/soak.ps1), [ref]$t, [ref]$e); $e.Count   # 应为 0
```

| 用例 | 覆盖 |
|---|---|
| 5a、5b | 文件开头的 BOM：第一条事件不丢；只在偏移 0 处跳过，从头重读后照样跳过，偏移非 0 时不动 |
| 2a | 只追加时不误判，游标记下文件身份和指纹 |
| 2b | 同一个文件在两次读取之间截断又长回旧偏移以上：按「偏移前的内容变了」从头重读，新内容的事件一条不漏，汇总列出 A 类项 |
| 2c | 同一路径换成另一个文件，旧偏移前的字节完全相同、创建时间不同：按「文件被替换」从头重读 |
| 2d | 文件变短：按「文件变短」从头重读 |
| 1a | 当天启动的进程：日志文件缺失到 110 秒不列，125 秒第一次超过宽限、只提示一次；汇总列出，文件出现后这一段照样列出 |
| 1b | 宽限内文件出现：不列 |
| 1c | 跨日：从 UTC 零点算，50 分钟不列，61 分钟列 |
| 1d | 读不到进程启动时间：从第一次发现算 60 分钟 |
| 1e | 换了日志文件：上一段结束，新文件另起一段 |
| 1f | 整轮没有读到日志：列出；读到过一次就不列，不存在的文件不算读到 |
| 3a | 启动检查看到进程（两个同名时跟踪最早启动的）、第一次采样前就没了：记进程不存在，健康改为已退出，结束时没有 `app.exit` 另列一项 |
| 3b | 读到 `app.exit` 后进程消失：进程不存在照样列，不列「没有正常退出」 |
| 3c | 启动检查和采样都没看到进程：列「整轮没有看到进程」，不记不存在的时段 |
| 3d | `Get-EndProcessFinding` 的四种组合；启动检查之后第一次采样同一 PID 不算重启，换 PID 算 |
| 4a | 回放时今天的日志被独占（`FileShare.None`）：重试一次仍失败，计数、标「基线不完整」、列 A 类；昨天的日志照样回放；今天的游标在启动时的文件末尾，之后只计新增 |
| 4b | 回放第一次读失败、重试成功（在用例自己的作用域里定义同名函数遮住 `Read-LogChunk`，用例结束即失效）：不计出错 |
| 6a | 手动执行的 `command.execute_refused`、三类 `command.execute_failed`、`executor.manual_send_refused`：字段拆分正确；被拒和 `stalled` 列 A 类，`run_failed` / `spawn_failed` 计入 ERROR；`trace_id`、`os_code` 不进汇总 |
| 6b | 没有手动执行事件：关键事件照样列出 0 次，不列 A 类；`executor.manual_send_refused` 只计数 |

**没有自动化的部分**是脚本主体里把真实进程接到这些函数上的几行（启动检查把 `GetProcessesByName` 的结果交给 `Register-StartupProcess`；`Invoke-Sample` 取进程启动时间交给 `Update-LogCoverage`），因为要有真实进程。手工核对时不需要 Quicktext，也不产生输入：

1. 建一个空目录，例如 `%TEMP%\qt-soak-manual\`，在里面建空的 `logs\` 子目录；
2. 启动记事本，随即运行 `pwsh -NoProfile -File scripts/soak/soak.ps1 -ProcessName notepad -DataDirectory $env:TEMP\qt-soak-manual -Hours 0.1 -IntervalSeconds 5`（共 6 分钟）；
3. 记事本启动后约 2 分钟，控制台应出现一次「当天日志文件 … 超过宽限仍不存在」的提示，之后不再重复（第 1 项）；
4. 提示出现后关掉记事本，等脚本结束：汇总的自动可疑项里应有「进程不存在 1 段」和「结束时进程 notepad 不在运行，日志里也没有它正常退出的记录」（第 3 项），以及「进程 … 启动后，当天日志文件 … 超过 120 秒仍不存在」和「整轮没有读到任何日志文件」（第 1 项）；
5. 不开记事本再跑一次（`-Hours 0.01`）：自动可疑项里应有「整轮没有看到进程 notepad」。
