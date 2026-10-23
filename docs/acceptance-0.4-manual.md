# 0.4 实机验收清单（S-25 书面记录模板）

> 状态：2026-10-23 依据 `576aa5a`（`feat/0.4-stability-foundation`）编写，**全部条目待实测**。本文件是记录模板：实测时直接在每条的「结果」行填写，填完后连同日志、诊断包和录屏按 [release-checklist.md](release-checklist.md) §5 存档。
>
> 依据：[开发计划](development-plan.md) §10 退出标准（第 2448 行）、§11 故障注入表（第 2481–2489 行）、隐私测试（第 2491 行）、§5.5 不得倒退清单（第 993–1006 行）、S-25（第 717 行）；[性能基线](performance-baseline.md)；[S-29 spike](spikes/s-29.md)；[验收模式](windows-acceptance-mode.md)。

## 0. 使用方法

- 条目编号为 `M4-<主题>-<序号>`。主题缩写：IN 输入与注入、CB 剪贴板、SV 监督与自愈、ST 安全模式与存储、FE 前端状态一致性、PR 隐私、CP 数据兼容、PF 性能与浸泡、SP S-29 spike、NR 不得倒退。
- 每条有五栏：**步骤 / 预期结果 / 通过标准 / 出处 / 结果**。结果行格式固定：`日期；机器；构建；☐通过 ☐不通过 ☐未测；备注`。选「未测」时必须在备注里写原因。
- **构建栏**：当前版本号仍为 `0.3.1`（见 §12.3），所以构建栏写「版本 + 提交短哈希 + release 或 debug-fi」，例如 `0.3.1+576aa5a release`。
- **「需确认：……」** 表示代码里没有定义、或无法从代码断定的行为。实测后把实际行为写进备注，汇总时提交给开发。
- **「待实测」** 指代码或文档里已经标注【待实测】的常量或假设。附表 A 列出每一处由哪条覆盖。
- **立即停止条件**：出现以下任一情况，立即暂停监听，保留现场（日志、诊断包、录屏），按 P0 处理（退出标准「无 P0 缺陷」）：
  - 错删正文；
  - 吞键或打错字；
  - 输出进了错误的窗口；
  - 覆盖了用户新复制的剪贴板内容；
  - 日志或诊断包里出现隐私哨兵串。
- 故障注入、截断或锁定配置等破坏性操作，一律在**独立数据目录**或**已备份的数据目录**上做（§1.3）。

## 1. 前置

### 1.1 机器

| 代号 | 用途 | Windows 版本（`winver`） | 输入法 / 布局 | 显示缩放 | 备注 |
|---|---|---|---|---|---|
| A | 主测机，物理机。实机最小集、剪贴板、安全模式、隐私、兼容、性能 | ____ | 微软拼音 + 英语（美国）；NR-07 另加德语（QWERTZ） | ____ | 装好记事本、Chrome、微信 PC 版 |
| B | 虚拟机，可快照还原。S-29 spike、浸泡负载 | ____ | 同上 | ____ | spike 会抢焦点、注入输入，只在这里运行 |
| C（可选） | Windows 10 22H2 或另一台硬件 | ____ | ____ | ____ | 抽测 IN、SV |

在 PowerShell 里记录版本：`(Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').DisplayVersion; [Environment]::OSVersion.Version`

### 1.2 构建

1. **release 构建**。以下内容都用它：
   - 实机最小集、剪贴板、安全模式与存储、隐私、兼容、性能、浸泡、不得倒退。

   获取：
   - 用 GitHub Actions `release` workflow 产出的 `…-package` artifact，或在本机执行 `scripts/build-windows.ps1`。
   - 特性门禁（`scripts/check-release-features.ps1`、`scripts/check-binary-strings.ps1`）在 release-checklist 里执行，本清单不再重复。只需确认启动日志第一行是 `app.start`，且其中 `debug_build=false`（`src-tauri/src/main.rs:45-50`）。
   - release 构建**不**带开发者工具，也没有 `debug_inject_fault` 命令。
2. **故障注入调试构建（下文称 debug-fi）**。只用于 §4 的注入条目和少数需要开发者工具的读数。
   - 构建：`npm run tauri build -- --debug --no-bundle --features fault-injection`，产物是 `target\debug\quicker-text.exe`。
   - 需确认：Tauri CLI 会把 `--features` 传给 `src-tauri`（`src-tauri/Cargo.toml:12` 定义了 `fault-injection = ["quicker-platform/fault-injection"]`）。验证方法见 §1.6：注入命令报 `command debug_inject_fault not found`，说明特性没有生效。
   - 启动时一律指定独立数据目录。debug 构建认 `QUICKER_TEXT_DATA_DIR`（`src-tauri/src/main.rs:30-40`）：

     ```powershell
     $env:QUICKER_TEXT_DATA_DIR = 'D:\qt-fi'
     & .\target\debug\quicker-text.exe
     ```

   - **不要**设置 `QUICKER_TEXT_TEST_INPUT`。debug 构建只要这个变量为 `1` 就进入验收模式，此时外部模拟输入不再被过滤（[windows-acceptance-mode.md](windows-acceptance-mode.md)）。测 NR-04 前先用 `Get-ChildItem env:QUICKER_TEXT*` 确认没有设置。
   - 开发者工具：在 debug 构建的窗口里按 F12，或右键选「检查」。
3. **0.3.1 安装包**：GitHub Release `v0.3.1`，供 §8 使用。
4. **单实例**：程序启用了单实例插件，第二次启动只会唤起已有的窗口。因此切换构建前，必须先从托盘菜单「退出」，或在「运行状态与恢复」点「退出全部」，然后用任务管理器确认没有 `quicker-text.exe` 进程。

### 1.3 数据目录、日志、崩溃记录与诊断包

- **数据目录**：
  - release 构建是 `%APPDATA%\com.quickertext.desktop`；debug-fi 构建是 `QUICKER_TEXT_DATA_DIR` 指定的目录。
  - 主要内容：
    - 配置：`active.json`、`draft.json`、`previous.json`、`prefs.json`、`store-meta.json`；
    - 历史版本：`revisions\<n>.json`、`revisions\<n>.metadata.json`；
    - 被隔离的损坏文件：`*.corrupt-<时间>.json`；
    - 崩溃记录：`crash-<UTC yyyymmdd-hhmmss>.txt`；
    - `logs\` 目录。
  - 在「运行状态与恢复」可以打开数据目录。
- **备份与还原**：做 ST、CP、PR 之前，先退出程序，把整个数据目录复制到 `D:\qt-backup\<日期时间>\`；测完退出程序，再复制回去。
- **日志**：
  - 位置是 `logs\quicker-text.YYYY-MM-DD.log`，每行带 `event=` 字段。下文写的 `event` 名称就是用来检索的关键字。
  - 在「运行状态与恢复」点「打开日志目录」可以直接打开。
  - 检索（片段 P0）：

    ```powershell
    $d = "$env:APPDATA\com.quickertext.desktop"   # debug-fi 改成 D:\qt-fi
    Select-String -Path "$d\logs\*.log" -Pattern 'store\.corrupt|store\.recovered' -Encoding UTF8 | Select-Object -Last 20
    ```

- **崩溃记录**：panic 发生时，数据目录里会出现 `crash-*.txt`，日志里有 `app.panic`（`src-tauri/src/diagnostics/panic.rs`）。
- **诊断包**：
  - 导出步骤：在「运行状态与恢复」点「导出诊断包」→ 在「导出诊断包？」里选「不包含短语内容（推荐）」或「包含短语内容」→ 在保存对话框里选位置 → 界面提示「诊断包已导出：<路径>」。
  - 包里的文件（`src-tauri/src/diagnostics/bundle.rs:8-11、218-262`）：
    - `manifest.json`、`system.json`、`health.json`、`config-summary.json`；
    - `logs/`：最近 7 天的日志；
    - `crashes/`：最新的崩溃记录；
    - `active.json`：只有选「包含短语内容」时才有。
- **`config-summary.json` 的关键字段**（`bundle.rs:1340-1411`）：
  - `active.file.status` / `draft.file.status` 取以下值之一：
    - `ok`；
    - `missing`；
    - `unreadable{kind, os_code}`；
    - `unparsable{category: config_malformed | schema_version, kind, line, column}`；
    - `newer_schema{version}`。
  - 还有 `active.counts`、`draft.counts`、`prefs.status`、`quarantined_files`。
  - 文件带 BOM 时会跳过 BOM，行号和列号从 BOM 之后算起。
- **详细运行信息**：「运行状态与恢复」页底部的「详细运行信息（高级）」展开后显示引擎详情 JSON，其中 `external_injected_keys` 的值为 `filtered` 或 `explicit_test_mode`（`src/RuntimeStatus.tsx:222-235`）。

### 1.4 PowerShell 片段

片段用 Windows PowerShell 5.1 或 pwsh 7 都能运行。`$d` 的取值见 P0。

**P1 独占锁定文件，按回车释放**（ST-01、ST-02、FE-03、NR-08）：

```powershell
$p = "$d\active.json"          # 按条目改成 draft.json 等
$f = [System.IO.File]::Open($p, 'Open', 'ReadWrite', 'None')
Read-Host "已锁定 $p ；按条目操作，观察后按回车释放"
$f.Close(); "已释放 $(Get-Date -Format HH:mm:ss.fff)"
```

**P2 占住剪贴板 N 秒**（CB-07、SV-11、NR-02）：

```powershell
Add-Type -Namespace QtTest -Name Clip -MemberDefinition '[DllImport("user32.dll")] public static extern bool OpenClipboard(IntPtr h); [DllImport("user32.dll")] public static extern bool CloseClipboard();'
[QtTest.Clip]::OpenClipboard([IntPtr]::Zero); "占用开始 $(Get-Date -Format HH:mm:ss.fff)"
Start-Sleep -Seconds 10        # 按条目改秒数
[QtTest.Clip]::CloseClipboard(); "占用结束 $(Get-Date -Format HH:mm:ss.fff)"
```

**P3 把文件截掉后一半，以无 BOM 的 UTF-8 写回**（ST-05、ST-06）：

```powershell
$p = "$d\active.json"
$t = [IO.File]::ReadAllText($p)
[IO.File]::WriteAllText($p, $t.Substring(0, [int]($t.Length / 2)), (New-Object Text.UTF8Encoding $false))
```

**P4 给文件加 UTF-8 BOM**（ST-07）：

```powershell
foreach ($n in 'prefs.json','active.json','draft.json') {
  $p = "$d\$n"; $t = [IO.File]::ReadAllText($p)
  [IO.File]::WriteAllText($p, $t, (New-Object Text.UTF8Encoding $true))
  Format-Hex $p | Select-Object -First 1    # 开头应为 EF BB BF
}
```

**P5 列出当前剪贴板的格式**（CB-04）：

```powershell
powershell.exe -STA -Command "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::GetDataObject().GetFormats(`$false)"
```

**P6 外部模拟按键**（NR-04）。运行后 8 秒内把焦点切到记事本：

```powershell
Add-Type -AssemblyName System.Windows.Forms
Start-Sleep -Seconds 8
[System.Windows.Forms.SendKeys]::SendWait('zzk ')
```

**P7 进程采样，每 5 分钟一行 CSV**（PF-04、PF-05）。计数器命令另见 performance-baseline.md 第 71 行。

```powershell
$out = "D:\qt-sample-$(Get-Date -Format yyyyMMdd-HHmm).csv"
while ($true) {
  $p = Get-Process quicker-text -ErrorAction SilentlyContinue
  if ($p) {
    [pscustomobject]@{ t = (Get-Date -Format s); private_mb = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
      ws_mb = [math]::Round($p.WorkingSet64 / 1MB, 1); handles = $p.HandleCount; threads = $p.Threads.Count
      cpu_s = [math]::Round($p.CPU, 1) } | Export-Csv $out -Append -NoTypeInformation -Encoding UTF8
  } else { "$(Get-Date -Format s) 进程不存在" | Add-Content "$out.missing.txt" }
  Start-Sleep -Seconds 300
}
```

**P8 生成批量导入文件**（FE-02）：

- 导出文件是 portable 格式：顶层有 `format`、`version`、`config.rules`，示例见 `crates/domain/tests/fixtures/portable.json`。
- 导入校验很严格（`crates/domain/src/package.rs:160`），所以不要添加任何未知字段。

```powershell
$src = [IO.File]::ReadAllText('D:\qt-0.4-rules.json') | ConvertFrom-Json
$base = $src.config.rules | Where-Object { $_.match.value -eq 'zzk' } | Select-Object -First 1
$bulk = for ($i = 0; $i -lt 5000; $i++) {
  $r = $base | ConvertTo-Json -Depth 30 | ConvertFrom-Json
  $r.id = "bulk-$i"; $r.match.value = "zqb$i"; $r.enabled = $false; $r }
$src.config.rules = @($src.config.rules) + $bulk
[IO.File]::WriteAllText('D:\qt-bulk-5000.json', ($src | ConvertTo-Json -Depth 40), (New-Object Text.UTF8Encoding $false))
```

**P9 检索隐私哨兵**（PR）：

```powershell
$pat = 'QTSENTINEL', '7F3A9C', '隐私哨兵'
Get-ChildItem "$d\logs", $d -File -Filter *.* -Recurse:$false | Where-Object { $_.Name -like '*.log' -or $_.Name -like 'crash-*.txt' } |
  Select-String -Pattern $pat -SimpleMatch -Encoding UTF8
