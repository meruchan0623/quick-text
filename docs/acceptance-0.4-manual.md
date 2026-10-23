# 0.4 实机验收清单（S-25 书面记录模板）

> 状态：2026-10-23 依据 `576aa5a`（`feat/0.4-stability-foundation`）编写，同日按两轮审查意见修订。
>
> **实测用的构建：「`integrate/0.4` 合并后的构建」**，即在 `integrate/0.4`（修订时为 `f1589cb`）上再合入本文件所在的分支 `docs/0.4-manual-acceptance` 之后构建出来的程序。`integrate/0.4` 已包含五个分支头：
>
> | 分支 | 分支头 | 本清单用到的内容 |
> |---|---|---|
> | 存储 | `43dda1f` | 升级前备份 R6 及审查修正 I-B1～I-B3、R7；回退目标记录 I-R1、I-R2 与 R8；会话结束清标记 R5 |
> | 执行器 | `b69f3c2` | 手动测试的并发上限与报错（SV-17） |
> | 鼠标重挂 | `384f8d1` | SV-16 |
> | 上次正常配置 | `b32062f` | ST-13 |
> | 发布卫生 | `996d725` | — |
>
> 本分支另带三个**不在** `integrate/0.4` 里的提交：`cfa41f2`（安全模式横幅按原因给出出路，并给 `backup_failed` 一条专门的横幅文案）、`e0fc068`（自检项改名「键盘鼠标监听」）、`9cbe400`（浸泡监视脚本 `scripts/soak/`）。只用 `integrate/0.4` 构建时，ST-01、CP-05 等条目的横幅文案对不上。
>
> 行号说明：
>
> - **本次修订改写过的部分**是 §1.3 的数据目录说明、§1.6 故障注入表的两行、ST-06、ST-12 d、ST-13、CP-01、CP-02、CP-04、CP-05、NR-08、新增的 SV-17，§12 的计数和 §12.3 第 1、4、10、19 条，以及附表 A 中 `startup.rs:195`、`lkg.rs:127` 两行。这些地方的行号如下：
>   - `src-tauri/src/store/` 下除 `mod.rs` 外的文件（`backup.rs`、`load.rs`、`fallback.rs`、`revisions.rs`、`meta.rs`、`prefs.rs`、`atomic.rs`）取自 `43dda1f`，这些文件在 `integrate/0.4` 中完全相同；
>   - `store/mod.rs`、`store/startup_tests.rs`（集成时加了一行导入和两个测试，行号比 `43dda1f` 多 1 行，第 2950 行以后多 138 行）、`src-tauri/src/lkg.rs`、`src-tauri/src/startup.rs`、`src-tauri/src/commands/runtime.rs`、`src-tauri/src/error.rs`、`src-tauri/src/diagnostics/logging.rs`、`crates/platform/src/windows/executor.rs` 和前端文件取自 `integrate/0.4`（`f1589cb`）。集成时 `mod.rs` 的 `mark_last_good` 加了备份检查和快照核对，比 `43dda1f` 长；`conclude` 改用 `CommandError::task_failed`，行号比 `b69f3c2` 多两行。
> - **其余条目**沿用编写时 `576aa5a` 的行号（附表 A 另有说明）。存储模块在 `576aa5a` 之后改动较多，这些条目里的存储模块行号只作参考，请按函数名或事件名检索。
>
> **构建不含以上全部提交时**，SV-14、SV-17、ST-12、ST-13、CP-04、CP-05、NR-08 等条目的预期不成立。**全部条目待实测**。本文件是记录模板：实测时直接在每条的「结果」行填写，填完后连同日志、诊断包和录屏按 [release-checklist.md](release-checklist.md) §5 存档。
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
- **恢复后再触发，先按一次物理空格或回车**。凡是步骤写「恢复后（或释放后、重新挂上后）再键入 `zzk`」，都先等界面回到「正常 / 监听中」，再在目标窗口里按一次**物理**空格或回车，然后键入触发词。原因：
  - 检测缓冲在这些情况下断开：分隔键（空格是分隔符）、重置键（回车、Tab、Esc、方向键、Home/End）、鼠标点击或滚轮（`crates/platform/src/windows/hook.rs:387`）、输入源重挂或会话信号（`crates/platform/src/windows/input.rs:382-389`）。
  - 「按空格、分号等指定键执行」的规则要求触发词前一个字符是分隔符，只有触发词恰好在缓冲开头时才不检查（`crates/domain/src/matching.rs:230-236`）。如果之前键入的字母还留在缓冲里（例如 SV-04 先键入的 `x`），它们会和新键入的 `zzk` 粘成一个词 `xzzk`，按设计不触发，看起来像「恢复后不工作」。
  - 恢复恰好发生在键入 `zzk` 中途时，`zzk` 会被拆成两段，同样不触发。
  - 微信里回车会直接发送消息，只用空格。
  - 下文涉及的条目：IN-08、CB-08、SV-02～SV-09、SV-11、SV-15、NR-02 b 等。步骤里写了「按空格」的，就是这条规则；没写的也照此执行。不按这条规则测出的「恢复后不触发」不能记为不通过，要按规则重测一次。

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
    - `store-meta.json` 里与本清单有关的字段（`src-tauri/src/store/meta.rs:24-81`）：
      - `last_good_revision`：上次正常运行时的生效版本（ST-13）；
      - `previous_revisions`：「恢复上一个版本」的目标记录，是一个数组，最多两项，每项形如 `{"active": 5, "revision": 4, "fingerprint": {"len": 1234, "sha256": "…"}}`。`active` 是那次发布写出的版本号，`revision` 是它替换掉的版本（`null` 表示替换的是空白占位配置，没有可回退的目标），`fingerprint` 是那次写出的 `active.json` 的长度和 SHA-256。每次发布在替换 `active.json` 之前写入：第一项描述这次发布，第二项是发布开始时描述当时生效版本的那一项（审查 I-R1：发布中途失败时，已生效的版本仍有自己的记录）。加载时只采用 `active` 和 `fingerprint` 都与读到的 `active.json` 一致的那一项（I-R2），没有就按旧规则推断（ST-13、NR-08）。开发期构建写过单个对象 `previous_revision`，现在不再读取，原样保留在文件里；
      - `backup_pre_0_4`：`true` 表示升级前备份已建立（新建的或已有的），`false` 表示数据目录里没有 0.4 以前的数据、不需要备份；`null`（旧文件里也可能没有这个键）表示还没处理过（CP-04、CP-05）；
      - `backup_pre_0_4_dir`：备份所在的目录名，`backup-pre-0.4` 或 `backup-pre-0.4-<UTC 时间>`，与 `backup_pre_0_4: true` 一起写入，其他时候为 `null`；
    - 历史版本：`revisions\<n>.json`、`revisions\<n>.metadata.json`；
    - 升级前备份：`backup-pre-0.4\`（同名位置被不完整的备份或别的东西占着时是 `backup-pre-0.4-<UTC 时间>\`），在 0.3.1 数据目录上首次启动 0.4 时建立，只建一次。目录里最后写入的文件是 `manifest.json`，列出每个备份文件的相对路径、长度和 SHA-256；只有带合法 `manifest.json` 且逐个文件核对一致的目录才算完整备份（CP-04）；
    - **延后隔离**（审查 I-B1）：`store-meta.json` 里的 `backup_pre_0_4` 还是 `null` 或没有这个键时（0.3.1 留下的目录，或这个文件读不出来、被重建），加载发现的损坏文件**先不改名**，日志记 `store.quarantine_deferred file="…"`（`src-tauri/src/store/fallback.rs:185-186、536-561`）。同一次启动里升级前备份把它复制进去之后，才改名为 `*.corrupt-<时间>.json`，日志接着有 `store.backup_pre_0_4 result="created"` 和 `store.quarantine`（`load.rs:99-104`、`backup.rs:552-613`）。备份失败或进入安全模式时，损坏文件留在原处，等下一次写入或下一次启动再处理。ST-05、ST-06、ST-10、ST-12 的预期按「`store-meta.json` 里的 `backup_pre_0_4` 已是 `true` 或 `false`」写：在还没处理过的目录上做，会多出上述日志、多建一个备份目录，安全模式下损坏文件也不会改名。所以先在该数据目录上正常启动一次 0.4 再做（新建的空目录启动一次即可，记 `backup_pre_0_4: false`）；
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
- 生成的文件**只含 5000 条合成规则**，不含原有规则。FE-02 导入时选「保留两份」，原有规则保持不动，新增的正好是这 5000 条；如果文件里也带着原有规则，「保留两份」会把它们再追加一遍，条数就对不上了。

```powershell
$src = [IO.File]::ReadAllText('D:\qt-0.4-rules.json') | ConvertFrom-Json
$base = $src.config.rules | Where-Object { $_.match.value -eq 'zzk' } | Select-Object -First 1
$bulk = for ($i = 0; $i -lt 5000; $i++) {
  $r = $base | ConvertTo-Json -Depth 30 | ConvertFrom-Json
  $r.id = "bulk-$i"; $r.match.value = "zqb$i"; $r.enabled = $false; $r }
$src.config.rules = @($bulk)
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

（原 P11「浸泡负载」已删除：它用 SendKeys 发送按键，必须在验收模式下运行，测不到真实输入路径。PF-05 改用人工打字或实体按键工具，加 `scripts/soak/soak.ps1` 监视，见 PF-05。）

**P12 剪贴板图片格式的长度与哈希**（CB-04 e）。读取 `CF_DIB`（8）和 `CF_ENHMETAFILE`（14）两种格式，输出字节数和 SHA256；只读，不改剪贴板：

```powershell
Add-Type -Namespace QtTest -Name ClipHash -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool OpenClipboard(IntPtr h);
[DllImport("user32.dll")] public static extern bool CloseClipboard();
[DllImport("user32.dll")] public static extern IntPtr GetClipboardData(uint f);
[DllImport("kernel32.dll")] public static extern IntPtr GlobalLock(IntPtr h);
[DllImport("kernel32.dll")] public static extern bool GlobalUnlock(IntPtr h);
[DllImport("kernel32.dll")] public static extern UIntPtr GlobalSize(IntPtr h);
[DllImport("gdi32.dll")] public static extern uint GetEnhMetaFileBits(IntPtr h, uint n, byte[] b);
public static byte[] Read(uint f) {
  IntPtr h = GetClipboardData(f); if (h == IntPtr.Zero) return null;
  if (f == 14) { uint n = GetEnhMetaFileBits(h, 0, null); byte[] e = new byte[n]; GetEnhMetaFileBits(h, n, e); return e; }
  IntPtr p = GlobalLock(h); if (p == IntPtr.Zero) return null;
  try { byte[] b = new byte[(int)GlobalSize(h).ToUInt64()]; Marshal.Copy(p, b, 0, b.Length); return b; } finally { GlobalUnlock(h); }
}
'@
$sha = [Security.Cryptography.SHA256]::Create()
if (-not [QtTest.ClipHash]::OpenClipboard([IntPtr]::Zero)) { throw '剪贴板被占用，稍后再试' }
try { foreach ($f in 8, 14) { $b = [QtTest.ClipHash]::Read($f)
  if ($null -eq $b) { "格式 ${f}：无" } else { "格式 ${f}：$($b.Length) 字节 SHA256=$(([BitConverter]::ToString($sha.ComputeHash($b))) -replace '-','')" } }
} finally { [void][QtTest.ClipHash]::CloseClipboard() }
```

- 同一个 PowerShell 窗口里 `Add-Type` 只能执行一次；再次读取时只运行 `$sha = …` 之后的部分。
- `GlobalSize` 返回的长度可能带对齐填充。前后哈希不同时，把两次的字节数一起记进备注交给开发判断，不要自行判定为通过。

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
| `zzh` | 粘贴带格式的文字 | 「带格式文字的 HTML 代码」填 `<b>粗体</b>普通<i>斜体</i>`；「无法显示格式时使用的普通文字」填 `粗体普通斜体`，**不能留空**（`src/ActionEditor.tsx:817-834`）。程序把两者同时放进剪贴板（HTML Format + CF_UNICODETEXT，`crates/platform/src/windows/clipboard.rs:561-573`）：富文本目标（Word、网页 contenteditable）取 HTML，显示粗体和斜体；纯文本目标（记事本、input、textarea）取普通文字，显示 `粗体普通斜体`。普通文字留空时，纯文本目标会静默粘贴出空内容（§12.3 第 17 条） | 剪贴板 |
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
| `executor_panic` | 下一次执行的第一批 SendInput，自动展开和手动测试都算（integrate `f1589cb` 的 `executor.rs:2266-2274`，手动测试 `2331-2333`、自动展开 `2444-2447`） | 0 | SV-02、SV-15、SV-17、PR-01 |
| `executor_hang` | 同上，线程睡眠 param 毫秒 | 200000（计划 §11 的 200 秒）；NR-02 用 30000 | SV-03、SV-17、NR-02 |
| `hook_panic` | 下一次键盘钩子回调（`hook.rs:243`） | 0 | SV-04 |
| `hook_unhook` | 输入消费循环的下一轮（`input.rs:916`） | 0 | SV-05 |
| `rawinput_unregister` | 同上（`input.rs:920`） | 0 | SV-06 |
| `rawinput_close` | 同上（`input.rs:924`） | 0 | SV-07 |
| `clipboard_busy` | 此后 param 毫秒内视为剪贴板被占用 | 5000 | CB-08 |
| `store_write_fail` | 下一次原子写，在替换那一步失败：报「重命名文件失败：<文件名>。」，不带系统错误码，目标文件保持原样（`src-tauri/src/store/atomic.rs:16-23`）。发布时第一次原子写是旧版本的 `revisions\<n>.json` 或 `<n>.metadata.json`，不是 `active.json` | 0 | NR-08 |
| `session_end` | 输入消费循环的下一轮，模拟 WM_QUERYENDSESSION 和 WM_ENDSESSION（`input.rs:997`、`session.rs:216-221`）。会话结束回调同时清除运行标记，之后被 taskkill 也不算崩溃（`241b3f3`，复审 R5） | 0 | SV-14 |
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
  - `#area` 的多行正确。
  - `zzh` 分目标判定：`#rich`（富文本）显示带粗体和斜体的「**粗体**普通*斜体*」；`#plain`、`#area`（纯文本）显示普通文字 `粗体普通斜体`，不出现 HTML 标签。纯文本目标里粘贴出空内容，先检查 `zzh` 的「无法显示格式时使用的普通文字」是否填了（§1.5）。
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
  - 两种情况下，切换之后的字符都**不会**进入 Chrome，「步骤二」不会出现在任何窗口，切换后记事本里也不再有新的输出。
  - 历史为「已停止」或「失败」，原因是「输入已过期」（`stale_input`）或「执行中切换了窗口」（`focus_changed`），两者都算通过。用 Alt+Tab 或鼠标点击切换时，通常先看到「输入已过期」：执行期间的物理按键和鼠标点击都会让检测代次前进（`crates/platform/src/windows/hook.rs:277-284、385-388`），这条检查排在焦点检查之前。
- **通过标准**：Chrome 里没有出现任何来自规则的字符；切换之后没有任何后续输出；历史原因是上述两者之一。
- **可选**：要专门看到「执行中切换了窗口」，需要不经键盘和鼠标就改变前台窗口的受控夹具（例如由开发提供、定时自己抢焦点的测试窗口）。没有夹具时本项记「未测」，不影响本条判定。
- **出处**：ReasonCode `focus_changed`、`stale_input`（`src/ipc/labels.ts:149-150`）；焦点策略「始终在指令开始时的窗口执行」（`src/ActionEditor.tsx:1473-1474`）；§5.5 #6。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（历史原因：____；可选项：____）

### M4-IN-06 修饰键不卡住；按住修饰键时执行

- **步骤**：
  1. 在记事本执行 `zzc` + 空格。之后手动输入 `abc`，确认输入的是小写字母，没有选中文字，也没有触发 Ctrl 组合键。
  2. 新建一条键入纯文本规则，正文含大写和符号（如 `ABC Hello! #1`），执行后同样手动输入 `abc` 检查。
  3. 执行 `zzw` + 空格，在 3 秒等待期间按住左 Ctrl 不放，直到超过第二步的时刻再松开。
  4. 执行后在记事本中输入 `a`，确认 Shift、Ctrl、Alt 都没有卡住。
- **预期结果**：
  - 第 1、2 步之后没有修饰键残留。
  - 第 3 步：「步骤二」不在 Ctrl 按住时输出（否则会变成 Ctrl+字母），松开 Ctrl 后也不再补发。历史为「已停止」或「失败」，原因是「输入已过期」（`stale_input`）或「按住了 Ctrl、Alt 等修饰键」（`modifier_held`），两者都算通过。物理按下 Ctrl 本身就是执行期间的一次按键，会让检测代次前进（`crates/platform/src/windows/hook.rs:277-284`），所以通常先看到「输入已过期」。