# 诊断包：先解压再检索
Expand-Archive 'D:\qt-diag.zip' -DestinationPath 'D:\qt-diag' -Force
Get-ChildItem 'D:\qt-diag' -File -Recurse | Select-String -Pattern $pat -SimpleMatch -Encoding UTF8
```

（`$d\logs` 下的日志要单独递归检索：`Get-ChildItem "$d\logs" -Recurse -File | Select-String ...`。）

**P10 文件哈希**（CP、ST）：`Get-FileHash "$d\active.json","$d\draft.json","$d\prefs.json" -Algorithm SHA256 | Format-Table -AutoSize`

**P11 浸泡负载**（PF-05）：

- 只在虚拟机 B 或专用的测试会话里运行。它会向**当前前台窗口**发送按键。
- 程序必须以验收模式启动，否则外部模拟输入会被过滤（见 PF-05）。

```powershell
Add-Type -AssemblyName System.Windows.Forms
$end = (Get-Date).AddHours(2); $n = 0; $log = 'D:\qt-soak-sent.log'
$words = 'lorem','ipsum','dolor','amet','quick','brown','jumps','over'
while ((Get-Date) -lt $end) {
  for ($i = 0; $i -lt 10; $i++) { [System.Windows.Forms.SendKeys]::SendWait("$(Get-Random $words) "); Start-Sleep -Milliseconds 1100 }
  $abbr = if ($n % 2) { 'zzq' } else { 'zzv' }
  [System.Windows.Forms.SendKeys]::SendWait("$abbr "); $n++
  "$(Get-Date -Format s) $abbr $n" | Add-Content $log
  Start-Sleep -Seconds 2
  [System.Windows.Forms.SendKeys]::SendWait('{ENTER}')
}
```

### 1.5 测试规则集

创建方法：
1. 在「文本指令管理」点「＋ 添加指令」。
2. 在「编辑文本指令」中填写「缩写词」，选择「操作类型」，填写「操作正文」，再点「保存」（提示「指令已保存到草稿」）。
3. 全部建完后点底部「应用设置」（提示「设置已应用，生效版本已更新」）。
4. 再点「导出」，保存为 `D:\qt-0.4-rules.json`（提示「已导出：<路径>」），以后在其他数据目录里直接「导入」即可。

除特别说明外，触发方式都选「输入缩写后，再按空格、分号等指定键执行」。

| 缩写 | 操作类型 | 正文 / 步骤 | 用于 |
|---|---|---|---|
| `zzk` | 键入纯文本 | `Hello QuickerText 0.4 你好` | 通用 |
| `zzp` | 粘贴纯文本 | `粘贴测试 Paste-OK` | 剪贴板、通用 |
| `zzm` | 粘贴纯文本 | 三行：`第一行` / `second line` / `第三行 3` | 多行 |
| `zzl` | 键入纯文本 | 约 300 字：「快速的棕色狐狸跳过懒狗 0123456789。」重复 15 遍 | 焦点切换、终止键 |
| `zzh` | 粘贴带格式的文字 | `<b>粗体</b>普通<i>斜体</i>`（需确认：编辑器是否按 HTML 源码接收） | 剪贴板 |
| `zzc` | 按快捷键 | `Ctrl+End`（在编辑器里录入） | 修饰键 |
| `zzw` | 按顺序执行多个操作 | 步骤：逐字键入「步骤一」→ 等待 3000 毫秒 → 逐字键入「步骤二」。焦点策略选「始终在指令开始时的窗口执行」 | 焦点切换、fail-stop |
| `zzi` | 键入纯文本，触发方式选「缩写输入完成后立即执行（不支持匹配规则）」 | `立即触发OK` | 立即触发 |
| `zzs` | 粘贴纯文本 | `QTSENTINEL-7F3A9C-隐私哨兵` | 仅 §7，测完删除 |
| `zzt` | 键入纯文本 | `QTSENTINEL-7F3A9C-隐私哨兵` | 仅 §7，测完删除 |
| `zzq` | 键入纯文本 | `[QT-SOAK-OK]` | 仅浸泡 |
| `zzv` | 粘贴纯文本 | `[QT-SOAK-PASTE]` | 仅浸泡 |
| `zzr`（可选） | 按顺序执行多个操作 | 步骤「鼠标按键」：左键单击 | IN-10 |

### 1.6 故障注入的调用方法（仅 debug-fi）

在 debug-fi 窗口里打开开发者工具，在 Console 执行：

```js
const inv = window.__TAURI_INTERNALS__.invoke;
await inv('debug_inject_fault', { point: 'executor_panic', param: 0 });
```

- 每次布防只触发一次（`crates/platform/src/fault.rs`，`fault::take`）。
- 布防时日志记 `fault.injected`，其中 `point` 为 Debug 形式，例如 `ExecutorPanic`（`src-tauri/src/commands/engine.rs:165-176`）。输入侧和会话侧的注入点在真正触发时还会再记一条 `fault.injected point="hook_unhook"` 等（`crates/platform/src/windows/input.rs:914-927`、`session.rs:216-221`）。
- 其他可用命令：
  - `await inv('self_test')`：按需自检。
  - `await inv('list_history', { since_seq: null })`：取执行历史，最多 200 条、30 分钟（`crates/platform/src/history.rs:14-17`）。

| 注入点 | 何时生效 | param | 条目 |
|---|---|---|---|
| `executor_panic` | 下一次执行的第一批 SendInput（`executor.rs:1748-1751、1824`） | 0 | SV-02、PR-01 |
| `executor_hang` | 同上，线程睡眠 param 毫秒 | 200000（计划 §11 的 200 秒）；NR-02 用 30000 | SV-03、NR-02 |
| `hook_panic` | 下一次键盘钩子回调（`hook.rs:243`） | 0 | SV-04 |
| `hook_unhook` | 输入消费循环的下一轮（`input.rs:916`） | 0 | SV-05 |
| `rawinput_unregister` | 同上（`input.rs:920`） | 0 | SV-06 |
| `rawinput_close` | 同上（`input.rs:924`） | 0 | SV-07 |
| `clipboard_busy` | 此后 param 毫秒内视为剪贴板被占用 | 5000 | CB-08 |
| `store_write_fail` | 下一次原子写（`src-tauri/src/store/atomic.rs:18`） | 0 | NR-08 |
| `session_end` | 输入消费循环的下一轮，模拟 WM_QUERYENDSESSION 和 WM_ENDSESSION（`input.rs:997`、`session.rs:216-221`） | 0 | SV-14 |
| `config_corrupt`、`store_locked`、`autostart_stale` | 只在启动期检查，进程起来后再布防已经来不及 | — | 不用。实机分别用 ST-05/06、ST-01/02、SV-12 的真实操作代替。`store_locked` 没有解除手段，**不要布防** |

## 2. 输入与注入（M4-IN）

### M4-IN-01 记事本：键入、粘贴、多行、立即触发、快捷键

- **步骤**：
  1. 用 release 构建导入 §1.5 的规则集，「应用设置」后确认右上角显示「当前生效 v{n}」，状态行显示「监听中」。
  2. 打开 Windows 11 记事本，新建标签页，用物理键盘依次输入以下内容，每种 5 次，每次之间按 Enter：
     - `zzk` + 空格；
     - `zzp` + 空格；
     - `zzm` + 空格；
     - `zzl` + 空格；
     - `zzi`；
     - `zzc` + 空格（先在前面写几行文字）。
  3. 每次执行后切到 Quicker Text 的「运行状态与恢复」，查看「最近执行」。
- **预期结果**：
  - 缩写和终止键被删除，正文逐字正确，光标停在正文末尾。
  - `zzm` 的换行正确；`zzc` 把光标移到文末。
  - 「最近执行」每次都有一行：结果为「已完成」，粘贴类可能显示「已发送 · 结果未知」。
- **通过标准**：30 次执行全部正确：没有错删、没有残留缩写、没有错字，每次都有历史记录。
- **出处**：S-25（development-plan.md:717）实机最小集「记事本」；`src/status.ts:269-272`（历史文案）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-IN-02 Chrome：input / textarea / contenteditable

- **步骤**：
  1. 在 Chrome 打开本地文件 `scripts/fixtures/browser-input.html`。
  2. 分别在 `#plain`（input）、`#area`（textarea）、`#rich`（contenteditable）中执行 `zzk`、`zzp`、`zzm`、`zzi`、`zzh`，每种 3 次。
  3. 在 `#rich` 里把光标移到已有文字中间再执行一次 `zzk`。
- **预期结果**：
  - 三种控件都正确展开。
  - `#area` 的多行正确，`#rich` 中 `zzh` 带粗体和斜体。
  - 在文字中间展开时不删除前后的无关文字。
- **通过标准**：没有错删、残留或吞键；页面事件记录区（`#events`）没有异常的重复输入。
- **出处**：S-25 实机最小集「Chrome」；docs/browser-ime-manual-tests.md B01、B02（0.2.0 的矩阵，仅作对照）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-IN-03 微信 PC 版

- **步骤**：
  1. 打开微信 PC 版的「文件传输助手」（**只用这个会话**：在微信里 Enter 就是发送）。
  2. 在输入框里执行 `zzk`、`zzp`、`zzm`、`zzi`，每种 3 次，每次检查后手动清空输入框，不要发送。
  3. 执行 `zzl`，观察长文本键入。
- **预期结果**：
  - 正文正确。
  - `zzm` 粘贴的多行文字留在输入框里，没有被逐行发送。
  - 没有触发微信的其他快捷键。
- **通过标准**：没有错删、残留，没有误发送。
- **出处**：S-25 实机最小集「微信 PC 版」。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-IN-04 微软拼音：中文模式与英文模式

- **步骤**：
  1. 把输入法切到微软拼音。
  2. 按 Shift 在中文模式和英文模式之间切换，在记事本、Chrome `#plain`、微信「文件传输助手」中按下表执行。
  3. 每格做 3 次，填写实际现象。
- **预期结果**：
  - 英文模式下与英语（美国）布局相同，能正常展开。
  - 中文模式下的组字策略属于 S-11，排在 0.5（development-plan.md:612、709），0.4 只记录现状作为基线。
- **通过标准**：
  - 英文模式的所有格都正确展开。
  - 中文模式的所有格都不能出现：错删已上屏的文字、候选框残留拼音、输出进别的窗口。
  - 中文模式下是否展开不作为 0.4 的判定项，如实记录即可。
- **出处**：S-25「各配合微软拼音」；S-11（development-plan.md:709）。
- **子表**：

| 程序 | 模式 | 操作 | 实际现象 | 错删 / 残留 / 错窗 | 判定 |
|---|---|---|---|---|---|
| 记事本 | 英文 | `zzk` + 空格 | | | |
| 记事本 | 英文 | `zzp` + 空格 | | | |
| 记事本 | 英文 | `zzi` | | | |
| 记事本 | 中文 | `zzk` + 空格（空格会选候选） | | | |
| 记事本 | 中文 | 先上屏「你好」，切英文后 `zzk` + 空格 | | | |
| Chrome `#plain` | 英文 | `zzk` + 空格 / `zzp` + 空格 / `zzi` | | | |
| Chrome `#plain` | 中文 | `zzk` + 空格 | | | |
| 微信 | 英文 | `zzk` + 空格 / `zzp` + 空格 / `zzi` | | | |
| 微信 | 中文 | `zzk` + 空格 | | | |

- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：0.4 在中文模式组字期间的预期是什么？参考全局设置 `ime_erasure` 的默认值 `escape`）____

### M4-IN-05 执行中切换窗口（focus_changed）

- **步骤**：
  1. 在记事本执行 `zzl` + 空格，键入开始后立刻按 Alt+Tab 切到 Chrome 的 `#plain`。
  2. 在记事本执行 `zzw` + 空格，等「步骤一」出现后，在 3 秒等待内切到 Chrome 的 `#plain`。
  3. 查看「最近执行」。
- **预期结果**：
  - 两种情况下，切换之后的字符都**不会**进入 Chrome，「步骤二」不会出现在任何窗口。
  - 历史为「已停止」或「失败」，原因「执行中切换了窗口」。
- **通过标准**：Chrome 里没有出现任何来自规则的字符；历史原因正确。
- **出处**：ReasonCode `focus_changed`（`src/ipc/labels.ts`）；焦点策略「始终在指令开始时的窗口执行」（`src/ActionEditor.tsx:1473-1474`）；§5.5 #6。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-IN-06 修饰键不卡住；按住修饰键时执行

- **步骤**：
  1. 在记事本执行 `zzc` + 空格。之后手动输入 `abc`，确认输入的是小写字母，没有选中文字，也没有触发 Ctrl 组合键。
  2. 新建一条键入纯文本规则，正文含大写和符号（如 `ABC Hello! #1`），执行后同样手动输入 `abc` 检查。
  3. 执行 `zzw` + 空格，在 3 秒等待期间按住左 Ctrl 不放，直到超过第二步的时刻再松开。
  4. 执行后在记事本中输入 `a`，确认 Shift、Ctrl、Alt 都没有卡住。
- **预期结果**：
  - 第 1、2 步之后没有修饰键残留。
  - 第 3 步第二步不在 Ctrl 按住时输出（否则会变成 Ctrl+字母）。历史原因「按住了 Ctrl、Alt 等修饰键」，或者等松开后再输出（需确认是哪一种）。
- **通过标准**：没有卡键；没有在修饰键按下时把正文打成快捷键。
- **出处**：ReasonCode `modifier_held`。待实测：`crates/platform/src/windows/keystate.rs:344`（注入的通用修饰键是否置位左侧键状态）、`keystate.rs:444`（INJECTION_QUIET 250 ms 是否足够）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-IN-07 终止键 Ctrl+Shift+Esc

- **步骤**：
  1. 在记事本执行 `zzl` + 空格，键入进行到一半时按 Ctrl+Shift+Esc。
  2. 观察键入是否停止，任务管理器是否弹出。
  3. 查看「最近执行」。
- **预期结果**：
  - 键入立即停止。
  - 历史显示「已停止」。
  - 钩子吞掉该组合键（`crates/platform/src/windows/hook.rs:262-275`），所以任务管理器**不应**弹出。
- **通过标准**：剩余字符不再输出；任务管理器是否弹出如实记录。
- **出处**：`hook.rs:262-275`。development-plan.md:215 标为【待核实】：Ctrl+Shift+Esc 同时是系统的任务管理器快捷键。计划的默认终止键是 Ctrl+Alt+Pause，见 §12.3。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（任务管理器是否弹出：____）

### M4-IN-08 锁屏与解锁、睡眠与唤醒

- **步骤**：
  1. 在监听中按 Win+L 锁屏，在锁屏密码框里输入 `zzk` + 空格，然后删除这些字符，正常解锁。
  2. 解锁后 5 秒内在记事本执行 `zzk` + 空格。
  3. 「开始」→「电源」→「睡眠」，等 1 分钟后唤醒，解锁，重复第 2 步。
  4. 检索日志里的 `session.signal`、`input.refused`、`input.check`、`supervisor.`。
- **预期结果**：
  - 锁屏界面上没有展开（安全桌面）。
  - 解锁和唤醒后能正常展开，状态行回到「监听中」。
  - 日志有 `session.signal`，没有 `supervisor.circuit_open`。
- **通过标准**：解锁和唤醒后 10 秒内展开成功；没有熔断；状态行没有停在「正在恢复…」。
- **出处**：`crates/platform/src/windows/session.rs`；self_test_probes.rs:7 的待实测（锁定的工作站）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-IN-09（可选）远程桌面会话

- **步骤**：
  1. 从另一台电脑用 RDP 登录机器 A，在远程会话里启动 release 构建。
  2. 在会话内的记事本执行 `zzk`、`zzp` 各 5 次。
  3. 静置 10 分钟后再执行一次，然后检索日志里的 `input.check`。
- **预期结果**：
  - 能正常展开。
  - 日志里没有误判的 `input.check source=raw_input result=…` 或 `source=keyboard_hook result=lost`。
- **通过标准**：展开成功，没有反复重挂。如果 RawInput 收不到键（没有设备句柄），记录现象。
- **出处**：待实测 `crates/platform/src/windows/input.rs:966`、`pipeline_tests_input.rs:1745`、`pipeline_tests_input.rs:2671`（远程会话的按键没有设备句柄）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-IN-10（可选）鼠标左右键互换

- **步骤**：
  1. 在「设置 → 蓝牙和其他设备 → 鼠标」把主按钮设为「右」。
  2. 在记事本里执行 `zzr`，看左键单击落在光标处时实际是左键还是右键效果（右键会弹出菜单）。
  3. 测完把主按钮改回「左」。
- **预期结果**：执行后没有鼠标键卡住，后续手动点击正常。实际是哪个键如实记录。
- **通过标准**：没有卡键，并记录实际行为。
- **出处**：待实测 `crates/platform/src/windows/inject.rs:331`（左右键互换时，注入的 MOUSEEVENTF_LEFTDOWN 置位哪个 VK）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

## 3. 剪贴板（M4-CB）

剪贴板选项只在启动时读取一次，所以改 `prefs.json` 前要先退出程序，改完再启动。`prefs.json` 的结构：

```json
{ "app": { "auto_listen_on_start": true, "clipboard": { "restore_policy": "best_effort", "restore_delay_ms": 300 } } }
```

- `restore_policy` 取 `full`、`best_effort` 或 `text_only`（`crates/platform/src/traits.rs:185-202`）。
- `restore_delay_ms` 的范围是 100–2000。

### M4-CB-01 默认剪贴板选项

- **步骤**：
  1. 退出程序，确认 `prefs.json` 里没有 `clipboard` 节（没有的话保持原样）。
  2. 启动 release 构建，检索日志里的 `clipboard.options_set`。
- **预期结果**：日志为 `clipboard.options_set policy=BestEffort restore_delay_ms=300`（`src-tauri/src/clipboard_options.rs:15`，默认值见 `traits.rs:206`）。
- **通过标准**：取值与默认值一致。
- **出处**：`clipboard_options.rs:15`；`traits.rs:185-206`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-CB-02 修改 restore_policy 和 restore_delay_ms

- **步骤**：
  1. 退出程序，把 `clipboard` 节改成 `{"restore_policy":"text_only","restore_delay_ms":1000}`，其他键保持不变，然后启动。
  2. 在 Chrome 选中一段网页文字复制（剪贴板里有 HTML）。用 P5 记下格式列表。
  3. 在记事本执行 `zzp` + 空格，约 1 秒后再用 P5 查看格式。
  4. 退出程序，改成 `full`，重启。用 Win+Shift+S 截一张图（剪贴板里是位图），然后执行 `zzp`。
  5. 改回 `best_effort` / `300`。
- **预期结果**：
  - 第 1 步的日志为 `clipboard.options_set policy=TextOnly restore_delay_ms=1000`。
  - 第 3 步：恢复发生在粘贴后约 1 秒，恢复的只有文本类格式，HTML 不在其中。
  - 第 4 步：如果有格式不能完整备份，`full` 会拒绝，日志出现 `clipboard.lease_refused reason=…`。此时缩写保留，**不改用键入**，历史显示失败及原因。如果所有格式都能备份，就正常粘贴并完整恢复。
- **通过标准**：日志取值与配置一致；三种策略的行为与上面描述一致；任何情况下都不用键入代替粘贴。
- **出处**：`traits.rs:185-202`；`crates/platform/src/windows/clipboard.rs`（`clipboard.lease_refused`、`clipboard.skipped`）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-CB-03 prefs.json 中的非法值

- **步骤**：
  1. 退出程序，把 `restore_delay_ms` 改为 `50`，启动，检索日志。
  2. 退出，改为 `restore_policy: "aggressive"`，`restore_delay_ms` 恢复为 300，启动，检索日志。
- **预期结果**：
  - 第 1 步：日志有 `store.prefs_invalid file=prefs.json field=… problem=…`，并带 `value`、`min`、`max`。`clipboard.options_set` 回退到该项的默认值。程序正常启动，`prefs.json` 不被隔离。
  - 第 2 步类似：日志带 `found`。
- **通过标准**：非法值不会导致启动失败，也不会进入安全模式；使用默认值并留下日志。
- **出处**：`src-tauri/src/store/prefs.rs`（`store.prefs_invalid`）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：界面是否有提示）____

### M4-CB-04 粘贴后格式完整恢复（best_effort）

- **步骤**（对子表的每一行做一次）：
  1. 准备好剪贴板内容，用 P5 记录格式列表 L1。
  2. 在记事本执行 `zzp` + 空格。
  3. 等 1 秒，用 P5 记录 L2，再把剪贴板粘贴到对应程序里核对内容。
  4. 检索日志里的 `clipboard.skipped`。
- **预期结果**：
  - L2 与 L1 一致，或者少掉的格式恰好列在 `clipboard.skipped formats=… policy=BestEffort` 里。后一种情况下历史显示「部分剪贴板格式未能恢复」。
  - 粘贴出来的内容与原来一致。
- **通过标准**：没有静默丢失格式（丢了就必须有 skipped 日志和历史提示）；恢复后的内容可用。
- **出处**：B-08；`clipboard.rs:47-49、98、125、1408`；`traits.rs:185-202`。
- **子表**：

| 行 | 剪贴板来源 | L1 格式数 | L2 格式数 | skipped 日志 | 粘贴核对 | 判定 |
|---|---|---|---|---|---|---|
| a | 记事本复制一行中文（CF_UNICODETEXT，系统合成 CF_LOCALE；待实测 `clipboard.rs:125`、`clipboard.rs:1408`） | | | | | |
| b | Chrome 选中网页复制（HTML Format） | | | | | |
| c | Win+Shift+S 截图（位图） | | | | | |
| d | 资源管理器复制 2 个文件（CF_HDROP） | | | | 粘贴到另一个文件夹 | |
| e（可选） | Word 或 Excel 复制图表（EMF / METAFILEPICT；待实测 `clipboard.rs:98`） | | | | | |

- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-CB-05 超大剪贴板内容

- **步骤**：
  1. 打开画图，把画布调整为 6000×4000 像素，Ctrl+A、Ctrl+C。
  2. 在记事本执行 `zzp` + 空格。
  3. 检索日志里的 `clipboard.skipped`、`clipboard.lease_refused`，查看历史。
- **预期结果**：
  - 超过 32 MiB 的格式不备份（`clipboard.rs:36`，MAX_FORMAT_BYTES）。
  - 其余格式足以恢复时：正常粘贴，历史显示「部分剪贴板格式未能恢复」。
  - 剪贴板会因此被清空时：拒绝粘贴，历史显示「剪贴板内容过大，未备份」，缩写保留，不改用键入。
- **通过标准**：二者必居其一；不卡顿超过 1 秒；不改用键入。
- **出处**：`clipboard.rs:36`；`traits.rs:185-202`（best_effort 下如果会留下空剪贴板就拒绝）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（实际走了哪一支：____）

### M4-CB-06 不进入剪贴板历史

- **步骤**：
  1. 打开「设置 → 系统 → 剪贴板 → 剪贴板历史记录」。
  2. 复制「原内容 A」，在记事本执行 `zzp` + 空格。
  3. 按 Win+V 查看历史列表。
  4. 可选：装有 Ditto 的机器上也查看 Ditto。
- **预期结果**：历史里有「原内容 A」，**没有**「粘贴测试 Paste-OK」。
- **通过标准**：规则正文不出现在任何剪贴板历史工具里。
- **出处**：`clipboard.rs:47-49`（排除格式：ExcludeClipboardContentFromMonitorProcessing、CanIncludeInClipboardHistory、CanUploadToCloudClipboard）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-CB-07 剪贴板被其他程序占用：健康链从租约、执行器探针到界面

- **步骤**：
  1. 用 release 构建，运行 P2（10 秒）。
  2. 在 10 秒内切到记事本执行 `zzp` + 空格，再执行 `zzk` + 空格。
  3. 仍在占用期间切到 Quicker Text，查看：
     - 「运行状态与恢复」的引擎状态；
     - 状态行；
     - 「平台能力」表。先切到别的窗口再切回来触发刷新，或者点「刷新状态」。
  4. P2 结束后再执行一次 `zzp` + 空格，然后再看一次状态。
  5. 检索日志里的 `clipboard.busy`、`executor.clipboard_probe_cached`。
- **预期结果**：
  - `zzp` 很快失败（10 次 × 15 ms 重试，`clipboard.rs:18-20`），历史显示「失败 · 剪贴板被其他程序占用」。
  - 缩写 `zzp` 保留，**没有任何正文被键入**。
  - `zzk`（键入）不受影响。
  - 日志有 `clipboard.busy attempts=10`。
  - 引擎状态里「剪贴板」显示「降级」，原因「剪贴板被其他程序占用」；状态行显示「部分功能降级：剪贴板」。
  - 平台能力里粘贴纯文本、带格式、文件、图片显示「降级」（`crates/platform/src/capabilities.rs:322、637-640`）。
  - P2 结束后，下一次粘贴成功，剪贴板状态恢复「正常」。不粘贴就不会自行恢复，需确认这是否符合预期。
- **通过标准**：链路上每一环（日志 → 引擎状态 → 状态行 → 平台能力）都能看到降级，并在下一次成功粘贴后恢复；没有改用键入。
- **出处**：
  - 租约记录健康状态：`clipboard.rs:186-216`；
  - 执行器探针：`crates/platform/src/windows/executor.rs:1350-1384`；
  - 能力表：`capabilities.rs:322、637-640`；
  - 合并记录：merge-log 第 525 行（剪贴板健康链）；
  - §5.5 #2。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（从失败到界面显示降级的延迟：____）

### M4-CB-08 故障注入 clipboard_busy

- **步骤**：
  1. 用 debug-fi 执行 `await inv('debug_inject_fault', { point: 'clipboard_busy', param: 5000 })`。
  2. 5 秒内在记事本执行 `zzp` + 空格。
  3. 在占用期间执行 `await inv('self_test')`，记下返回结果。
  4. 5 秒后再执行一次 `zzp`。
- **预期结果**：
  - 第 2 步的现象与 CB-07 相同：失败，原因是剪贴板被占用，不改用键入。
  - 第 3 步：这个注入点不影响自检探针，自检的剪贴板项通过。
  - 第 4 步粘贴成功，剪贴板状态恢复「正常」。
- **通过标准**：同 CB-07。
- **出处**：development-plan.md:2486；`clipboard.rs`（FAULT_BUSY_UNTIL）；`crates/platform/src/windows/self_test_probes.rs:139-152`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-CB-09（可选）延迟渲染和慢所有者

- **步骤**：
  1. 在 Excel 里复制一片单元格（Excel 会延迟渲染），然后在记事本执行 `zzp` + 空格，粘贴到 Excel 核对恢复结果。
  2. 再复制一次，**关闭 Excel**（选择保留剪贴板内容），然后执行 `zzp`。
  3. 检索日志里的 `clipboard.owner_hung`、`clipboard.read_timeout`、`clipboard.slow`。
- **预期结果**：
  - 没有出现超过 1 秒的卡顿。
  - 如果所有者渲染慢，就记为降级，并留下上面的日志之一。
  - 恢复后的内容可以粘贴回 Excel。
- **通过标准**：不卡死；慢的时候有日志，并且健康状态显示降级。
- **出处**：待实测 `clipboard.rs:41`（OWNER_PROBE_TIMEOUT 200 ms）、`clipboard.rs:738`（等待 WM_RENDERFORMAT 的时长）、`clipboard.rs:827`（慢但还活着的所有者被记为 hung）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

## 4. 监督与自愈（M4-SV，含故障注入）

**P5-A 时序常量**（核对用。恢复耗时的观察值记到对应条目的备注里）：

| 常量 | 值 | 位置 | 标注 |
|---|---|---|---|
| BACKOFF | 500 ms, 1 s, 2 s, 5 s, 15 s | `crates/platform/src/supervisor.rs:70` | |
| CIRCUIT_WINDOW / CIRCUIT_FAILURES | 10 分钟 / 5 次 | `supervisor.rs:78、80` | |
| TICK | 1 s | `supervisor.rs:81` | |
| VERIFY_TIMEOUT | 3 s | `supervisor.rs:85` | 待实测（:83） |
| STARTUP_GRACE | 10 s | `supervisor.rs:88` | 待基线确定（:87） |
| SUPERVISOR_STALE | 5 s | `supervisor.rs:93` | |
| ATTEMPT_TIMEOUT | 10 s | `supervisor.rs:103` | 待实测（:101） |
| WATCHDOG_INTERVAL / UNSUPERVISED_AFTER | 5 s / 3 次 | `crates/platform/src/supervisor_wiring.rs:42、109` | |
| STARTUP_HEALTHY_WAIT / HEALTH_POLL | 30 s / 100 ms | `crates/platform/src/self_test.rs:24、25` | |
| PROBE_WAIT / PROBE_POLL / CHECK_TIMEOUT | 1500 / 25 / 2500 ms | `self_test_probes.rs:15、16、18` | |
| MATCH_WINDOW / MISS_LIMIT | 500 ms / 5 | `crates/platform/src/windows/liveness.rs:33、37` | 待 0.4 基线确定（:30、:35） |
| MAX_STRIKE_SHIFT / REPEAT_RUN | 4 / 8 | `liveness.rs:46、55` | 待实测（:43、:52） |
| HOOK_QUEUE_CAPACITY / INSTALL_TIMEOUT | 1024 / 2 s | `hook.rs:35、565` | 待实测（:32） |
| RAW_QUEUE_CAPACITY / KEYBOARD_LAYOUT_CACHE_INTERVAL | 1024 / 0 | `crates/platform/src/windows/rawinput.rs:106、108` | 待实测（:101） |
| DETECTOR_START_TIMEOUT / DETECTOR_CLOSE_TIMEOUT | 3 s / 1 s | `rawinput.rs:118、120` | |
| IDLE_PARK / RESYNC_SECOND_PASS | 1 s / 200 ms | `input.rs:125、380` | |
| INJECTION_QUIET / MODIFIER_VERIFY_AFTER | 250 ms / 30 s | `keystate.rs:447、496` | 待实测（:444） |
| REGISTER_RETRY × REGISTER_ATTEMPTS | 5 s × 12 | `session.rs:226、228` | 待实测（:225） |
| OPEN_ATTEMPTS × 间隔 / LOCK_HOLD_BUDGET | 10 × 15 ms / 250 ms | `clipboard.rs:18、20、25` | |
| MAX_FORMAT_BYTES / OWNER_PROBE_TIMEOUT_MS | 32 MiB / 200 ms | `clipboard.rs:36、42` | 待实测（:41） |
| DEFAULT_RESTORE_DELAY | 300 ms | `traits.rs:206` | |
| STALL_BUDGET / SEND_LOCK_WAIT | 10 s / 250 ms | `executor.rs:185、189` | 待基线确定（:183） |
| LOCKED_RETRY | 5 s | `src-tauri/src/startup.rs:398` | |
| 加载重试 RETRY_DELAYS_MS | 100, 200, 400, 800 | `src-tauri/src/store/fallback.rs:29` | |
| CRASH_LOOP_THRESHOLD / CRASH_WINDOW / BOOT_ID_TOLERANCE | 2 次 / 5 分钟 / 10 s | `src-tauri/src/store/meta.rs:16、14、19` | |