- **通过标准**：没有卡键；没有在修饰键按下时把正文打成快捷键；第 3 步按下 Ctrl 之后没有任何后续输出；历史原因是上述两者之一。
- **可选**：要专门看到「按住了 Ctrl、Alt 等修饰键」，需要不产生执行期间按键事件就让修饰键处于按下状态的受控夹具（由开发提供）。没有夹具时本项记「未测」，不影响本条判定。
- **出处**：ReasonCode `modifier_held`、`stale_input`（`src/ipc/labels.ts:150-152`）。待实测：`crates/platform/src/windows/keystate.rs:344`（注入的通用修饰键是否置位左侧键状态）、`keystate.rs:444`（INJECTION_QUIET 250 ms 是否足够）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（第 3 步历史原因：____；可选项：____）

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
  2. 解锁后 5 秒内在记事本先按一次物理空格（§0），再执行 `zzk` + 空格。
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
  - 日志里没有误判的 `input.check source="raw_input" result=…` 或 `source="keyboard_hook" result="lost"`。
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
  1. 准备好剪贴板内容，用 P5 记录格式列表 L1。c、e 两行再用 P12 记录 `CF_DIB`（8）和 `CF_ENHMETAFILE`（14）的字节数和 SHA256（H1）。
  2. 在记事本执行 `zzp` + 空格。
  3. 等 1 秒，用 P5 记录 L2；c、e 两行再用 P12 记录 H2。然后把剪贴板粘贴到对应程序里核对内容。
  4. 检索日志里的 `clipboard.skipped`。
- **预期结果**：
  - L2 与 L1 一致，或者少掉的格式恰好列在 `clipboard.skipped formats=… policy=BestEffort` 里。后一种情况下历史显示「部分剪贴板格式未能恢复」。
  - c、e 两行：H1 里存在的格式，H2 里也存在，字节数和 SHA256 都相同。
  - 粘贴出来的内容与原来一致。
- **通过标准**：
  - 没有静默丢失格式（丢了就必须有 skipped 日志和历史提示）；恢复后的内容可用。
  - **e 行必做**：Word 或 PowerPoint 复制图表或形状，L1 里要有 `CF_ENHMETAFILE`（P12 输出「格式 14」不是「无」）；如果复制出来的内容不含 EMF，换一个对象重做，直到含 EMF 为止。
  - c、e 两行没有 P12 前后哈希的证据，S-07（「恢复后 CF_DIB 内容与触发前逐字节一致」「Word 富文本（含 CF_ENHMETAFILE）」）记为**未完成**，不能只凭 L1/L2 格式列表判通过。
- **出处**：S-07（development-plan.md:707）；B-08；`clipboard.rs:47-49、98、125、1408`（`clipboard.rs:96-99`：CF_METAFILEPICT 靠恢复后的 CF_ENHMETAFILE 由系统重新合成，待实测）；`traits.rs:185-202`。
- **子表**：

| 行 | 剪贴板来源 | L1 格式数 | L2 格式数 | skipped 日志 | P12 前后（字节数 / 哈希是否相同） | 粘贴核对 | 判定 |
|---|---|---|---|---|---|---|---|
| a | 记事本复制一行中文（CF_UNICODETEXT，系统合成 CF_LOCALE；待实测 `clipboard.rs:125`、`clipboard.rs:1408`） | | | | — | | |
| b | Chrome 选中网页复制（HTML Format） | | | | — | | |
| c | Win+Shift+S 截图（位图，含 CF_DIB） | | | | 8：____ | | |
| d | 资源管理器复制 2 个文件（CF_HDROP） | | | | — | 粘贴到另一个文件夹 | |
| e（必做） | Word 或 PowerPoint 复制图表或形状（CF_ENHMETAFILE，系统合成 CF_METAFILEPICT；待实测 `clipboard.rs:98`） | | | | 8：____；14：____ | 粘贴回 Word | |

- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（c、e 两行的 H1/H2 字节数：____）

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
  4. 5 秒后，在记事本先按一次物理空格（§0），再执行一次 `zzp` + 空格。
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

熔断（10 分钟内 5 次故障进入「故障」）是 S-03 的退出标准，单列为必做条目 SV-15。其他条目里如果意外遇到熔断，在该条备注里记录，并点一次「修复引擎」（提示「已开始修复引擎，结果会自动更新」）。

本节所有「恢复后再执行」的步骤，都按 §0 先按一次物理空格或回车。

### M4-SV-01 空闲 10 分钟不误判

- **步骤**：
  1. 用 release 构建启动，状态行显示「监听中」后，不碰键盘鼠标 10 分钟（电脑不睡眠）。
  2. 然后在记事本执行 `zzk` + 空格。
  3. 检索这 10 分钟内的日志：`supervisor.recovering`、`input.check`、`executor.stalled`、`supervisor.respawn`、`supervisor.unsupervised`。
- **预期结果**：上述事件都没有出现（`input.check … result="registered"` 这类例行信息除外）；展开成功。
- **通过标准**：空闲期间没有任何恢复或重挂动作。
- **出处**：`supervisor_wiring.rs:42、109`；`supervisor.rs:93`；liveness 常量的基线（`liveness.rs:30、35`）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-SV-02 executor_panic

- **步骤**：
  1. 用 debug-fi 执行 `await inv('debug_inject_fault', { point: 'executor_panic', param: 0 })`。
  2. 在记事本执行 `zzk` + 空格。
  3. 观察「运行状态与恢复」的引擎状态和「最近执行」。
  4. 5 秒后，等状态回到「正常」，在记事本先按一次物理空格，再执行一次 `zzk` + 空格。
  5. 检查数据目录里的 `crash-*.txt`，检索日志。
- **预期结果**：
  - 第一次执行失败，历史显示「失败 · 执行器内部错误」，没有输出任何字符（panic 发生在第一批 SendInput 之前）。
  - 引擎状态里执行器从「恢复中」回到「正常」。
  - 日志依次有：`app.panic`（panic 钩子先记）→ `executor.panic`（执行器捕获后记，`executor.rs:1527-1533`）→ `supervisor.recovering … incidents=1`（监督器下一次巡检，最多约 1 秒后）→ `executor.rebuilt` → `supervisor.recovered`。执行器的故障标记只在重建时清除（`executor.rs:1141、1166`），所以 `supervisor.recovering` 必然出现。
  - 生成了一份 `crash-*.txt`；进程没有退出。
  - 第二次执行成功。
- **通过标准**：自动恢复（不需要手动操作），5 秒内恢复正常，下一次触发成功。
- **出处**：development-plan.md:2483；`executor.rs:1528、1748-1751`；`supervisor.rs:70`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（恢复耗时：____）

### M4-SV-03 executor_hang（阻塞 200 秒）

- **步骤**：
  1. 用 debug-fi 执行 `await inv('debug_inject_fault', { point: 'executor_hang', param: 200000 })`。
  2. 在记事本执行 `zzk` + 空格，开始计时。
  3. 15 秒后，在记事本先按一次物理空格，再执行一次 `zzk` + 空格。
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
  3. 按一次物理空格，把 `x` 和后面的触发词隔开，再执行 `zzk` + 空格。
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
  3. 等状态行回到「监听中」后，先按一次物理空格，再执行 `zzk` + 空格。
  4. 变体 b：再布防一次，**不打字**，等 30 秒后检索日志，看是否不靠按键也能检出并重挂。
- **预期结果**：
  - 日志依次有 `fault.injected point="hook_unhook"`、`input.check source="keyboard_hook" result="lost"`、`input.reattach component="keyboard_hook" … ok=true`（`input.rs:839-844`；字符串字段在日志里带双引号，检索时注意）。
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
  3. 等状态行回到「监听中」后，先按一次物理空格，再执行 `zzk` + 空格。
  4. 检索日志。
- **预期结果**：
  - 日志依次有 `fault.injected point="rawinput_unregister"`、`input.check source="raw_input" result="unregistered"`、`input.reattach component="raw_input" reason="raw_input_missing" ok=true`（`input.rs:693-697、955-963`）。
  - 展开成功。
- **通过标准**：由交叉校验检出并重新注册；不需要手动操作。
- **出处**：development-plan.md:2448、2485；待实测 `rawinput.rs:933`（从不拥有窗口的线程注册，失败时退回到重建）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（走的是重新注册还是重建：____）

### M4-SV-07 rawinput_close

- **步骤**：
  1. 用 debug-fi 执行 `await inv('debug_inject_fault', { point: 'rawinput_close', param: 0 })`。
  2. 等 5 秒，输入几个字母；等状态行回到「监听中」后，先按一次物理空格，再执行 `zzk` + 空格。
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
  4. 先按一次物理空格，再执行 `zzk` + 空格。
  5. 检索日志里的 `input.check`、`input.queue_full`。
  6. 把键盘设置还原。
- **预期结果**：没有 `input.check … result="lost"` 误判，没有 `input.queue_full`；展开成功。
- **通过标准**：自动重复和连按都不会引起误判或重挂。
- **出处**：待实测 `liveness.rs:52`（自动重复每秒 2.5–30 次）、`liveness.rs:115`（连按五十次、丢失 key-up）、`hook.rs:32`、`rawinput.rs:101`（队列容量）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-SV-09 管理员窗口（只有一个输入源能看到）与按需自检

- **步骤**：
  1. 以管理员身份运行一个记事本，在里面输入 20 个字母和 `zzk` + 空格。
  2. 切回普通记事本，立即按一次物理空格，再执行 `zzk` + 空格。
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
  4. P2 结束后点顶部的「恢复监听」，然后在记事本先按一次物理空格，再执行 `zzp` + 空格。
  5. 检索日志里的 `self_test.check`、`self_test.startup`。
- **预期结果**：
  - 系统通知标题「启动自检未通过」，正文「未通过：剪贴板。输入监听保持暂停，可从托盘菜单选择「恢复监听」，或打开管理界面查看引擎状态。」（`startup.rs:348-381`）。
  - 横幅显示「启动自检未通过：…，没有自动开始监听。…排除问题后可点「恢复监听」。」（`src/status.ts:146-210`），状态行显示「已暂停」。
  - 恢复后粘贴成功。
- **通过标准**：自检失败时不自动开始监听，并且有明确提示；恢复后功能正常。
- **出处**：B-10；`self_test_probes.rs:139-152`；`App.tsx:1429`（提示文字说明全部通过才开始监听）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（横幅里列出的检查项名称：____。横幅和系统通知里的检查项名称应一致；键盘钩子心跳一项在两处都叫「键盘鼠标监听」，`e0fc068` 已修，见 §12.3 第 7 条）

### M4-SV-12 修复过期的自启动项（只用 release）

- **步骤**：
  1. 确认「登录后自动启动」已勾选，然后退出程序。
  2. 在 PowerShell 里先保存原值，再写入过期路径，并读回确认（用 `Set-ItemProperty`，避免 `reg add` 在 PowerShell 里转义引号出错）：

     ```powershell
     $run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
     $orig = (Get-ItemProperty -Path $run -Name QuickerText -ErrorAction SilentlyContinue).QuickerText
     "原值：$orig"
     Set-ItemProperty -Path $run -Name QuickerText -Value '"C:\old\quicker-text.exe" --background'
     (Get-ItemProperty -Path $run -Name QuickerText).QuickerText
     ```

     读回的值必须正好是 `"C:\old\quicker-text.exe" --background`（路径带双引号），否则不要继续。把「原值」抄进备注。
  3. 手动启动 release 构建，观察横幅，检索日志。
  4. 在同一个 PowerShell 窗口执行 `(Get-ItemProperty -Path $run -Name QuickerText).QuickerText`，读回修正后的值。
  5. 还原：如果第 2 步的原值不为空，执行 `Set-ItemProperty -Path $run -Name QuickerText -Value $orig`；如果原值为空（原来没有这一项），执行 `Remove-ItemProperty -Path $run -Name QuickerText`。PowerShell 窗口已经关闭时，用备注里抄下的原值替换 `$orig`。
- **预期结果**：
  - 横幅显示「登录自动启动项指向的程序位置已过期，已自动修正。」。
  - 日志有 `autostart.repair`。
  - 第 4 步读回的值改为当前 exe 的路径（带双引号）加 ` --background`。
- **通过标准**：自动修正并有提示；还原后注册表与测试前一致。
- **出处**：development-plan.md:2488；`src-tauri/src/autostart.rs:12-14、90-115`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（原值：____；修正后的值：____）

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
  - 注入后日志依次有 `fault.injected point="session_end"`、`session.end source="fault" callbacks=N`（`session.rs:169、216-221`），紧接着有 `store.run_marker_cleared`：会话结束回调本身就清除运行标记，不再依赖 `RunEvent::Exit`（`241b3f3`，复审 R5；存储分支 `src-tauri/src/store/meta.rs:164-200`）。
  - taskkill 之后连续两次启动都正常：没有 `store.crash_loop`，没有进入「连续多次启动失败」的安全模式。
- **通过标准**：注入后立即有 `store.run_marker_cleared`；两次启动都没有 crash_loop。
- **出处**：development-plan.md:2489；`input.rs:997`；`session.rs:216-221`；`241b3f3`（R5）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（注入后进程是否继续监听：____。如果没有 `store.run_marker_cleared`，先确认构建包含 `241b3f3`）

### M4-SV-15 熔断：10 分钟内 5 次故障进入「故障」，手动修复（必做）

- **步骤**（debug-fi，独立数据目录；不要给 `zzk` 打开执行通知，以免和要观察的系统通知混淆）：
  1. 记下开始时间。以下 5 轮必须在 10 分钟内做完（`CIRCUIT_WINDOW`，`supervisor.rs:78、80`）。
  2. 每一轮：
     1. 在开发者工具执行 `await inv('debug_inject_fault', { point: 'executor_panic', param: 0 })`。
     2. 点一下记事本，按一次物理空格，执行 `zzk` + 空格（这一次失败）。
     3. 等引擎状态回到「正常」、状态行回到「监听中」，再开始下一轮。前 4 轮每轮都应自动恢复。
  3. 第 5 轮之后不要再操作，观察：
     - 「运行状态与恢复」的总体状态、状态行和「修复引擎」按钮；
     - 托盘图标和托盘提示文字；
     - 有没有系统通知。
  4. 等 3 分钟，确认不会自行恢复；期间按一次物理空格后执行 `zzk` + 空格。
  5. 点「修复引擎」。等状态回到「正常」后，按一次物理空格，执行 `zzk` + 空格。
  6. 检索日志里的 `supervisor.`、`executor.panic`、`executor.rebuilt`。
- **预期结果**：
  - 日志里 `supervisor.recovering` 的 `incidents=` 依次为 1、2、3、4、5；第 5 次之后紧接着 `supervisor.circuit_open cause="incidents"`（`supervisor.rs:529-546、804-814`）。
  - 第 5 轮之后：总体状态「故障」，状态行「输入监听已停止」，出现「修复引擎」按钮（`src/StatusArea.tsx:68-74`、`src/status.ts:75-76`）；托盘提示文字「Quicker Text · 输入监听已停止」（`crates/platform/src/health.rs:327`）。托盘、提示文字和界面三处一致。
  - **系统通知**：S-03 要求熔断时发通知。代码里没有熔断通知（托盘只在恢复监听被拒绝、启动自检失败、规则执行时发通知），**预计此项不通过**，记为缺陷（§12.3 第 16 条）。
  - 第 4 步：不自行恢复；`zzk` 不展开，历史显示失败，记事本里没有任何输出。
  - 第 5 步：界面提示「已开始修复引擎，结果会自动更新」；日志依次有 `supervisor.repair`、`executor.rebuilt`、`supervisor.recovered`；回到「正常」，`zzk` 展开成功。
- **通过标准**：第 5 次故障后进入「故障」且三处一致；不自行恢复；「修复引擎」能恢复。系统通知一项单独判定（预计不通过），不影响前面几项的判定，但 S-03 要在系统通知补上并复测后才算完成。
- **出处**：S-03（development-plan.md:704）；`supervisor.rs:70、78、80、529-546、804-814、884-925`；`src/App.tsx:735`；`src-tauri/src/tray.rs:78-119`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（5 轮用时：____；系统通知：☐有 ☐无；三处是否一致：____）

### M4-SV-16（可选，仅观察）鼠标钩子不误判丢失