熔断（10 分钟内失败 5 次进入「故障」，此时出现「修复引擎」按钮，见 `src/StatusArea.tsx:70-74`）很难在实机上稳定复现，由 L2 测试覆盖，本清单不单列。如果实测中意外遇到，在对应条目的备注里记录，并点一次「修复引擎」（提示「已开始修复引擎，结果会自动更新」）。

### M4-SV-01 空闲 10 分钟不误判

- **步骤**：
  1. 用 release 构建启动，状态行显示「监听中」后，不碰键盘鼠标 10 分钟（电脑不睡眠）。
  2. 然后在记事本执行 `zzk` + 空格。
  3. 检索这 10 分钟内的日志：`supervisor.recovering`、`input.check`、`executor.stalled`、`supervisor.respawn`、`supervisor.unsupervised`。
- **预期结果**：上述事件都没有出现（`input.check … result=registered` 这类例行信息除外）；展开成功。
- **通过标准**：空闲期间没有任何恢复或重挂动作。
- **出处**：`supervisor_wiring.rs:42、109`；`supervisor.rs:93`；liveness 常量的基线（`liveness.rs:30、35`）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-SV-02 executor_panic

- **步骤**：
  1. 用 debug-fi 执行 `await inv('debug_inject_fault', { point: 'executor_panic', param: 0 })`。
  2. 在记事本执行 `zzk` + 空格。
  3. 观察「运行状态与恢复」的引擎状态和「最近执行」。
  4. 5 秒后再执行一次 `zzk` + 空格。
  5. 检查数据目录里的 `crash-*.txt`，检索日志。
- **预期结果**：
  - 第一次执行失败，历史显示「失败 · 执行器内部错误」，没有输出任何字符（panic 发生在第一批 SendInput 之前）。
  - 引擎状态里执行器从「恢复中」回到「正常」。
  - 日志依次有 `executor.panic`、`app.panic`、`executor.rebuilt`、`supervisor.recovered`。
  - 生成了一份 `crash-*.txt`；进程没有退出。
  - 第二次执行成功。
- **通过标准**：自动恢复（不需要手动操作），5 秒内恢复正常，下一次触发成功。
- **出处**：development-plan.md:2483；`executor.rs:1528、1748-1751`；`supervisor.rs:70`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（恢复耗时：____）

### M4-SV-03 executor_hang（阻塞 200 秒）

- **步骤**：
  1. 用 debug-fi 执行 `await inv('debug_inject_fault', { point: 'executor_hang', param: 200000 })`。
  2. 在记事本执行 `zzk` + 空格，开始计时。
  3. 15 秒后再执行一次 `zzk` + 空格。
  4. 保持记事本在前台，等满 200 秒以上，观察有没有多出来的字符或退格。
  5. 检索日志。
- **预期结果**：
  - 约 10 秒后（STALL_BUDGET），日志依次有 `executor.stalled stalled_ms=… budget_ms=10000`、`executor.fenced epoch=… in_flight=…`、`executor.rebuilt`。
  - 第一次执行的历史为「失败 · 执行超时」。
  - 第 3 步展开成功。
  - 旧线程在第 200 秒醒来后不再发出任何输入，可能出现 `executor.retired`。
- **通过标准**：自动恢复；200 秒后记事本里没有多余的字符或退格；进程没有退出。
- **出处**：development-plan.md:2483；B-01（development-plan.md:1819）；`executor.rs:183-185、1230、1279、1448`。STALL_BUDGET 待基线确定。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（从卡住到重建完成的耗时：____）

### M4-SV-04 hook_panic

- **步骤**：
  1. 用 debug-fi 执行 `await inv('debug_inject_fault', { point: 'hook_panic', param: 0 })`。
  2. 在记事本输入 `x`。
  3. 再执行 `zzk` + 空格。
  4. 检索日志里的 `hook.panic`。
- **预期结果**：
  - 回调里的 panic 被捕获并计数，日志为 `hook.panic count=1`（`input.rs:357`）。
  - 键盘仍然可用，后续展开正常，不需要重装钩子。
- **通过标准**：日志有记录，后续输入和展开正常。
- **出处**：development-plan.md:2484；`hook.rs:220-243`；`input.rs:345-374`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：触发 panic 的那个 `x` 是否传给了记事本）____

### M4-SV-05 hook_unhook（钩子被静默摘除，靠交叉校验检出）

- **步骤**：
  1. 用 debug-fi 执行 `await inv('debug_inject_fault', { point: 'hook_unhook', param: 0 })`。
  2. 在记事本输入 12 个普通字母，例如 `abcdefghijkl`。
  3. 等状态行回到「监听中」后，执行 `zzk` + 空格。
  4. 变体 b：再布防一次，**不打字**，等 30 秒后检索日志，看是否不靠按键也能检出并重挂。
- **预期结果**：
  - 日志依次有 `fault.injected point="hook_unhook"`、`input.check source=keyboard_hook result=lost`、`input.reattach component=keyboard_hook … ok=true`（`input.rs:839-844`）。
  - 状态行可能短暂显示「正在恢复…」或「部分功能降级：…」。
  - 重挂后展开成功。
  - 摘除期间输入的字母不会触发展开。
- **通过标准**：由交叉校验检出并重建（退出标准原文要求）；不需要手动操作；连续按键不超过约 33 次就出结论（K=5、R=8，`liveness.rs:51`）。
- **出处**：development-plan.md:2448、2484；S-06；`liveness.rs:30-55`。待实测：`hook.rs:577`（替换钩子时正在途中的事件）、`hook.rs:773`（钩子线程忙时的 ping）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（检出需要的按键数：____；变体 b 是否自行重挂：____。需确认：S-06 的「10 秒内」在不打字时由谁保证）

### M4-SV-06 rawinput_unregister

- **步骤**：
  1. 用 debug-fi 执行 `await inv('debug_inject_fault', { point: 'rawinput_unregister', param: 0 })`。
  2. 在记事本输入 12 个字母。
  3. 执行 `zzk` + 空格。
  4. 检索日志。
- **预期结果**：
  - 日志依次有 `fault.injected point="rawinput_unregister"`、`input.check source=raw_input result=unregistered`、`input.reattach component=raw_input reason=raw_input_missing ok=true`（`input.rs:693-697、955-963`）。
  - 展开成功。
- **通过标准**：由交叉校验检出并重新注册；不需要手动操作。
- **出处**：development-plan.md:2448、2485；待实测 `rawinput.rs:933`（从不拥有窗口的线程注册，失败时退回到重建）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（走的是重新注册还是重建：____）

### M4-SV-07 rawinput_close

- **步骤**：
  1. 用 debug-fi 执行 `await inv('debug_inject_fault', { point: 'rawinput_close', param: 0 })`。
  2. 等 5 秒，输入几个字母，然后执行 `zzk` + 空格。
  3. 检索日志里的 `rawinput.closed`、`rawinput.rebuild_*`、`input.reattach`、`supervisor.recovered`。
- **预期结果**：日志有 `rawinput.closed`，随后检测窗口重建成功，没有 `rawinput.rebuild_stuck` 或 `rawinput.rebuild_failed`；展开成功。
- **通过标准**：自动恢复；记录重建耗时，即从 `rawinput.closed` 到恢复的时间差。
- **出处**：development-plan.md:2485；待实测 `rawinput.rs:970`（重建延迟）、`rawinput.rs:997`（关闭期间残留）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（重建耗时：____）

### M4-SV-08 长按自动重复、快速连按

- **步骤**：
  1. 在记事本里按住 `a` 5 秒，再按住 Backspace 5 秒。
  2. 在「控制面板 → 键盘」把重复速度调到最快、重复延迟调到最短，重复第 1 步；然后把重复速度调到最慢再做一次。
  3. 快速连按同一个键 50 次。
  4. 执行 `zzk` + 空格。
  5. 检索日志里的 `input.check`、`input.queue_full`。
  6. 把键盘设置还原。
- **预期结果**：没有 `input.check … result=lost` 误判，没有 `input.queue_full`；展开成功。
- **通过标准**：自动重复和连按都不会引起误判或重挂。
- **出处**：待实测 `liveness.rs:52`（自动重复每秒 2.5–30 次）、`liveness.rs:115`（连按五十次、丢失 key-up）、`hook.rs:32`、`rawinput.rs:101`（队列容量）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-SV-09 管理员窗口（只有一个输入源能看到）与按需自检

- **步骤**：
  1. 以管理员身份运行一个记事本，在里面输入 20 个字母和 `zzk` + 空格。
  2. 切回普通记事本，立即执行 `zzk` + 空格。
  3. 检索日志里的 `input.check`、`input.reattach`。
  4. 用 debug-fi 在开发者工具执行 `setTimeout(() => inv('self_test').then(r => console.log(JSON.stringify(r))), 5000)`，5 秒内把管理员记事本切到前台，等结果打印出来。
- **预期结果**：
  - 在管理员窗口里不展开。这是预期行为，管理员窗口的支持（S-20）不在 0.4。
  - 不应反复重挂。即使出现一次 `input.check … lost`，之后的重试间隔也会逐次加倍（MAX_STRIKE_SHIFT）。
  - 切回普通窗口后展开成功。
  - 第 4 步自检的键盘相关项是否误报失败，如实记录。
- **通过标准**：切回普通窗口后能立即展开；没有每 5 个键就重挂一次的循环；自检结果有记录。
- **出处**：待实测 `liveness.rs:43`（单侧来源）；`self_test_probes.rs:7`；merge-log 第 1270 行（TODO 22：管理员窗口在前台时自检误报）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：TODO 22 是否已修；第 4 步的自检结果：____）

### M4-SV-10 启动自检耗时、登录冷启动

- **步骤**：
  1. 在「文本指令参数设置」勾选「启动时自动开始监听」和「登录后自动启动」，点「保存」，再点「应用设置」。
  2. 热启动：退出程序再启动。记下日志里 `app.start` 与 `self_test.startup health=… auto_listen=… outcome=…` 之间的时间差，并确认 `self_test.finished passed=true`（这条日志只有通过与否和失败项，没有耗时字段，耗时用时间戳相减）。做 3 次。
  3. 冷启动：重启电脑并登录，记下从登录到托盘图标出现、到状态行显示「监听中」的时间。做 3 次。
  4. 检索日志里的 `self_test.check`、`session.registered`、`session.start_failed`。
- **预期结果**：
  - 两种启动下自检都通过，自动开始监听，没有出现「启动自检未通过」的通知或横幅。
  - 会话通知注册成功（`session.registered`）。
- **通过标准**：冷启动也不误报自检失败；耗时如实记录，与 PF-02 共用这些数据。
- **出处**：待实测 `src-tauri/src/startup.rs:294`（登录时冷启动的时序）、`session.rs:225`（开机早期注册）、`self_test_probes.rs:7`；STARTUP_GRACE 待基线确定（`supervisor.rs:87`）；`App.tsx:1424-1442`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（热启动 3 次：____；冷启动 3 次：____）

### M4-SV-11 启动自检失败（剪贴板被占用）

- **步骤**：
  1. 确认「启动时自动开始监听」已勾选，然后退出程序。
  2. 运行 P2，占用时间改为 40 秒。
  3. 在占用期间启动 release 构建，观察系统通知、横幅和状态行。
  4. P2 结束后点顶部的「恢复监听」，然后执行 `zzp` + 空格。
  5. 检索日志里的 `self_test.check`、`self_test.startup`。
- **预期结果**：
  - 系统通知标题「启动自检未通过」，正文「未通过：剪贴板。输入监听保持暂停，可从托盘菜单选择「恢复监听」，或打开管理界面查看引擎状态。」（`startup.rs:348-381`）。
  - 横幅显示「启动自检未通过：…，没有自动开始监听。…排除问题后可点「恢复监听」。」（`src/status.ts:146-210`），状态行显示「已暂停」。
  - 恢复后粘贴成功。
- **通过标准**：自检失败时不自动开始监听，并且有明确提示；恢复后功能正常。
- **出处**：B-10；`self_test_probes.rs:139-152`；`App.tsx:1429`（提示文字说明全部通过才开始监听）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（横幅里列出的检查项名称：____，对照 §12.3 第 7 条）

### M4-SV-12 修复过期的自启动项（只用 release）

- **步骤**：
  1. 确认「登录后自动启动」已勾选，然后退出程序。
  2. 执行：

     ```powershell
     reg add HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v QuickerText /t REG_SZ /d "\"C:\old\quicker-text.exe\" --background" /f
     ```

  3. 手动启动 release 构建，观察横幅，检索日志。
  4. 执行 `reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v QuickerText`。
- **预期结果**：
  - 横幅显示「登录自动启动项指向的程序位置已过期，已自动修正。」。
  - 日志有 `autostart.repair`。
  - 注册表的值改为当前 exe 的路径加 `--background`。
- **通过标准**：自动修正并有提示。
- **出处**：development-plan.md:2488；`src-tauri/src/autostart.rs:12-14、90-115`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-SV-13 真实注销或重启不算崩溃

- **步骤**：
  1. release 构建在运行（「登录后自动启动」已勾选）。在「开始」菜单选择注销，再登录。
  2. 5 分钟内再注销、登录一次。
  3. 再选择「重启」一次。
  4. 每次登录后观察有没有安全模式横幅，检索上一份日志末尾和本次启动的日志。
- **预期结果**：
  - 每次关机前的日志末尾有 `store.run_marker_cleared`（真实注销会经由 WM_ENDSESSION 进入 RunEvent::Exit，`startup.rs:384-395`）。
  - 启动后没有 `store.crash_loop`，没有进入「连续多次启动失败」的安全模式。
- **通过标准**：连续两次注销加一次重启之后仍然正常启动。
- **出处**：development-plan.md:2489；`meta.rs:14-19`；待实测 `session.rs:231`（隐藏窗口能否收到两条消息、关机时间预算）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-SV-14 故障注入 session_end

- **步骤**：
  1. 用 debug-fi 执行 `await inv('debug_inject_fault', { point: 'session_end', param: 0 })`。
  2. 记录界面和进程的状态。
  3. 按计划 §11 用 `taskkill /F /IM quicker-text.exe` 结束进程，然后连续启动两次（第一次启动后正常退出，再启动第二次）。
  4. 检索日志里的 `session.end`、`store.crash_loop`、`store.run_marker_cleared`。
- **预期结果**：
  - 日志有 `fault.injected point="session_end"` 和 `session.end source=fault callbacks=N`（`session.rs:169、216-221`）。
  - 计划预期之后连续启动两次不会被误判为崩溃循环。
- **通过标准**：按计划，两次启动都正常，没有进入 crash_loop。
- **出处**：development-plan.md:2489；`input.rs:997`；`session.rs:216-221`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：这个注入点只调用会话结束回调，不走 RunEvent::Exit，因此**不清除运行标记**；随后 taskkill 会被算作一次崩溃。真实注销由 SV-13 覆盖，两者差异见 §12.3。注入后进程是否继续监听：____）

## 5. 安全模式与存储（M4-ST）

### M4-ST-01 配置被锁定进入安全模式，修订号大于 0

- **步骤**：
  1. 用 release 构建，在已备份的数据目录上操作，确认已至少「应用设置」两次（「当前生效 v{N}」，N ≥ 2）。记下规则条数，然后退出。
  2. 用 P1 锁定 `active.json`，保持窗口不关。
  3. 启动 Quicker Text，观察横幅、状态行、规则列表和各写入入口，持续约 5 秒以上：
     - 「＋ 添加指令」；
     - 编辑后「保存」；
     - 批量启用 / 停用；
     - 「应用设置」；
     - 「放弃未应用草稿」；
     - 「恢复上一个版本」；
     - 「登录后自动启动」。
  4. 在 P1 窗口按回车释放锁。
  5. 观察 5 秒内界面的变化，然后点「恢复监听」。
  6. 编辑一条规则的正文，点「保存」，再点「应用设置」，然后核对规则条数。
- **预期结果**：
  - 第 3 步：
    - 横幅显示「已进入安全模式：配置文件被其他程序占用。输入监听已暂停，设置暂时不能修改；可以导入配置或打开数据目录检查。」；
    - 状态行显示「已暂停（安全模式）」；
    - 写入入口不可用。「应用设置」仍可点击，但会提示「当前处于安全模式（配置文件被其他程序占用），暂时不能修改设置。」；
    - 「启动时自动开始监听」仍可操作（`App.tsx:1432-1434`）；
    - 日志有 `store.read_failed` 和 `store.safe_mode reason=Locked`（原因按 Debug 形式记录，`src-tauri/src/store/mod.rs:241`）。
  - 第 5 步（锁释放后 5 秒内，LOCKED_RETRY）：
    - 日志有 `store.locked_recovered outcome=…` 和 `store.safe_mode_exit`；
    - 横幅消失，规则列表换成磁盘上的真实规则，「当前生效 v{N}」；
    - 读取过程中界面可能短暂提示「安全模式状态已变化，正在重新读取当前配置；读取完成前暂时不能修改。」；
    - 监听仍然暂停，要手动「恢复监听」（退出安全模式只清除 SAFE_MODE，不清除 MANUAL，见 `src-tauri/src/store/mod.rs:228-258`）。例外：如果锁在启动自检作出决定**之前**就释放，会像正常启动一样自动开始监听（`src-tauri/src/store/startup_tests.rs:1496-1536`，review A6-1）。锁保持 5 秒以上时，通常是自检先作出决定。
  - 第 6 步生效版本变成 v{N+1}，规则条数与第 1 步相同，只有被编辑的那一条变了。
- **通过标准**：
  - 锁定期间没有任何写入成功，也不会回退到旧版本；
  - 释放后自动读回真实规则；
  - 之后保存不丢规则（I6）。
- **出处**：development-plan.md:2487（store_locked 预期进入 SafeMode(Locked)，不回退）；`fallback.rs:121-233`；`startup.rs:398`；提交 `576aa5a`、`d611a8b`（I6、TODO 20）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（从释放到界面换成真实规则的耗时：____）

### M4-ST-02 配置被锁定进入安全模式，修订号为 0（I6）

- **步骤**：
  1. 用全新的数据目录：
     - release：退出程序，把 `%APPDATA%\com.quickertext.desktop` 改名备份；
     - 或者用 debug-fi，设 `QUICKER_TEXT_DATA_DIR=D:\qt-rev0`。
  2. 启动，**不要点「应用设置」**。用「导入」把 `qt-0.4-rules.json` 导入为草稿，或手动添加 3 条规则并「保存」到草稿。确认「当前生效 v0」，然后退出。
  3. 用 P1 锁定 `draft.json`，然后启动程序，观察约 5 秒。
  4. 释放锁，观察界面。
- **预期结果**：
  - 锁定期间进入 Locked 安全模式，界面显示的是占位配置（修订号 0）。
  - 释放后，**修订号同样是 0**，界面仍会重新读取，规则列表显示第 2 步保存到草稿的那些规则，不会停在空列表。
  - 写入入口在读取完成后恢复可用。
- **通过标准**：修订号相同的情况下也能读回真实草稿；之后「保存」不丢规则。
- **出处**：提交 `576aa5a`（安全模式变化而修订号不变时重新读取 store）；`src-tauri/src/store/load.rs:70-116`（锁释放后重读并安装）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-ST-03（可选）锁定期间导出诊断包

- **步骤**：在 ST-01 第 3 步的锁定期间，导出诊断包（不包含短语内容），解压后查看 `config-summary.json`。
- **预期结果**：`active.file.status` 为 `unreadable`，其中 `os_code` 为 32（共享冲突）。导出没有卡死。
- **通过标准**：诊断包可以导出，状态描述正确。
- **出处**：`bundle.rs:476-479、618-625`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-ST-04 锁定期间导入

- **步骤**：在 ST-01 第 3 步的锁定期间点「导入」，选择 `qt-0.4-rules.json`，选择「保留现有」，看到「导入预览 · 确认后写入草稿」后点「确认导入草稿」。
- **预期结果**：
  - 预览能正常显示。
  - 确认后报错「当前处于安全模式（配置文件被其他程序占用），暂时不能修改设置。」，没有写入任何文件，对话框保持打开。
- **通过标准**：Locked 状态下不写入；提示明确。
- **出处**：`src/App.tsx:899-922`；`src-tauri/src/store/mod.rs:85-94`（Locked 和 NewerSchema 下导入只能预览、不能写入）、`mod.rs:290-299`；`src/ipc/errors.ts`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（横幅写着「可以导入配置」，但 Locked 下导入按设计会被拒绝，见 §12.3 第 6 条。需确认：横幅文字是否要按原因区分）

### M4-ST-05 截断 active.json：诊断包显示 eof

- **步骤**：
  1. 在已备份的数据目录上，程序运行中执行 P3 截断 `active.json`。
  2. 不重启，直接导出诊断包（不包含短语内容），解压后查看 `config-summary.json`。
- **预期结果**：`active.file.status` 为 `unparsable`，其中 `category` 为 `config_malformed`，`kind` 为 `eof`，并带 `line` 和 `column`。
- **通过标准**：诊断包和日志对损坏的判断一致（与 ST-06 中 `store.corrupt` 的 kind、行号、列号相同）。
- **出处**：提交 `a33d16a`（区分 eof / syntax / utf8 / shape）；`bundle.rs:480-490`；`src-tauri/src/store/fallback.rs:410-413`（`store.corrupt`）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（line / column：____）

### M4-ST-06 截断后重启：隔离并恢复

- **步骤**：
  1. 接着 ST-05，退出程序再启动。
  2. 观察横幅，查看数据目录，检索日志。
  3. 再退出、启动一次，观察横幅是否再次出现。
  4. 点一次「应用设置」，然后再重启。
- **预期结果**：
  - 日志依次有：
    - `store.corrupt file=active.json code=config_malformed kind=eof line=… column=…`；
    - `store.quarantine file=active.json quarantined_as=…`；
    - `store.recovered file=… from=… revision=…`。
  - 数据目录里出现 `active.corrupt-<时间>.json`。
  - 横幅显示「配置文件无法读取，已自动从历史版本 v{n} 恢复。请检查最近的改动是否还在。」和「active.json 已损坏，已改名为 … 保留，可在数据目录中查看。」。
  - 草稿不受影响。
  - 第 3 步恢复横幅再次出现，因为恢复时不重写 `active.json`。第 4 步之后不再出现。
- **通过标准**：
  - 不进入安全模式，自动恢复到最近的可用版本；
  - 损坏的文件被保留；
  - 记下恢复到的版本号。
- **出处**：development-plan.md:2487（config_corrupt）；`fallback.rs:121-233`（候选顺序：active → LKG → revisions 从新到旧 → previous）；`src-tauri/src/store/revisions.rs:30-69`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（恢复到 v____，应用前的生效版本为 v____。需确认：是否存在 `revisions/N.json`，使恢复能回到 vN 而不是 v(N-1)）

### M4-ST-07 带 BOM 的 prefs.json、active.json、draft.json

- **步骤**：
  1. 退出程序，执行 P4（给三个文件加 BOM）。
  2. 启动，观察横幅，检索日志里的 `store.corrupt`、`store.quarantine`、`store.prefs_invalid`、`clipboard.options_set`。
  3. 导出诊断包，查看 `config-summary.json`。
- **预期结果**：
  - 正常启动，没有隔离、没有横幅。
  - `clipboard.options_set` 的取值与 `prefs.json` 一致。
  - 诊断包里 `active.file.status` 和 `draft.file.status` 为 `ok`，`active.counts` 和 `draft.counts` 与界面上的条数一致，`prefs.status` 为 `ok`。
- **通过标准**：BOM 不被当作损坏；诊断包显示 ok 并带条数。
- **出处**：`bundle.rs:1340-1411`；`src-tauri/src/store/prefs.rs`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-ST-08 来自更新版本的配置（NewerSchema）

- **步骤**：
  1. 退出程序，用 P10 记下哈希。
  2. 用记事本把 `active.json` 顶层的 `"version": 1` 改成 `"version": 2`，保存，然后启动。
  3. 退出后再用 P10 核对哈希，把版本改回 1，再启动。
- **预期结果**：
  - 日志有 `store.newer_schema` 和 `store.safe_mode reason=NewerSchema`。
  - 横幅显示「已进入安全模式：配置文件来自更新的版本。…」，处于只读状态，不写运行标记。
  - `active.json` 的哈希与第 2 步改完后的一致，没有被改写。
  - 改回 1 之后正常启动。
- **通过标准**：不改写更新版本的文件；提示明确。
- **出处**：B-04；`fallback.rs`；`src-tauri/src/store/mod.rs:200-289`（只读安全模式）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-ST-09 崩溃循环（crash_loop）

- **步骤**：
  1. 启动 release 构建，等窗口出现后执行 `taskkill /F /IM quicker-text.exe`。
  2. 1 分钟内再启动一次，同样 taskkill。
  3. 第三次启动，观察。
  4. 从托盘「退出」（正常退出），然后再启动。
- **预期结果**：
  - 第 3 步：日志有 `store.crash_loop crash_count=2` 和 `store.safe_mode reason=CrashLoop`；横幅显示「已进入安全模式：连续多次启动失败。…」；「启动时自动开始监听」可以操作。
  - 第 4 步正常启动。
- **通过标准**：5 分钟内连续崩溃 2 次后进入安全模式；正常退出一次即可解除。
- **出处**：`meta.rs:14-19`；B-04。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-ST-10 没有可用的配置（NoValidConfig），用导入解除