- **步骤**（release；只观察，不判通过与否）：
  1. 在记事本里快速连续单击 30 次、双击 10 次、拖动选中文字 10 次，然后滚动滚轮几下。
  2. 检索这段时间的日志里的 `input.check source="mouse_hook" result="lost"`。
  3. 可选：做 SV-05（debug-fi）时，在 `hook_unhook` 布防之后点几下鼠标，看日志里是否出现 `input.check source="mouse_hook" result="lost"` 和随后的 `input.reattach component="mouse_hook" reason="hook_missing" ok=true`。
- **预期结果**：
  - 第 1 步不出现 `result="lost"`。
  - 触摸、触控笔和外部注入的点击不参与交叉校验，被忽略。
- **说明**：这一分支（F5-2，鼠标钩子丢失检出与重挂）由 L2 测试覆盖，均在鼠标重挂分支 `384f8d1` 上：`crates/platform/src/windows/pipeline_tests_input.rs:3891、3936、4247`；日志出处 `crates/platform/src/windows/input.rs:1009-1012`。本条只补充实机观察。
- **出处**：鼠标重挂分支 `384f8d1`；S-06。
- **结果**：日期 ____；机器 ____；构建 ____；☐已观察 ☐未测；备注（是否出现 `result="lost"`：____）

### M4-SV-17 手动测试动作：真实执行、出错、卡住与并发上限

编辑器里的「测试动作（真实执行）」和「单步测试（真实执行）」都调用 `execute_action`，过程如下（`runtime.rs:52-144`）：
1. 先检查安全模式和并发上限。
2. 隐藏主窗口，3 秒后在当时的前台窗口执行。
3. 结束后显示主窗口。

每次测试在自己的线程里执行。测试卡住时，执行器把它围栏（fence）掉，命令立即返回；但这个线程要等它自己醒来才结束，结束前一直占着一个名额。名额最多 4 个（`MAX_MANUAL_JOBS`）。名额满了以后再点测试，会被直接拒绝：不隐藏窗口，也不写执行历史（`runtime.rs:150-161、197-215、236-256`）。手动测试不计入上次正常配置（`lkg.rs:19-23`）。

- **步骤**：
  1. a 正常（release 或 debug-fi）：
     1. 打开记事本，放在主窗口后面。在「文本指令管理」打开 `zzk` 的「编辑文本指令」。
     2. 点「测试动作（真实执行）」，在 3 秒内点一下记事本的编辑区。
     3. 等主窗口回来，看弹出的结果和「最近执行」。
     4. 从「应用设置」起如果只做过手动测试、没有在别处触发缩写，再等 70 秒，检索日志里的 `store.last_good`。
  2. b 执行出错（debug-fi）：
     1. 布防 `executor_panic`（§1.6），按 a 的第 1、2 步再测一次。
     2. 记下界面提示里的追踪编号，检查数据目录里的 `crash-*.txt`，检索日志。
  3. 退出并重启程序。b 的 panic 和下面 4 次卡住各算一次故障，不重启的话，10 分钟内合计 5 次会触发熔断（SV-15）。
  4. c 卡住（debug-fi）：
     1. 布防 `executor_hang`，param 200000（§1.6）。
     2. 按 a 的第 1、2 步再测一次，开始计时。
     3. 观察主窗口何时回来，以及界面提示、引擎状态和「最近执行」。
  5. d 并发上限（debug-fi，紧接 c，c 算第 1 轮）：
     1. 每轮先等引擎状态回到「正常」，然后布防 `executor_hang` 200000，点「测试动作（真实执行）」，等主窗口回来。这样再做 3 轮。4 轮要在第 1 轮开始后 200 秒内做完。
     2. 第 4 轮结束、状态回到「正常」后，立即再点一次「测试动作（真实执行）」（不布防）。
     3. 保持记事本在前台，等到第 4 轮开始后满 200 秒，再按 a 测一次。
     4. 检索日志里的 `command.execute_`、`executor.`、`supervisor.`。
- **预期结果**：
  - a：
    - 点击后主窗口立即隐藏，约 3 秒后记事本里出现 `Hello QuickerText 0.4 你好`，随后主窗口回来，弹出「动作测试结果」。
    - 「最近执行」多一条，方式为「手动测试」，结果为成功。
    - 第 4 步：日志里没有 `store.last_good`。手动测试不算「在触发路径上成功展开一次」。
  - b：
    - 记事本里没有输出：panic 发生在第一批发送之前。
    - 主窗口回来后，界面提示「操作没有完成，发生了内部错误（追踪编号 <id>）。可在「运行状态」中导出诊断包反馈问题。」。
    - 「最近执行」多一条，方式为「手动测试」，结果为「失败 · 执行器内部错误」。
    - 日志有：
      - `app.panic`；
      - `executor.panic stage="…" kind="str" epoch=<n> manual=true`；
      - `command.execute_failed category="run_failed" reason=ExecutorPanic trace_id=<id>`，其中 `<id>` 与界面上的相同；
      - 随后 `supervisor.recovering`、`executor.rebuilt`、`supervisor.recovered`。
    - 生成一份 `crash-*.txt`，进程没有退出。
  - c：
    - 主窗口隐藏，记事本里没有输出。
    - 点击后约 13 秒（3 秒等待加 10 秒 STALL_BUDGET，巡检最多再晚 1 秒），主窗口回来。界面提示「输入引擎暂时不可用（恢复中），请稍后再试，或在「运行状态」中修复引擎。」。
    - 日志依次有：
      - `executor.stalled stalled_ms=… budget_ms=10000 manual=true`；
      - `executor.fenced epoch=<n> in_flight=false manual=true`；
      - `command.execute_failed category="stalled" reason=ExecutorTimeout health=Recovering`；
      - 随后 `supervisor.recovering`、`executor.rebuilt`、`supervisor.recovered`。
    - 「最近执行」多一条，方式为「手动测试」，结果为「失败 · 执行超时」。这一条由围栏代写，因为卡住的线程自己写不了。
    - 引擎状态经「恢复中」回到「正常」。
  - d：
    - 第 1 步：每轮都同 c。日志里 `supervisor.recovering` 的 `incidents=` 依次增加，不到 5 次，所以不熔断。
    - 第 2 步：
      - 主窗口不隐藏，立即提示「上一个动作仍在执行，请稍后再试。」。
      - 「最近执行」没有新条目，日志有 `command.execute_refused reason="jobs_exhausted" limit=4 health=Healthy`。
      - 如果这时状态不是「正常」（例如还在「恢复中」），提示会变为「输入引擎暂时不可用（恢复中）…」，日志的 `health=` 也跟着变。这种情况不判不通过：等「正常」后再点一次。
    - 第 3 步：
      - 200 秒内和之后，记事本里都没有多出来的字符或退格。卡住的线程醒来后什么都不发。
      - 名额在线程结束时释放，最后一次测试同 a，能成功。
  - 第 4 步检索到的 `command.execute_failed` 和 `command.execute_refused` 都只带原因码、健康状态、上限和追踪编号，没有动作正文。
- **通过标准**：
  - a 在记事本里只输出一次，且不记录上次正常配置。
  - b 界面和日志的追踪编号一致，并且自动恢复。
  - c 约 13 秒内返回并恢复，之后没有多余输出。
  - d 第 5 次被直接拒绝、不隐藏窗口；卡住的线程结束后又能测试。
- **说明**：有几种情况实机造不出来，由单元测试覆盖：
  - 线程启动失败（`category="spawn_failed"`）：`runtime.rs:1139`；
  - 等线程时没有得到结果（`CommandError::task_failed`，记 `command.internal`）：`runtime.rs:815`；
  - 名额与执行器占用的先后：`runtime.rs:986`、`1046`。
- **出处**：integrate `f1589cb`：
  - `src-tauri/src/commands/runtime.rs`：
    - `52-144`：`execute_action`。其中 60 行检查安全模式，61 行准入，85 行隐藏窗口，103 行等 3 秒，138 行显示窗口。
    - `150-161`：`MAX_MANUAL_JOBS`；`197-215`：`admit`；`236-256`：`jobs_exhausted`。
    - `432-461`：`conclude`；`463-482`：`stalled`；`484-496`：`run_failed`；`498-513`：`spawn_failed`。
    - 测试：`617`、`682`、`747`、`815`、`885`、`986`、`1046`、`1139`。
  - `crates/platform/src/windows/executor.rs`：
    - `1373-1436`：`run_manual`，panic 时记 `executor.panic … manual=true`；
    - `1641-1658`：围栏时把执行器标为 `ExecutorTimeout` 故障，交给监督器重建；
    - `1664-1707`：替卡住的测试写历史，记 `executor.fenced`；
    - `1732-1767`：`executor.stalled`；
    - `2266-2274`：注入点；`2331-2333`：手动测试每批发送前经过注入点。
  - `src-tauri/src/lkg.rs:19-23`：手动测试不计入上次正常配置。
  - `crates/platform/src/supervisor.rs:529-545`：故障计数与熔断。
  - 前端：
    - `src/ActionEditor.tsx:358-364`、`1547`、`1608-1617`；
    - `src/ipc/errors.ts:210-225`；
    - `src/ipc/labels.ts:40、160-161、172`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（c 从点击到主窗口回来的耗时：____；d 第 2 步的提示和 `health=`：____；b 的追踪编号：____）

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
    - 横幅显示「已进入安全模式：配置文件被其他程序占用。输入监听已暂停，期间不会写入任何文件；占用解除后会自动退出安全模式，之后点「恢复监听」即可继续。也可以打开数据目录检查。」（`src/status.ts:151-153`，`cfa41f2`；不再提「导入配置」）；
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
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（Locked 横幅已不再提「导入配置」，与这里按设计拒绝导入一致；`cfa41f2` 已修，见 §12.3 第 6 条）

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
    - `store.corrupt file=active.json code="config_malformed" kind="eof" line=… column=…`；
    - `store.quarantine file=active.json quarantined_as=…`；
    - `store.recovered file=<n>.json from=… revision=Some(<n>)`。
  - 数据目录里出现 `active.corrupt-<时间>.json`。
  - 恢复来源取决于 `store-meta.json` 里有没有 `last_good_revision`（候选顺序：active → 上次正常配置 → revisions 从新到旧 → previous）：
    - 有（程序此前在「正常」状态持续 60 秒以上并成功展开过一次，ST-13）：`from=LastGood`，横幅「配置文件无法读取，已自动从上次正常运行时的配置恢复。请检查最近的改动是否还在。」；
    - 没有：`from=Revision`，横幅「配置文件无法读取，已自动从历史版本 v{n} 恢复。请检查最近的改动是否还在。」。
  - 另有横幅「active.json 已损坏，已改名为 … 保留，可在数据目录中查看。」。
  - 草稿不受影响。
  - 第 3 步恢复横幅再次出现，因为恢复时不重写 `active.json`，这是设计如此（§12.3 第 5 条）。第 4 步之后不再出现。
- **通过标准**：
  - 不进入安全模式，自动恢复到上次正常配置或最近的可用版本；
  - 损坏的文件被保留；
  - 记下恢复来源和版本号。
- **出处**：development-plan.md:2487（config_corrupt）；`43dda1f`：`src-tauri/src/store/fallback.rs:155-373`（`load_or_fallback`，`store.recovered` 在 230-236）、`fallback.rs:612-648`（`fallback_candidates` 候选顺序）、`fallback.rs:516-587`（`store.corrupt`、`store.quarantine`；`store-meta.json` 没有 `backup_pre_0_4` 时改为 `store.quarantine_deferred`，见 §1.3）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（`from=____`，恢复到 v____，应用前的生效版本为 v____，`last_good_revision`=____）

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
  - 横幅显示「已进入安全模式：配置文件来自更新的版本。当前版本以只读方式打开，不会修改或覆盖它，输入监听已暂停；请改用更新版本的 Quicker Text，或打开数据目录检查。」（`src/status.ts:154-155`，`cfa41f2`；不提「导入配置」），处于只读状态，不写运行标记。
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
  - 第 3 步：日志有 `store.crash_loop crash_count=2` 和 `store.safe_mode reason=CrashLoop`；横幅显示「已进入安全模式：连续多次启动失败。输入监听已暂停，设置暂时不能修改；可以导入配置或打开数据目录检查。」（`src/status.ts:156`）；「启动时自动开始监听」可以操作。
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
  - 第 2 步：所有损坏的文件都被隔离；日志有 `store.safe_mode reason=NoValidConfig`；横幅显示「已进入安全模式：没有找到可用的配置。输入监听已暂停，设置暂时不能修改；可以导入配置或打开数据目录检查。」（`src/status.ts:156`）。
  - 第 3 步：导入写入草稿，提示「导入草稿已保存，请应用设置」，此时仍在安全模式。
  - 第 4、5 步：「应用设置」在安全模式下仍可点击。发布成功后日志有 `store.safe_mode_import`，安全模式解除，规则出现。监听仍然暂停，需要点「恢复监听」。
- **通过标准**：导入加应用可以解除安全模式，且规则完整。
- **出处**：`src-tauri/src/store/mod.rs:85-94`（只有 CrashLoop 和 NoValidConfig 能用导入解除）、`mod.rs:290-299`；`src-tauri/src/store/revisions.rs:20-31、110-113`；`src/App.tsx:528-532`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-ST-11 prefs.json 损坏

- **步骤**：退出程序，把 `prefs.json` 的内容改成 `{"app":`，然后启动。
- **预期结果**：
  - 日志有 `store.quarantine file="prefs.json" to=Some("prefs.corrupt-<时间>.json") reason=Eof line=1 column=…`（`src-tauri/src/store/prefs.rs:352-360`；这里的字段名是 `to=`，与配置文件的 `quarantined_as=` 不同）。
  - 数据目录里出现 `prefs.corrupt-<时间>.json`，程序使用默认偏好正常启动，不进入安全模式。
- **通过标准**：自动隔离并使用默认值。
- **出处**：`src-tauri/src/store/prefs.rs`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：是否显示 config_quarantined 横幅）

### M4-ST-12 S-04 损坏样本：非法 UTF-8、规则字段类型错误、无法编译的正则、某个版本文件损坏

背景：S-04 要求 5 种损坏样例下都能启动且不丢数据。截断（`eof`）见 ST-05、ST-06，所有版本都损坏见 ST-10，来自更新版本见 ST-08。本条补其余四类：整个文件无法解码（a）、单条规则读不了（b）、规则能读但编译不了（c）、回退时某个版本文件也坏了（d）。

每个样本都在已备份的数据目录上做，做完把整个目录还原，再做下一个。