- **步骤**：
  1. 退出程序，把 `active.json`、`previous.json` 和 `revisions\` 下所有 `<n>.json`（不含 `.metadata.json`）的内容都改成 `{`。
  2. 启动，观察。
  3. 点「导入」，选择 `qt-0.4-rules.json`，选择「替换全部」，点「确认导入草稿」。
  4. 观察横幅，然后点底部「应用设置」。
  5. 观察横幅、规则列表和监听状态。
- **预期结果**：
  - 第 2 步：所有损坏的文件都被隔离；日志有 `store.safe_mode reason=NoValidConfig`；横幅显示「已进入安全模式：没有找到可用的配置。…」。
  - 第 3 步：导入写入草稿，提示「导入草稿已保存，请应用设置」，此时仍在安全模式。
  - 第 4、5 步：「应用设置」在安全模式下仍可点击。发布成功后日志有 `store.safe_mode_import`，安全模式解除，规则出现。监听仍然暂停，需要点「恢复监听」。
- **通过标准**：导入加应用可以解除安全模式，且规则完整。
- **出处**：`src-tauri/src/store/mod.rs:85-94`（只有 CrashLoop 和 NoValidConfig 能用导入解除）、`mod.rs:290-299`；`src-tauri/src/store/revisions.rs:20-31、110-113`；`src/App.tsx:528-532`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-ST-11 prefs.json 损坏

- **步骤**：退出程序，把 `prefs.json` 的内容改成 `{"app":`，然后启动。
- **预期结果**：
  - 日志有 `store.quarantine file=prefs.json to=… reason=… line=… column=…`。
  - 数据目录里出现 `prefs.corrupt-<时间>.json`，程序使用默认偏好正常启动，不进入安全模式。
- **通过标准**：自动隔离并使用默认值。
- **出处**：`src-tauri/src/store/prefs.rs`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：是否显示 config_quarantined 横幅）

## 6. 前端状态一致性（M4-FE）

### M4-FE-01 WebView2 连按两次 Esc，没有弹窗凭空消失

背景：Chromium 中，两次 Esc 之间没有用户激活时，第二次 Esc 会强制关闭 `<dialog>`。`src/Modal.tsx` 的处理是：在 `cancel` 事件里 preventDefault，强制关闭时重新调用 `showModal()`，再交给弹窗自己的 onClose 决定怎么处理。

- **步骤**：对子表每一行，打开对应的弹窗，**快速连按两次 Esc**（中间不点击），再试一次连按三次。
- **预期结果**：见子表。核心是：不会出现弹窗消失了，状态却没有按「取消」处理的情况。
- **通过标准**：子表全部符合。
- **出处**：`src/Modal.tsx`；`App.tsx:733-816、1862-1911`；提交 `72804aa`（review C10）。
- **子表**：

| 弹窗 | 打开方式 | 连按 Esc 后的预期 | 实际 | 判定 |
|---|---|---|---|---|
| 编辑文本指令（有未保存的改动） | 改动「操作正文」后不保存 | 弹出「放弃未保存的改动？」，编辑器仍在，改动还在。在询问框上按 Esc 等于取消，回到编辑器 | | |
| 编辑文本指令（没有改动） | 直接打开 | 正常关闭（需确认） | | |
| 「遇到重复的指令时怎么处理？」 | 「导入」后选好文件 | 视为取消，不导入 | | |
| 「导入预览 · 确认后写入草稿」（空闲时） | 导入到预览这一步 | 正常关闭，没有写入 | | |
| 「导入预览」（保存进行中） | 见 FE-02 | 弹窗不关，Esc 被忽略 | | |
| 「窗口信息」 | 点「选择当前窗口加入黑名单」 | 正常关闭，黑名单不变 | | |
| 「导出诊断包？」 | 点「导出诊断包」 | 视为取消，不导出 | | |
| 「放弃全部未应用草稿？」 | 点「放弃未应用草稿」 | 视为取消，草稿还在 | | |
| 「回滚到上一个生效版本？」 | 点「恢复上一个版本」 | 视为取消，生效版本不变 | | |
| 「退出 Quicker Text？」 | 点「退出全部」 | 视为取消，程序继续运行 | | |

- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-FE-02 导入草稿保存进行中，对话框不能关闭

- **步骤**：
  1. 用 P8 生成 `qt-bulk-5000.json`。
  2. 点「导入」，选择该文件，选择「保留两份」，在预览中点「确认导入草稿」。
  3. 立即连按 Esc 两次，并点「关闭」和「✕」。
- **预期结果**：
  - 保存期间「关闭」和「✕」不可用，Esc 被忽略，对话框保持打开。
  - 保存完成后提示「导入草稿已保存，请应用设置」。
- **通过标准**：保存完成前对话框不消失；没有重复导入（草稿条数等于原条数加 5000，只加一次）。
- **出处**：`src/App.tsx:923-925、1870、1905`（`importing` 期间忽略关闭）；提交 `72804aa`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：保存耗时是否足够长，能观察到这个窗口期。如果太快，以 FE-03 为准）

### M4-FE-03 导入草稿保存失败：对话框保留，可以重试

- **步骤**：
  1. 用 P1 锁定 `draft.json`。
  2. 「导入」一个小文件，然后点「确认导入草稿」。
  3. 释放锁，在同一个对话框里再点一次「确认导入草稿」。
- **预期结果**：
  - 第 2 步立即报错「写入文件失败：draft.json（系统错误 32）」（原子替换用 MoveFileExW，不重试），对话框和预览保持不变。
  - 第 3 步成功，草稿只增加一次。
- **通过标准**：失败后不丢预览、不关对话框；重试后不重复写入。
- **出处**：`src-tauri/src/store/atomic.rs`；`src/ipc/errors.ts`（io 错误文案）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（实际的错误码：____）

### M4-FE-04 外部修改配置文件：revision_conflict

- **步骤**：
  1. 程序运行中，用记事本打开 `active.json`，把某条规则的 `description` 改一个字，保存（保持 JSON 合法）。
  2. 在 Quicker Text 里编辑另一条规则，点「保存」，再点「应用设置」。
  3. 点「刷新状态」或重启后再应用一次。
- **预期结果**：
  - 第 2 步报错「设置已在别处更新（当前版本 N），请刷新后再试。」，`active.json` 保留外部修改，监听状态不变。
  - 第 3 步重新读取后能应用成功。
- **通过标准**：不覆盖外部修改；提示明确。
- **出处**：`revisions.rs:30-69`（磁盘哈希不一致时返回 RevisionConflict）；`src/ipc/errors.ts`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-FE-05 安全模式下的写入入口

- **步骤**：
  1. 用 ST-09 的方法进入 crash_loop 安全模式。
  2. 逐个点击以下入口，记录是不可用还是报错：
     - 「＋ 添加指令」；
     - 编辑后「保存」；
     - 批量启用 / 停用；
     - 「放弃未应用草稿」；
     - 「恢复上一个版本」；
     - 「登录后自动启动」；
     - 「应用设置」（没有导入过草稿时）；
     - 「启动时自动开始监听」；
     - 「导入」；
     - 「导出」；
     - 「导出诊断包」；
     - 「打开日志目录」。
- **预期结果**：
  - 前六项不可用，或报错「当前处于安全模式（连续多次启动失败），暂时不能修改设置。」。
  - 「应用设置」可以点击（它是导入后解除安全模式的出口），但没有导入过的草稿时，后端会以同样的安全模式错误拒绝。
  - 「启动时自动开始监听」、「导入」、「导出」（CrashLoop 下允许）、「导出诊断包」和「打开日志目录」可以使用。
- **通过标准**：没有任何写入绕过安全模式；可用的入口与横幅文字一致。
- **出处**：`src/App.tsx:528-541`（ipc-contract §4）、`App.tsx:1432-1434`（ipc-contract §5）；`src-tauri/src/store/revisions.rs:27-31`；`src-tauri/src/store/export.rs:43`；`src/status.ts:146-210`；`src/ipc/errors.ts`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-FE-06 事件推送的时延

- **步骤**：
  1. 把 Quicker Text 窗口和记事本并排放置，「运行状态与恢复」页保持可见。
  2. 在记事本执行 `zzk` + 空格，看「最近执行」多久出现新的一行（不点「刷新状态」）。
  3. 从托盘菜单暂停再恢复，看顶部按钮「暂停监听 / 恢复监听」和状态行的切换。
  4. 用 debug-fi 注入 `hook_unhook` 并输入字母，看状态行的变化顺序。
- **预期结果**：
  - 第 2、3 步的界面更新都在约 200 毫秒内（状态合并窗口是 100 ms）。
  - 第 4 步状态行出现「正在恢复…」或「部分功能降级」，最后回到「监听中」。
- **通过标准**：界面不需要手动刷新就能跟上后端状态；肉眼看不到超过 1 秒的延迟。
- **出处**：performance-baseline.md:62（后端事件到界面 ≤ 200ms）；`crates/platform/src/events.rs`（STATE_MERGE 100 ms、HEALTH_RESEND 5 s）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-FE-07 平台能力表在窗口获得焦点时刷新

- **步骤**：
  1. 在 CB-07 的占用期间，执行失败的 `zzp` 之后，先不切回 Quicker Text，等 5 秒。
  2. 然后单击 Quicker Text 窗口让它获得焦点，看「平台能力」表。
  3. 如果没有变化，点「刷新状态」。
- **预期结果**：窗口获得焦点后，平台能力表更新为粘贴类「降级」。
- **通过标准**：窗口获得焦点或点「刷新状态」后，平台能力表与引擎状态一致。
- **出处**：`src/RuntimeStatus.tsx`；`capabilities.rs:637-640`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

## 7. 隐私（M4-PR）

### M4-PR-01 哨兵执行 100 次后检索日志和崩溃记录

- **步骤**：
  1. 用 release 构建建好 `zzs`、`zzt`，「应用设置」。
  2. 在记事本执行 `zzs` + 空格和 `zzt` + 空格各 50 次。中间穿插 5 次 CB-07 的占用失败，再在记事本手动输入一次 `QTSENTINEL-7F3A9C-隐私哨兵`。
  3. 用 debug-fi（同样建好规则）布防 `executor_panic`，然后执行 `zzs`，生成一份崩溃记录。
  4. 两个构建分别运行 P9 的日志和崩溃记录部分。
- **预期结果**：P9 没有任何命中。
- **通过标准**：日志和 `crash-*.txt` 中零命中。
- **出处**：development-plan.md:2491（S-08）；`crates/platform/src/history.rs:4-7`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（日志行数、崩溃记录份数：____）

### M4-PR-02 诊断包（不包含短语内容）

- **步骤**：接着 PR-01，导出诊断包并选「不包含短语内容（推荐）」，保存为 `D:\qt-diag.zip`，用 P9 解压后检索。
- **预期结果**：零命中。包里没有 `active.json`。
- **通过标准**：零命中。
- **出处**：development-plan.md:2491；`bundle.rs:8-11、247-249`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-PR-03 诊断包（包含短语内容）的范围

- **步骤**：再导出一份诊断包，选「包含短语内容」，解压后用 P9 检索。
- **预期结果**：只有 `active.json` 里有哨兵（这是用户选择包含短语内容的结果）；`logs/`、`crashes/`、`health.json`、`config-summary.json`、`system.json`、`manifest.json` 都没有命中。
- **通过标准**：命中范围只限于 `active.json`。
- **出处**：`bundle.rs:247-249`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-PR-04 执行历史和界面

- **步骤**：
  1. 查看「最近执行」的各列（时间、指令、方式、程序、结果、耗时）。
  2. 展开「详细运行信息（高级）」。
  3. 在 debug-fi 执行 `JSON.stringify(await inv('list_history', { since_seq: null })).includes('QTSENTINEL')`。
- **预期结果**：
  - 任何地方都不出现哨兵正文或窗口标题，只有程序名。
  - 第 3 步返回 `false`。
- **通过标准**：执行历史中零命中。
- **出处**：development-plan.md:2491；`history.rs:4-7`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（测完删除 `zzs`、`zzt` 并「应用设置」）

## 8. 数据兼容：0.3.1 与 0.4 互读（M4-CP）

### M4-CP-01 0.3.1 升级到 0.4 无损

- **步骤**：
  1. 在装有 0.3.1 并有真实规则（或一份复制来的用户数据）的机器上，退出 0.3.1，备份数据目录，用 P10 记下哈希，记下界面上的规则条数和草稿状态。
  2. 换成 0.4 release 构建并启动。
  3. 观察横幅，检索 `store.` 开头的日志，核对条数。
  4. 抽 5 条不同操作类型的规则实际执行一遍。
  5. 不应用，退出后再用 P10 核对哈希。
- **预期结果**：
  - 没有进入安全模式，没有隔离，没有 `store.corrupt`。
  - 条数相同，抽测的规则都能执行。
  - `active.json`、`draft.json` 的哈希不变。加载时会新写 `revisions\<n>.metadata.json`，这是新增文件，可以接受。
- **通过标准**：无损加载（退出标准原文）。
- **出处**：development-plan.md:2448；§9（development-plan.md:2339-2340）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（规则条数：0.3.1 ____ / 0.4 ____）

### M4-CP-02 0.4 写入的数据 0.3.1 仍能读取

- **步骤**：
  1. 接着 CP-01，在 0.4 中编辑一条规则，点「应用设置」，修改一项参数，然后退出。
  2. 安装或运行 0.3.1，启动，核对条数和被编辑的那一条，并执行一次。
- **预期结果**：0.3.1 正常加载，看得到 0.4 的修改，忽略 0.4 新增的文件（`store-meta.json`、`revisions\*.metadata.json`、`prefs.json` 中的新键）。
- **通过标准**：0.3.1 可以读取（退出标准原文）。
- **出处**：development-plan.md:2448、2340。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-CP-03 保留未知字段和读不懂的规则

- **步骤**：
  1. 退出程序，在 `active.json` 的顶层 `config` 对象里加入 `"x_acceptance_future": {"keep": true}`，并在某条规则对象里加入 `"x_rule_future": 1`。
  2. 另复制一条规则，把它的 `action.type` 改成 `"future_action"`，`id` 改成唯一值。
  3. 启动，看界面怎样显示这条读不懂的规则。
  4. 编辑**另一条**规则，「应用设置」，然后用记事本打开新的 `active.json` 检索这三处。
- **预期结果**：
  - 正常启动。
  - 三处内容原样保留在新的 `active.json` 里。存储格式保留未知字段（B-43），读不懂的规则原样存放在 `unreadable_rules` 中。
- **通过标准**：没有丢失。
- **出处**：`crates/domain/src/model.rs:50-63`；`crates/domain/src/stored.rs:115`；`package.rs:160`（导入是严格模式，与存储不同）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（界面上如何显示读不懂的规则：____）

### M4-CP-04 跨版本首次写入前的备份 backup-pre-0.4

- **步骤**：在 CP-01 的数据目录上，0.4 第一次「应用设置」之后，查看数据目录里有没有 `backup-pre-0.4\`。
- **预期结果**：按计划 §9 应该有这个目录，包含 `active.json`、`draft.json`、`previous.json` 和 `revisions\` 中的相关文件。
- **通过标准**：按计划判定。**预计不通过**：代码里没有找到这项实现（§12.3）。
- **出处**：development-plan.md:1805、2339。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

## 9. 性能与浸泡（M4-PF）

测量规则（performance-baseline.md §2、§5）：
- 一律用 release 构建，每项至少测 3 轮；
- 采样程序不要和输入延迟测量同时运行（performance-baseline.md:73）；
- 每次测量按 performance-baseline.md §5 的模板另起一份记录，并在 §6 追加一行。

### M4-PF-01 触发到开始输出的 p95（基线，1.0 的目标是 < 100 ms）

- **步骤**：
  1. 方法 A（各阶段计时）：
     1. 用 debug-fi 构建（结论里注明是调试构建，数值偏大）。
     2. 在记事本执行 `zzk` + 空格 100 次（每批不超过 200 条，30 分钟内取完）。
     3. 在开发者工具执行：

        ```js
        const h = await inv('list_history', { since_seq: null });
        const xs = h.map(e => ((e.timings.queue_us ?? 0) + (e.timings.resolve_us ?? 0)) / 1000).sort((a, b) => a - b);
        const q = p => xs[Math.min(xs.length - 1, Math.floor(p * xs.length))];
        ({ n: xs.length, p50: q(0.5), p95: q(0.95), max: xs[xs.length - 1] })
        ```

  2. 方法 B：在 release 构建中执行同样的 100 次，记下「最近执行」耗时列（即 total_us，包含退格、输出和恢复）的 p50 和 p95，作为上界。
  3. 可选方法 C：用 240 fps 录屏，从终止键按下到第一个字符变化逐帧计数。
- **预期结果**：得到三组数字中的至少两组；p95 低于 100 ms 记为达到 1.0 目标，否则只记作基线。
- **通过标准**：0.4 只要求「有数据」（performance-baseline.md:43），所以完成测量并存档即为通过。
- **出处**：performance-baseline.md:43；`crates/platform/src/health.rs:238-245`（StageTimings）；`executor.rs:412-437`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（A：p50 ____ / p95 ____；B：p50 ____ / p95 ____。需确认：从钩子到入队的时间不在 queue_us 里；release 构建没有开发者工具，`get_health` 也还没有 p50/p95 汇总（diagnostics.rs:506 是 TODO））

### M4-PF-02 启动到「正常」< 2 s

- **步骤**：
  1. 热启动 3 次：取日志里 `app.start` 与 `self_test.startup health=healthy` 的时间差。
  2. 冷启动（重启后首次启动）3 次：同样记录。
  3. 可以复用 SV-10 的数据。
- **预期结果**：热启动少于 2 s；冷启动如实记录。
- **通过标准**：完成测量；热启动超过 2 s 时在备注里说明原因。
- **出处**：performance-baseline.md:46；`self_test.rs:24`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（热：____；冷：____）

### M4-PF-03 冷启动到主窗口显示 ≤ 1.5 s

- **步骤**：重启后，手动双击启动（不带 `--background`），用录屏计算从双击到主窗口首次绘制完成的时间。测 3 轮。
- **预期结果**：记录数值。
- **通过标准**：完成测量。
- **出处**：performance-baseline.md:66。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-PF-04 空闲时的 CPU 和内存

- **步骤**：
  1. 程序常驻托盘，窗口隐藏，5 分钟不输入。
  2. 运行 performance-baseline.md:71 的 Get-Counter 命令（每 5 秒一次，共 60 次）。
  3. 同时记下本程序创建的 `msedgewebview2.exe` 合计 CPU。
- **预期结果**：主进程平均 CPU 低于 0.5%（除以逻辑核数之后）；WebView2 合计低于 0.5%。
- **通过标准**：完成测量；超出预算时记录并上报。
- **出处**：performance-baseline.md:50、52、71。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-PF-05 2 小时浸泡

- **步骤**：
  1. 在虚拟机 B 上用 release 构建，以验收模式启动：

     ```powershell
     $env:QUICKER_TEXT_TEST_INPUT = '1'
     $env:QUICKER_TEXT_DATA_DIR = 'D:\qt-soak'
     & .\quicker-text.exe --acceptance-test-input
     ```

     导入规则集（含 `zzq`、`zzv`），「应用设置」，确认「详细运行信息（高级）」显示 `explicit_test_mode`。
  2. 打开一个空白记事本，保存为 `D:\soak.txt`，让它保持在前台。
  3. 另开 PowerShell 运行 P7（采样），再运行 P11（负载）。
  4. 每 10 分钟手动做一次：
     - 暂停再恢复（托盘）；
     - `Set-Clipboard -Value "soak $(Get-Date)"`；
     - 每 20 分钟对「最近执行」截图一次（历史只保留 30 分钟）。
  5. 结束后保存 `soak.txt`，统计数据：

     ```powershell
     $t = [IO.File]::ReadAllText('D:\soak.txt')
     ([regex]::Matches($t, '\[QT-SOAK-OK\]')).Count; ([regex]::Matches($t, '\[QT-SOAK-PASTE\]')).Count
     ([regex]::Matches($t, '\bzz[qv]\b')).Count      # 残留的缩写，等于失败次数
     (Get-Content D:\qt-soak-sent.log).Count          # 发送的触发次数
     ```

  6. 检索日志中 error 的条数，以及 `supervisor.circuit_open`、`input.queue_full`、`executor.stalled`。
  7. 把结果填进 performance-baseline.md §4 的记录表。
- **预期结果**：
  - 发送次数 = 成功次数 + 残留次数，每次残留都能在截图的历史或日志里找到带原因的失败。
  - 句柄数、线程数不是单调增长。
  - 结束时健康状态为「正常」。
  - 记下 Private Bytes 从开始到结束的增长（1.0 要求 < 20 MB，0.4 只记录）。
- **通过标准**：没有静默失效（退出标准原文）；句柄数和线程数不单调增长；结束时状态「正常」。
- **出处**：development-plan.md:2448；performance-baseline.md:76-89；[windows-acceptance-mode.md](windows-acceptance-mode.md)。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（触发 ____ / 成功 ____ / 失败 ____ / 静默 ____；Private Bytes ____ → ____；句柄 ____ → ____；线程 ____ → ____。需确认：`scripts/soak/` 不存在，P11 是临时替代；失败不一定都写日志，历史只保留 200 条或 30 分钟，靠截图补足；外部模拟输入不能代表实体键盘路径）

## 10. S-29 spike（M4-SP）

所有 spike 只在虚拟机 B 上运行。可执行文件在宿主机上用 `cargo build -p quicker-platform --examples` 生成，再复制进虚拟机（s-29.md:91）。

### M4-SP-01 焦点获取与归还 T-01 ～ T-08

- **步骤**：
  1. 在虚拟机里打开记事本、Word、Chrome。
  2. 按 s-29.md §2.6（第 89–111 行）逐条执行：

     ```
     spike_focus --i-understand-this-steals-focus --approach plain|alt|attach|noactivate|all [--wait-secs 5] [--hold-ms 800] [--settle-ms 200] [--hide-first] [--no-alt-mask]
     ```

     T-06 需要以管理员身份运行的记事本。
  3. 把每条的「记录」栏填进 s-29.md §2.6 的表。
- **预期结果**：见 s-29.md 表中的「预期」栏。该项的预期结论是「降级方案」。
- **通过标准**：T-01 ～ T-08 都有记录，并按 s-29.md:111 的判定规则得出结论。
- **出处**：s-29.md §0（第 11 行）、§2.6。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（结论：可行 / 降级方案 / 不可行）

### M4-SP-02 管理员窗口检测与输入可见性 T-09 ～ T-14

- **步骤**：
  1. T-13 可以直接执行：管理员记事本在前台时运行 `spike_integrity` 和 `spike_integrity <PID>`（s-29.md:196、300-303）。
  2. T-09 ～ T-12 和 T-14 需要一个临时探针程序（s-29.md:188）。**这个探针还没有编写**，本轮预计只能标「未测」，原因写「探针未编写」。
- **预期结果**：T-13 有记录；其余按探针是否到位决定。
- **通过标准**：T-13 有记录；T-09 ～ T-11 的结果用于回填 C.2 #3。
- **出处**：s-29.md §0（第 12 行）、§3.5（第 186–199 行）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-SP-03 Backspace 擦除单元 T-15 ～ T-21

- **步骤**：
  1. 按 s-29.md §4.5（第 267–281 行）操作，**不需要注入**：
     1. 把展开文本复制到剪贴板；
     2. 在目标程序里先输入前文 `x `，再粘贴；
     3. 一次一次按 Backspace，数到只剩前文为止，这个次数就是 U。
  2. 对 T-15 ～ T-19 逐条记录。T-20 需要 espanso，可选。T-21 要等 B-34 实现之后才能做，本轮标「未测」。
- **预期结果**：见 s-29.md 表中的「预期」栏。
- **通过标准**：T-15 ～ T-19 都有记录，并建立「擦除模型表」（按进程映像名列出）。
- **出处**：s-29.md §0（第 13 行）、§4.5。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-SP-04 回填 s-29.md

- **步骤**：
  1. 把实测结果填进 s-29.md §2.6、§3.5、§4.5 各表的「记录」栏，更新 §0 结论摘要（第 9–13 行）的「预期结论」和「待实测」两栏。
  2. 更新 §7 附录 C.2 回填表（第 317–324 行）：
     - #3 ← T-09、T-10、T-11；
     - #8 ← T-06；
     - #27 ← T-15 ～ T-21；
     - #28 ← T-01 ～ T-08。
  3. 由开发同步 development-plan.md 附录 C.2 的状态。
- **预期结果**：s-29.md 不再有已经测过却仍写着「待实测」的地方。
- **通过标准**：回填的提交已合入。
- **出处**：s-29.md:5、317-324。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（提交：____）

## 11. 不得倒退 8 项（M4-NR）

依据 development-plan.md §5.5（第 993–1006 行）。各行的「现有实现位置」仍指向拆分前的旧文件，见 §12.3。

### M4-NR-01 剪贴板租约：不覆盖用户新复制的内容

- **步骤**：
  1. 退出程序，把 `restore_delay_ms` 设为 2000，然后启动。
  2. 复制「原内容 A」。在记事本执行 `zzp` + 空格，**2 秒内**选中另一段文字「新内容 B」并按 Ctrl+C。
  3. 等 3 秒，在别处按 Ctrl+V。
  4. 对照组：再复制 A，执行 `zzp`，不做别的，3 秒后按 Ctrl+V。
  5. 把 `restore_delay_ms` 改回 300。
- **预期结果**：
  - 第 3 步粘贴出来的是「新内容 B」，不是 A，也不是规则正文；历史可能显示「执行中剪贴板被修改」。
  - 第 4 步粘贴出来的是 A。
- **通过标准**：用户的新内容保留（§5.5 #1 的 L3 用例）。
- **出处**：development-plan.md:999；`clipboard.rs:1535-1549`（只有本程序仍是剪贴板所有者、内容也没被改动时才还原）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-NR-02 粘贴失败不改用键入

- **步骤**：
  1. 场景 a（release）：重做 CB-07 的第 1–2 步。
  2. 场景 b（debug-fi）：
     1. 布防 `executor_hang`，param 30000；
     2. 在记事本执行 `zzp` + 空格；
     3. 约 10 秒后出现 `executor.rebuilt`，此时立即再执行一次 `zzp` + 空格；
     4. 检索 `executor.clipboard_busy`；
     5. 等满 30 秒，确认没有多余的输出。
- **预期结果**：
  - 场景 a：失败，原因「剪贴板被其他程序占用」，正文一个字也没有被键入。
  - 场景 b：如果旧任务还占着剪贴板，第二次执行会被拒绝，日志有 `executor.clipboard_busy`，历史显示「失败 · 剪贴板被其他程序占用」，**不改用键入**（`executor.rs:1715-1733`）。
- **通过标准**：两个场景都没有出现键入的正文。
- **出处**：development-plan.md:1000（§5.5 #2）。与 B-01 ⑤ 的写法相冲突，见 §12.3：development-plan.md:1819 写的是「新纪元的执行器在旧线程释放剪贴板之前改用键入输出」。代码按 §5.5 #2 实现，本条按 §5.5 判定。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：注入的阻塞发生在第一批 SendInput，也就是退格那一批，此时是否已经持有剪贴板占用，取决于租约在退格之前还是之后取得。如果没有出现 `executor.clipboard_busy`，就记为「场景 b 未复现」）

### M4-NR-03 过期或不连续的输入不执行

- **步骤**：
  1. 在记事本输入 `zz`，用鼠标点一下文中别的位置，再输入 `k` + 空格。
  2. 输入 `zz`，按 ← 再按 →，再输入 `k` + 空格。
  3. 输入 `zz`，按 Alt+Tab 切到 Chrome 的 `#plain`，输入 `k` + 空格。
- **预期结果**：三种情况都不展开。
- **通过标准**：三种情况各做 5 次，零展开。
- **出处**：development-plan.md:1001（§5.5 #3）；browser-ime-manual-tests.md B05、B06。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-NR-04 外部模拟输入使缓冲失效

- **步骤**：
  1. 用 release 构建正常启动，**不是**验收模式。确认 `Get-ChildItem env:QUICKER_TEXT*` 为空，「详细运行信息（高级）」显示 `external_injected_keys: filtered`。
  2. 运行 P6，8 秒内切到记事本，P6 会发送 `zzk `。
  3. 把 P6 的发送内容改成 `SendWait('k')`，运行后在记事本手动输入 `zz`，等 P6 发出 `k`，再手动按空格。
- **预期结果**：两种情况都不展开（P6 发出的字符正常上屏）。
- **通过标准**：各做 5 次，零展开。
- **出处**：development-plan.md:1002（§5.5 #4）；`hook.rs:253-259`（`if injected && !pipe.test_mode`）；windows-acceptance-mode.md。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-NR-05 密码框、只读框不注入

- **步骤**：
  1. 在 Chrome 的 `browser-input.html` 中，分别在 `#password` 和 `#readonly` 里输入 `zzk` + 空格。
  2. 原生只读编辑框：打开任意文件的「属性」，点进「位置」字段（只读的编辑框），输入 `zzk` + 空格。
  3. 查看「最近执行」。
- **预期结果**：
  - 不展开，也不吞终止键。
  - 历史显示「目标是密码框或只读控件」，或者没有执行记录（需确认是哪一种）。
- **通过标准**：零注入。
- **出处**：development-plan.md:1003（§5.5 #5，原文的 L3 用例是「记事本只读模式、浏览器密码框」）；browser-ime-manual-tests.md B04。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：Windows 11 记事本没有只读模式，所以用「属性」对话框的「位置」字段代替，且需确认它是可以获得焦点的只读 Edit 控件）

### M4-NR-06 注入计数不符时停止后续步骤（fail-stop）

- **步骤**：
  1. SendInput 返回的数量少于请求数，这种情况在实机上很难稳定制造。UIPI 拦截时返回值也不一定能反映出来（s-29.md:12）。
  2. 实机只验证「后续步骤不执行」这一可观察的结果：
     - 重做 IN-05 第 2 步（`zzw` 在等待期间切换窗口）；
     - 再做一次：等待期间切到以管理员身份运行的记事本。
  3. 计数不符本身由 L2 测试覆盖，在备注里写下对应测试的名称。
- **预期结果**：「步骤二」不出现在任何窗口；历史为「已停止」或「失败」，并带原因。
- **通过标准**：后续步骤不执行，并且记入执行历史。
- **出处**：development-plan.md:1004（§5.5 #6）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：有没有实机办法让 SendInput 返回不足的计数；L2 测试名称：____）

### M4-NR-07 切换键盘布局后立即展开

- **步骤**：
  1. 在系统里添加「德语（德国）」键盘（QWERTZ，Y 和 Z 互换）。
  2. 把 `zzk` 的正文临时改成 `Hello yz QuickerText`，「应用设置」。
  3. 在记事本按 Win+Space 切到德语布局，**立即**按物理键 Y、Y、K（在德语布局下上屏 `zzk`），再按空格。
  4. 切回英语（美国），立即按物理键 Z、Z、K，再按空格。
  5. 测完删除德语键盘，并把正文改回原样。
- **预期结果**：
  - 两次都展开，正文逐字正确，包括 `yz` 没有被互换。
  - 布局缓存间隔为 0，所以检测用的是当前布局（`rawinput.rs:108`）。
- **通过标准**：切换后第一次输入就能展开，且字符正确。
- **出处**：development-plan.md:1005（§5.5 #7）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-NR-08 发布事务化：失败时保留生效配置和暂停状态

- **步骤**：
  1. 场景 a（debug-fi）：
     1. 编辑一条规则并「保存」到草稿，记下「当前生效 v{N}」、状态行（监听中），用 P10 记下 `active.json` 的哈希；
     2. 布防 `store_write_fail`；
     3. 点「应用设置」；
     4. 再点一次「应用设置」。
  2. 场景 a′：先点「暂停监听」，再按场景 a 做一遍。
  3. 场景 b（release）：用 P1 锁定 `active.json` 后点「应用设置」；释放锁后再点一次。
- **预期结果**：
  - 第一次「应用设置」报错「写入文件失败：…」。生效版本仍是 v{N}，`active.json` 的哈希不变，草稿还在，监听或暂停状态与操作前相同。
  - 第二次应用成功，生效版本变为 v{N+1}。
  - 场景 b 的错误码为 32。
- **通过标准**：失败的发布不改变生效配置和暂停状态（§5.5 #8）。
- **出处**：development-plan.md:1006；`revisions.rs:30-69`（交换期间持有 MANUAL，结束后恢复）；`atomic.rs:18`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：注入的失败落在 `revisions/<旧>.json` 备份还是 `active.json` 的写入上，两者都应该使发布整体失败）

## 12. 汇总

### 12.1 条目数

| 节 | 条目 | 数量 | 通过 | 不通过 | 未测 |
|---|---|---|---|---|---|
| 2 输入与注入 | M4-IN-01 ～ 10（09、10 可选） | 10 | | | |
| 3 剪贴板 | M4-CB-01 ～ 09（09 可选） | 9 | | | |
| 4 监督与自愈 | M4-SV-01 ～ 14 | 14 | | | |
| 5 安全模式与存储 | M4-ST-01 ～ 11（03 可选） | 11 | | | |
| 6 前端状态一致性 | M4-FE-01 ～ 07 | 7 | | | |
| 7 隐私 | M4-PR-01 ～ 04 | 4 | | | |
| 8 数据兼容 | M4-CP-01 ～ 04 | 4 | | | |
| 9 性能与浸泡 | M4-PF-01 ～ 05 | 5 | | | |
| 10 S-29 spike | M4-SP-01 ～ 04 | 4 | | | |
| 11 不得倒退 | M4-NR-01 ～ 08 | 8 | | | |
| **合计** | | **76** | | | |

### 12.2 条目与 0.4 退出标准的对应

退出标准见 development-plan.md:2448；「不得倒退」一项见 development-plan.md:2455。

| 退出标准 | 覆盖条目 | 判定 |
|---|---|---|
| E1 故障注入点（§11）全部能自动恢复或明确提示；`hook_unhook`、`rawinput_unregister` 由交叉校验检出并重建 | 各注入点及其覆盖条目：<br>• `executor_panic`：SV-02<br>• `executor_hang`：SV-03、NR-02<br>• `hook_panic`：SV-04<br>• `hook_unhook`：SV-05<br>• `rawinput_unregister`：SV-06<br>• `rawinput_close`：SV-07<br>• `clipboard_busy`：CB-08，真实场景 CB-07<br>• `store_write_fail`：NR-08<br>• `config_corrupt`：用真实操作代替，ST-05、ST-06<br>• `store_locked`：用真实操作代替，ST-01、ST-02<br>• `autostart_stale`：用真实操作代替，SV-12<br>• `session_end`：SV-14，真实注销 SV-13<br>补充：SV-01、SV-08 ～ SV-11、ST-03、ST-04、ST-07 ～ ST-11、CB-01 ～ CB-06、CB-09 | ☐ |
| E2 2 小时浸泡无静默失效 | PF-05（PF-04 作为空闲参照） | ☐ |
| E3 隐私测试通过（日志、诊断包、执行历史不含正文） | PR-01 ～ PR-04、CB-06 | ☐ |
| E4 S-25 实机最小集有书面记录 | IN-01 ～ IN-08（记事本、Chrome、微信，各配合微软拼音）；CB-04 ～ CB-06（development-plan.md:2524：0.4 的最低覆盖还包括剪贴板子表，第 2545–2554 行）。本文件填写后存档 | ☐ |
| E5 0.3.1 数据无损加载，0.4 写入的数据 0.3.1 可读 | CP-01、CP-02（CP-03、CP-04 为 §9 的配套要求） | ☐ |
| E6 无 P0 缺陷 | 全部条目。任一条触发 §0 的立即停止条件即为 P0 | ☐ |
| E7 不得倒退清单全部通过 | NR-01 ～ NR-08 | ☐ |
| 前端底座与状态一致性（0.4 交付内容，不是独立的退出标准） | FE-01 ～ FE-07、ST-01、ST-02（I6） | ☐ |
| 记录项（不阻断发布）：性能基线、S-29 结论 | PF-01 ～ PF-04、SP-01 ～ SP-04、IN-09、IN-10 | ☐ |

### 12.3 编写时发现的文档与代码不一致（只记录，不在本次修改）

1. **backup-pre-0.4 没有实现**：development-plan.md:1805、2339 要求跨版本首次写入前备份，代码里没有找到（CP-04）。
2. **B-01 ⑤ 与 §5.5 #2 相冲突**：
   - development-plan.md:1819 写的是：剪贴板被卡住的旧线程占着时，新纪元的执行器「改用键入输出」；
   - §5.5 #2 和代码（`executor.rs:1715-1733`）都是拒绝，不改用键入（NR-02）。
3. **session_end 场景与代码不同**：
   - 计划 §11（development-plan.md:2489）预期这个注入点之后结束进程，连续启动两次不会被误判为崩溃循环；
   - 但这个注入点只调用会话结束回调（`session.rs:216-221`），不清除运行标记；
   - 真实注销走 RunEvent::Exit，会清除运行标记（`startup.rs:384-395`）（SV-13、SV-14）。
4. **LKG 没有接上**：计划把 last-good 版本作为第一回退，但生产代码不调用 `mark_last_good`，回退实际上直接从 `revisions/` 开始（ST-06）。
5. **恢复横幅每次启动都会出现**：从回退候选恢复后不重写 `active.json`，所以直到下一次「应用设置」之前，每次启动都显示恢复横幅（ST-06）。是否符合预期，需确认。
6. **横幅说「可以导入配置」，Locked 时导入却被拒绝**：safe_mode 横幅对所有原因用同一句话（`src/status.ts:146-210`）。但后端只允许 CrashLoop 和 NoValidConfig 用导入解除，Locked、NewerSchema 下只能预览（`src-tauri/src/store/mod.rs:85-94`）（ST-04、ST-08）。
7. **自检项名称不一致**：系统通知里写「键盘鼠标监听」（`startup.rs:363`），前端横幅写「键盘监听心跳」（`src/ipc/labels.ts:177`）。
8. **终止键与计划默认值不同**：代码是 Ctrl+Shift+Esc（`hook.rs:262-275`），计划默认值是 Ctrl+Alt+Pause；development-plan.md:215 标为【待核实】（IN-07）。
9. **p50/p95 汇总缺失**：`get_health` 还没有 p50/p95 汇总（`src-tauri/src/commands/diagnostics.rs:506` 是 TODO），只有每条历史的 StageTimings。前端也没有调用 `report_metric`、`self_test`、`set_log_level`（PF-01）。
10. **`scripts/soak/` 不存在**：计划 L6（development-plan.md:2477）和 performance-baseline.md:82 都要求有它（PF-05）。
11. **版本号仍是 0.3.1**：所以本清单的构建栏要求写提交哈希。
12. **§5.5「现有实现位置」过时**：引用的仍是拆分前的 `windows.rs`、`windows_espanso.rs`、`main.rs` 行号；performance-baseline.md:40 也是如此。
13. **S-06 与代码参数的关系**：计划 S-06 写「50 次按键」，代码中 K=5、R=8，最多 33 次按下（`liveness.rs:47-51`，符合要求）。但「10 秒内」这一条在用户不打字时由谁保证，代码注释里没有说明（SV-05 变体 b）。
14. **计划 §11 的实机矩阵列不对应**：development-plan.md:2518-2554 的「自动」列在 0.4 没有对应物。其中管理员记事本一行（S-20）不在 0.4 范围。
15. **TODO 26**：另一个窗口里的 `save_draft` 没有纳入保存进行中的跟踪（合并记录）。本清单没有为它单列条目。

## 附表 A：代码与文档中的「待实测」由哪条覆盖

| 位置 | 内容 | 条目 |
|---|---|---|
| `crates/platform/src/supervisor.rs:83` | VERIFY_TIMEOUT：实机上钩子和 RawInput 重新注册的耗时 | SV-02、SV-05 ～ SV-07（恢复耗时） |
| `supervisor.rs:87` | STARTUP_GRACE 待基线确定 | SV-10、PF-02 |
| `supervisor.rs:101` | ATTEMPT_TIMEOUT：实机重挂的上限（钩子约 2 s，RawInput 约 4 s） | SV-05 ～ SV-07 |
| `crates/platform/src/windows/executor.rs:183` | STALL_BUDGET 待基线确定：剪贴板长时间重试、打开慢的目标必须低于它 | SV-03、CB-09、PF-05 |
| `crates/platform/src/windows/hook.rs:32` | 钩子队列容量 | SV-08、PF-05 |
| `hook.rs:577` | 替换钩子时，正在途中发往旧钩子的事件 | SV-05 |
| `hook.rs:773` | 钩子线程忙或卡住时的 ping | SV-05 变体 b、SV-09 |
| `crates/platform/src/windows/rawinput.rs:101` | RawInput 队列容量 | SV-08、PF-05 |
| `rawinput.rs:933` | 从不拥有窗口的线程重新注册 | SV-06 |
| `rawinput.rs:970` | 检测窗口重建的耗时 | SV-07 |
| `rawinput.rs:997` | 关闭期间开始退出时的残留 | SV-07（附带观察） |
| `crates/platform/src/windows/liveness.rs:30、35` | MATCH_WINDOW、MISS_LIMIT 待 0.4 基线确定 | SV-01、SV-05、SV-08 |
| `liveness.rs:43` | 管理员窗口在前台时只有一个输入源能看到 | SV-09 |
| `liveness.rs:52` | 自动重复每秒 2.5–30 次 | SV-08 |
| `liveness.rs:115` | RawInput 与钩子的自动重复行为，丢失 key-up | SV-08 |
| `crates/platform/src/windows/keystate.rs:344` | 注入的通用修饰键是否置位左侧键状态 | IN-06 |
| `keystate.rs:444` | SendInput 返回到 GetAsyncKeyState 反映之间的延迟（INJECTION_QUIET） | IN-06 |
| `crates/platform/src/windows/input.rs:966`；`pipeline_tests_input.rs:1745、2671` | 远程会话的按键没有设备句柄 | IN-09 |
| `crates/platform/src/windows/inject.rs:331` | 左右键互换时注入的左键置位哪个 VK | IN-10 |
| `crates/platform/src/windows/clipboard.rs:41` | OWNER_PROBE_TIMEOUT 对慢但还活着的所有者的取值 | CB-09 |
| `clipboard.rs:98` | 复制 HMETAFILE（METAFILEPICT） | CB-04 e |
| `clipboard.rs:125` | 只写 CF_UNICODETEXT 后是否有 CF_LOCALE | CB-04 a |
| `clipboard.rs:738` | 等待所有者处理 WM_RENDERFORMAT 的时长 | CB-09 |
| `clipboard.rs:827` | 超时后慢但还活着的所有者被记为 hung | CB-09 |
| `clipboard.rs:1408` | 写入后 CF_LOCALE 是否被列出；渲染合成格式是否推进序列号 | CB-04 a、NR-01 |
| `crates/platform/src/windows/self_test_probes.rs:7` | 四项探针在真实桌面上的表现（登录冷启动、锁定的工作站、剪贴板被占用） | SV-10、SV-11、IN-08、CB-08 |
| `crates/platform/src/windows/session.rs:225` | 开机早期注册会话通知 | SV-10 |
| `session.rs:231` | 隐藏窗口能否收到两条结束消息，回调可以占用多少关机时间 | SV-13 |
| `src-tauri/src/startup.rs:294` | 登录时冷启动的时序 | SV-10 |
| `src-tauri/src/tray.rs:301` | macOS 通知 | 不适用（本清单只测 Windows） |
| `crates/platform/src/supervisor_wiring.rs:490` | macOS 的 native_ready（权限、Secure Input） | 不适用（本清单只测 Windows） |
| docs/spikes/s-29.md T-01 ～ T-21 | 三项 spike 的实测 | SP-01 ～ SP-04 |
| docs/performance-baseline.md 各「待实测」栏（第 40–67 行、第 89 行、第 115–116 行） | 性能基线和浸泡记录 | PF-01 ～ PF-05 |
| development-plan.md:215【待核实】 | Ctrl+Shift+Esc 的实际表现 | IN-07 |
| merge-log 第 525 行 | 剪贴板健康链在实机上的表现 | CB-07、FE-07 |
| merge-log 第 1270 行（TODO 22） | 管理员窗口在前台时自检误报 | SV-09 |