- **步骤**：
  - a）非法 UTF-8：
    1. 退出程序，确认 `revisions\` 下有可用的历史版本。
    2. 在 `active.json` 的第一个双引号后插入字节 `FF FE`，然后记下哈希：

       ```powershell
       $p = "$d\active.json"; $b = [IO.File]::ReadAllBytes($p)
       $i = [Array]::IndexOf($b, [byte]0x22)
       [IO.File]::WriteAllBytes($p, [byte[]]($b[0..$i] + [byte[]](0xFF, 0xFE) + $b[($i + 1)..($b.Length - 1)]))
       Get-FileHash $p -Algorithm SHA256
       ```

    3. 启动，观察横幅、数据目录和日志；用 `Get-FileHash` 查看新出现的 `active.corrupt-<时间>.json`。
  - b）规则字段类型错误：
    1. 退出程序。用记事本打开 `active.json`，找到 `zzp` 那条规则，把它的 `"enabled": true` 改成 `"enabled": "yes"`，记下这条规则的 `id`。
    2. 启动，观察横幅、规则列表和日志；在记事本里输入 `zzk` 加空格、`zzp` 加空格；导出诊断包（不包含短语内容）。
    3. 在「运行状态与恢复」点「放弃未应用草稿」→「放弃草稿」。`draft.json` 是单独读取的，第 1 步只改了 `active.json`，不放弃草稿的话，编辑器里显示的仍是改之前的草稿（`src-tauri/src/store/mod.rs:319-330`）。
    4. 改另一条规则（例如在 `zzk` 的正文末尾加一个字），保存，点「应用设置」。
    5. 退出程序，用记事本打开新的 `active.json`，查找第 1 步记下的 id。
  - b′）（可选）顶层或 `globals` 字段类型错误：退出程序，把 `active.json` 顶层的 `"enabled": true` 改成 `"enabled": "yes"`，或把 `globals` 里的 `"word_separators"` 改成数字 `5`，然后启动。
  - c）无法编译的正则：
    1. 退出程序。在 `active.json` 的 `rules` 数组末尾追加一条规则：复制 `zzk` 那条规则的整个对象，把 `id` 改成 `st12-regex`，把 `match` 改成 `{"kind": "regex", "value": "zzx("}`，其他不变（保持启用，触发方式仍为按键后执行）。别忘了在前一条规则后面加逗号。
    2. 启动后同 b 第 3 步，放弃未应用草稿。
    3. 观察横幅、规则列表和日志；输入 `zzk` 加空格；导出诊断包。
    4. 直接点「应用设置」；然后在编辑器里把 `st12-regex` 停用（或把正则改成 `zzx\(`），保存，再点「应用设置」。
  - d）某个版本文件损坏：
    1. 退出程序。打开 `store-meta.json`，记下 `last_good_revision`（没有这个键就记「无」）。在 `revisions\` 下确定启动时最先尝试的版本文件 `<k>.json`（不含 `.metadata.json`）：有 `last_good_revision` 时 k 就是它，没有时 k 是编号最大的那个。确认除它之外至少还有一个 `<n>.json`。
    2. 用记事本把 `active.json` 和 `revisions\<k>.json` 的内容都改成 `{`，保存。
    3. 启动，观察横幅、数据目录、`revisions\` 目录和日志。
    4. 点「应用设置」，记下新的「当前生效版本」。
- **预期结果**：
  - a）
    - 日志依次有：
      - `store.corrupt file=active.json code="config_malformed" kind="utf8" line=0 column=0`（非法 UTF-8 没有位置）；
      - `store.quarantine file=active.json quarantined_as=active.corrupt-<时间>.json`；
      - `store.recovered …`。
    - 恢复来源和横幅与 ST-06 相同，不进入安全模式。
    - `active.corrupt-<时间>.json` 的哈希与第 2 步记下的一致：损坏的文件被原样保留。
  - b）
    - 正常启动，不进入安全模式，没有文件被隔离。
    - 日志有 `store.diagnostic code=rule_unreadable path=rules[<序号>]` 和 `store.rules_skipped count=1`（`src-tauri/src/store/fallback.rs:224-243、570-573`）。
    - 横幅「有 1 条规则无法读取或无法使用，已跳过：「<id>」。其他规则不受影响。」（`src/status.ts:179-184`）。
    - `zzk` 照常展开，`zzp` 不展开。
    - 诊断包 `active.counts.rules` 比原来少 1：读不了的条目不计入规则数（`src-tauri/src/diagnostics/bundle.rs:634-694`）。
    - 第 3 步之后，规则列表末尾出现「无法读取的规则」行，带 id，说明为「这条规则含有当前界面无法编辑的内容，只能查看；已保留在配置中。」（`src/App.tsx:1270-1290`）。
    - 第 5 步：新 `active.json` 的 `rules` 数组里仍有这条规则，`"enabled": "yes"` 原样保留，没有被删掉或改成默认值（`crates/domain/src/stored.rs:282-297`；`crates/domain/src/model.rs:48-51`）。
  - b′）正常启动；日志有 `store.diagnostic code=field_reset path=enabled` 或 `path=globals.word_separators`；该字段按默认值使用；没有「已跳过」横幅（`crates/domain/src/stored.rs:181-228`）。
  - c）
    - 正常启动；日志有 `store.diagnostic code=regex_unsupported path=rules[<序号>].match.value` 和 `store.rules_skipped count=1`；横幅同 b，id 为 `st12-regex`。
    - 规则列表里 `st12-regex` 作为普通规则显示，可以编辑，但不生效；`zzk` 照常展开。
    - 诊断包 `active.counts.diagnostics` 里有 `code` 为 `regex_unsupported`、`severity` 为 `error`、`count` 为 1 的一项。
    - 第 4 步：带着这条规则直接「应用设置」会被拒绝，因为发布和保存草稿都严格编译（`src-tauri/src/store/revisions.rs:33`、`src-tauri/src/store/mod.rs:300-304`）。界面提示以「内容没有通过检查：」开头，后面是后端的英文原文，因为 `regex_unsupported` 没有中文文案（`src/ipc/errors.ts:174-187`）；记下原文。生效版本不变。停用或改正之后可以保存和应用。
  - d）
    - 日志依次有：
      - `store.corrupt file=active.json …` 和 `store.quarantine file=active.json …`；
      - `store.corrupt file=<k>.json code="config_malformed" kind="eof" …` 和 `store.quarantine file=<k>.json quarantined_as=<k>.corrupt-<时间>.json`；
      - `store.recovered file=<m>.json from=Revision revision=Some(<m>)`，m 是除 k 以外编号最大的版本（候选顺序：上次正常配置 → revisions 从新到旧，已试过的 k 不再重复，`src-tauri/src/store/fallback.rs:612-648`）。
    - 数据目录里出现 `active.corrupt-<时间>.json`，`revisions\` 里出现 `<k>.corrupt-<时间>.json`，都原样保留。
    - 不进入安全模式。横幅「配置文件无法读取，已自动从历史版本 v<m> 恢复。请检查最近的改动是否还在。」，另有两条「… 已损坏，已改名为 … 保留，可在数据目录中查看。」（`src/status.ts:160-179`）。
    - 第 4 步可以应用。新版本号是「当前版本 m」和「`revisions\` 下本程序文件名里出现过的所有编号」中的最大者加 1。这些文件名包括 `<n>.json`、`<n>.metadata.json`、隔离出去的 `<n>.corrupt-<时间>.json` 和 `<n>.metadata.corrupt-<时间>.json`，以及写到一半的 `<n>.json.tmp`；其他文件名不算（`src-tauri/src/store/revisions.rs:159-180、194-228`，复审 R7-2、R8-2）。所以新版本号**一定大于 k**，不会再次用 v<k>，`<k>.corrupt-<时间>.json` 和 `<k>.metadata.json` 都不会被覆盖。通常还会大于损坏之前的生效版本：那个版本发布时写过 `<n>.metadata.json`，编号已被占用。新版本号不大于 k 判为不通过。
    - 第 4 步之后，`revisions\` 里有 `<m>.json`（原本就在）和新版本的 `<新>.metadata.json`；新版本自己的 `<新>.json` 要等它被下一次发布取代、或被记为上次正常配置时才写（`revisions.rs:82-129`；`integrate/0.4` 的 `store/mod.rs:301-341`）。
- **通过标准**：四类损坏都不导致启动失败或丢数据。整个文件坏了就隔离并恢复，回退途中遇到的坏版本也隔离并跳过；单条规则坏了只跳过这一条，读不了的原文在保存后仍保留；横幅、日志、诊断包三处的结论一致。
- **出处**：S-04（development-plan.md:705）；B-43；`crates/domain/src/stored.rs:112-155、251-300`；`src-tauri/src/store/fallback.rs:142-194、224-243、389-460、544-574`（这一处是 `576aa5a` 的行号，用于 a～c）；d 用 `43dda1f`：`fallback.rs:499-587`（读取、`store.corrupt`、隔离）、`fallback.rs:612-648`（候选顺序）、`revisions.rs:159-228`（新版本号，测试 `revisions.rs:339` `every_name_of_this_program_occupies_its_revision`）；`crates/domain/src/matching.rs:312-342、435-453`。
- **结果**：日期 ____；机器 ____；构建 ____；a ☐通过 ☐不通过 ☐未测；b ☐通过 ☐不通过 ☐未测；b′ ☐通过 ☐不通过 ☐未测；c ☐通过 ☐不通过 ☐未测；d ☐通过 ☐不通过 ☐未测；备注（c 第 4 步的界面提示原文：____；d：k=____，m=____，第 4 步的新版本 v____）

### M4-ST-13 上次正常配置（LKG）：记录、恢复，以及恢复后发布再回滚

- **前提**：
  - 构建是「`integrate/0.4` 合并后的构建」，其中包含：上次正常配置分支的 `b32062f`；存储分支的 `7b727ca`（复审 R3，回滚目标记在 `store-meta.json`）、`5a75e16`（记录带 `active.json` 的指纹）、`e70dab1`（R7，版本号不复用已被占用的号码）和 `43dda1f`（R8，有记录时不再推测回滚目标）。
  - 在已备份的数据目录上做，`store-meta.json` 里已有 `backup_pre_0_4`（§1.3）。
  - 准备数据目录以外的一个文件夹，下文用 `D:\qt-st13\`。
- **步骤**：
  1. 在「文本指令管理」把 `zzk` 的正文末尾加上 `-A`，保存，点「应用设置」。在「运行状态与恢复」记下「当前生效版本」vA。
  2. 等 2～3 秒，在记事本里输入 `zzk` 加空格，确认展开出的文字以 `-A` 结尾。然后保持「正常」，从应用算起至少 70 秒，期间不要注入故障、不要锁屏。检索日志里的 `store.last_good`，打开 `store-meta.json` 查看，确认 `revisions\<vA>.json` 存在。
  3. 把 `-A` 改成 `-B`，保存，应用，记下 vB。**之后不要再触发任何展开**，直接从托盘「退出」。打开 `store-meta.json`，记下 `previous_revisions` 的内容。
  4. 先把完好的 `active.json`（B 版）复制到数据目录以外，第 8 步要用：

     ```powershell
     Copy-Item "$d\active.json" D:\qt-st13\B.json
     ```

     然后执行 P3 截断 `active.json`，启动。
  5. 观察横幅、「当前生效版本」和「草稿状态」，在记事本里输入 `zzk` 加空格，检索日志。
  6. 点「应用设置」，记下新的生效版本 vN，然后退出程序。
  7. 打开 `store-meta.json` 查看 `previous_revisions`，再算出 `active.json` 的长度和 SHA-256：

     ```powershell
     (Get-Item "$d\active.json").Length
     (Get-FileHash "$d\active.json" -Algorithm SHA256).Hash.ToLower()
     ```

  8. 放一个诱饵：先确认 `revisions\<vN-1>.json` 不存在（vN-1 通常就是 vB），然后把第 4 步的 B 版复制过去：

     ```powershell
     Copy-Item D:\qt-st13\B.json "$d\revisions\<vN-1>.json"
     ```

     启动程序，在「运行状态与恢复」点「恢复上一个版本」→「回滚」。
  9. 在记事本里输入 `zzk` 加空格，查看「当前生效版本」。
  10. 做完把数据目录整个还原，不要把诱饵文件留在里面。
- **预期结果**：
  - 第 2 步：
    - 约 60 秒后日志有 `store.last_good revision=<vA>`（`integrate/0.4` 的 `src-tauri/src/store/mod.rs:336-340`）；
    - `store-meta.json` 里 `"last_good_revision": <vA>`；
    - `revisions\<vA>.json` 存在。这个快照是记录上次正常配置时写下的，发布本身不写新版本的快照（`mod.rs:301-341`）。
  - 第 3 步：
    - `last_good_revision` 仍是 vA：B 版没有成功展开过，不满足条件。
    - vB 通常是 vA+1。
    - `previous_revisions` 有两项（I-R1）：
      - 第一项是 `{"active": <vB>, "revision": <vA>, "fingerprint": {…}}`；
      - 第二项是发布 vA 时写下的那一项（`"active": <vA>`），因为它与发布 vB 之前磁盘上的 `active.json` 相符，所以被保留下来。
  - 第 5 步：
    - 日志依次有：
      - `store.corrupt file=active.json code="config_malformed" kind="eof" …`；
      - `store.quarantine file=active.json quarantined_as=active.corrupt-<时间>.json`；
      - `store.recovered file=<vA>.json from=LastGood revision=Some(<vA>)`。
    - 横幅「配置文件无法读取，已自动从上次正常运行时的配置恢复。请检查最近的改动是否还在。」，另有「active.json 已损坏，已改名为 … 保留，可在数据目录中查看。」。
    - 不进入安全模式；「当前生效版本」为 vA，`zzk` 展开出 `-A`。
    - `draft.json` 没有损坏、单独读取，所以编辑器里 `zzk` 可能仍显示 `-B`，「草稿状态」为「存在未应用的设置」。这是现状，不算失败，记下即可。
  - 第 6 步：
    - 发布成功，vN 通常是 vB+1。`revisions\<vB>.json` 并不存在：vB 既没有被下一次发布取代，也没有被记为上次正常配置。但发布 vB 时写下的 `revisions\<vB>.metadata.json` 已经占用了 vB 这个编号（`revisions.rs:159-180、194-228`，R7-2）。
    - vN 不大于 vB 判为不通过：那样会覆盖 vB 的元数据，旧记录也可能被错当成新版本的记录。
  - 第 7 步：
    - `previous_revisions` **只有一项**：`{"active": <vN>, "revision": <vA>, "fingerprint": {"len": <长度>, "sha256": "<小写十六进制>"}}`。
    - `len` 和 `sha256` 与第 7 步算出的长度和小写哈希一致。`Get-FileHash` 输出大写，所以命令里用了 `.ToLower()`。
    - 只有一项，是因为发布 vN 时 `active.json` 已被隔离，磁盘上没有与旧记录相符的文件，旧记录不再保留（`revisions.rs:48-61、112-127`）。
    - 有第二项，或 `revision` 不是 vA，判为不通过。
    - 如果文件里还有旧版程序写的 `previous_revision` 键，它保持原样，不参与判断。
  - 第 9 步：
    - `zzk` 展开出 `-A`，即回到恢复时的版本 vA，而不是诱饵里的 vB。
    - 「当前生效版本」为 vN+1，因为回滚本身也是一次发布。
    - 回到 `-B` 判为不通过：说明程序没有用记录，而是按 `revisions\<vN-1>.json` 推测回滚目标。
    - 为什么要放诱饵：没有可用的记录时，程序依次推测 `revisions\<vN-1>.json`、版本低于当前版本的 `previous.json`、低于当前版本的最近一个快照（`fallback.rs:670-726`，R8-1）。不放诱饵时 `<vN-1>.json` 不存在，推测最后也会落到最近的快照 vA，看不出用的是记录还是碰巧推测对了。
- **通过标准**：
  - 只有在「正常」状态持续 60 秒、并且在当前版本上成功展开过一次之后，才记录为上次正常配置；
  - `active.json` 损坏时优先恢复到上次正常配置；
  - 恢复后发布不复用任何已被占用的版本号；
  - 恢复后发布、再重启，「恢复上一个版本」按记录回到恢复时的版本，不受 `revisions\<vN-1>.json` 影响。
- **出处**：
  - B-04；development-plan.md §7.2 LKG。
  - `integrate/0.4`：
    - `src-tauri/src/lkg.rs:7-23`（条件；手动测试不计入）、`lkg.rs:158`（60 秒）；
    - `src-tauri/src/store/mod.rs:276-341`（`mark_last_good`）。
  - `43dda1f`：
    - `src-tauri/src/store/fallback.rs:612-648`（候选顺序）；
    - `fallback.rs:670-726`（`previous`：记录与读到的 `active.json` 版本号、指纹都相符才用；否则按上面的顺序推测）；
    - `revisions.rs:48-61、112-127`（发布时在写 `active.json` 之前写记录，并保留与磁盘相符的旧记录）；
    - `revisions.rs:159-180、194-228`（版本号）；
    - `meta.rs:24-81`（`previous_revisions` 的格式）。
  - 测试（`integrate/0.4` 的 `src-tauri/src/store/startup_tests.rs`）：
    - `rollback_target_survives_a_restart_after_recovery`（881）；
    - `rollback_target_without_a_record_is_guessed_as_before`（940）；
    - `a_recorded_snapshot_that_is_gone_leaves_no_rollback_target`（1017）；
    - `rollback_target_guess_takes_previous_json_before_an_older_snapshot`（1055）；
    - `mark_last_good_is_the_first_fallback`（1504）；
    - `a_failed_publish_keeps_the_rollback_target_of_the_committed_revision`（2636）；
    - `a_record_for_a_reused_revision_number_is_ignored`（2681）；
    - `an_unchanged_republish_does_not_reuse_the_lost_revision_number`（2723）；
    - `a_revision_number_used_only_by_metadata_is_not_given_out`（2763）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（vA=____，vB=____，vN=____，回滚后的版本=____；第 7 步 `previous_revisions` 的项数：____；第 5 步编辑器显示 `-A` 还是 `-B`：____）

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
  1. 用 P8 生成 `qt-bulk-5000.json`（只含 5000 条合成规则，不含原有规则）。记下导入前的规则条数 M。
  2. 点「导入」，选择该文件，在「遇到重复的指令时怎么处理？」里选「保留两份（keep_both）」，点「导入」，在预览中点「确认导入草稿」。
  3. 立即连按 Esc 两次，并点「关闭」和「✕」。
- **预期结果**：
  - 保存期间「关闭」和「✕」不可用，Esc 被忽略，对话框保持打开。
  - 保存完成后提示「导入草稿已保存，请应用设置」。
  - 草稿条数为 M + 5000：原有规则不变，新增的正好是文件里的 5000 条。
- **通过标准**：保存完成前对话框不消失；没有重复导入（只增加 5000 条，只加一次）。
- **出处**：`src/App.tsx:923-925、1870、1905`（`importing` 期间忽略关闭）；提交 `72804aa`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（需确认：保存耗时是否足够长，能观察到这个窗口期。如果太快，以 FE-03 为准）

### M4-FE-03 导入草稿保存失败：对话框保留，可以重试

- **步骤**：
  1. 用 P1 锁定 `draft.json`。
  2. 「导入」一个小文件，然后点「确认导入草稿」。
  3. 释放锁，在同一个对话框里再点一次「确认导入草稿」。
- **预期结果**：
  - 第 2 步立即报错「重命名文件失败：draft.json（系统错误 5）。」或「…（系统错误 32）。」，对话框和预览保持不变。临时文件 `draft.json.tmp` 能写成，失败的是用 MoveFileExW 替换被锁的 `draft.json` 那一步，不重试，所以报的是「重命名文件」（`src-tauri/src/store/atomic.rs:23`；`src/ipc/errors.ts:200-205`）。
  - 第 3 步成功，草稿只增加一次。
- **通过标准**：失败后不丢预览、不关对话框；重试后不重复写入。
- **出处**：`src-tauri/src/store/atomic.rs`；`src/ipc/errors.ts`（io 错误文案）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（实际的错误码：____）

### M4-FE-04 外部修改配置文件：revision_conflict

- **步骤**：
  1. 程序运行中，用记事本打开 `active.json`，在 `zzk` 那条规则的 `description` 末尾加上 `-EXT`，保存（保持 JSON 合法）。
  2. 在 Quicker Text 里编辑另一条规则（例如在 `zzp` 的正文末尾加 `-APP`），点「保存」，再点「应用设置」。
  3. 在「运行状态与恢复」点「刷新状态」，再点一次「应用设置」。
  4. 退出程序再启动。在「运行状态与恢复」点「放弃未应用草稿」→「放弃草稿」，让草稿按磁盘上的生效配置（含 `-EXT`）重建。
  5. 重新做第 2 步的修改（`zzp` 加 `-APP`），保存，点「应用设置」。
  6. 退出程序，用记事本打开 `active.json`，查找 `-EXT` 和 `-APP`。
- **预期结果**：
  - 第 2 步报错「设置已在别处更新（当前版本 N），请刷新后再试。」，`active.json` 保留外部修改，监听状态不变。
  - 第 3 步仍然报同样的错。「刷新状态」只取内存里的状态（`src-tauri/src/commands/runtime.rs:13-15`），不重新读磁盘；比对用的磁盘哈希只在启动和发布时更新（`src-tauri/src/store/load.rs:85`、`revisions.rs:103`），所以冲突要到重启后才解除。提示里的「请刷新后再试」做不到，记入 §12.3 第 18 条。
  - 第 4 步之后编辑器里能看到 `-EXT`，`zzp` 没有 `-APP`。
  - 第 5 步应用成功；第 6 步 `active.json` 里 `-EXT` 和 `-APP` 都在。
  - 注意：如果第 4 步不放弃草稿、直接应用，发布的是重启前保存的旧草稿，草稿里没有 `-EXT`，外部修改会被覆盖而且没有任何提示。这是已知问题，0.5 修（§12.3 第 18 条）。实测时可以在第 6 步之后另行验证一次，记进备注，但本条只按上面的步骤判定。
- **通过标准**：冲突时不覆盖外部修改，提示明确；按「重启 → 放弃未应用草稿 → 重做修改 → 应用」之后，两处修改都保留。
- **出处**：`revisions.rs:30-41`（磁盘哈希不一致时返回 RevisionConflict）；`src-tauri/src/store/mod.rs:319-330`（放弃草稿时按生效配置重建）；`src/ipc/errors.ts:196-197`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（第 3 步是否仍冲突：____；如另行验证了旧草稿覆盖，结果：____）

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

执行历史只在内存里：最多 200 条、30 分钟，退出或重启后就没有了（`crates/platform/src/history.rs:14-17`）。所以本条必须在执行哨兵的**同一次运行**里、执行完 30 分钟内检查，否则看到的是空表，什么也证明不了。

- **步骤**：
  1. release：PR-01 第 2 步做完后，不退出、不重启，立即打开「最近执行」，确认表里有刚才的 `zzs`、`zzt` 记录，再查看各列（时间、指令、方式、程序、结果、耗时）。
  2. 同一次运行中，展开「详细运行信息（高级）」。
  3. debug-fi（单独记录）：同样建好 `zzs`、`zzt` 并应用，在记事本各执行 5 次。不重启，在开发者工具的 Console 执行：

     ```js
     const h = await inv('list_history', { since_seq: null });
     [h.length, JSON.stringify(h).includes('QTSENTINEL'), JSON.stringify(h).includes('隐私哨兵')]
     ```

- **预期结果**：
  - 第 1 步表里有记录；任何地方都不出现哨兵正文或窗口标题，只有程序名。
  - 第 3 步第一个值不小于 10，后两个值都是 `false`。
- **通过标准**：执行历史不为空，且其中零命中。release 界面（第 1、2 步）和 debug-fi 原始数据（第 3 步）分别判定。
- **出处**：development-plan.md:2491；`history.rs:4-7、14-17`；`src-tauri/src/commands/engine.rs:128-133`。
- **结果**：
  - release 界面：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；距 PR-01 第 2 步结束 ____ 分钟；表中记录条数 ____。
  - debug-fi `list_history`：日期 ____；构建 ____；☐通过 ☐不通过 ☐未测；返回值 ____。
  - 备注（测完删除 `zzs`、`zzt` 并「应用设置」）____

## 8. 数据兼容：0.3.1 与 0.4 互读（M4-CP）

### M4-CP-01 0.3.1 升级到 0.4 无损

- **步骤**：
  1. 在装有 0.3.1 并有真实规则（或一份复制来的用户数据）的机器上，退出 0.3.1，备份数据目录，用 P10 记下哈希，记下界面上的规则条数和草稿状态。
  2. 换成 0.4 release 构建并启动。
  3. 观察横幅，检索 `store.` 开头的日志，核对条数。
  4. 抽 5 条不同操作类型的规则实际执行一遍。
  5. 不应用，退出后再用 P10 核对哈希，并打开 `store-meta.json`。
- **预期结果**：
  - 没有进入安全模式，没有隔离，没有 `store.corrupt`。
  - 条数相同，抽测的规则都能执行。
  - 第一次启动时就出现 `backup-pre-0.4\`：加载时要写元数据快照，写之前先备份。
    - 日志有 `store.backup_pre_0_4 result="created" files=<N> dir="backup-pre-0.4"`。
    - `store-meta.json` 里有 `"backup_pre_0_4": true` 和 `"backup_pre_0_4_dir": "backup-pre-0.4"`。
    - 备份内容（包括最后写入的 `manifest.json`）在 CP-04 核对。
  - `active.json`、`draft.json` 的哈希不变。加载时会写当前版本的 `revisions\<n>.metadata.json`（迁移元数据快照），可以接受：它写在备份之后，原来的文件已在备份里。
  - 启动后监听是暂停的：0.3.1 的目录里没有 `prefs.json`，「启动时自动开始监听」按默认关闭。日志有 `self_test.startup health=… auto_listen=false outcome="auto_listen_off"`。先点「恢复监听」再做第 4 步。
- **通过标准**：无损加载（退出标准原文）。
- **出处**：
  - development-plan.md:2448；§9（development-plan.md:2339-2340）。
  - `43dda1f`：
    - `src-tauri/src/store/load.rs:99-105`（加载时先备份，备份成功才写元数据快照）、`load.rs:169-190`（`snapshot_active_metadata`）；
    - `backup.rs:476-527`（日志与 `store-meta.json`）；
    - `prefs.rs:340-343`（没有 `prefs.json` 时自动监听为关）。
  - `integrate/0.4`：`src-tauri/src/startup.rs:297-299`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（规则条数：0.3.1 ____ / 0.4 ____；备份 files=____）

### M4-CP-02 0.4 写入的数据 0.3.1 仍能读取

- **步骤**：
  1. 接着 CP-01，在 0.4 中编辑一条规则，点「应用设置」，修改一项参数，然后退出。
  2. 安装或运行 0.3.1，启动，核对条数和被编辑的那一条，并执行一次。
- **预期结果**：
  - 0.3.1 正常加载，看得到 0.4 的修改。
  - 0.3.1 忽略 0.4 新增的内容：
    - `store-meta.json`，包括 `previous_revisions`、`last_good_revision`、`backup_pre_0_4`、`backup_pre_0_4_dir` 等字段；
    - `backup-pre-0.4\` 目录，包括其中的 `manifest.json`；
    - `prefs.json`：0.4 新增的文件，0.3.1 不读。
  - `revisions\*.json` 与 `*.metadata.json` 是两个版本共用的格式。
- **通过标准**：0.3.1 可以读取（退出标准原文）。
- **出处**：development-plan.md:2448、2340；`43dda1f`：`src-tauri/src/store/meta.rs:24-54`（`store-meta.json` 的字段）、`backup.rs:81-85`（`prefs.json` 是 0.4 新增的）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注 ____

### M4-CP-03 保留未知字段和读不懂的规则

`active.json` 的最外层就是配置对象（`version`、`revision`、`enabled`、`globals`、`rules`），没有 `config` 这一层包装。

- **步骤**：
  1. 退出程序，用 P10 记下哈希。在 `active.json` 里做两处修改：
     - 在最外层对象里加入 `"x_acceptance_future": {"keep": true}`；
     - 在 `zzk` 这条规则对象里（与 `id`、`match` 同级，不要放进 `action`）加入 `"x_rule_future": 1`。
  2. 在 `rules` 数组里再复制一条规则，`id` 改成唯一值（例如 `cp03-future`），把它的 `action.type` 改成 `"future_action"`。
  3. 启动。如果「运行状态与恢复」里的「放弃未应用草稿」可以点，就点它并在「放弃全部未应用草稿？」中选「放弃草稿」，让草稿等于刚改过的 `active.json`。（否则 `draft.json` 里没有这三处修改，下一步应用的是旧草稿；也可以在第 1 步对 `draft.json` 做同样的修改来代替。）
  4. 记下横幅、日志和规则列表怎样显示这条读不懂的规则。
  5. 编辑**另一条**规则（例如改 `zzp` 的描述），「应用设置」，然后用记事本打开新的 `active.json`，检索这三处。
- **预期结果**：
  - 正常启动，没有进入安全模式，没有隔离。
  - 日志有 `store.diagnostic code=rule_unreadable path=rules[<序号>]` 和 `store.rules_skipped count=1`；横幅「有 1 条规则无法读取或无法使用，已跳过：「cp03-future」。其他规则不受影响。」。
  - 规则列表里这一行显示「无法读取的规则」、它的 `id`，以及「这条规则含有当前界面无法编辑的内容，只能查看；已保留在配置中。」。
  - 新的 `active.json` 里三处内容都原样保留：最外层的 `x_acceptance_future`、`zzk` 里的 `x_rule_future`，以及 `cp03-future` 整条（仍在 `rules` 数组里，字段和取值不变）。文件里**不会**出现 `unreadable_rules` 这个键：程序内部把读不懂的条目单独存放，写回时放回 `rules` 数组。
- **通过标准**：三处都没有丢失；没有出现新的键。
- **出处**：`crates/domain/src/model.rs:48-63`（`Config` 的写回规则：读不懂的条目放回 `rules`，不新增键，B-43）；`crates/domain/src/stored.rs:248-300`（逐条读取规则，读不懂的保留原文并给出 `rule_unreadable`）；`src/App.tsx:1270-1290`（只读行）；`src/status.ts:179-184`（横幅）；`package.rs:160`（导入是严格模式，与存储不同）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（第 3 步是否点了「放弃未应用草稿」：____；界面如何显示读不懂的规则：____）

### M4-CP-04 跨版本首次写入前的备份 backup-pre-0.4

R6 实现了这项备份，审查 I-B1～I-B3、R7-1 又补了几处；规则总述见 `43dda1f` 的 `src-tauri/src/store/backup.rs:1-45`。

**什么时候备份**：0.4 第一次改动 0.3.1 会读的文件之前，先把 0.3.1 需要的文件逐字节复制到 `backup-pre-0.4\`。这里的改动包括：加载时的元数据快照、保存草稿、放弃草稿、发布、回滚、记录上次正常配置，以及把损坏文件改名隔离。加载时就会触发，之后每个写入命令前也会检查一次；`store-meta.json` 的 `backup_pre_0_4` 变成 `true` 或 `false` 之后就不再检查。`store-meta.json` 和 `prefs.json` 是 0.4 新增的文件，0.3.1 不读，写它们不经过这道检查。

**原子性**：先复制到临时目录 `backup-pre-0.4.tmp-<UTC 时间>\`，最后写入 `manifest.json`，再把整个目录改名，所以要么有完整的备份，要么没有。

- **步骤**：
  1. 接着 CP-01（第一次启动 0.4 之后）。用 CP-01 第 1 步记下的 P10 哈希（0.4 启动前）作对照。打开 `store-meta.json`，记下 `backup_pre_0_4` 和 `backup_pre_0_4_dir`。
  2. 列出备份里的文件（哈希、长度、相对路径）和 `manifest.json` 的内容，再看数据目录里有哪些以 `backup-pre-0.4` 开头的项：

     ```powershell
     $b = "$d\backup-pre-0.4"
     Get-ChildItem $b -Recurse -File | ForEach-Object { '{0}  {1,8}  {2}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Length, $_.FullName.Substring($b.Length + 1) }
     (Get-Content "$b\manifest.json" -Raw -Encoding UTF8 | ConvertFrom-Json).files | Format-Table path, len, sha256 -AutoSize
     Get-ChildItem $d -Filter 'backup-pre-0.4*'   # 只应有 backup-pre-0.4 这一个目录
     ```

  3. 做完 CP-02 第 1 步的「应用设置」后，再重启一次 0.4，记下 `backup-pre-0.4\` 的文件列表和修改时间。
  4. 空目录对照：
     - 退出程序，执行 `Rename-Item $d 'com.quickertext.desktop.cp04'`，启动 0.4（会新建一个空数据目录）。
     - 看日志和 `store-meta.json`，然后退出，删掉新建的目录。
     - debug-fi 也可以直接把 `QUICKER_TEXT_DATA_DIR` 指向一个空目录。
  5. 同名文件对照（审查 I-B3）：
     - 把 CP-01 第 1 步备份的 0.3.1 数据复制成新的 `$d`，然后建一个与备份目录同名的空文件：`New-Item -ItemType File "$d\backup-pre-0.4"`。
     - 启动 0.4，看日志、`store-meta.json` 和数据目录里的 `backup-pre-0.4*`。
     - 退出，删掉这个 `$d`，再把 `.cp04` 改回原名。
- **预期结果**：
  - 第 2 步：
    - 数据目录里只有 `backup-pre-0.4\` 一个备份目录，没有 `backup-pre-0.4-<时间>\`，也没有残留的 `backup-pre-0.4.tmp-*`。
    - `store-meta.json` 有 `"backup_pre_0_4": true` 和 `"backup_pre_0_4_dir": "backup-pre-0.4"`。
    - 备份里只有下列文件中**存在的那些**，不存在的跳过。相对路径与数据目录里相同，哈希都与 0.4 启动前的 P10 记录相同：
      - `active.json`、`draft.json`、`previous.json`、`migration-unresolved.json`、`active-migration-unresolved.json`；
      - `revisions\` 中下面三个版本的 `<n>.json` 和 `<n>.metadata.json`：
        - 当前生效版本 n；
        - n−1，即 0.3.1 回滚的目标；
        - 0.4 加载时定下的回滚目标。0.3.1 的目录没有 `previous_revisions` 记录，所以这个目标是推测出来的（推测顺序见 ST-13），通常就是 n−1 或 `previous.json` 的版本；
      - `prefs.json`（0.3.1 不写这个文件，正常没有）和 `revisions\index.json`；
      - 数据目录和 `revisions\` 里已有的隔离文件 `*.corrupt-*`；其中每个 `revisions\<m>.corrupt-*.json`，连同它对应的 `revisions\<m>.metadata.json` 也一起复制；
      - 这次加载发现、尚未改名的损坏文件（§1.3 延后隔离）；
      - `manifest.json`：最后写入，不在 P10 记录里。
    - 更早的版本不在备份里。
    - `manifest.json`：
      - `version` 为 1；
      - `files` 每项有 `path`（用 `/` 分隔，例如 `revisions/12.json`）、`len` 和 `sha256`（小写十六进制），与第 2 步列出的长度和哈希逐项一致；
      - `manifest.json` 自己不在列表里。
    - 日志有 `store.backup_pre_0_4 result="created" files=<N> dir="backup-pre-0.4"`。N 等于 `manifest.json` 中 `files` 的项数，也就是备份里的文件数减 1。
  - 第 3 步：没有第二次备份。目录内容和修改时间都不变，日志里也不再有 `store.backup_pre_0_4`：`backup_pre_0_4` 已有取值时直接跳过，不写日志。
  - 第 4 步：
    - 日志有 `store.backup_pre_0_4 result="not_needed" files=0 dir=""`；
    - 没有 `backup-pre-0.4\`；
    - `store-meta.json` 有 `"backup_pre_0_4": false`，`"backup_pre_0_4_dir": null`。
  - 第 5 步：
    - 日志先有警告 `store.backup_pre_0_4 result="existing_incomplete" dir=backup-pre-0.4 reason="not_a_directory"`，再有 `store.backup_pre_0_4 result="created" files=<N> dir="backup-pre-0.4-<UTC 时间>"`。
    - 同名的空文件原样留着；新备份在 `backup-pre-0.4-<UTC 时间>\`，内容与第 2 步相同（含 `manifest.json`）。
    - `store-meta.json` 有 `"backup_pre_0_4": true`，`"backup_pre_0_4_dir": "backup-pre-0.4-<UTC 时间>"`。
    - 没有备份失败的横幅，可以照常保存。`fe31dec` 中这样做会让备份失败，旧版 CP-05 就是用它造失败的；现在它不再造成失败，CP-05 已改用别的办法。
  - 其他情况（不单独测，日志里见到时对照）：
    - 已有一个完整的备份（`manifest.json` 逐项核对一致）、但 `store-meta.json` 没有记录时，不新建备份，只补记录：`result="existing" files=0 dir="<目录名>"`。
    - 不完整的原因还可能是 `manifest_missing`、`manifest_unreadable`、`manifest_invalid`、`file_missing`、`file_unreadable`、`file_changed`。
    - 上一次中断留下的 `backup-pre-0.4.tmp-*` 会先被删掉：`result="stale_temp_removed" file="<目录名>"`；删不掉时是警告 `result="stale_temp_kept"`。
- **通过标准**：
  - 备份内容和哈希与上面一致，`manifest.json` 与备份里的文件逐项一致；
  - 只备份一次；
  - 空目录不备份；
  - `backup-pre-0.4` 被占用时不动原物，另建带时间的备份目录。
- **出处**：
  - development-plan.md:1805、2339。
  - `43dda1f`（与 `integrate/0.4` 相同）：
    - `src-tauri/src/store/backup.rs:74-85`（复制哪些文件）、`backup.rs:117-130`（`manifest.json` 的格式）；
    - `backup.rs:148-185`（是否需要备份、文件清单）、`backup.rs:196-227`（隔离文件及其元数据）、`backup.rs:231-258`（残留的临时目录）；
    - `backup.rs:286-355`（复制、写 `manifest.json`、核对已有备份）、`backup.rs:372-459`（已有备份、带时间的目录名、最后整体改名）；
    - `backup.rs:476-527`（日志和 `store-meta.json`）、`backup.rs:634-640`（哪三个版本）；
    - `load.rs:99-105`；`meta.rs:41-50`。
  - 测试（`integrate/0.4` 的 `startup_tests.rs`）：
    - `first_write_backs_up_a_031_directory_byte_for_byte`（2388）、`fresh_directory_gets_no_backup`（2432）；
    - `an_existing_backup_is_never_overwritten`（2511）、`an_interrupted_backup_is_made_again`（2547）；
    - `a_corrupt_active_is_backed_up_before_it_is_quarantined`（2865）、`an_incomplete_existing_backup_is_kept_and_a_dated_one_made`（3095）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（files=____；`manifest.json` 项数 ____；与 P10 不一致的文件：____；第 5 步的备份目录名：____）

### M4-CP-05 备份失败时拒绝写入，修好后补做备份

**做法**：用一个名为 `prefs.json` 的**空目录**让备份失败。`prefs.json` 在备份清单里（`backup.rs:81-85`）。备份逐个用 `fs::read` 读取要复制的文件：不存在的跳过，遇到别的读取错误，整个备份就失败（`backup.rs:286-311`）。在 Windows 上把目录当文件读，得到的是「拒绝访问」（系统错误 5），所以备份停在 `prefs.json`。单元测试 `failed_backup_refuses_writes_until_it_succeeds`（`integrate/0.4` 的 `startup_tests.rs:2454`）就是这样造失败的。

**为什么不用别的办法**：这个做法不改文件权限，不需要管理员权限，也不需要任何注入输入的程序；展开用手动打字。给某个 0.3.1 文件设拒绝读取的 ACL 也能造成失败，但要改文件权限，事后还要恢复，本条不用。旧版本条用「与备份目录同名的文件」挡住备份，现在已经不会失败（见 CP-04 第 5 步）。

- **步骤**：
  1. 退出程序。把数据目录恢复成 CP-01 第 1 步备份的那份 0.3.1 数据：没有 `store-meta.json`，也没有任何 `backup-pre-0.4*`。
     - 如果 `store-meta.json` 里的 `backup_pre_0_4` 已经是 `true` 或 `false`，就不会再备份，本条无法测。
     - 数据目录里如果有 `prefs.json` 文件（0.3.1 不写这个文件，正常没有），先把它移到数据目录以外。
  2. 用 P10 记下哈希，然后建一个名为 `prefs.json` 的空目录：`New-Item -ItemType Directory "$d\prefs.json"`。
  3. 启动 0.4 release，看横幅、日志和 `store-meta.json`。
  4. 点顶部的「恢复监听」，在记事本里手动输入 `zzk` + 空格。
     - 然后保持「正常」至少 70 秒，期间不要注入故障、不要锁屏，再检索日志。
     - 不要动「启动时自动开始监听」开关：点了会报「读取文件失败：prefs.json（系统错误 5）。」，与本条无关。
  5. 编辑一条规则并保存，再点「应用设置」。
  6. 不退出程序，删掉那个目录：`Remove-Item "$d\prefs.json"`。再保存一次并「应用设置」。
  7. 记下横幅是否消失，然后重启程序再看一次。
- **预期结果**：
  - 第 3 步：
    - 没有进入安全模式，「当前生效版本」与 0.3.1 的相同。
    - 横幅：
      - 带 `cfa41f2` 的构建：「升级前备份失败，暂时不能保存，已有配置照常生效。」，只有一条。
      - 只用 `integrate/0.4` 构建：「启动时发现一个问题：其他（backup_failed）。可在「运行状态与恢复」中导出诊断包了解详情。」，不判为不通过。
    - 日志有下面几条（顺序不固定）：
      - **备份失败**（警告）：`store.backup_pre_0_4 result="failed" files=<k> op=Read file="prefs.json" error_kind=PermissionDenied os_code=5`。
        - k 是在 `prefs.json` 之前已经复制的文件数，即 5 个 0.3.1 文件和三个版本的 `revisions\` 文件中实际存在的个数（清单见 CP-04 第 2 步）。
        - 这些复制件随即删掉，不留 `backup-pre-0.4.tmp-*`。
      - **返回给调用方的错误**：紧接着一条 `command.io op=Read path=Some("backup-pre-0.4") error=拒绝访问。 (os error 5)`，英文系统是 `Access is denied. (os error 5)`。路径固定写备份目录名。
      - **其他读 `prefs.json` 的地方**也各自失败：
        - `command.io op=Read path=Some("prefs.json") error=拒绝访问。 (os error 5)`；
        - `store.prefs_unavailable file="prefs.json" error=Read failed (path: Some("prefs.json"), os error: Some(5))`：剪贴板设置改用默认值；
        - `log.level_restore_failed error=Read failed (path: Some("prefs.json"), os error: Some(5))`：日志级别用默认值。
        - 界面每次读取状态都会再读一次 `prefs.json`，所以 `path=Some("prefs.json")` 的 `command.io` 会反复出现，属正常。
      - **监听保持暂停**：`self_test.startup health=… auto_listen=false outcome="auto_listen_off"`。`prefs.json` 读不出来时，「启动时自动开始监听」按关闭处理。
    - `store-meta.json`：
      - `backup_pre_0_4` 为 `null`（既不是 `true` 也不是 `false`），`backup_pre_0_4_dir` 为 `null`；
      - 数据目录里没有任何 `backup-pre-0.4*`。
    - 0.3.1 的文件哈希与第 2 步相同。加载时的元数据快照也没有写：备份失败时根本不尝试写。
  - 第 4 步：
    - 「恢复监听」可以点，照常展开。暂停和恢复不写 0.3.1 的文件，不经过备份检查。
    - 约 60 秒后：
      - 日志有一条警告 `store.last_good_skipped revision=<n> reason="io"`；
      - 同时又有一组 `store.backup_pre_0_4 result="failed" …` 和 `command.io op=Read path=Some("backup-pre-0.4") …`。
    - 原因：记录上次正常配置也要先通过备份检查，被拒绝后什么都不写。
    - 之后保持「正常」时，每隔 1、2、4……分钟（最长 32 分钟）重试一次。每次重试都再记那两行，`store.last_good_skipped` 不再重复。
    - `store-meta.json` 的 `last_good_revision` 仍为 `null`，`revisions\` 里没有新文件。
  - 第 5 步：
    - 保存和应用都被拒绝，提示「读取文件失败：backup-pre-0.4（系统错误 5）。」。
    - 每次拒绝，日志都有一组 `store.backup_pre_0_4 result="failed" …` 和 `command.io op=Read path=Some("backup-pre-0.4") …`。
    - P10 哈希不变。横幅仍在，而且仍只有一条：`backup_failed` 只列一次。
  - 第 6 步：
    - 保存和应用成功。日志有 `store.backup_pre_0_4 result="created" files=<N> dir="backup-pre-0.4"`：这次保存先做备份，再写草稿。
    - 出现 `backup-pre-0.4\`，内容按 CP-04 第 2 步核对，其中没有 `prefs.json`：它已删掉，不存在的文件跳过。
    - `store-meta.json` 有 `"backup_pre_0_4": true` 和 `"backup_pre_0_4_dir": "backup-pre-0.4"`。
    - 备份成功时，补写加载时没写的元数据快照 `revisions\<n>.metadata.json`，n 为第 3 步的生效版本。
    - 这时不要求出现 `store.last_good`：「应用设置」换了生效版本，上次正常配置的条件对新版本重新计算（ST-13）。
  - 第 7 步：横幅当场不会消失，要到重启后才消失（§12.3 第 19 条），如实记录；重启后不应再出现。
    - 原因：后端在备份成功时已经撤销了这个问题，但撤销不发事件，而前端只合并新增的问题。
- **通过标准**：
  - 备份失败期间不写任何 0.3.1 会读的文件，包括元数据快照和上次正常配置的快照；
  - 已有配置照常展开；
  - 问题修好后，下一次写入补做备份并成功；
  - 第 7 步横幅当场没有消失不判为不通过，只记录。
- **出处**：
  - `43dda1f`（与 `integrate/0.4` 相同）：
    - `src-tauri/src/store/backup.rs:81-85`（`prefs.json` 在备份清单里）、`backup.rs:286-311`（不存在的跳过，其他读取错误让备份失败）、`backup.rs:454-457`（失败时删掉临时目录）；
    - `backup.rs:491-505`（失败日志与返回的错误）；
    - `backup.rs:531-542`（`backup_before_write`）、`backup.rs:618-631`（`note_backup`：`backup_failed` 只列一次，备份成功后撤销并补写元数据快照）；
    - `load.rs:99-105`、`load.rs:125-127`（加载时备份失败就不写元数据快照）；
    - `prefs.rs:228-241`、`prefs.rs:267-283`、`prefs.rs:340-343`（`prefs.json` 读不出来时的日志和默认值）。
  - `integrate/0.4`：
    - `src-tauri/src/error.rs:145-147`（`command.io`）；
    - `src-tauri/src/store/mod.rs:301-303`（`mark_last_good` 先过备份检查）；
    - `src-tauri/src/lkg.rs:85-109`、`lkg.rs:337-376`（拒绝后重试、`store.last_good_skipped`）；
    - `src-tauri/src/startup.rs:297-299`、`startup.rs:353-359`（`auto_listen_off`）；
    - `src-tauri/src/diagnostics/level.rs:269-276`。
  - 测试：`startup_tests.rs` 的 `failed_backup_refuses_writes_until_it_succeeds`（2454）；`lkg.rs` 的 `a_mark_refused_while_the_backup_fails_is_recorded_once_the_backup_succeeds`（900）。
  - 文案：
    - 横幅：`src/status.ts:217-219`（`cfa41f2`）；
    - 错误提示：`src/ipc/errors.ts:200-205`、`src/ipc/labels.ts:92-101`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（第 3 步 files=____；第 4 步是否有 `store.last_good_skipped … reason="io"`：____；第 6 步 files=____；第 7 步横幅当场是否消失：____）

## 9. 性能与浸泡（M4-PF）

测量规则（performance-baseline.md §2、§5）：
- 一律用 release 构建，每项至少测 3 轮；
- 采样程序不要和输入延迟测量同时运行（performance-baseline.md:73）；
- 每次测量按 performance-baseline.md §5 的模板另起一份记录，并在 §6 追加一行。

### M4-PF-01 触发到开始输出的 p95（基线，1.0 的目标是 < 100 ms）

先说清楚能拿到什么数据：

- 每次执行的计时只存在内存里的执行历史中（最多 200 条、30 分钟），字段是 `queue_us`、`resolve_us`、`erase_us`、`output_us`、`restore_us`、`total_us`（微秒）。日志和诊断包里都没有这些数字，`get_health` 也还没有 p50/p95 汇总（`src-tauri/src/commands/diagnostics.rs:506` 是 TODO）。
- release 构建没有开发者工具。界面「最近执行」的「耗时」列只显示 `total_us`，并四舍五入到毫秒（`src/StatusArea.tsx:138`、`:155`；`src/status.ts:272-277`）。分阶段的数字只能在 debug-fi 里用 `list_history` 取。
- 各阶段并不首尾相接，不能直接加起来。执行依次经过 Resolve → Preflight → Guard → Clipboard → Erase → Output → Cleanup → Restore → Done。Preflight、Guard、Clipboard 三段没有单独的字段；Cleanup 算进 `output_us`。`queue_us` 从入队算起，钩子收到终止键到入队的时间不在任何字段里。所以只能这样推算：
  - 入队 → 开始擦除 ≈ `total_us − erase_us − output_us − restore_us`；
  - 入队 → 开始输出 ≈ `total_us − output_us − restore_us`。

  缺失的字段（例如这次没有擦除或恢复）按 0 计。

- **步骤**：
  1. 方法 A（debug-fi，分阶段）：
     1. 用 debug-fi 构建，结论里注明是调试构建，数值偏大。
     2. 在记事本手动输入 `zzk` + 空格 100 次，30 分钟内做完并取数。
     3. 在开发者工具执行（`inv` 的定义见 §1.6；只统计结果为 `succeeded` 的执行）：

        ```js
        const h = (await inv('list_history', { since_seq: null })).filter(e => e.outcome === 'succeeded' && e.timings.total_us !== null);
        const v = (e, k) => e.timings[k] ?? 0;
        const toErase = h.map(e => (v(e, 'total_us') - v(e, 'erase_us') - v(e, 'output_us') - v(e, 'restore_us')) / 1000);
        const toOutput = h.map(e => (v(e, 'total_us') - v(e, 'output_us') - v(e, 'restore_us')) / 1000);
        const total = h.map(e => v(e, 'total_us') / 1000);
        const stat = xs => { xs = [...xs].sort((a, b) => a - b); const q = p => xs[Math.min(xs.length - 1, Math.floor(p * xs.length))]; return { n: xs.length, p50: q(0.5), p95: q(0.95), max: xs[xs.length - 1] }; };
        ({ toErase: stat(toErase), toOutput: stat(toOutput), total: stat(total) })
        ```

  2. 方法 B（release，上界）：在 release 构建中同样执行 100 次，分 2～3 批，每批做完就把「最近执行」的「耗时」列抄下来或截图，再自己算 p50 和 p95。这是 `total_us`，包含擦除、输出和恢复，只能作上界。
  3. 可选方法 C：用 240 fps 录屏，从终止键按下到第一个退格生效、第一个字符出现，逐帧计数。只有这个方法包含钩子到入队的那一段。
  4. 可选（未验证）：release 下想拿分阶段数字，可以在启动前设 `$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9222'`（只对这次启动有效），再用 Edge 的 `edge://inspect` 连上窗口，执行方法 A 的脚本。连不上就记「不可用」。
- **预期结果**：至少得到 A、B 两组数字；有条件时加上 C。「入队 → 开始输出」的 p95 低于 100 ms 记为达到 1.0 目标，否则只记作基线。
- **通过标准**：0.4 只要求「有数据」（performance-baseline.md:43），所以完成测量并存档即为通过。
- **出处**：performance-baseline.md:43；`crates/platform/src/health.rs:236-245`（StageTimings）；`crates/platform/src/windows/executor.rs:409-437`（各字段的起止）；`crates/platform/src/windows/actions.rs:31-42`（阶段顺序）；`src/StatusArea.tsx:138-155`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（A 入队→擦除 p50 ____ / p95 ____；A 入队→输出 p50 ____ / p95 ____；A total p50 ____ / p95 ____；B p50 ____ / p95 ____；C ____；第 4 步是否可用：____）

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

用 release 构建正常启动（不用验收模式），由人打字或用实体按键工具触发，这样走的是真实的键盘输入路径。`scripts/soak/soak.ps1` 只读日志、只查询进程，不注入输入，也不写数据目录；它统计「静默失效」的可疑项，但看不到每次执行的结果，所以还要人工对照执行历史。

- **步骤**：
  1. 在虚拟机 B（或一台日常机器）上用 release 构建正常启动，导入规则集（含 `zzq`、`zzv`），「应用设置」，等健康页显示「正常」。
  2. 在仓库根目录另开 PowerShell，每轮用一个新的输出目录：

     ```powershell
     powershell -NoProfile -ExecutionPolicy Bypass -File scripts\soak\soak.ps1 -Hours 2 -OutputDirectory D:\soak\round<N>
     ```

     数据目录不是默认的 `%APPDATA%\com.quickertext.desktop` 时，加 `-DataDirectory <目录>`。脚本已经每 10 秒采样内存、句柄和线程，P7 可以不跑。
  3. 打开一个空白记事本，保存为 `D:\soak.txt`。在 2 小时内手动输入 `zzq` + 空格和 `zzv` + 空格，每个触发单独一行，自己记下次数（例如每 10 分钟 20 次，分布在整个 2 小时内）。暂停期间不要输入。
  4. 每 10 分钟做一次：托盘暂停再恢复；`Set-Clipboard -Value "soak $(Get-Date)"`。
  5. 每 30 分钟内至少看一次「最近执行」，记下成功、失败、「未执行」的条数；有失败或未执行时截图（执行历史只在内存里，最多 200 条、30 分钟）。
  6. 脚本到时自动写出汇总；提前结束按 Ctrl+C，已有数据照样汇总（直接关掉终端窗口不会写最终汇总）。保存 `soak.txt`，统计：

     ```powershell
     $t = [IO.File]::ReadAllText('D:\soak.txt')
     ([regex]::Matches($t, '\[QT-SOAK-OK\]')).Count; ([regex]::Matches($t, '\[QT-SOAK-PASTE\]')).Count
     ([regex]::Matches($t, '\bzz[qv]\b')).Count      # 残留的缩写，等于没有展开的次数
     ```

  7. 打开 `D:\soak\round<N>\summary.txt`，看「静默失效」可疑项（A 自动可疑项、B 人工复核项）、句柄和线程的趋势、Private Bytes 的增长、结束时的健康状态。结果填进 performance-baseline.md §4 的记录表。
- **预期结果**：
  - 自己记的触发次数 = 两种标记数之和 + 残留缩写数；每个残留都能在执行历史（截图）里找到一条带原因的失败或「未执行」。
  - `summary.txt` 的 A 部分为空，或者每一项都能解释（例如第 4 步有意的暂停）；B 部分逐项对照过执行历史。
  - 句柄、线程没有标「疑似单调增长」。
  - 结束时健康页显示「正常」。`summary.txt` 里的健康是从日志推断的，「正常 ↔ 降级」不写日志，以界面为准。
  - 记下 Private Bytes 从开始到结束的增长（1.0 要求 < 20 MB，0.4 只记录）。
- **通过标准**：没有静默失效（退出标准原文），即 A 部分没有解释不了的项，B 部分对照后没有「打了触发词却没有任何记录」的情况；句柄和线程不单调增长；结束时状态「正常」。
- **出处**：development-plan.md:2448；performance-baseline.md:76-89；`scripts/soak/README.md`（用法、输出、「静默失效」的判定方法；`9cbe400`）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（触发 ____ / 成功 ____ / 残留 ____ / 残留中找不到记录的 ____；A 项 ____；B 项 ____；Private Bytes ____ → ____；句柄 ____ → ____；线程 ____ → ____；输出目录 ____）

## 10. S-29 spike（M4-SP）

S-29 在计划里是 0.4 的 **P0** 条目（development-plan.md:630、2381），0.4 末必须给出三项结论（第 720 行），所以本节四条都是必做，结果要计入发布判定，不是只记录的参考项。每个 T-xx 都要有记录，不能留空；只有维护者书面同意缩小范围（写明谁、哪天、去掉哪几项、理由），才可以标「未测」，并把同意记录附在备注里。

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

- **前提**：T-09 ～ T-12 和 T-14 需要一个临时探针程序（s-29.md:188），目前还没有。**由开发者在本轮验收前写好**，在宿主机编译，只复制到虚拟机 B 运行。按 s-29.md §3.5，探针要做到：
  - 创建一个隐藏窗口，注册 `RIDEV_INPUTSINK` 的键盘和鼠标 RawInput，安装 `WH_KEYBOARD_LL` 和 `WH_MOUSE_LL`，只打印收到的事件数，不记录按键内容（T-09 ～ T-11）；
  - 能按批次调用 `SendInput`，打印请求数、返回数和 `GetLastError`（T-12）；
  - 加一个 `EVENT_SYSTEM_FOREGROUND` 回调，打印它与第一个按键的间隔（T-14）。

  探针还没到位时，本条不能开始，也不能用「探针未编写」作为「未测」的理由（见本节开头）。
- **步骤**：
  1. T-13：管理员记事本在前台时运行 `spike_integrity` 和 `spike_integrity <PID>`（s-29.md:196、300-303）。
  2. T-09 ～ T-12、T-14：按 s-29.md §3.5 的表逐条用探针执行，记录栏填事件数、返回值、错误码或毫秒数。
  3. SV-09 的日志（管理员窗口在前台时的 `input.check`）只作旁证，不能代替 T-09 ～ T-11 的探针计数。
- **预期结果**：见 s-29.md §3.5 表中的「预期」栏；T-10（RawInput 在管理员窗口前台时能否收到）和 T-12（被 UIPI 拦截时 `SendInput` 实际返回什么）的预期本来就是「未知」，以实测为准。
- **通过标准**：T-09 ～ T-14 每一条都有记录，没有空白；T-09 ～ T-11 的结果回填 C.2 #3，并据此给出 ② 项的结论（可行 / 降级方案 / 不可行）。
- **出处**：development-plan.md:630、720、2381；s-29.md §0（第 12 行）、§3.5（第 186–199 行）。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（探针的提交或来源：____；T-09 ____；T-10 ____；T-11 ____；T-12 ____；T-13 ____；T-14 ____；缩小范围的书面同意：____）

### M4-SP-03 Backspace 擦除单元 T-15 ～ T-21

- **步骤**：
  1. 按 s-29.md §4.5（第 267–281 行）操作，**不需要注入**：
     1. 把展开文本复制到剪贴板；
     2. 在目标程序里先输入前文 `x `，再粘贴；
     3. 一次一次按 Backspace，数到只剩前文为止，这个次数就是 U。
  2. 对 T-15 ～ T-19 逐条记录。T-20 需要 espanso，可选，没做时写「未做（可选）」，不要留空。T-21 按 s-29.md:279 要等 B-34 实现（0.5.x）之后才能做，记「不适用：B-34 未实现」。
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
     3. 约 10 秒后出现 `executor.rebuilt`，此时立即在记事本先按一次物理空格（§0），再执行一次 `zzp` + 空格；
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

发布按固定顺序进行（`43dda1f` 的 `revisions.rs:22-158`）。下面称「第 n 项」，与测试步骤的「第 n 步」区分。先做检查，这时什么都还没写：
1. 定新版本号：比生效版本和 `revisions\` 里本程序文件用过的所有号码都大 1。只有 `<n>.metadata.json`、隔离后的 `<n>.corrupt-*.json` 或 `.tmp` 的号码也算用过（R7）。`revisions\` 列不出来时拒绝发布。
2. 编译配置。
3. 备份检查（CP-05）。
4. 比较 `active.json` 的哈希：与加载时或本次会话上次发布时的不同，就报版本冲突。
5. 在 `store-meta.json` 里找描述当前 `active.json` 的那条回滚记录：`active` 等于生效版本，指纹等于磁盘上的文件（审查 I-R1）。
6. 持有 MANUAL 暂停原因，发布结束后不论成败都恢复原样。

然后按顺序写文件：

7. `revisions\<旧>.json`（已有就不写）。旧配置是空白占位时，第 7、8 项都跳过。
8. `revisions\<旧>.metadata.json`。
9. `draft.json`。
10. `revisions\<新>.metadata.json`。
11. `migration-unresolved.json`。
12. `active-migration-unresolved.json`。
13. `store-meta.json` 的 `previous_revisions`，有两项：
    - 第一项是 `{"active": <新>, "revision": <旧>, "fingerprint": <新 active.json 的长度和 SHA-256>}`；
    - 第二项是第 5 项找到的记录。

    这一步写不了只记警告 `store.meta_write_failed`，发布照常进行。
14. 最后才是 `active.json`。

除第 13 项外，任何一项失败，整个发布就失败：内存里的生效配置不变，暂停状态恢复原样。但第 7～13 项已经写下的文件不会撤回。场景 a、b、c 分别让失败落在第一次写入、开始写入之前、最后一步写入上。

- **步骤**：
  1. 场景 a（debug-fi，`store_write_fail`）：
     1. 编辑一条规则并保存到草稿。记下「当前生效 v{N}」和状态行（监听中），用 P10 记下 `active.json` 的哈希。
     2. 布防 `store_write_fail`（§1.6）。
     3. 点「应用设置」。
     4. 再点一次「应用设置」。
  2. 场景 a′：先点「暂停监听」，再按场景 a 做一遍，最后恢复监听。
  3. 场景 b（release，P1 独占锁定 `active.json`）：用 P1 锁定 `active.json` 后点「应用设置」；释放锁后再点一次。
  4. 场景 c（release，只允许读的占用）：
     1. 在 `zzk` 的描述末尾加 `-C1`，点「应用设置」，记下 v{N}。用 P10 记下 `active.json` 的哈希，转成小写；再用 `(Get-Item "$d\active.json").Length` 记下长度。
     2. 把描述末尾改成 `-C2`，保存到草稿。
     3. 另开 PowerShell 执行 `$f = [IO.File]::Open("$d\active.json", 'Open', 'Read', 'Read')`。这样程序还能读这个文件，但不能替换它。
     4. 点「应用设置」，记下提示。查看 `revisions\` 里各文件的修改时间，以及 `store-meta.json` 的 `previous_revisions`。
     5. 不释放占用，退出程序；然后执行 `$f.Close()`，再启动程序。
     6. 在「运行状态与恢复」点「恢复上一个版本」→「回滚」。看 `zzk` 的描述和「当前生效」的版本号，再看一次 `revisions\` 和 `store-meta.json`。
     7. 退出程序再启动，再点一次「恢复上一个版本」→「回滚」，看 `zzk` 的描述和版本号。
- **预期结果**：
  - 场景 a：
    - 第一次应用报错「重命名文件失败：<N>.json。」。如果 `revisions\<N>.json` 已经存在，则是「重命名文件失败：<N>.metadata.json。」。注入的错误不是系统错误，所以没有系统错误码。
    - 日志有 `command.io op=Rename path=Some("<N>.json") error=injected store_write_fail`（或对应的 metadata 文件名）。注入时临时文件已删掉，不留 `.tmp`。
    - 生效版本仍是 v{N}，`active.json` 的哈希不变，草稿还在，状态行与操作前相同（监听中）。
    - 第二次应用成功，生效版本变为 v{N+1}，因为注入只生效一次。失败的那次还没写到 `<N+1>.metadata.json`，所以号码 N+1 没被占用。
  - 场景 a′：同场景 a；失败后和成功后状态行都仍是「已暂停」，没有被发布顺带恢复监听。
  - 场景 b：报「读取文件失败：active.json（系统错误 32）。」。发布在第 4 项比较哈希时停下，什么都没写，状态不变；释放后再应用得到 v{N+1}。
  - 场景 c：
    - 第 4 步：
      - 报「重命名文件失败：active.json（系统错误 5）。」或系统错误 32；日志有 `command.io op=Rename path=Some("active.json") error=拒绝访问。 (os error 5)`（英文系统为 `Access is denied. (os error 5)`）。
      - `active.json` 的哈希不变，生效版本仍是 v{N}，状态行恢复原样。
      - 第 7～13 项写下的文件不会撤回：`revisions\<N>.json`（原来没有时）、`<N>.metadata.json`、`<N+1>.metadata.json`、`draft.json`（`-C2`），还有替换失败留下的 `active.json.tmp`。真实的替换失败不删临时文件；下次写 `active.json` 时它被覆盖，不影响加载。
      - `store-meta.json` 的 `previous_revisions` 有两项：
        - `{"active": <N+1>, "revision": <N>, …}`；
        - `{"active": <N>, "revision": <M>, "fingerprint": {"len": <第 1 步的长度>, "sha256": "<第 1 步的哈希>"}}`。M 是第 1 步那次发布替换掉的版本，一般是 N−1。

        只有第一项、没有第二项，判不通过（I-R1）。
    - 第 5 步重启后，生效的仍是 v{N}（`-C1`），草稿是 `-C2`。
    - 第 6 步：
      - 第一项的 `active`（N+1）与生效的 v{N} 对不上，不被采用；第二项的版本号和指纹都对得上，回滚目标就是它记的 `revisions\<M>.json`（I-R2）。所以 `zzk` 的描述回到加 `-C1` 之前的样子。
      - 新版本号是 N+2，不是 N+1，因为第 4 步留下的 `<N+1>.metadata.json` 占用了 N+1（R7）。这个文件的修改时间不变，`active.json.tmp` 也不在了。
      - `previous_revisions` 变为 `{"active": <N+2>, "revision": <N>, …}` 和 `{"active": <N>, "revision": <M>, …}`。
      - 如果描述仍是 `-C1`、版本号是 N+1，或者 `<N+1>.metadata.json` 被改写，判不通过。
    - 第 7 步：
      - 重启后采用的是 `{"active": <N+2>, "revision": <N>}`，回滚目标是 `revisions\<N>.json`，中间缺的 N+1 不影响：有记录就按记录，不再推测（R8）。`zzk` 的描述回到 `-C1`，生效版本 v{N+3}。
      - 数据目录里有 0.3.1 留下的 `previous.json` 时，这一步还能分辨回滚目标是按记录还是推测出来的：推测时 `<N+1>.json` 不存在，会先取 `previous.json`（`fallback.rs:710-725`），得到的不是 `-C1`。
- **通过标准**：
  - 失败的发布不改变生效配置和暂停状态（§5.5 #8）。错误提示与上面一致。
  - 场景 c 失败后，已生效版本的回滚记录还在；重启后的回滚按记录进行，回滚本身（也是一次发布）能成功，版本号不复用。
- **出处**：
  - development-plan.md:1006。
  - `43dda1f`：
    - `src-tauri/src/store/revisions.rs`：
      - `32`（版本号）、`37`（备份检查）、`38-46`（哈希比较）；
      - `48-61`（找出生效版本的记录，I-R1）；
      - `62-77`（持有 MANUAL、结束后恢复）；
      - `82-133`（写入顺序、失败时恢复暂停状态；`112-126` 记录写在 `active.json` 之前，写不了只告警）；
      - `159-180`、`194-228`（新版本号跳过已用过的号码，R7）。
    - `atomic.rs:16-23`：`store_write_fail` 与真实替换失败都报 `Rename`；注入时删临时文件，真实失败不删。
    - `load.rs:226-243`：读文件失败报 `Read`。
    - `fallback.rs:670-726`：版本号和指纹都对得上的记录才采用，采用了就按记录，不再推测（I-R2、R8）；没有记录时的推测顺序见 683-693、710-725 行。
  - integrate `f1589cb`：
    - `src-tauri/src/error.rs:145-147`：`command.io` 日志，只带文件名。
    - `src-tauri/src/store/startup_tests.rs`：
      - `2636`：`a_failed_publish_keeps_the_rollback_target_of_the_committed_revision`；
      - `2763`：`a_revision_number_used_only_by_metadata_is_not_given_out`；
      - `3270`：`store_write_fail_keeps_active_and_pause`。
    - `src-tauri/src/store/revisions.rs`：
      - `276`：`failed_publish_preserves_active_and_pause`；
      - `299`：`publish_restores_only_the_manual_reason`。
    - `src/ipc/errors.ts:200-205`。
- **结果**：日期 ____；机器 ____；构建 ____；☐通过 ☐不通过 ☐未测；备注（场景 a 报错的文件名：____；场景 c 的系统错误码：____；场景 c 第 4 步 `previous_revisions` 的两项：____；第 6 步的版本号和 `zzk` 的描述：____；第 7 步的版本号和 `zzk` 的描述：____）

## 12. 汇总

### 12.1 条目数

| 节 | 条目 | 数量 | 通过 | 不通过 | 未测 |
|---|---|---|---|---|---|
| 2 输入与注入 | M4-IN-01 ～ 10（09、10 可选） | 10 | | | |
| 3 剪贴板 | M4-CB-01 ～ 09（09 可选） | 9 | | | |
| 4 监督与自愈 | M4-SV-01 ～ 17（16 可选） | 17 | | | |
| 5 安全模式与存储 | M4-ST-01 ～ 13（03 可选） | 13 | | | |
| 6 前端状态一致性 | M4-FE-01 ～ 07 | 7 | | | |
| 7 隐私 | M4-PR-01 ～ 04 | 4 | | | |
| 8 数据兼容 | M4-CP-01 ～ 05 | 5 | | | |
| 9 性能与浸泡 | M4-PF-01 ～ 05 | 5 | | | |
| 10 S-29 spike | M4-SP-01 ～ 04 | 4 | | | |
| 11 不得倒退 | M4-NR-01 ～ 08 | 8 | | | |
| **合计** | | **82** | | | |

### 12.2 条目与 0.4 退出标准的对应

退出标准见 development-plan.md:2448；「不得倒退」一项见 development-plan.md:2455。

| 退出标准 | 覆盖条目 | 判定 |
|---|---|---|
| E1 故障注入点（§11）全部能自动恢复或明确提示；`hook_unhook`、`rawinput_unregister` 由交叉校验检出并重建 | 各注入点及其覆盖条目：<br>• `executor_panic`：SV-02、SV-15（熔断）、SV-17（手动测试）、PR-01<br>• `executor_hang`：SV-03、SV-17（手动测试）、NR-02<br>• `hook_panic`：SV-04<br>• `hook_unhook`：SV-05<br>• `rawinput_unregister`：SV-06<br>• `rawinput_close`：SV-07<br>• `clipboard_busy`：CB-08，真实场景 CB-07<br>• `store_write_fail`：NR-08<br>• `config_corrupt`：用真实操作代替，ST-05、ST-06<br>• `store_locked`：用真实操作代替，ST-01、ST-02<br>• `autostart_stale`：用真实操作代替，SV-12<br>• `session_end`：SV-14，真实注销 SV-13<br>补充：SV-01、SV-08 ～ SV-11、SV-16（可选）、ST-03、ST-04、ST-07 ～ ST-13、CB-01 ～ CB-06、CB-09 | ☐ |
| E2 2 小时浸泡无静默失效 | PF-05（PF-04 作为空闲参照） | ☐ |
| E3 隐私测试通过（日志、诊断包、执行历史不含正文） | PR-01 ～ PR-04、CB-06 | ☐ |
| E4 S-25 实机最小集有书面记录 | IN-01 ～ IN-08（记事本、Chrome、微信，各配合微软拼音）；CB-04 ～ CB-06（development-plan.md:2524：0.4 的最低覆盖还包括剪贴板子表，第 2545–2554 行）。本文件填写后存档 | ☐ |
| E5 0.3.1 数据无损加载，0.4 写入的数据 0.3.1 可读 | CP-01、CP-02（CP-03 ～ CP-05 为 §9 的配套要求：保留未知字段、升级前备份及其失败处理） | ☐ |
| E6 无 P0 缺陷 | 全部条目。任一条触发 §0 的立即停止条件即为 P0 | ☐ |
| E7 不得倒退清单全部通过 | NR-01 ～ NR-08 | ☐ |
| 前端底座与状态一致性（0.4 交付内容，不是独立的退出标准） | FE-01 ～ FE-07、ST-01、ST-02（I6） | ☐ |
| S-29 可行性 spike（0.4 的 P0 条目，development-plan.md:630、2381；必做，计入发布判定） | SP-01 ～ SP-04，每个 T-xx 都有记录（§10 开头） | ☐ |
| 记录项（不阻断发布）：性能基线、可选条目 | PF-01 ～ PF-04、IN-09、IN-10、SV-16 | ☐ |

0.4 的 P0 条目（development-plan.md:602-639）大多以自动化测试和代码审查验收。下表只列验收标准里有实机要求的几项，逐条对到本清单（验收标准原文见 development-plan.md:704-720）：

| P0 条目 | 计划中的实机验收要点 | 覆盖条目 |
|---|---|---|
| S-03 引擎监督器与真实健康状态 | `WM_CLOSE` 后 5 秒内自动重建；10 分钟内失败 5 次熔断为「故障」并发通知；托盘图标、托盘提示文字、运行状态区三处一致 | 重建：SV-07；熔断与三处一致：SV-15；各故障注入条目（SV-02 ～ SV-07）都核对状态显示。熔断时没有系统通知，SV-15 对这一项单独判定（§12.3 第 16 条） |
| S-04 启动自愈与安全模式（B-04、B-43） | 5 种损坏样例都能启动；坏文件改名为 `*.corrupt-<时间>.json`；横幅说明从哪个版本恢复；全程无崩溃 | 截断 JSON：ST-05、ST-06；非法 UTF-8：ST-12 a；字段类型错误：ST-12 b（b′ 可选）；正则无法编译：ST-12 c；某个版本文件损坏：ST-12 d；所有版本都损坏：ST-10；上次正常配置作为第一回退：ST-13。另见 ST-07 ～ ST-09、ST-11 |
| S-06 输入源存活检测与自动重挂 | `hook_unhook`、`rawinput_unregister` 后 10 秒内（或 50 次硬件按键内）恢复触发，日志有 `input.reattach` | SV-05、SV-06；误判方面见 SV-01、SV-08、SV-09 |
| S-07 剪贴板句柄格式与恢复策略 | 位图恢复后 CF_DIB 逐字节一致；截图、Word 富文本（含 CF_ENHMETAFILE）、文件列表下粘贴类短语都成功且能再次粘贴；超过 32MB 的格式按策略处理；不进入 Win+V 历史 | CB-04 c、d、e（e 必做，配合 P12 比较字节）；CB-05；CB-06。CF_PALETTE 与 0x80–0x8F 的跳过由单元测试覆盖（`crates/platform/src/windows/clipboard.rs:2079-2087`），实机出现时记在 CB-04 的「skipped 日志」列 |
| S-29 可行性 spike | 焦点获取与归还、管理员窗口下的输入可见性、Backspace 补偿三项各给出结论 | SP-01 ～ SP-04，必做，计入发布判定（§10 开头） |

### 12.3 编写时发现的文档与代码不一致

本节只记录，本文件的提交不改代码。第 1 ～ 15 条是初稿（`576aa5a`）时记下的，修订时逐条回到代码复核：已经由其他提交修复的标「已修」并写明提交，这些提交不在 `feat/0.4-stability-foundation` 上的，实测构建必须包含它们（见文首）。第 16 ～ 19 条是修订时新发现的。

1. **backup-pre-0.4**（已修，存储分支 `43dda1f`：R6 及审查修正 I-B1～I-B3、R7-1）：development-plan.md:1805、2339 要求跨版本首次写入前备份，初稿时代码里没有。`43dda1f` 的做法：
   - 加载时和每次写入前检查，备份失败时拒绝写入，并列出 `backup_failed`（CP-05）；
   - 备份包含加载时隔离的坏文件和各版本的 metadata 文件（I-B2、R7-1，CP-04）；
   - 加载时发现的坏 `active.json` 要等备份收进去之后才改名隔离，备份失败时留在原处（I-B1）；
   - 同名备份目录不完整时保留它，另建带日期的备份（I-B3，CP-04 第 5 步）。

   integrate `f1589cb` 让记录上次正常配置（`mark_last_good`）也先过这道备份检查（`store/mod.rs:301-303`，见第 4 条）。
2. **B-01 ⑤ 与 §5.5 #2 相冲突**：
   - development-plan.md:1819 写的是：剪贴板被卡住的旧线程占着时，新纪元的执行器「改用键入输出」；
   - §5.5 #2 和代码（`executor.rs:1715-1733`）都是拒绝，不改用键入（NR-02）。
3. **session_end 场景与代码不同**（已修，R5 `241b3f3`，`fix/0.4-final-store` 分支）：
   - 计划 §11（development-plan.md:2489）预期这个注入点之后结束进程，连续启动两次不会被误判为崩溃循环；
   - 初稿时这个注入点只调用会话结束回调（`session.rs:216-221`），不清除运行标记，连续重启会进入 CrashLoop；
   - `241b3f3` 改为在会话结束回调里清除运行标记，SV-14 按修复后的行为验收；真实注销仍走 RunEvent::Exit 清除运行标记（`startup.rs:384-395`，SV-13）。
4. **LKG 没有接上**（已修，`b32062f`，`fix/0.4-final-lkg` 分支）：计划把上次正常配置作为第一回退，初稿时生产代码不调用 `mark_last_good`，回退直接从 `revisions/` 开始。`b32062f` 已接上：在「正常」状态持续 60 秒以上并成功展开一次后记录（ST-06、ST-13）；`lkg.rs` 自己列出的限制见附表 A。integrate `f1589cb` 又加了两条：
   - 记录前先过升级前备份检查；`revisions\<n>.json` 已存在但内容不是生效配置时拒绝记录（`store/mod.rs:301-326`）。
   - 被拒绝后在同一状态下重试，等待时间从 60 秒起逐次加倍，最长 32 分钟（`lkg.rs:85-102`）。所以备份失败期间不会记录上次正常配置，备份恢复后才会记录（CP-05）。
5. **恢复横幅每次启动都会出现**（按设计）：从回退候选恢复后不重写 `active.json`，所以直到下一次「应用设置」之前，每次启动都显示恢复横幅（ST-06）。加载流程按设计只隔离坏文件、只写运行标记，不写配置文件（`src-tauri/src/store/fallback.rs:121-141`），所以不算缺陷；ST-06 第 4 步核对应用之后不再出现。
6. **横幅说「可以导入配置」，Locked 时导入却被拒绝**（已修，`cfa41f2`）：初稿时 safe_mode 横幅对所有原因用同一句话。后端只允许 CrashLoop 和 NoValidConfig 用导入解除，Locked、NewerSchema 下只能预览（`src-tauri/src/store/mod.rs:85-94`）。`cfa41f2` 改为按原因给出出路，只有这两种原因提「可以导入配置」（`src/status.ts:146-157`）（ST-04、ST-08 ～ ST-10）。
7. **自检项名称不一致**（已修，`e0fc068`）：系统通知里写「键盘鼠标监听」（`startup.rs:363`），初稿时前端横幅写「键盘监听心跳」。`e0fc068` 把前端标签改为「键盘鼠标监听」，与通知一致（SV-11）。
8. **终止键与计划默认值不同**：代码是 Ctrl+Shift+Esc（`hook.rs:262-275`），计划默认值是 Ctrl+Alt+Pause；development-plan.md:215 标为【待核实】（IN-07）。
9. **p50/p95 汇总缺失**（未修）：`get_health` 还没有 p50/p95 汇总（`src-tauri/src/commands/diagnostics.rs:506` 是 TODO），只有每条历史的 StageTimings，日志和诊断包里都没有耗时。release 界面只在「最近执行」显示总耗时（`total_us`，按毫秒取整），各阶段耗时只能在 debug-fi 构建里用 `list_history` 取（PF-01）。前端也没有调用 `report_metric`、`self_test`、`set_log_level`。
10. **`scripts/soak/`**（已补，`9cbe400`）：计划 L6（development-plan.md:2477）要求的浸泡监视脚本已加在 `scripts/soak/`，PF-05 使用。performance-baseline.md:82 原写「目前尚不存在」，本次修订已同步。
11. **版本号仍是 0.3.1**：所以本清单的构建栏要求写提交哈希。
12. **§5.5「现有实现位置」过时**：引用的仍是拆分前的 `windows.rs`、`windows_espanso.rs`、`main.rs` 行号；performance-baseline.md:40 也是如此。
13. **S-06 与代码参数的关系**：计划 S-06 写「50 次按键」，代码中 K=5、R=8，最多 33 次按下（`liveness.rs:47-51`，符合要求）。但「10 秒内」这一条在用户不打字时由谁保证，代码注释里没有说明（SV-05 变体 b）。
14. **计划 §11 的实机矩阵列不对应**：development-plan.md:2518-2554 的「自动」列在 0.4 没有对应物。其中管理员记事本一行（S-20）不在 0.4 范围。
15. **TODO 26**：另一个窗口里的 `save_draft` 没有纳入保存进行中的跟踪（合并记录）。本清单没有为它单列条目。
16. **熔断时没有系统通知**（未修）：S-03（development-plan.md:704）要求熔断为「故障」时发通知。托盘只在恢复监听被拒绝、启动自检失败、规则执行时发通知（`src-tauri/src/tray.rs:78-119`），`supervisor.circuit_open` 之后没有通知。SV-15 对这一项单独判定。
17. **`zzh` 的普通文字留空时，纯文本目标粘贴出空内容**（未修）：编辑器和领域模型都不检查「无法显示格式时使用的普通文字」是否为空（`src/ActionEditor.tsx:827-830`；`crates/domain/src/model.rs:549`），执行时照样把空的 CF_UNICODETEXT 和 HTML Format 一起放进剪贴板（`crates/platform/src/windows/clipboard.rs:561-573`），记事本等纯文本目标静默粘贴出空内容，没有任何提示。本清单在 §1.5 要求 `zzh` 的普通文字不能留空。
18. **外部修改后「请刷新后再试」做不到，旧草稿发布会覆盖外部修改**（未修，0.5 修）：比对用的磁盘哈希只在启动和发布时更新（`src-tauri/src/store/load.rs:85`、`revisions.rs:103`），「刷新状态」只取内存里的状态（`src-tauri/src/commands/runtime.rs:13-15`），冲突要到重启后才解除；重启后如果不放弃草稿直接应用，发布的是旧草稿，外部修改被覆盖而且没有提示（FE-04）。
19. **备份补做成功后，backup_failed 横幅当场不消失**（未修）：后端在备份成功时撤销这个问题（`43dda1f` 的 `backup.rs:618-631`，`note_backup`），但撤销不发事件；前端只在安全模式解除时用后端的列表替换问题列表，其他时候只合并新增（`src/ipc/events.ts:310-321、439-456`），所以横幅要到重启后才消失（CP-05 第 7 步，只记录）。

## 附表 A：代码与文档中的「待实测」由哪条覆盖

代码行号以 `576aa5a` 为准；表尾几行是修订时在其他分支头上新增的「待实测」，行号以第一列写明的提交为准，其中 `f1589cb` 是 `integrate/0.4`（这些分支上的文件行号整体也有移动，例如鼠标重挂分支的 `hook.rs`、`rawinput.rs`）。

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
| `clipboard.rs:98` | 复制 HMETAFILE（METAFILEPICT） | CB-04 e（必做，配合 P12 比较字节） |
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
| `f1589cb`：`src-tauri/src/startup.rs:195` | 真实会话结束时，tao 能否在进程结束前把它变成 RunEvent::Exit（会话结束回调已先清除运行标记） | SV-13 |
| `f1589cb`：`src-tauri/src/lkg.rs:127` | 上次正常配置的限制：Health 只取最新值、检查与写入之间的窗口、发布安装与事件之间的成功不计、只有 Windows 记录、60 秒的实际时序 | ST-13 |
| `384f8d1`：`crates/platform/src/windows/input.rs:901` | 鼠标点击交叉校验沿用按键的 MISS_LIMIT 及退避 | SV-16 |
| `384f8d1`：`input.rs:998`；`rawinput.rs:407`；`pipeline_tests_input.rs:3785、4004` | 注入的点击是否进入 RawInput（只按「RawInput 看到、鼠标钩子没看到」一个方向判定） | SV-16 |
