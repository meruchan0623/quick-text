#requires -Version 5.1
<#
.SYNOPSIS
    Quicker Text 浸泡监视（只读）：按间隔采样进程资源，统计日志事件，结束或 Ctrl+C 时写出汇总。

.DESCRIPTION
    计划 L6 / S-27 的浸泡监视工具，用法见同目录的 README.md。

    脚本本身不产生任何输入。触发词由人工打字或实体按键工具输入：外部模拟输入（SendInput、SendKeys
    之类）会让检测缓冲失效，不能代表实体键盘路径。

    脚本只做三件事：
      1. 按固定间隔查询进程（工作集、Private Bytes、句柄、线程、GDI/USER 对象、CPU 时间），写 soak.csv；
      2. 以共享只读方式打开当天的日志文件，只读新增部分，统计事件名和少量白名单字段的计数与时间戳，
         写 events.csv；
      3. 到时或 Ctrl+C 时写 summary.txt 并显示；运行中每 10 分钟刷新一次 summary.txt。

    不写数据目录，不改任何设置，不需要管理员权限，不装钩子，不注入输入。汇总和 events.csv 里不抄日志
    原文，只有事件名、白名单字段的值（只允许字母、数字和 _ . -，其他一律记为「(其他)」）、计数和时间戳。

.PARAMETER Hours
    监视时长（小时），默认 2。

.PARAMETER IntervalSeconds
    采样间隔（秒），默认 10。

.PARAMETER OutputDirectory
    输出目录，默认 %TEMP%\qt-soak-<开始时间>。不能放在数据目录里；已有上一轮结果的目录会被拒绝
    （每轮一个新目录）。

.PARAMETER ProcessName
    进程名（不带 .exe），默认 quicker-text。

.PARAMETER DataDirectory
    应用数据目录，默认 %APPDATA%\com.quickertext.desktop。日志在其下的 logs 目录。

.PARAMETER UnhealthySuspectSeconds
    健康处于「正在启动 / 恢复中 / 故障」、或输入源丢失后未重新挂接，持续多少秒算可疑，默认 60。

.PARAMETER PauseSuspectSeconds
    暂停（手动暂停、安全模式、启动时未自动监听等）持续多少秒算可疑，默认 300。

.PARAMETER ActiveWindowMinutes
    连续有输入活动多少分钟的时段列入人工复核（对照界面的执行历史），默认 10。

.EXAMPLE
    pwsh -NoProfile -File scripts/soak/soak.ps1

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\soak\soak.ps1 -Hours 0.5 -IntervalSeconds 5 -OutputDirectory D:\soak\round1
#>
[CmdletBinding()]
param(
    [ValidateRange(0.01, 168.0)]
    [double] $Hours = 2,

    [ValidateRange(1, 3600)]
    [int] $IntervalSeconds = 10,

    [string] $OutputDirectory = '',

    [ValidateNotNullOrEmpty()]
    [string] $ProcessName = 'quicker-text',

    [string] $DataDirectory = '',

    [ValidateRange(5, 86400)]
    [int] $UnhealthySuspectSeconds = 60,

    [ValidateRange(5, 86400)]
    [int] $PauseSuspectSeconds = 300,

    [ValidateRange(1, 1440)]
    [int] $ActiveWindowMinutes = 10
)

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

$script:Inv = [System.Globalization.CultureInfo]::InvariantCulture
$script:Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$script:Utf8Bom = New-Object System.Text.UTF8Encoding($true)
# S-27: 0.4 只记录内存增长，不判失败；1.0 要求增长 < 20 MB（development-plan.md「浸泡」一节）。
$script:S27LimitMB = 20
$script:MaxChunkBytes = 8MB
# Bytes before the read offset compared on each read to notice a log file that
# was truncated and grew back, or was replaced (Read-LogChunk).
$script:FingerprintBytes = 64
$script:CheckpointMinutes = 10
$script:ListLimit = 30
# How long the day's log file may be missing while the process runs before it is
# listed (A.8). tracing_appender creates the file when logging starts
# (diagnostics/logging.rs file_writer), so a process started today should have
# it at once; after UTC midnight the new day's file appears only with the first
# line written, which an idle app may not write for a while.
$script:LogStartupGraceSeconds = 120
$script:LogRolloverGraceMinutes = 60
# Pause before the one retry of a log the start-up replay could not read.
$script:BaselineRetryDelayMs = 500

# Log line head written by tracing_subscriber's default format (diagnostics/logging.rs):
#   2026-10-23T08:12:34.567890Z  WARN <thread> <target>: <message> event="x.y" key=value ...
$script:HeadRx = [regex]::new(
    '^(?<ts>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?)\s+(?<lvl>TRACE|DEBUG|INFO|WARN|ERROR)\s',
    [System.Text.RegularExpressions.RegexOptions]::CultureInvariant)
$script:EventRx = [regex]::new(
    '(?:^|\s)event="?(?<ev>[a-z0-9_]+(?:\.[a-z0-9_]+)+)"?(?=\s|$)',
    [System.Text.RegularExpressions.RegexOptions]::CultureInvariant)
$script:SafeValueRx = [regex]::new('^[A-Za-z0-9_.\-]{1,48}$',
    [System.Text.RegularExpressions.RegexOptions]::CultureInvariant)
$script:FieldRx = @{}

# Fields broken out per event. Names follow the `tracing` calls in the code
# (see README.md for the sources); values pass $SafeValueRx or become 「(其他)」.
$script:DetailFields = @{
    'app.start'              = @('version', 'debug_build')
    'input.reattach'         = @('component', 'ok')
    'input.check'            = @('source', 'result')
    'input.refused'          = @('reason')
    'input.queue_full'       = @('source')
    'fault.injected'         = @('point')
    'supervisor.recovered'   = @('state')
    'supervisor.circuit_open' = @('cause')
    'runtime.failed_latched' = @('reason')
    'store.safe_mode'        = @('reason')
    'self_test.finished'     = @('passed')
    'self_test.startup'      = @('health', 'outcome')
    'engine.manual_pause'    = @('paused')
    'executor.panic'         = @('stage', 'kind')
    'executor.manual_send_refused' = @('reason')
    'command.execute_failed' = @('category', 'reason', 'health', 'thread', 'kind')
    'command.execute_refused' = @('reason', 'limit', 'health')
    'session.end'            = @('source')
}

# Events shown in the 「关键事件」 section even when they did not occur.
$script:KeyEvents = @(
    @('supervisor.recovering', '必需组件掉线，开始恢复（健康 → 恢复中）'),
    @('supervisor.recovered', '恢复结束；state = 恢复后的健康'),
    @('supervisor.repair', '请求「修复引擎」'),
    @('supervisor.circuit_open', '熔断：引擎故障，等待修复'),
    @('runtime.failed_latched', '故障锁定，直到修复'),
    @('supervisor.unsupervised', '监督线程缺席，健康按故障发布'),
    @('supervisor.engine_unavailable', '引擎没有启动，健康以故障结束'),
    @('supervisor.driver_hung', '监督线程和替补都卡住，引擎故障'),
    @('supervisor.attempt_failed', '一次恢复尝试失败'),
    @('supervisor.attempt_hung', '恢复尝试超时仍未返回'),
    @('supervisor.panic', '监督 tick panic'),
    @('supervisor.respawn', '看门狗替换监督线程'),
    @('input.check', '输入源检查；result = lost / window_missing / unregistered 为丢失'),
    @('input.reattach', '输入组件重新挂接；ok=false 为失败'),
    @('input.refused', '候选未展开；signal = 挂接或会话信号交叉，hook_missing = 钩子缺失'),
    @('input.queue_full', '输入队列满，记录被丢弃'),
    @('hook.panic', '钩子回调 panic'),
    @('rawinput.panic', 'RawInput 回调 panic'),
    @('input.consumer_panic', '输入消费线程 panic'),
    @('executor.panic', '执行任务 panic，执行器继续服务'),
    @('executor.stalled', '执行任务超出卡住预算，被退役'),
    @('executor.fenced', '卡住的任务被隔离'),
    @('executor.unwound', '任务异常展开，已释放按住的键'),
    @('executor.send_refused', '发送锁被卡住的发送占着，没有发送'),
    @('executor.clipboard_busy', '剪贴板被卡住的任务占着，粘贴被拒'),
    @('executor.rebuilt', '执行器重建'),
    @('executor.manual_send_refused', '手动执行的输入没有发送；reason = send_lock_busy / release_owed / fenced / unclaimed'),
    @('command.execute_refused', '手动执行（「测试动作」「单步测试」）开始前被拒：之前的 4 个手动执行任务都还没结束；reason、limit、health'),
    @('command.execute_failed', '手动执行失败；category = stalled（WARN，被隔离或被发送互斥拒绝，执行器等重建）/ run_failed / spawn_failed（ERROR）'),
    @('clipboard.lease_refused', '剪贴板备份被拒，没有取得租约'),
    @('rawinput.dropped_busy', '执行器忙，RawInput 匹配被丢弃（debug 级，默认日志级别下不出现）'),
    @('runtime.events_dropped', '执行事件推送丢失（界面和托盘少收，执行历史不受影响）'),
    @('fault.injected', '注入的故障（布置或触发）'),
    @('fault.config_corrupt', '故障注入损坏 active.json'),
    @('self_test.startup', '启动自检结束；health、outcome'),
    @('self_test.finished', '一次自检结束；passed'),
    @('store.safe_mode', '进入安全模式；reason'),
    @('store.safe_mode_exit', '离开安全模式'),
    @('store.safe_mode_import', '安全模式下导入并发布'),
    @('engine.manual_pause', '手动暂停 / 继续；paused'),
    @('app.start', '进程启动'),
    @('app.exit', '进程正常退出'),
    @('app.panic', '进程内 panic')
)

$script:HealthLabel = @{
    notstarted = '未运行'; unknown = '未知'; starting = '正在启动'; healthy = '正常'
    degraded = '降级'; recovering = '恢复中'; failed = '故障'; exited = '已退出'
}
$script:ListenLabel = @{ listening = '监听中'; paused = '暂停'; unknown = '未知' }
$script:PauseWhyLabel = @{
    manual = '手动暂停'; safe_mode = '安全模式'; auto_listen_off = '启动时未开启自动监听'
    failed = '启动自检未通过'; refused = '启动时恢复监听被拒'
}
$script:LogResetLabel = @{
    shrunk = '文件变短'; replaced = '文件被替换（创建时间变了）'; rewritten = '偏移前的内容变了（截断后又长回来，或被替换）'
}
$script:UnhealthyStates = @('starting', 'recovering', 'failed')
$script:FailedEvents = @('supervisor.circuit_open', 'runtime.failed_latched', 'supervisor.unsupervised',
    'supervisor.engine_unavailable', 'supervisor.driver_hung')

# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------

function Write-Line([string] $Text) {
    try { $Host.UI.WriteLine($Text) } catch { try { [Console]::WriteLine($Text) } catch { } }
}

function Add-Count([hashtable] $Table, [string] $Key, [int] $N) {
    if ($Table.ContainsKey($Key)) { $Table[$Key] = $Table[$Key] + $N } else { $Table[$Key] = $N }
}

function Get-SortedKeys([hashtable] $Table) {
    $keys = [System.Collections.Generic.List[string]]::new()
    foreach ($k in $Table.Keys) { $keys.Add([string]$k) }
    $keys.Sort([System.StringComparer]::Ordinal)
    return $keys.ToArray()
}

function Format-Stamp([DateTime] $T) { return $T.ToString('MM-dd HH:mm:ss', $script:Inv) }

function Format-Full([DateTime] $T) { return $T.ToString('yyyy-MM-dd HH:mm:ss', $script:Inv) }

function Format-Dur([double] $Seconds) {
    if ($Seconds -lt 0) { $Seconds = 0 }
    $ts = [TimeSpan]::FromSeconds([Math]::Round($Seconds))
    $h = [int][Math]::Floor($ts.TotalHours)
    if ($h -gt 0) { return ('{0}小时{1:00}分{2:00}秒' -f $h, $ts.Minutes, $ts.Seconds) }
    if ($ts.Minutes -gt 0) { return ('{0}分{1:00}秒' -f $ts.Minutes, $ts.Seconds) }
    return ('{0}秒' -f $ts.Seconds)
}

function Format-MB($Bytes) {
    if ($null -eq $Bytes) { return '-' }
    return ([double]$Bytes / 1MB).ToString('0.0', $script:Inv) + ' MB'
}

function Format-SignedMB($Bytes) {
    if ($null -eq $Bytes) { return '-' }
    $mb = [double]$Bytes / 1MB
    $text = $mb.ToString('0.0', $script:Inv) + ' MB'
    if ($mb -gt 0) { return '+' + $text }
    return $text
}

function Format-Num($X) {
    if ($null -eq $X) { return '-' }
    return ([double]$X).ToString('0', $script:Inv)
}

function Format-SignedNum($X) {
    if ($null -eq $X) { return '-' }
    $d = [double]$X
    $text = $d.ToString('0', $script:Inv)
    if ($d -gt 0) { return '+' + $text }
    return $text
}

function Get-Label([hashtable] $Table, [string] $Key) {
    if ($Table.ContainsKey($Key)) { return $Table[$Key] }
    return $Key
}

function ConvertTo-CsvValue($V) {
    if ($null -eq $V) { return '' }
    if ($V -is [bool]) { if ($V) { return 'true' } else { return 'false' } }
    if ($V -is [double] -or $V -is [single] -or $V -is [decimal]) { return ([double]$V).ToString('0.###', $script:Inv) }
    if ($V -is [DateTime]) { return $V.ToString('yyyy-MM-dd HH:mm:ss', $script:Inv) }
    return [System.Convert]::ToString($V, $script:Inv)
}

function Test-PathInside([string] $Child, [string] $Parent) {
    $c = $Child.TrimEnd('\', '/') + '\'
    $p = $Parent.TrimEnd('\', '/') + '\'
    return $c.StartsWith($p, [StringComparison]::OrdinalIgnoreCase)
}

function Get-LogPathForUtc([DateTime] $Utc) {
    # tracing_appender Rotation::DAILY: <prefix>.<UTC yyyy-MM-dd>.<suffix>
    return [System.IO.Path]::Combine($script:LogDirectory, 'quicker-text.' + $Utc.ToString('yyyy-MM-dd', $script:Inv) + '.log')
}

function ConvertFrom-LogTime([string] $Text, [DateTime] $Fallback) {
    $dto = [DateTimeOffset]::MinValue
    $styles = [System.Globalization.DateTimeStyles]::AssumeUniversal
    if ([DateTimeOffset]::TryParse($Text, $script:Inv, $styles, [ref] $dto)) { return $dto.LocalDateTime }
    return $Fallback
}

function Get-FieldValue([string] $Line, [string] $Name, [int] $StartAt) {
    $rx = $script:FieldRx[$Name]
    if ($null -eq $rx) {
        $rx = [regex]::new('(?:^|\s)' + [regex]::Escape($Name) + '=(?:"(?<v>[^"]*)"|(?<v>[^\s"]+))',
            [System.Text.RegularExpressions.RegexOptions]::CultureInvariant)
        $script:FieldRx[$Name] = $rx
    }
    $m = $rx.Match($Line, $StartAt)
    if (-not $m.Success) { return '' }
    $v = $m.Groups['v'].Value
    if ($script:SafeValueRx.IsMatch($v)) { return $v }
    return '(其他)'
}

function ConvertTo-HealthCode([string] $Debug) {
    switch ($Debug) {
        'Starting' { return 'starting' }
        'Healthy' { return 'healthy' }
        'Degraded' { return 'degraded' }
        'Recovering' { return 'recovering' }
        'Failed' { return 'failed' }
    }
    return ''
}

# ---------------------------------------------------------------------------
# Period trackers (safe mode, process absent)
# ---------------------------------------------------------------------------

function New-Tracker {
    return [pscustomobject]@{
        IsOpen  = $false
        Since   = [DateTime]::MinValue
        Key     = ''
        Periods = ([System.Collections.Generic.List[object]]::new())
    }
}

function Open-Period($Tracker, [DateTime] $At, [string] $Key) {
    if ($Tracker.IsOpen) { return }
    $Tracker.IsOpen = $true
    $Tracker.Since = $At
    $Tracker.Key = $Key
}

function Close-Period($Tracker, [DateTime] $At) {
    if (-not $Tracker.IsOpen) { return }
    $Tracker.IsOpen = $false
    if ($script:Baseline) { return }
    $from = $Tracker.Since
    if ($from -lt $script:RunStart) { $from = $script:RunStart }
    if ($At -lt $from) { $At = $from }
    $Tracker.Periods.Add([pscustomobject]@{ From = $from; To = $At; Key = $Tracker.Key; Open = $false })
}

function Get-Periods($Tracker, [DateTime] $End) {
    $all = [System.Collections.Generic.List[object]]::new()
    foreach ($p in $Tracker.Periods) { $all.Add($p) }
    if ($Tracker.IsOpen) {
        $from = $Tracker.Since
        if ($from -lt $script:RunStart) { $from = $script:RunStart }
        $all.Add([pscustomobject]@{ From = $from; To = $End; Key = $Tracker.Key; Open = $true })
    }
    return $all.ToArray()
}

# ---------------------------------------------------------------------------
# State machines fed by log events
# ---------------------------------------------------------------------------

function Set-Health([string] $Code, [DateTime] $At, [string] $Via) {
    if ($script:Health -eq $Code) { return }
    if ($script:Baseline) {
        $script:Health = $Code
        $script:BaselineHealthVia = $Via + '，' + (Format-Stamp $At)
        return
    }
    $n = $script:HealthTransitions.Count
    if ($n -gt 0 -and $At -lt $script:HealthTransitions[$n - 1].At) { $At = $script:HealthTransitions[$n - 1].At }
    $script:HealthTransitions.Add([pscustomobject]@{ At = $At; From = $script:Health; To = $Code; Via = $Via })
    $script:Health = $Code
}

function Set-Listen([string] $Code, [string] $Why, [DateTime] $At) {
    if ($script:Listen -eq $Code -and $script:ListenWhy -eq $Why) { return }
    if ($script:Baseline) { $script:Listen = $Code; $script:ListenWhy = $Why; return }
    $n = $script:ListenTransitions.Count
    if ($n -gt 0 -and $At -lt $script:ListenTransitions[$n - 1].At) { $At = $script:ListenTransitions[$n - 1].At }
    $script:ListenTransitions.Add([pscustomobject]@{ At = $At; From = $script:Listen; To = $Code; Via = $Why })
    $script:Listen = $Code
    $script:ListenWhy = $Why
}

function Open-InputLost([string] $Source, [DateTime] $At, [string] $Result) {
    if ([string]::IsNullOrEmpty($Source)) { $Source = '(未知)' }
    if ($script:InputLost.ContainsKey($Source)) { return }
    $script:InputLost[$Source] = [pscustomobject]@{ Source = $Source; From = $At; Result = $Result }
}

function Close-InputLost([string] $Source, [DateTime] $At, [string] $Via) {
    if (-not $script:InputLost.ContainsKey($Source)) { return }
    $o = $script:InputLost[$Source]
    $script:InputLost.Remove($Source)
    if ($script:Baseline) { return }
    $from = $o.From
    if ($from -lt $script:RunStart) { $from = $script:RunStart }
    if ($At -lt $from) { $At = $from }
    $script:InputLostPeriods.Add([pscustomobject]@{
            Source = $Source; From = $from; To = $At; Result = $o.Result; Via = $Via; Open = $false })
}

function Close-AllInputLost([DateTime] $At, [string] $Via) {
    foreach ($k in @($script:InputLost.Keys)) { Close-InputLost ([string]$k) $At $Via }
}

function Update-State([string] $Ev, [hashtable] $Vals, [DateTime] $At) {
    switch ($Ev) {
        'app.start' {
            if (-not $script:Baseline) {
                $script:AppStarts++
                if ($script:LastProcessEvent -eq 'start') { $script:UncleanStarts++ }
            }
            $script:LastProcessEvent = 'start'
            Set-Health 'starting' $At 'app.start'
            Set-Listen 'unknown' 'app.start' $At
            Close-Period $script:SafeTracker $At
            Close-AllInputLost $At 'app.start'
        }
        'app.exit' {
            $script:LastProcessEvent = 'exit'
            Set-Health 'exited' $At 'app.exit'
            Set-Listen 'unknown' 'app.exit' $At
            Close-Period $script:SafeTracker $At
            Close-AllInputLost $At 'app.exit'
        }
        'self_test.startup' {
            $h = ConvertTo-HealthCode $Vals['health']
            if ($h) { Set-Health $h $At 'self_test.startup' }
            switch ($Vals['outcome']) {
                'listening' { Set-Listen 'listening' 'self_test.startup' $At }
                'failed_user_resumed' { Set-Listen 'listening' 'self_test.startup' $At }
                'auto_listen_off' { Set-Listen 'paused' 'auto_listen_off' $At }
                'safe_mode' { Set-Listen 'paused' 'safe_mode' $At }
                'failed' { Set-Listen 'paused' 'failed' $At }
                'refused' { Set-Listen 'paused' 'refused' $At }
            }
        }
        'supervisor.recovering' { Set-Health 'recovering' $At $Ev }
        'supervisor.repair' { Set-Health 'recovering' $At $Ev }
        'supervisor.recovered' {
            $h = ConvertTo-HealthCode $Vals['state']
            if ($h) { Set-Health $h $At $Ev }
            # Healthy after a recovery means every input source is back, including a
            # RawInput window the supervisor rebuilt without an input.reattach line.
            if ($h -eq 'healthy') { Close-AllInputLost $At $Ev }
        }
        'engine.manual_pause' {
            if ($Vals['paused'] -eq 'true') { Set-Listen 'paused' 'manual' $At }
            elseif ($Vals['paused'] -eq 'false') { Set-Listen 'listening' 'manual' $At }
        }
        'store.safe_mode' {
            Open-Period $script:SafeTracker $At $Vals['reason']
            Set-Listen 'paused' 'safe_mode' $At
        }
        'store.safe_mode_exit' {
            # Leaving safe mode keeps the manual pause (store/mod.rs set_safe_mode).
            Close-Period $script:SafeTracker $At
            if ($script:Listen -eq 'paused') { Set-Listen 'paused' 'manual' $At }
        }
        'store.safe_mode_import' { Close-Period $script:SafeTracker $At }
        'input.check' {
            $r = $Vals['result']
            if ($r -eq 'lost' -or $r -eq 'window_missing' -or $r -eq 'unregistered') {
                Open-InputLost $Vals['source'] $At $r
            }
        }
        'input.reattach' {
            if ($Vals['ok'] -eq 'true') { Close-InputLost $Vals['component'] $At 'input.reattach' }
            elseif ($Vals['ok'] -eq 'false' -and -not $script:Baseline) { $script:ReattachFailed++ }
        }
    }
    if ($script:FailedEvents -contains $Ev) { Set-Health 'failed' $At $Ev }
}

# ---------------------------------------------------------------------------
# Log reading
# ---------------------------------------------------------------------------

function Invoke-LogLine([string] $Line, [DateTime] $Fallback) {
    $m = $script:HeadRx.Match($Line)
    if (-not $m.Success) {
        if (-not $script:Baseline) { $script:ContinuationLines++ }
        return
    }
    $lvl = $m.Groups['lvl'].Value
    $at = ConvertFrom-LogTime $m.Groups['ts'].Value $Fallback
    $em = $script:EventRx.Match($Line, $m.Length)
    $ev = ''
    if ($em.Success) { $ev = $em.Groups['ev'].Value }
    if (-not $script:Baseline) {
        $script:LogLines++
        $script:TickLines++
        Add-Count $script:LevelCounts $lvl 1
        if ($lvl -eq 'ERROR') {
            $script:TickErrors++
            if ($ev) { Add-Count $script:ErrorEvents $ev 1 } else { Add-Count $script:ErrorEvents '(无 event 字段)' 1 }
        }
    }
    if (-not $ev) {
        if (-not $script:Baseline) { $script:NoEventLines++ }
        return
    }
    $vals = @{}
    $parts = [System.Collections.Generic.List[string]]::new()
    $fields = $script:DetailFields[$ev]
    if ($null -ne $fields) {
        foreach ($f in $fields) {
            $v = Get-FieldValue $Line $f $em.Index
            $vals[$f] = $v
            if ($v) { $parts.Add($f + '=' + $v) }
        }
    }
    if (-not $script:Baseline) {
        $detail = [string]::Join(' ', $parts.ToArray())
        $script:TickEvents++
        $st = $script:Events[$ev]
        if ($null -eq $st) {
            $st = [pscustomobject]@{ N = 0; First = $at; Last = $at; Levels = @{}; Details = @{} }
            $script:Events[$ev] = $st
        }
        $st.N = $st.N + 1
        $st.Last = $at
        Add-Count $st.Levels $lvl 1
        if ($detail) { Add-Count $st.Details $detail 1 }
        if ($null -ne $script:EventsWriter) {
            $row = (ConvertTo-CsvValue $at) + ',' + $at.ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss.fff'Z'", $script:Inv) +
                ',' + $lvl + ',' + $ev + ',"' + $detail + '"'
            $script:EventsWriter.WriteLine($row)
        }
    }
    Update-State $ev $vals $at
}

# Where reading one log file goes on from: the byte offset, the file's identity
# (its creation time in UTC ticks) and a fingerprint of the bytes just before
# the offset. Identity and fingerprint are $null when not known yet.
function New-LogCursor([long] $Offset) {
    return [pscustomobject]@{ Offset = $Offset; Identity = $null; Tail = $null }
}

# Base64 of the $script:FingerprintBytes bytes before $Offset (fewer near the
# start), or $null at offset 0 or when they cannot all be read.
function Get-TailFingerprint([System.IO.Stream] $Stream, [long] $Offset) {
    $n = [int][Math]::Min([long]$script:FingerprintBytes, $Offset)
    if ($n -le 0) { return $null }
    $buf = [byte[]]::new($n)
    [void]$Stream.Seek($Offset - $n, [System.IO.SeekOrigin]::Begin)
    $got = 0
    while ($got -lt $n) {
        $r = $Stream.Read($buf, $got, $n - $got)
        if ($r -le 0) { break }
        $got += $r
    }
    if ($got -lt $n) { return $null }
    return [System.Convert]::ToBase64String($buf)
}

# Reads complete lines of $Path from $Cursor ($null: from 0) up to $StopAt
# (exclusive), sharing the file with the writer (ReadWrite | Delete). Never
# writes. Returns the new cursor in .Cursor.
#
# The app only appends, so the cursor starts over from 0 (.Reset says why) when
# the file at $Path is no longer the one it was taken on:
#   replaced  - another creation time (deleted and created again, or renamed over);
#   shrunk    - shorter than the offset (truncated);
#   rewritten - the bytes before the offset changed (truncated and grown back past
#               the offset between two reads, or replaced by a file that kept the
#               creation time through NTFS tunnelling).
# A UTF-8 BOM is skipped at offset 0 only (including after such a restart).
function Read-LogChunk([string] $Path, $Cursor, [long] $StopAt) {
    if ($null -eq $Cursor) { $Cursor = New-LogCursor 0 }
    $Offset = [long]$Cursor.Offset
    $result = [pscustomobject]@{
        Status = 'ok'; Offset = $Offset; Reset = ''; Bytes = [long]0; Cursor = $Cursor
        Lines  = ([System.Collections.Generic.List[string]]::new())
    }
    if (-not [System.IO.File]::Exists($Path)) { $result.Status = 'missing'; return $result }
    $identity = $Cursor.Identity
    $tail = $Cursor.Tail
    $tailAt = $Offset
    $fs = $null
    try {
        $share = [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete
        $fs = [System.IO.FileStream]::new($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, $share)
        $created = $null
        try { $created = [System.IO.File]::GetCreationTimeUtc($Path).Ticks } catch { $created = $null }
        $len = $fs.Length
        if ($Offset -gt 0) {
            if ($null -ne $Cursor.Identity -and $null -ne $created -and $created -ne $Cursor.Identity) {
                $result.Reset = 'replaced'
            } elseif ($len -lt $Offset) {
                $result.Reset = 'shrunk'
            } elseif ($null -ne $Cursor.Tail -and (Get-TailFingerprint $fs $Offset) -ne $Cursor.Tail) {
                $result.Reset = 'rewritten'
            }
            if ($result.Reset) { $Offset = [long]0; $tail = $null; $tailAt = [long]0 }
        }
        if ($null -ne $created) { $identity = $created }
        $end = [Math]::Min($len, $StopAt)
        while ($Offset -lt $end) {
            $chunkStart = $Offset
            $want = [int][Math]::Min($end - $Offset, [long]$script:MaxChunkBytes)
            $buf = [byte[]]::new($want)
            [void]$fs.Seek($Offset, [System.IO.SeekOrigin]::Begin)
            $got = 0
            while ($got -lt $want) {
                $n = $fs.Read($buf, $got, $want - $got)
                if ($n -le 0) { break }
                $got += $n
            }
            if ($got -le 0) { break }
            $lf = [Array]::LastIndexOf($buf, [byte]10, $got - 1)
            if ($lf -lt 0) {
                if ($got -ge $script:MaxChunkBytes) {
                    # One line longer than a whole chunk: skip it rather than stall.
                    $Offset += $got
                    $result.Status = 'oversized'
                    continue
                }
                break # the last line is not complete yet
            }
            # tracing_appender writes no BOM, but a file saved by an editor may
            # start with one; it would hide the first line's timestamp from $HeadRx.
            $skip = 0
            if ($chunkStart -eq 0 -and $lf -ge 2 -and $buf[0] -eq 0xEF -and $buf[1] -eq 0xBB -and $buf[2] -eq 0xBF) { $skip = 3 }
            $text = $script:Utf8NoBom.GetString($buf, $skip, $lf + 1 - $skip)
            $Offset += ($lf + 1)
            $result.Bytes += ($lf + 1)
            foreach ($raw in $text.Split([char]10)) {
                $t = $raw.TrimEnd([char]13)
                if ($t.Length -gt 0) { $result.Lines.Add($t) }
            }
            if ($got -lt $want) { break }
        }
        if ($Offset -ne $tailAt) {
            $tail = Get-TailFingerprint $fs $Offset
            $tailAt = $Offset
        }
    } catch {
        $base = $_.Exception.GetBaseException()
        if ($base -is [System.IO.FileNotFoundException] -or $base -is [System.IO.DirectoryNotFoundException]) {
            $result.Status = 'missing'
        } else {
            $result.Status = 'error'
        }
    } finally {
        if ($null -ne $fs) { $fs.Dispose() }
    }
    # A fingerprint taken at another offset says nothing about this one.
    if ($Offset -ne $tailAt) { $tail = $null }
    $result.Offset = $Offset
    $result.Cursor = [pscustomobject]@{ Offset = $Offset; Identity = $identity; Tail = $tail }
    return $result
}

function Invoke-LogRead([string] $Path, [DateTime] $Now) {
    $chunk = Read-LogChunk $Path $script:Cursors[$Path] ([long]::MaxValue)
    if ($chunk.Reset) {
        $script:LogResets++
        Add-Count $script:LogResetKinds $chunk.Reset 1
    }
    if ($chunk.Status -eq 'error') { $script:LogReadErrors++ }
    if ($chunk.Status -eq 'oversized') { $script:LogOversized++ }
    if ($chunk.Status -eq 'ok' -or $chunk.Status -eq 'oversized') { $script:LogReadableCount++ }
    $script:Cursors[$Path] = $chunk.Cursor
    $script:LogBytes += $chunk.Bytes
    foreach ($l in $chunk.Lines) { Invoke-LogLine $l $Now }
    return $chunk.Status
}

# Reads what was added since the last call. Handles the daily rollover: the
# old file is drained before switching, and the new day's file is read from 0.
function Update-Logs([DateTime] $Now) {
    $today = Get-LogPathForUtc ([DateTime]::UtcNow)
    if ($today -ne $script:CurrentLog) {
        if ($script:CurrentLog) {
            $null = Invoke-LogRead $script:CurrentLog $Now
            $script:PreviousLog = $script:CurrentLog
            $script:LogRollovers++
        }
        $script:CurrentLog = $today
        if (-not $script:Cursors.ContainsKey($today)) { $script:Cursors[$today] = New-LogCursor 0 }
    } elseif ($script:PreviousLog) {
        $null = Invoke-LogRead $script:PreviousLog $Now
    }
    return (Invoke-LogRead $script:CurrentLog $Now)
}

# Follows stretches where the process runs but the day's log file $Path does not
# exist ($Missing). Each stretch gets a grace counted from when the file should
# have appeared:
#   startup  - the process started on the current UTC day ($ProcStartUtc): from
#              its start, $LogStartupGraceSeconds;
#   rollover - it started before: from UTC midnight, $LogRolloverGraceMinutes;
#   unknown  - start time not readable: from the first such sample, the
#              rollover grace.
# Returns $true on the sample where a stretch first exceeds its grace.
function Update-LogCoverage([DateTime] $Now, [string] $Path, [bool] $Missing, $ProcStartUtc) {
    $g = $script:LogGap
    if ($null -ne $g -and (-not $Missing -or $g.Path -ne $Path)) {
        $script:LogGaps.Add($g)
        $script:LogGap = $null
        $g = $null
    }
    if (-not $Missing) { return $false }
    $nowUtc = $Now.ToUniversalTime()
    if ($null -eq $g) {
        $dayStart = [DateTime]::SpecifyKind($nowUtc.Date, [DateTimeKind]::Utc)
        if ($null -ne $ProcStartUtc -and $ProcStartUtc -ge $dayStart) {
            $kind = 'startup'; $ref = $ProcStartUtc; $grace = [double]$script:LogStartupGraceSeconds
        } elseif ($null -ne $ProcStartUtc) {
            $kind = 'rollover'; $ref = $dayStart; $grace = $script:LogRolloverGraceMinutes * 60.0
        } else {
            $kind = 'unknown'; $ref = $nowUtc; $grace = $script:LogRolloverGraceMinutes * 60.0
        }
        $g = [pscustomobject]@{
            Path = $Path; Kind = $kind; Ref = $ref; Grace = $grace; First = $Now; Last = $Now; Over = $false
        }
        $script:LogGap = $g
    }
    $g.Last = $Now
    if (-not $g.Over -and ($nowUtc - $g.Ref).TotalSeconds -ge $g.Grace) {
        $g.Over = $true
        return $true
    }
    return $false
}

# One file of the start-up replay, read from 0 up to $StopAt. A read error is
# retried once; if it fails again the replay is marked incomplete.
function Read-BaselineChunk([string] $Path, [long] $StopAt) {
    $c = Read-LogChunk $Path $null $StopAt
    if ($c.Status -eq 'error') {
        Start-Sleep -Milliseconds $script:BaselineRetryDelayMs
        $c = Read-LogChunk $Path $null $StopAt
    }
    if ($c.Status -eq 'error') {
        $script:BaselineReadErrors++
        $script:BaselineIncomplete = $true
        $script:BaselineErrorFiles.Add([System.IO.Path]::GetFileName($Path))
    } elseif ($c.Status -eq 'ok' -or $c.Status -eq 'oversized') {
        $script:LogReadableCount++
    }
    return $c
}

# Replays yesterday's and today's log (UTC days) up to now to learn the state
# the app is in when monitoring starts. Nothing is counted.
function Invoke-Baseline([bool] $ProcessRunning) {
    $script:Baseline = $true
    try {
        $utc = [DateTime]::UtcNow
        $yesterday = Get-LogPathForUtc ($utc.AddDays(-1))
        $today = Get-LogPathForUtc $utc
        $todayLen = [long]0
        if ([System.IO.File]::Exists($today)) {
            try { $todayLen = ([System.IO.FileInfo]::new($today)).Length } catch { $todayLen = [long]0 }
        }
        $c = Read-BaselineChunk $yesterday ([long]::MaxValue)
        foreach ($l in $c.Lines) { Invoke-LogLine $l $script:RunStart }
        $c = Read-BaselineChunk $today $todayLen
        foreach ($l in $c.Lines) { Invoke-LogLine $l $script:RunStart }
        $script:CurrentLog = $today
        if ($c.Status -eq 'error') {
            # Monitoring still starts at the end of what was there before the run,
            # so history the replay could not read is not counted as new.
            $script:Cursors[$today] = New-LogCursor ([Math]::Max($todayLen, [long]$c.Offset))
        } else {
            $script:Cursors[$today] = $c.Cursor
        }
    } finally {
        $script:Baseline = $false
    }

    if (-not $ProcessRunning) {
        $script:Health = 'notstarted'
        $script:BaselineHealthVia = '监视开始时进程不在运行'
        $script:Listen = 'unknown'; $script:ListenWhy = ''
        $script:SafeTracker.IsOpen = $false
        $script:InputLost.Clear()
        $script:LastProcessEvent = ''
    } else {
        if ($script:Health -eq 'notstarted' -or $script:Health -eq 'exited') {
            $script:Health = 'unknown'
            $script:BaselineHealthVia = '昨天和今天（UTC）的日志里找不到当前进程的启动记录'
            $script:Listen = 'unknown'; $script:ListenWhy = ''
            $script:SafeTracker.IsOpen = $false
            $script:InputLost.Clear()
        }
        $script:LastProcessEvent = 'start'
    }
    if ($script:BaselineIncomplete) {
        $note = '基线不完整：回放 ' + [string]::Join('、', $script:BaselineErrorFiles.ToArray()) + ' 出错'
        if ($script:BaselineHealthVia) { $script:BaselineHealthVia += '；' + $note } else { $script:BaselineHealthVia = $note }
    }
    if ($script:SafeTracker.IsOpen) { $script:SafeTracker.Since = $script:RunStart }
    foreach ($k in @($script:InputLost.Keys)) { $script:InputLost[$k].From = $script:RunStart }
    $script:HealthTransitions.Add([pscustomobject]@{
            At = $script:RunStart; From = ''; To = $script:Health; Via = $script:BaselineHealthVia })
    $script:ListenTransitions.Add([pscustomobject]@{
            At = $script:RunStart; From = ''; To = $script:Listen; Via = $script:ListenWhy })
}

# ---------------------------------------------------------------------------
# Process sampling
# ---------------------------------------------------------------------------

function Get-UnavailableReasons([bool] $Exists) {
    $why = [System.Collections.Generic.List[string]]::new()
    if (-not $Exists) {
        if ($script:ProcessEverSeen) { $why.Add('进程不存在') }
    } elseif ($script:UnhealthyStates -contains $script:Health) {
        $why.Add('健康「' + (Get-Label $script:HealthLabel $script:Health) + '」')
    }
    if ($Exists -and $script:Listen -eq 'paused') {
        $why.Add('暂停（' + (Get-Label $script:PauseWhyLabel $script:ListenWhy) + '）')
    }
    if ($Exists -and $script:SafeTracker.IsOpen) { $why.Add('安全模式') }
    if ($Exists) {
        foreach ($k in @(Get-SortedKeys $script:InputLost)) { $why.Add('输入源丢失 ' + $k) }
    }
    return [string]::Join('、', $why.ToArray())
}

# The process to follow among same-named ones: the one tracked so far, else the
# earliest started.
function Select-TrackedProcess([object[]] $Procs, $TrackedPid) {
    $chosen = $null
    if ($null -ne $TrackedPid) {
        foreach ($p in $Procs) { if ($p.Id -eq $TrackedPid) { $chosen = $p; break } }
    }
    if ($null -eq $chosen) {
        $bestStart = [DateTime]::MaxValue
        foreach ($p in $Procs) {
            $st = [DateTime]::MaxValue
            try { $st = $p.StartTime } catch { }
            if ($null -eq $chosen -or $st -lt $bestStart) { $chosen = $p; $bestStart = $st }
        }
    }
    return $chosen
}

# Records that the tracked process is $ProcId at $Now. Returns $true when it is
# another PID than the one tracked so far (the process restarted).
function Register-ProcessSeen([int] $ProcId, [DateTime] $Now) {
    $changed = $false
    if ($null -ne $script:TrackedPid -and $ProcId -ne $script:TrackedPid) {
        $changed = $true
        $script:PidChanges++
    }
    $script:TrackedPid = $ProcId
    if (-not $script:SeenPids.Contains($ProcId)) { $script:SeenPids.Add($ProcId) }
    $script:ProcessEverSeen = $true
    Close-Period $script:AbsentTracker $Now
    return $changed
}

# The start-up check: picks the process to follow among $Procs and records it as
# seen, so one that is gone by the first sample counts as gone. Returns whether
# one was running.
function Register-StartupProcess([object[]] $Procs) {
    $chosen = Select-TrackedProcess $Procs $null
    if ($null -eq $chosen) { return $false }
    $null = Register-ProcessSeen $chosen.Id $script:RunStart
    return $true
}

# No process found at $Now. Counts only once the process has been seen.
function Set-ProcessAbsent([DateTime] $Now) {
    if ($script:ProcessEverSeen) {
        Open-Period $script:AbsentTracker $Now ''
        if ($script:Health -ne 'exited' -and $script:Health -ne 'notstarted') {
            Set-Health 'exited' $Now '采样时进程不存在'
            Set-Listen 'unknown' '采样时进程不存在' $Now
            Close-Period $script:SafeTracker $Now
            Close-AllInputLost $Now '采样时进程不存在'
        }
    }
}

function Invoke-Sample {
    $now = [DateTime]::Now
    $script:TickLines = 0
    $script:TickEvents = 0
    $script:TickErrors = 0
    $logStatus = 'ok'
    try { $logStatus = Update-Logs $now } catch { $script:LogReadErrors++; $logStatus = 'error' }
    $logDirExists = [System.IO.Directory]::Exists($script:LogDirectory)

    $procs = @()
    try { $procs = [System.Diagnostics.Process]::GetProcessesByName($ProcessName) } catch { $procs = @() }
    $count = 0
    if ($null -ne $procs) { $count = $procs.Length }
    if ($count -gt 1) { $script:MultiProcessSamples++ }

    $chosen = Select-TrackedProcess $procs $script:TrackedPid
    $procStartUtc = $null

    $s = [pscustomobject]@{
        Time = $now; Exists = $false; ProcId = $null; PidChanged = $false; Count = $count
        WS = $null; PB = $null; Handles = $null; Threads = $null; Gdi = $null; User = $null
        CpuMs = $null; CpuPct = $null; IdleSec = $null
        Health = ''; Listen = ''; Safe = $false; Lost = ''; Unavail = ''
        Lines = 0; Events = 0; Errors = 0; LogFound = $false
    }

    if ($null -ne $chosen) {
        $procId = $chosen.Id
        $s.Exists = $true
        $s.ProcId = $procId
        $s.PidChanged = Register-ProcessSeen $procId $now
        try { $procStartUtc = $chosen.StartTime.ToUniversalTime() } catch { $procStartUtc = $null }
        try { $s.WS = [double]$chosen.WorkingSet64 } catch { }
        try { $s.PB = [double]$chosen.PrivateMemorySize64 } catch { }
        try { $s.Handles = [double]$chosen.HandleCount } catch { }
        try { $s.Threads = [double]$chosen.Threads.Count } catch { }
        if ($script:NativeOk) {
            try {
                $g = [QtSoakNative]::GuiObjects($procId)
                if ($g[0] -ge 0) { $s.Gdi = [double]$g[0] }
                if ($g[1] -ge 0) { $s.User = [double]$g[1] }
            } catch { }
        }
        try {
            $cpu = $chosen.TotalProcessorTime
            if ($null -ne $script:LastCpu -and $script:LastCpuPid -eq $procId) {
                $deltaMs = ($cpu - $script:LastCpu).TotalMilliseconds
                $elapsedMs = ($now - $script:LastCpuAt).TotalMilliseconds
                $s.CpuMs = [double]$deltaMs
                if ($elapsedMs -gt 0) {
                    $s.CpuPct = [double]($deltaMs / ($elapsedMs * [Environment]::ProcessorCount) * 100.0)
                }
            }
            $script:LastCpu = $cpu
            $script:LastCpuAt = $now
            $script:LastCpuPid = $procId
        } catch { }
        if (-not $logDirExists) { $script:LogDirMissingSamples++ }
        elseif ($logStatus -eq 'missing') { $script:LogMissingSamples++ }
    } else {
        Set-ProcessAbsent $now
    }
    foreach ($p in $procs) { try { $p.Dispose() } catch { } }
    $logMissing = $s.Exists -and $logDirExists -and $logStatus -eq 'missing'
    if (Update-LogCoverage $now $script:CurrentLog $logMissing $procStartUtc) {
        Write-Line ('注意：进程在运行，当天日志文件 ' + [System.IO.Path]::GetFileName([string]$script:CurrentLog) +
            ' 超过宽限仍不存在。应用一直空闲时，可在「运行状态与恢复」里点一次「导出诊断包」让它写一条日志（diagnostics.export）；' +
            '文件仍不出现请检查 -DataDirectory。')
    }

    if ($script:NativeOk) {
        try {
            $idle = [QtSoakNative]::InputIdleMilliseconds()
            if ($idle -ge 0) { $s.IdleSec = [double]([Math]::Round($idle / 1000.0, 1)) }
        } catch { }
    }

    $s.Health = $script:Health
    $s.Listen = $script:Listen
    $s.Safe = [bool]$script:SafeTracker.IsOpen
    $s.Lost = [string]::Join('+', @(Get-SortedKeys $script:InputLost))
    $s.Unavail = Get-UnavailableReasons $s.Exists
    $s.Lines = $script:TickLines
    $s.Events = $script:TickEvents
    $s.Errors = $script:TickErrors
    $s.LogFound = [System.IO.File]::Exists($script:CurrentLog)
    $script:Samples.Add($s)

    if ($null -ne $script:CsvWriter) {
        $cells = @(
            (ConvertTo-CsvValue $now),
            $now.ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss'Z'", $script:Inv),
            (ConvertTo-CsvValue $s.Exists), (ConvertTo-CsvValue $s.ProcId), (ConvertTo-CsvValue $s.PidChanged),
            (ConvertTo-CsvValue $s.Count), (ConvertTo-CsvValue $s.WS), (ConvertTo-CsvValue $s.PB),
            (ConvertTo-CsvValue $s.Handles), (ConvertTo-CsvValue $s.Threads), (ConvertTo-CsvValue $s.Gdi),
            (ConvertTo-CsvValue $s.User), (ConvertTo-CsvValue $s.CpuMs), (ConvertTo-CsvValue $s.CpuPct),
            (ConvertTo-CsvValue $s.IdleSec), $s.Health, $s.Listen, (ConvertTo-CsvValue $s.Safe), $s.Lost,
            (ConvertTo-CsvValue $s.Lines), (ConvertTo-CsvValue $s.Events), (ConvertTo-CsvValue $s.Errors),
            (ConvertTo-CsvValue $s.LogFound)
        )
        $script:CsvWriter.WriteLine([string]::Join(',', $cells))
    }

    $clock = $now.ToString('HH:mm:ss', $script:Inv)
    if ($s.Exists) {
        $line = '[' + $clock + '] PID ' + $s.ProcId + '  PB ' + (Format-MB $s.PB) + '  WS ' + (Format-MB $s.WS) +
            '  句柄 ' + (Format-Num $s.Handles) + '  线程 ' + (Format-Num $s.Threads) +
            '  健康 ' + (Get-Label $script:HealthLabel $s.Health) + '  ' + (Get-Label $script:ListenLabel $s.Listen) +
            '  日志 +' + $s.Lines + ' 行 / 事件 +' + $s.Events + ' / ERROR +' + $s.Errors
        if ($s.PidChanged) { $line += '  【PID 变了】' }
    } elseif ($script:ProcessEverSeen) {
        $line = '[' + $clock + '] 进程 ' + $ProcessName + ' 不存在  健康 ' + (Get-Label $script:HealthLabel $s.Health)
    } else {
        $line = '[' + $clock + '] 等待进程 ' + $ProcessName + ' 出现'
    }
    Write-Line $line
}

# ---------------------------------------------------------------------------
# Statistics
# ---------------------------------------------------------------------------

function Get-Median([double[]] $Sorted) {
    $n = $Sorted.Length
    if ($n -eq 0) { return 0.0 }
    if ($n % 2 -eq 1) { return $Sorted[($n - 1) / 2] }
    return ($Sorted[$n / 2 - 1] + $Sorted[$n / 2]) / 2.0
}

function Get-SeriesStats([object[]] $Samples, [string] $Prop) {
    $xs = [System.Collections.Generic.List[double]]::new()
    $vs = [System.Collections.Generic.List[double]]::new()
    $t0 = [DateTime]::MinValue
    $have = $false
    foreach ($s in $Samples) {
        $v = $s.$Prop
        if ($null -eq $v) { continue }
        if (-not $have) { $t0 = $s.Time; $have = $true }
        $xs.Add(($s.Time - $t0).TotalHours)
        $vs.Add([double]$v)
    }
    $r = [pscustomobject]@{
        N = $vs.Count; Start = $null; End = $null; Min = $null; Max = $null; Growth = $null
        Medians = $null; Trend = '样本不足（少于 8 个）'; SlopePerHour = $null
    }
    if ($vs.Count -eq 0) { return $r }
    $r.Start = $vs[0]
    $r.End = $vs[$vs.Count - 1]
    $min = $vs[0]; $max = $vs[0]
    foreach ($v in $vs) { if ($v -lt $min) { $min = $v }; if ($v -gt $max) { $max = $v } }
    $r.Min = $min
    $r.Max = $max
    $r.Growth = $r.End - $r.Start
    if ($vs.Count -ge 8) {
        $meds = [System.Collections.Generic.List[double]]::new()
        for ($i = 0; $i -lt 4; $i++) {
            $a = [int][Math]::Floor($vs.Count * $i / 4.0)
            $b = [int][Math]::Floor($vs.Count * ($i + 1) / 4.0)
            $part = [double[]]::new($b - $a)
            for ($j = $a; $j -lt $b; $j++) { $part[$j - $a] = $vs[$j] }
            [Array]::Sort($part)
            $meds.Add((Get-Median $part))
        }
        $r.Medians = $meds.ToArray()
        if ($meds[0] -lt $meds[1] -and $meds[1] -lt $meds[2] -and $meds[2] -lt $meds[3]) {
            $r.Trend = '疑似单调增长（四段中位数逐段上升）'
        } else {
            $r.Trend = '未见单调增长'
        }
        $n = [double]$vs.Count
        $sx = 0.0; $sy = 0.0; $sxx = 0.0; $sxy = 0.0
        for ($i = 0; $i -lt $vs.Count; $i++) {
            $sx += $xs[$i]; $sy += $vs[$i]; $sxx += $xs[$i] * $xs[$i]; $sxy += $xs[$i] * $vs[$i]
        }
        $den = $n * $sxx - $sx * $sx
        if ($den -gt 1e-12) { $r.SlopePerHour = ($n * $sxy - $sx * $sy) / $den }
    }
    return $r
}

function Get-StatePeriods($Transitions, [DateTime] $End, [string[]] $States) {
    $list = [System.Collections.Generic.List[object]]::new()
    $cur = $null
    for ($i = 0; $i -lt $Transitions.Count; $i++) {
        $t = $Transitions[$i]
        $inside = $States -contains $t.To
        if ($inside) {
            if ($null -eq $cur) {
                $cur = [pscustomobject]@{
                    From = $t.At; To = $End; Open = $true
                    States = ([System.Collections.Generic.List[string]]::new())
                    Vias = ([System.Collections.Generic.List[string]]::new())
                }
            }
            if (-not $cur.States.Contains($t.To)) { $cur.States.Add($t.To) }
            if ($t.Via -and -not $cur.Vias.Contains($t.Via)) { $cur.Vias.Add($t.Via) }
        } elseif ($null -ne $cur) {
            $cur.To = $t.At
            $cur.Open = $false
            $list.Add($cur)
            $cur = $null
        }
    }
    if ($null -ne $cur) { $list.Add($cur) }
    return $list.ToArray()
}

function Get-StateDurations($Transitions, [DateTime] $End) {
    $d = @{}
    for ($i = 0; $i -lt $Transitions.Count; $i++) {
        $from = $Transitions[$i].At
        $to = $End
        if ($i + 1 -lt $Transitions.Count) { $to = $Transitions[$i + 1].At }
        $sec = ($to - $from).TotalSeconds
        if ($sec -lt 0) { $sec = 0 }
        $k = $Transitions[$i].To
        if ($d.ContainsKey($k)) { $d[$k] = $d[$k] + $sec } else { $d[$k] = $sec }
    }
    return $d
}

# Groups samples matching $Predicate into windows; a gap of one sample is bridged.
function Get-SampleWindows([object[]] $Samples, [scriptblock] $Predicate) {
    $list = [System.Collections.Generic.List[object]]::new()
    $cur = $null
    $gap = 0
    foreach ($s in $Samples) {
        if (& $Predicate $s) {
            if ($null -eq $cur) {
                $cur = [pscustomobject]@{
                    From = $s.Time.AddSeconds(-$IntervalSeconds); To = $s.Time; Count = 0
                    Notes = ([System.Collections.Generic.List[string]]::new())
                }
            }
            $cur.To = $s.Time
            $cur.Count = $cur.Count + 1
            if ($s.Unavail) {
                foreach ($part in $s.Unavail.Split([char]0x3001)) {
                    if ($part -and -not $cur.Notes.Contains($part)) { $cur.Notes.Add($part) }
                }
            }
            $gap = 0
        } elseif ($null -ne $cur) {
            $gap++
            if ($gap -gt 1) { $list.Add($cur); $cur = $null; $gap = 0 }
        }
    }
    if ($null -ne $cur) { $list.Add($cur) }
    return $list.ToArray()
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

function Add-SeriesLines($Out, [string] $Name, $Stats, [bool] $Bytes) {
    if ($Stats.N -eq 0) { $Out.Add($Name + '：没有数据'); return }
    if ($Bytes) {
        $Out.Add($Name + '：起始 ' + (Format-MB $Stats.Start) + '，结束 ' + (Format-MB $Stats.End) +
            '，最低 ' + (Format-MB $Stats.Min) + '，峰值 ' + (Format-MB $Stats.Max) +
            '，增长（结束 − 起始）' + (Format-SignedMB $Stats.Growth) +
            '，峰值 − 起始 ' + (Format-SignedMB ($Stats.Max - $Stats.Start)))
    } else {
        $Out.Add($Name + '：起始 ' + (Format-Num $Stats.Start) + '，结束 ' + (Format-Num $Stats.End) +
            '，最低 ' + (Format-Num $Stats.Min) + '，峰值 ' + (Format-Num $Stats.Max) +
            '，增长 ' + (Format-SignedNum $Stats.Growth))
    }
    $trend = '  趋势：' + $Stats.Trend
    if ($null -ne $Stats.Medians) {
        $parts = [System.Collections.Generic.List[string]]::new()
        foreach ($m in $Stats.Medians) { if ($Bytes) { $parts.Add((Format-MB $m)) } else { $parts.Add((Format-Num $m)) } }
        $trend += '；四段中位数 ' + [string]::Join(' / ', $parts.ToArray())
    }
    if ($null -ne $Stats.SlopePerHour) {
        if ($Bytes) { $trend += '；线性斜率 ' + (Format-SignedMB $Stats.SlopePerHour) + '/小时' }
        else { $trend += '；线性斜率 ' + ([double]$Stats.SlopePerHour).ToString('+0.0;-0.0;0', $script:Inv) + '/小时' }
    }
    $Out.Add($trend)
}

function Get-EventCount([string] $Name) {
    $st = $script:Events[$Name]
    if ($null -eq $st) { return 0 }
    return $st.N
}

function Get-DetailCount([string] $Name, [string] $Detail) {
    $st = $script:Events[$Name]
    if ($null -eq $st) { return 0 }
    $n = 0
    foreach ($k in $st.Details.Keys) {
        if ((' ' + $k + ' ').Contains(' ' + $Detail + ' ')) { $n += $st.Details[$k] }
    }
    return $n
}

function Format-EventStats($St) {
    $levels = [System.Collections.Generic.List[string]]::new()
    foreach ($l in @(Get-SortedKeys $St.Levels)) { $levels.Add($l + '×' + $St.Levels[$l]) }
    return [string]$St.N + ' 次（' + [string]::Join(' ', $levels.ToArray()) + '；首次 ' + (Format-Stamp $St.First) +
        '，末次 ' + (Format-Stamp $St.Last) + '）'
}

function Add-Limited($Out, $Items, [string] $Indent) {
    $n = 0
    foreach ($i in $Items) {
        if ($n -ge $script:ListLimit) { $Out.Add($Indent + '……另有 ' + ($Items.Count - $n) + ' 项，见 soak.csv / events.csv'); break }
        $Out.Add($Indent + $i)
        $n++
    }
}

function Format-ResetKinds {
    $parts = [System.Collections.Generic.List[string]]::new()
    foreach ($k in @('shrunk', 'replaced', 'rewritten')) {
        if ($script:LogResetKinds.ContainsKey($k)) { $parts.Add((Get-Label $script:LogResetLabel $k) + ' ' + $script:LogResetKinds[$k] + ' 次') }
    }
    return [string]::Join('、', $parts.ToArray())
}

# The A item for how the run ended on the process side, or ''. $LastExists is
# the last sample's (the start-up check's when there was no sample).
function Get-EndProcessFinding([bool] $LastExists, [bool] $EverSeen, [string] $LastProcessEvent) {
    if (-not $EverSeen) {
        return '整轮没有看到进程 ' + $ProcessName + '（启动时的检查和每次采样都没有）：这一轮没有监视到应用，不能算浸泡。' +
            '请先启动应用，或检查 -ProcessName。'
    }
    if (-not $LastExists -and $LastProcessEvent -ne 'exit') {
        return '结束时进程 ' + $ProcessName + ' 不在运行，日志里也没有它正常退出的记录（最后一次 app.start 之后没有 app.exit）：' +
            '进程可能崩溃或被结束，请看「进程」一节和数据目录里的崩溃记录。'
    }
    return ''
}

# A items for stretches where the process ran but the day's log file stayed
# missing past its grace (Update-LogCoverage).
function Get-LogGapFindings {
    $all = [System.Collections.Generic.List[object]]::new()
    foreach ($g in $script:LogGaps) { $all.Add($g) }
    if ($null -ne $script:LogGap) { $all.Add($script:LogGap) }
    $items = [System.Collections.Generic.List[string]]::new()
    foreach ($g in $all) {
        if (-not $g.Over) { continue }
        $name = [System.IO.Path]::GetFileName([string]$g.Path)
        $tail = ''
        if ([object]::ReferenceEquals($g, $script:LogGap)) { $tail = '，到结束仍不存在' }
        $seen = '采样 ' + (Format-Stamp $g.First) + ' 到 ' + (Format-Stamp $g.Last) + $tail
        $lost = '这段时间的日志脚本看不到。'
        switch ($g.Kind) {
            'startup' {
                $items.Add('进程 ' + (Format-Stamp $g.Ref.ToLocalTime()) + ' 启动后，当天日志文件 ' + $name + ' 超过 ' +
                    $script:LogStartupGraceSeconds + ' 秒仍不存在（' + $seen + '）：应用初始化日志时就会建当天的文件，' +
                    '日志可能写到了别的数据目录（-DataDirectory），或应用没能打开日志文件（log.file_unavailable 只写到 stderr）。' + $lost)
            }
            'rollover' {
                $items.Add('UTC 零点（本地 ' + (Format-Stamp $g.Ref.ToLocalTime()) + '）之后，当天日志文件 ' + $name + ' 超过 ' +
                    $script:LogRolloverGraceMinutes + ' 分钟仍不存在（' + $seen + '）：应用在零点后写第一行日志时才建新文件，' +
                    '一直空闲时也可能不写；这段时间若有暂停、恢复、导出诊断包或注入故障而文件仍没出现，说明日志没写出来，' +
                    '健康和监听状态停在零点前的推断上。')
            }
            default {
                $items.Add('当天日志文件 ' + $name + ' 超过 ' + $script:LogRolloverGraceMinutes + ' 分钟仍不存在（' + $seen +
                    '；读不到进程启动时间，按跨日宽限从第一次发现算）：日志可能写到了别的数据目录，或应用没能写日志。' + $lost)
            }
        }
    }
    return $items.ToArray()
}

function Build-Summary([string] $Status) {
    $end = [DateTime]::Now
    if ($script:HealthTransitions.Count -eq 0) {
        $script:HealthTransitions.Add([pscustomobject]@{ At = $script:RunStart; From = ''; To = $script:Health; Via = '没有读到起始日志' })
    }
    if ($script:ListenTransitions.Count -eq 0) {
        $script:ListenTransitions.Add([pscustomobject]@{ At = $script:RunStart; From = ''; To = $script:Listen; Via = '' })
    }
    $o = [System.Collections.Generic.List[string]]::new()
    $samples = $script:Samples.ToArray()
    $auto = [System.Collections.Generic.List[string]]::new()
    $manual = [System.Collections.Generic.List[string]]::new()

    $o.Add('Quicker Text 浸泡监视汇总（scripts/soak/soak.ps1，汇总格式 1）')
    $o.Add('==============================================================')
    $o.Add('状态：' + $Status)
    $o.Add('开始：' + (Format-Full $script:RunStart) + '    结束：' + (Format-Full $end) +
        '    实际时长：' + (Format-Dur ($end - $script:RunStart).TotalSeconds) + '（计划 ' + (Format-Dur ($Hours * 3600)) + '）')
    $o.Add('采样间隔 ' + $IntervalSeconds + ' 秒，采样 ' + $samples.Length + ' 次；进程名 ' + $ProcessName)
    $o.Add('数据目录（只读）：' + $DataDirectory)
    $o.Add('输出目录：' + $script:OutDir)
    if ($script:NativeOk) { $o.Add('输入活动探测（GetLastInputInfo）与 GDI/USER 计数：可用') }
    else { $o.Add('输入活动探测与 GDI/USER 计数：不可用（' + $script:NativeError + '）') }
    $o.Add('阈值：健康异常 / 输入源丢失 ≥ ' + $UnhealthySuspectSeconds + ' 秒，暂停 ≥ ' + $PauseSuspectSeconds +
        ' 秒，连续输入活动 ≥ ' + $ActiveWindowMinutes + ' 分钟')
    $o.Add('')

    # ---- memory / handles ----
    $lastPid = $script:TrackedPid
    $segment = [System.Collections.Generic.List[object]]::new()
    foreach ($s in $samples) { if ($s.Exists -and $s.ProcId -eq $lastPid) { $segment.Add($s) } }
    $seg = $segment.ToArray()
    $o.Add('== 内存（按最后一个进程实例统计） ==')
    if ($seg.Length -eq 0) {
        $o.Add('没有采到进程数据。')
    } else {
        $o.Add('进程实例 PID ' + $lastPid + '，样本 ' + $seg.Length + ' 个，' + (Format-Stamp $seg[0].Time) + ' 到 ' +
            (Format-Stamp $seg[$seg.Length - 1].Time))
        $pb = Get-SeriesStats $seg 'PB'
        Add-SeriesLines $o 'Private Bytes' $pb $true
        if ($null -ne $pb.Growth) {
            $limit = [double]$script:S27LimitMB * 1MB
            if ($pb.Growth -ge $limit) { $verdict = '达到或超过' } else { $verdict = '低于' }
            $o.Add('  S-27 参考线 ' + $script:S27LimitMB + ' MB：增长' + $verdict + '参考线（0.4 只记录，不判失败；1.0 要求增长 < 20 MB）')
        }
        Add-SeriesLines $o 'Working Set' (Get-SeriesStats $seg 'WS') $true
        if ($script:SeenPids.Count -gt 1) {
            $peak = $null; $peakPid = $null
            foreach ($s in $samples) { if ($null -ne $s.PB -and ($null -eq $peak -or $s.PB -gt $peak)) { $peak = $s.PB; $peakPid = $s.ProcId } }
            $o.Add('全程（所有实例）Private Bytes 峰值 ' + (Format-MB $peak) + '（PID ' + $peakPid + '）')
        }
        $o.Add('')
        $o.Add('== 句柄 / 线程 / GDI / USER（同一实例） ==')
        Add-SeriesLines $o '句柄' (Get-SeriesStats $seg 'Handles') $false
        Add-SeriesLines $o '线程' (Get-SeriesStats $seg 'Threads') $false
        Add-SeriesLines $o 'GDI 对象' (Get-SeriesStats $seg 'Gdi') $false
        Add-SeriesLines $o 'USER 对象' (Get-SeriesStats $seg 'User') $false
        $cpu = 0.0
        foreach ($s in $seg) { if ($null -ne $s.CpuMs) { $cpu += $s.CpuMs } }
        $o.Add('CPU 时间（同一实例，监视期间）：' + ($cpu / 1000.0).ToString('0.0', $script:Inv) + ' 秒')
    }
    $o.Add('')

    # ---- process ----
    $o.Add('== 进程 ==')
    $restarts = 0
    if ($script:SeenPids.Count -gt 1) { $restarts = $script:SeenPids.Count - 1 }
    $pidList = [System.Collections.Generic.List[string]]::new()
    foreach ($p in $script:SeenPids) { $pidList.Add([string]$p) }
    if ($script:SeenPids.Count -eq 0) { $o.Add('监视期间没有看到进程。') }
    else { $o.Add('看到的 PID：' + [string]::Join('、', $pidList.ToArray()) + '；进程重启次数（不同 PID 数 − 1）：' + $restarts) }
    $o.Add('相邻采样 PID 变化 ' + $script:PidChanges + ' 次；同名进程多于 1 个的采样 ' + $script:MultiProcessSamples + ' 次')
    $absent = @(Get-Periods $script:AbsentTracker $end)
    $absentSec = 0.0
    $absentLines = [System.Collections.Generic.List[string]]::new()
    foreach ($p in $absent) {
        $sec = ($p.To - $p.From).TotalSeconds
        $absentSec += $sec
        $tail = ''
        if ($p.Open) { $tail = '（到结束仍不存在）' }
        $absentLines.Add((Format-Stamp $p.From) + ' 到 ' + (Format-Stamp $p.To) + '，' + (Format-Dur $sec) + $tail)
    }
    $o.Add('进程不存在的时段（启动时的检查或采样第一次看到进程之后，按采样）：' + $absent.Length + ' 段，合计 ' + (Format-Dur $absentSec))
    Add-Limited $o $absentLines '  '
    $lastExists = $script:ProcessEverSeen
    if ($samples.Length -gt 0) { $lastExists = [bool]$samples[$samples.Length - 1].Exists }
    if ($lastExists) { $o.Add('结束时进程：在运行') } else { $o.Add('结束时进程：不在运行') }
    $o.Add('日志：app.start ' + $script:AppStarts + ' 次（其中前面没有 app.exit 的 ' + $script:UncleanStarts + ' 次），app.exit ' +
        (Get-EventCount 'app.exit') + ' 次，app.panic ' + (Get-EventCount 'app.panic') + ' 次')
    $endFinding = Get-EndProcessFinding $lastExists $script:ProcessEverSeen $script:LastProcessEvent
    if ($endFinding) { $auto.Add($endFinding) }
    if ($restarts -gt 0) { $auto.Add('进程重启 ' + $restarts + ' 次（PID：' + [string]::Join('、', $pidList.ToArray()) + '）。') }
    if ($absent.Length -gt 0) { $auto.Add('进程不存在 ' + $absent.Length + ' 段，合计 ' + (Format-Dur $absentSec) + '（见「进程」一节）。') }
    if ($script:UncleanStarts -gt 0) { $auto.Add('有 ' + $script:UncleanStarts + ' 次启动前面没有 app.exit（上一个进程没有正常退出）。') }
    if ((Get-EventCount 'app.panic') -gt 0) { $auto.Add('app.panic ' + (Get-EventCount 'app.panic') + ' 次（崩溃文件在数据目录的 crash 记录里）。') }
    $o.Add('')

    # ---- health ----
    $o.Add('== 健康（从日志推断） ==')
    $ht = $script:HealthTransitions
    $o.Add('起始：' + (Get-Label $script:HealthLabel $ht[0].To) + '（依据：' + $ht[0].Via + '）')
    $o.Add('结束：' + (Get-Label $script:HealthLabel $script:Health))
    $dur = Get-StateDurations $ht $end
    $durParts = [System.Collections.Generic.List[string]]::new()
    foreach ($k in @('healthy', 'degraded', 'starting', 'recovering', 'failed', 'exited', 'notstarted', 'unknown')) {
        if ($dur.ContainsKey($k)) { $durParts.Add((Get-Label $script:HealthLabel $k) + ' ' + (Format-Dur $dur[$k])) }
    }
    $o.Add('各状态累计：' + [string]::Join('，', $durParts.ToArray()))
    $toRecovering = 0; $toFailed = 0; $backHealthy = 0
    for ($i = 1; $i -lt $ht.Count; $i++) {
        if ($ht[$i].To -eq 'recovering') { $toRecovering++ }
        if ($ht[$i].To -eq 'failed') { $toFailed++ }
        if (($ht[$i].To -eq 'healthy' -or $ht[$i].To -eq 'degraded') -and
            ($ht[$i].From -eq 'recovering' -or $ht[$i].From -eq 'failed')) { $backHealthy++ }
    }
    $o.Add('进入「恢复中」' + $toRecovering + ' 次，进入「故障」' + $toFailed + ' 次，从恢复中 / 故障回到正常或降级 ' + $backHealthy + ' 次')
    $tLines = [System.Collections.Generic.List[string]]::new()
    for ($i = 1; $i -lt $ht.Count; $i++) {
        $tLines.Add((Format-Stamp $ht[$i].At) + '  ' + (Get-Label $script:HealthLabel $ht[$i].From) + ' → ' +
            (Get-Label $script:HealthLabel $ht[$i].To) + '（' + $ht[$i].Via + '）')
    }
    $o.Add('转换 ' + $tLines.Count + ' 次：')
    Add-Limited $o $tLines '  '
    $o.Add('说明：正常 ↔ 降级、正在启动 → 正常不写日志；「正常」以 self_test.startup 和 supervisor.recovered 为准，降级只在这两处可见。')

    $unhealthy = @(Get-StatePeriods $ht $end $script:UnhealthyStates)
    foreach ($p in $unhealthy) {
        $sec = ($p.To - $p.From).TotalSeconds
        if ($sec -ge $UnhealthySuspectSeconds -or $p.Open) {
            $names = [System.Collections.Generic.List[string]]::new()
            foreach ($st in $p.States) { $names.Add((Get-Label $script:HealthLabel $st)) }
            $tail = ''
            if ($p.Open) { $tail = '，到结束仍未恢复' }
            $auto.Add('健康「' + [string]::Join(' → ', $names.ToArray()) + '」' + (Format-Stamp $p.From) + ' 到 ' +
                (Format-Stamp $p.To) + '，持续 ' + (Format-Dur $sec) + $tail + '（依据：' + [string]::Join('、', $p.Vias.ToArray()) + '）。')
        }
    }
    switch ($script:Health) {
        'healthy' { }
        'degraded' { $manual.Add('日志推断的结束健康是「降级」；降级 ↔ 正常不写日志，请在界面的健康页核对结束时的实际状态。') }
        'unknown' { $manual.Add('日志里推断不出健康（' + $ht[0].Via + '）；请在界面的健康页核对，结束时应为「正常」。') }
        default { $auto.Add('日志推断的结束健康是「' + (Get-Label $script:HealthLabel $script:Health) + '」，不是「正常」。') }
    }
    $o.Add('')

    # ---- listening / safe mode / input sources ----
    $o.Add('== 监听、安全模式、输入源（从日志推断） ==')
    $lt = $script:ListenTransitions
    $startListen = Get-Label $script:ListenLabel $lt[0].To
    if ($lt[0].To -eq 'paused') { $startListen += '（' + (Get-Label $script:PauseWhyLabel $lt[0].Via) + '）' }
    $o.Add('监听起始：' + $startListen + '；结束：' + (Get-Label $script:ListenLabel $script:Listen))
    $paused = @(Get-StatePeriods $lt $end @('paused'))
    $pLines = [System.Collections.Generic.List[string]]::new()
    foreach ($p in $paused) {
        $sec = ($p.To - $p.From).TotalSeconds
        $whys = [System.Collections.Generic.List[string]]::new()
        foreach ($v in $p.Vias) { $whys.Add((Get-Label $script:PauseWhyLabel $v)) }
        $tail = ''
        if ($p.Open) { $tail = '，到结束仍暂停' }
        $text = (Format-Stamp $p.From) + ' 到 ' + (Format-Stamp $p.To) + '，' + (Format-Dur $sec) + $tail + '（' + [string]::Join('、', $whys.ToArray()) + '）'
        $pLines.Add($text)
        if ($sec -ge $PauseSuspectSeconds) { $auto.Add('暂停 ' + $text + '：这段时间打的触发词不会展开。') }
    }
    $o.Add('暂停时段 ' + $paused.Length + ' 段：')
    Add-Limited $o $pLines '  '

    $safe = @(Get-Periods $script:SafeTracker $end)
    $o.Add('安全模式 ' + $safe.Length + ' 段：')
    foreach ($p in $safe) {
        $tail = ''
        if ($p.Open) { $tail = '，到结束仍在安全模式' }
        $text = (Format-Stamp $p.From) + ' 到 ' + (Format-Stamp $p.To) + '，reason=' + $p.Key + $tail
        $o.Add('  ' + $text)
        $auto.Add('安全模式 ' + $text + '。')
    }

    $lost = [System.Collections.Generic.List[object]]::new()
    foreach ($p in $script:InputLostPeriods) { $lost.Add($p) }
    foreach ($k in @(Get-SortedKeys $script:InputLost)) {
        $x = $script:InputLost[$k]
        $from = $x.From
        if ($from -lt $script:RunStart) { $from = $script:RunStart }
        $lost.Add([pscustomobject]@{ Source = $k; From = $from; To = $end; Result = $x.Result; Via = ''; Open = $true })
    }
    $o.Add('输入源丢失 ' + $lost.Count + ' 段（input.check 丢失 → 同一组件 input.reattach ok=true 或 supervisor.recovered state=Healthy 结束）：')
    foreach ($p in $lost) {
        $sec = ($p.To - $p.From).TotalSeconds
        $tail = '，由 ' + $p.Via + ' 结束'
        if ($p.Open) { $tail = '，到结束仍未重新挂接' }
        $text = $p.Source + ' result=' + $p.Result + '：' + (Format-Stamp $p.From) + ' 到 ' + (Format-Stamp $p.To) + '，' + (Format-Dur $sec) + $tail
        $o.Add('  ' + $text)
        if ($sec -ge $UnhealthySuspectSeconds -or $p.Open) { $auto.Add('输入源丢失 ' + $text + '。') }
    }
    if ($script:ReattachFailed -gt 0) { $auto.Add('input.reattach ok=false ' + $script:ReattachFailed + ' 次（重新挂接失败）。') }
    $hm = Get-DetailCount 'input.refused' 'reason=hook_missing'
    if ($hm -gt 0) { $auto.Add('input.refused reason=hook_missing ' + $hm + ' 次：钩子缺失时有候选被丢弃，没有展开。') }
    $qf = Get-EventCount 'input.queue_full'
    if ($qf -gt 0) { $auto.Add('input.queue_full ' + $qf + ' 次：输入记录被丢弃，期间的触发词可能漏检。') }
    $ed = Get-EventCount 'runtime.events_dropped'
    if ($ed -gt 0) { $auto.Add('runtime.events_dropped ' + $ed + ' 次：执行事件没推送到界面和托盘（执行历史不受影响，界面上的实时计数可能偏少）。') }
    # Manual runs (commands/runtime.rs): run_failed and spawn_failed are ERROR
    # and listed under the ERROR rule; these two are WARN.
    $cr = Get-EventCount 'command.execute_refused'
    if ($cr -gt 0) {
        $auto.Add('command.execute_refused ' + $cr + ' 次：手动执行（「测试动作」「单步测试」）开始前被拒，之前的 4 个手动执行任务都还没结束，' +
            '有执行线程卡住没有返回。')
    }
    $cs = Get-DetailCount 'command.execute_failed' 'category=stalled'
    if ($cs -gt 0) {
        $auto.Add('command.execute_failed category=stalled ' + $cs + ' 次：手动执行卡住被隔离，或被发送互斥拒绝；执行器停用到监督线程重建为止，' +
            '期间的触发词不会展开。')
    }
    $o.Add('')

    # ---- key events ----
    $o.Add('== 关键事件计数（事件名来自代码，含 0 次） ==')
    foreach ($pair in $script:KeyEvents) {
        $name = $pair[0]
        $st = $script:Events[$name]
        if ($null -eq $st) { $o.Add($name + '：0 次 —— ' + $pair[1]); continue }
        $o.Add($name + '：' + (Format-EventStats $st) + ' —— ' + $pair[1])
        foreach ($d in @(Get-SortedKeys $st.Details)) { $o.Add('    ' + $d + '：' + $st.Details[$d]) }
    }
    $o.Add('触发词展开的执行成功 / 失败次数、执行器忙丢弃（钩子路径）：日志不记录，见「人工复核」。' +
        '手动执行只记失败和被拒（command.execute_failed / command.execute_refused），成功不记。')
    $o.Add('')

    # ---- ERROR ----
    $errTotal = 0
    foreach ($k in $script:ErrorEvents.Keys) { $errTotal += $script:ErrorEvents[$k] }
    $o.Add('== ERROR 级日志 ==')
    $o.Add('合计 ' + $errTotal + ' 条')
    foreach ($k in @(Get-SortedKeys $script:ErrorEvents)) { $o.Add('  ' + $k + '：' + $script:ErrorEvents[$k]) }
    if ($errTotal -gt 0) { $auto.Add('出现 ERROR 级日志 ' + $errTotal + ' 条（见「ERROR 级日志」一节）。') }
    $o.Add('')

    # ---- all events ----
    $o.Add('== 全部事件（按名称） ==')
    $allNames = @(Get-SortedKeys $script:Events)
    if ($allNames.Length -eq 0) { $o.Add('（监视期间日志里没有新事件）') }
    foreach ($k in $allNames) { $o.Add($k + '：' + (Format-EventStats $script:Events[$k])) }
    $o.Add('')

    # ---- manual-review heuristics on input activity ----
    if ($script:NativeOk) {
        $activePred = { param($s) $null -ne $s.IdleSec -and $s.IdleSec -lt $IntervalSeconds }
        $overlapPred = { param($s) $null -ne $s.IdleSec -and $s.IdleSec -lt $IntervalSeconds -and $s.Unavail }
        $overlap = @(Get-SampleWindows $samples $overlapPred)
        foreach ($w in $overlap) {
            $manual.Add('有输入活动但引擎不可用：' + (Format-Stamp $w.From) + ' 到 ' + (Format-Stamp $w.To) + '（' +
                [string]::Join('、', $w.Notes.ToArray()) + '）。这段时间打的触发词不会展开；若是有意暂停或注入故障可忽略。')
        }
        $activeWindows = @(Get-SampleWindows $samples $activePred)
        $activeSamples = 0
        foreach ($s in $samples) { if (& $activePred $s) { $activeSamples++ } }
        foreach ($w in $activeWindows) {
            $mins = ($w.To - $w.From).TotalMinutes
            if ($mins -ge $ActiveWindowMinutes) {
                $manual.Add('连续输入活动 ' + (Format-Stamp $w.From) + ' 到 ' + (Format-Stamp $w.To) + '（' + (Format-Dur ($mins * 60)) +
                    '）：请对照界面执行历史，确认这段时间每个触发都有执行记录。')
            }
        }
        $script:ActiveSampleCount = $activeSamples
    }

    if ($script:LogDirMissingSamples -gt 0) {
        $auto.Add('进程在运行但日志目录不存在（' + $script:LogDirMissingSamples + ' 次采样）：数据目录可能不对（-DataDirectory），或应用没能写日志（log.file_unavailable 只写到 stderr）。')
    }
    foreach ($i in @(Get-LogGapFindings)) { $auto.Add([string]$i) }
    if ($script:LogReadableCount -eq 0) {
        $auto.Add('整轮没有读到任何日志文件（含启动时的回放；日志目录 ' + $script:LogDirectory + '）：健康、监听和事件都无从推断，' +
            '这一轮的日志部分不能用。请检查 -DataDirectory，并确认应用在写日志。')
    }
    if ($script:BaselineReadErrors -gt 0) {
        $auto.Add('启动时回放日志出错 ' + $script:BaselineReadErrors + ' 次（' + [string]::Join('、', $script:BaselineErrorFiles.ToArray()) +
            '，各重试一次后仍失败）：起始的健康、监听、安全模式和输入源状态可能不对（「健康」一节的起始依据标了「基线不完整」）；' +
            '监视期间的新增日志从脚本启动时的文件末尾读起，计数不受影响。')
    }
    if ($script:LogReadErrors -gt 0) { $auto.Add('日志读取出错 ' + $script:LogReadErrors + ' 次，统计可能不全。') }
    if ($script:LogResets -gt 0) {
        $auto.Add('日志文件从头重读 ' + $script:LogResets + ' 次（' + (Format-ResetKinds) +
            '）：应用只追加日志，文件被截断或替换过；截断或替换之前、上次读取之后写进旧文件的事件会漏读，' +
            '新文件里和旧文件相同的开头会再计一次，事件计数和状态推断可能不准。')
    }
    if ($script:SampleFailures -gt 0) { $auto.Add('采样出错 ' + $script:SampleFailures + ' 次（最后一次：' + $script:LastSampleFailure + '）。') }

    # ---- silent failure ----
    $o.Add('== 「静默失效」可疑项 ==')
    $o.Add('判定方法：')
    $o.Add('  日志不记触发词的每次执行，脚本也看不到界面里的执行历史，所以「有输入却没有展开」不能直接判定。')
    $o.Add('  脚本只把能从日志和进程看出的「引擎可能没在工作」的迹象列为自动可疑项，把需要对照执行历史的列为人工复核项。')
    $o.Add('  A. 自动可疑项（任一出现即列出）：')
    $o.Add('     1. 健康（日志推断）处于正在启动 / 恢复中 / 故障，连续 ≥ ' + $UnhealthySuspectSeconds + ' 秒，或到结束仍未恢复；')
    $o.Add('        状态来源：app.start → 正在启动；self_test.startup health；supervisor.recovering、supervisor.repair → 恢复中；')
    $o.Add('        supervisor.recovered state；supervisor.circuit_open、runtime.failed_latched、supervisor.unsupervised、')
    $o.Add('        supervisor.engine_unavailable、supervisor.driver_hung → 故障；app.exit 或采样时进程不存在 → 已退出。')
    $o.Add('     2. 结束时健康（日志推断）为正在启动 / 恢复中 / 故障 / 已退出 / 未运行。')
    $o.Add('     3. 进入过安全模式（store.safe_mode）。')
    $o.Add('     4. 输入源丢失（input.check result=lost / window_missing / unregistered）后 ' + $UnhealthySuspectSeconds +
        ' 秒内没有同一组件 input.reattach ok=true（或 supervisor.recovered state=Healthy）；或 input.reattach ok=false。')
    $o.Add('     5. input.refused reason=hook_missing、input.queue_full、runtime.events_dropped、')
    $o.Add('        command.execute_refused、command.execute_failed category=stalled 出现过（手动执行的另两类是 ERROR 级，归第 8 条）。')
    $o.Add('     6. 进程重启、进程不存在（启动时的检查看到过就算）、启动前没有 app.exit、app.panic；')
    $o.Add('        结束时进程不在运行且最后一次 app.start 之后没有 app.exit；整轮没有看到进程。')
    $o.Add('     7. 暂停（engine.manual_pause paused=true、安全模式、self_test.startup outcome=auto_listen_off / failed / refused 等）连续 ≥ ' +
        $PauseSuspectSeconds + ' 秒。')
    $o.Add('     8. 出现 ERROR 级日志；进程在运行但日志目录不存在；日志读取或采样出错；日志文件被截断或替换后从头重读；')
    $o.Add('        进程在运行而当天日志文件超过宽限仍不存在（当天启动的进程从启动算 ' + $script:LogStartupGraceSeconds +
        ' 秒，跨日从 UTC 零点算 ' + $script:LogRolloverGraceMinutes + ' 分钟）；')
    $o.Add('        整轮没有读到任何日志文件；启动时回放日志出错（重试一次后仍失败，起始状态标「基线不完整」）。')
    $o.Add('  B. 人工复核项：')
    $o.Add('     1. 输入活动与上面的不可用时段重叠。输入活动 = 采样时 GetLastInputInfo 的空闲时间 < 采样间隔，')
    $o.Add('        不区分键盘和鼠标，外部注入的输入也算；只说明「有人在操作」，不说明打了触发词。')
    $o.Add('     2. 连续输入活动 ≥ ' + $ActiveWindowMinutes + ' 分钟的时段：请对照界面执行历史，确认触发都有执行记录。')
    $o.Add('     3. 结束健康为「降级」或「未知」（这两种日志里看不全）。')
    $o.Add('     4. 下列各项脚本判定不了，每轮都要人工看：')
    $o.Add('        - 触发词展开的执行成功 / 失败次数：日志不记录（手动执行只记失败和被拒）。执行历史只在内存里（最多 200 条、30 分钟），每 30 分钟内至少看一次并记下。')
    $o.Add('        - 执行器忙丢弃：钩子路径只进执行历史；RawInput 路径的 rawinput.dropped_busy 是 debug 级，默认日志级别下不出现。')
    $o.Add('        - 正常 ↔ 降级的切换：不写日志。')
    $o.Add('        - 结束时的实际健康：以界面健康页为准。')
    $o.Add('        - 每次触发是否真的产生了输出：只有测试者自己知道。')
    $o.Add('')
    $o.Add('自动可疑项（' + $auto.Count + '）：')
    if ($auto.Count -eq 0) { $o.Add('  无') } else { Add-Limited $o $auto '  - ' }
    $o.Add('人工复核项（' + $manual.Count + ' 项，另加 B.4 的固定项）：')
    if ($manual.Count -eq 0) { $o.Add('  无') } else { Add-Limited $o $manual '  - ' }
    if ($script:NativeOk) {
        $o.Add('有输入活动的采样：' + $script:ActiveSampleCount + ' / ' + $samples.Length)
    } else {
        $o.Add('输入活动探测不可用，B.1 / B.2 没有做，请按自己的打字记录核对。')
    }
    $o.Add('')

    # ---- log reading ----
    $o.Add('== 日志读取 ==')
    $o.Add('日志目录：' + $script:LogDirectory)
    $o.Add('当前文件：' + [System.IO.Path]::GetFileName([string]$script:CurrentLog))
    $lv = [System.Collections.Generic.List[string]]::new()
    foreach ($k in @('ERROR', 'WARN', 'INFO', 'DEBUG', 'TRACE')) { if ($script:LevelCounts.ContainsKey($k)) { $lv.Add($k + ' ' + $script:LevelCounts[$k]) } }
    $o.Add('新增日志行 ' + $script:LogLines + '（' + [string]::Join('，', $lv.ToArray()) + '），无 event 字段的行 ' + $script:NoEventLines +
        '，续行 ' + $script:ContinuationLines + '，读取 ' + ([double]$script:LogBytes / 1KB).ToString('0.0', $script:Inv) + ' KB')
    $resetText = '0 次'
    if ($script:LogResets -gt 0) { $resetText = [string]$script:LogResets + ' 次（' + (Format-ResetKinds) + '）' }
    $o.Add('跨日换文件 ' + $script:LogRollovers + ' 次；文件被截断或替换后从头重读 ' + $resetText + '；超长行跳过 ' + $script:LogOversized +
        ' 次；读取出错 ' + $script:LogReadErrors + ' 次')
    $gapCount = $script:LogGaps.Count
    if ($null -ne $script:LogGap) { $gapCount++ }
    $gapOver = @(Get-LogGapFindings).Length
    $o.Add('进程在运行而当天日志文件尚不存在的采样 ' + $script:LogMissingSamples + ' 次，' + $gapCount + ' 段，其中超过宽限的 ' + $gapOver +
        ' 段（宽限：当天启动的进程 ' + $script:LogStartupGraceSeconds + ' 秒，跨日 ' + $script:LogRolloverGraceMinutes +
        ' 分钟）；日志目录不存在的采样 ' + $script:LogDirMissingSamples + ' 次')
    $o.Add('读到日志文件 ' + $script:LogReadableCount + ' 次（含启动时的回放）；启动时回放出错 ' + $script:BaselineReadErrors + ' 次')
    $o.Add('隐私：本汇总和 events.csv 只含事件名、白名单字段值、计数和时间戳，不含日志原文。')
    return [string]::Join("`r`n", $o.ToArray()) + "`r`n"
}

function Write-SummaryFile([string] $Status) {
    $text = $null
    try { $text = Build-Summary $Status } catch { $text = '汇总生成失败：' + $_.Exception.Message + "`r`n" }
    try { [System.IO.File]::WriteAllText($script:SummaryPath, $text, $script:Utf8Bom) }
    catch { Write-Line ('写 summary.txt 失败：' + $_.Exception.Message) }
    return $text
}

# Run state, reset before a run (and by soak.Tests.ps1 before each case).
function Initialize-SoakState {
    $script:Samples = [System.Collections.Generic.List[object]]::new()
    $script:TrackedPid = $null
    $script:SeenPids = [System.Collections.Generic.List[int]]::new()
    $script:PidChanges = 0
    $script:MultiProcessSamples = 0
    $script:ProcessEverSeen = $false
    $script:AbsentTracker = New-Tracker
    $script:LastCpu = $null
    $script:LastCpuAt = [DateTime]::MinValue
    $script:LastCpuPid = $null
    $script:SampleFailures = 0
    $script:LastSampleFailure = ''
    $script:ActiveSampleCount = 0

    $script:Baseline = $false
    $script:CurrentLog = $null
    $script:PreviousLog = $null
    $script:Cursors = @{}
    $script:LogLines = 0
    $script:ContinuationLines = 0
    $script:NoEventLines = 0
    $script:LevelCounts = @{}
    $script:LogBytes = [long]0
    $script:LogResets = 0
    $script:LogResetKinds = @{}
    $script:LogRollovers = 0
    $script:LogMissingSamples = 0
    $script:LogDirMissingSamples = 0
    $script:LogReadErrors = 0
    $script:LogOversized = 0
    $script:LogReadableCount = 0
    $script:LogGap = $null
    $script:LogGaps = [System.Collections.Generic.List[object]]::new()
    $script:BaselineReadErrors = 0
    $script:BaselineIncomplete = $false
    $script:BaselineErrorFiles = [System.Collections.Generic.List[string]]::new()
    $script:TickLines = 0
    $script:TickEvents = 0
    $script:TickErrors = 0
    $script:Events = @{}
    $script:ErrorEvents = @{}

    $script:Health = 'notstarted'
    $script:BaselineHealthVia = ''
    $script:HealthTransitions = [System.Collections.Generic.List[object]]::new()
    $script:Listen = 'unknown'
    $script:ListenWhy = ''
    $script:ListenTransitions = [System.Collections.Generic.List[object]]::new()
    $script:SafeTracker = New-Tracker
    $script:InputLost = @{}
    $script:InputLostPeriods = [System.Collections.Generic.List[object]]::new()
    $script:ReattachFailed = 0
    $script:AppStarts = 0
    $script:UncleanStarts = 0
    $script:LastProcessEvent = ''

    $script:CsvWriter = $null
    $script:EventsWriter = $null
    $script:Completed = $false
    $script:Failure = ''
    $script:Finished = $false
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

if ([string]::IsNullOrWhiteSpace($DataDirectory)) {
    if ([string]::IsNullOrEmpty($env:APPDATA)) { throw '找不到 APPDATA 环境变量；请用 -DataDirectory 指定数据目录。' }
    $DataDirectory = Join-Path $env:APPDATA 'com.quickertext.desktop'
}
$DataDirectory = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($DataDirectory)
$script:LogDirectory = Join-Path $DataDirectory 'logs'
if ($ProcessName.EndsWith('.exe', [StringComparison]::OrdinalIgnoreCase)) {
    $ProcessName = $ProcessName.Substring(0, $ProcessName.Length - 4)
}

$script:RunStart = [DateTime]::Now
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ('qt-soak-' + $script:RunStart.ToString('yyyyMMdd-HHmmss', $script:Inv))
}
$script:OutDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory)
if (Test-PathInside $script:OutDir $DataDirectory) {
    throw ('输出目录不能放在数据目录里：' + $script:OutDir)
}
$csvPath = Join-Path $script:OutDir 'soak.csv'
$eventsPath = Join-Path $script:OutDir 'events.csv'
$script:SummaryPath = Join-Path $script:OutDir 'summary.txt'
foreach ($f in @($csvPath, $eventsPath, $script:SummaryPath)) {
    if ([System.IO.File]::Exists($f)) { throw ('输出目录里已有上一轮的结果（' + $f + '）。每轮请用新的输出目录。') }
}
[void][System.IO.Directory]::CreateDirectory($script:OutDir)

# Read-only Win32 queries: last input time (GetLastInputInfo) and GDI/USER object
# counts (GetGuiResources). No hooks, no input, nothing written.
$script:NativeOk = $false
$script:NativeError = ''
try {
    if (-not ('QtSoakNative' -as [type])) {
        Add-Type -Language CSharp -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class QtSoakNative
{
    [StructLayout(LayoutKind.Sequential)]
    private struct LASTINPUTINFO
    {
        public uint cbSize;
        public uint dwTime;
    }

    [DllImport("user32.dll")]
    private static extern bool GetLastInputInfo(ref LASTINPUTINFO plii);

    [DllImport("user32.dll")]
    private static extern uint GetGuiResources(IntPtr hProcess, uint uiFlags);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(uint dwDesiredAccess, bool bInheritHandle, int dwProcessId);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr hObject);

    private const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;

    public static long InputIdleMilliseconds()
    {
        LASTINPUTINFO info = new LASTINPUTINFO();
        info.cbSize = (uint)Marshal.SizeOf(typeof(LASTINPUTINFO));
        if (!GetLastInputInfo(ref info)) { return -1; }
        return (long)unchecked((uint)Environment.TickCount - info.dwTime);
    }

    public static long[] GuiObjects(int pid)
    {
        long[] result = new long[] { -1, -1 };
        IntPtr h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
        if (h == IntPtr.Zero) { return result; }
        try
        {
            result[0] = GetGuiResources(h, 0);
            result[1] = GetGuiResources(h, 1);
        }
        finally
        {
            CloseHandle(h);
        }
        return result;
    }
}
'@
    }
    $script:NativeOk = $true
} catch {
    $script:NativeError = $_.Exception.Message
}

# State
Initialize-SoakState

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------

try {
    $script:CsvWriter = New-Object System.IO.StreamWriter($csvPath, $false, $script:Utf8Bom)
    $script:CsvWriter.AutoFlush = $true
    $script:CsvWriter.WriteLine('TimeLocal,TimeUtc,ProcessExists,ProcessId,PidChanged,ProcessCount,WorkingSetBytes,PrivateBytes,Handles,Threads,GdiObjects,UserObjects,CpuMsDelta,CpuPercent,InputIdleSec,HealthFromLog,ListeningFromLog,SafeModeFromLog,InputLostFromLog,NewLogLines,NewEvents,NewErrors,LogFileFound')
    $script:EventsWriter = New-Object System.IO.StreamWriter($eventsPath, $false, $script:Utf8Bom)
    $script:EventsWriter.AutoFlush = $true
    $script:EventsWriter.WriteLine('TimeLocal,TimeUtc,Level,Event,Detail')

    $startProcs = @()
    try { $startProcs = @([System.Diagnostics.Process]::GetProcessesByName($ProcessName)) } catch { $startProcs = @() }
    $running = Register-StartupProcess $startProcs
    foreach ($p in $startProcs) { try { $p.Dispose() } catch { } }
    Invoke-Baseline $running

    Write-Line 'Quicker Text 浸泡监视（只读：只查询进程、只读日志，不写数据目录，不产生输入）'
    Write-Line ('进程名 ' + $ProcessName + '；数据目录 ' + $DataDirectory)
    Write-Line ('时长 ' + (Format-Dur ($Hours * 3600)) + '，间隔 ' + $IntervalSeconds + ' 秒；输出 ' + $script:OutDir)
    Write-Line ('起始健康（日志推断）：' + (Get-Label $script:HealthLabel $script:Health) + '（' + $script:BaselineHealthVia + '）')
    if ($script:BaselineIncomplete) {
        Write-Line ('注意：启动时回放日志出错（' + [string]::Join('、', $script:BaselineErrorFiles.ToArray()) +
            '，已重试一次），起始状态可能不对；汇总会列为自动可疑项。')
    }
    if (-not [System.IO.Directory]::Exists($script:LogDirectory)) {
        Write-Line ('注意：日志目录不存在：' + $script:LogDirectory + '。数据目录不对时请用 -DataDirectory 指定。')
    }
    if (-not [string]::IsNullOrEmpty($env:QUICKER_TEXT_DATA_DIR) -and -not $PSBoundParameters.ContainsKey('DataDirectory')) {
        Write-Line '注意：当前终端设置了 QUICKER_TEXT_DATA_DIR。release 版只在验收模式下使用它；脚本默认读 %APPDATA% 下的数据目录，要读别的目录请用 -DataDirectory。'
    }
    if (-not $script:NativeOk) { Write-Line ('注意：输入活动探测和 GDI/USER 计数不可用：' + $script:NativeError) }
    Write-Line '触发词请人工打字或用实体按键工具输入；外部模拟输入不能代表实体键盘路径。按 Ctrl+C 可提前结束，汇总照样写出。'
    Write-Line ''

    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    $totalSec = $Hours * 3600.0
    $nextCheckpoint = $script:CheckpointMinutes * 60.0
    while ($true) {
        try {
            Invoke-Sample
        } catch [System.Management.Automation.PipelineStoppedException] {
            throw
        } catch {
            $script:SampleFailures++
            $script:LastSampleFailure = $_.Exception.GetType().Name + ': ' + $_.Exception.Message
            Write-Line ('采样出错：' + $script:LastSampleFailure)
        }
        $elapsed = $clock.Elapsed.TotalSeconds
        if ($elapsed -ge $nextCheckpoint -and $elapsed -lt $totalSec) {
            $null = Write-SummaryFile ('进行中（检查点 ' + [DateTime]::Now.ToString('HH:mm:ss', $script:Inv) + '）')
            $nextCheckpoint += $script:CheckpointMinutes * 60.0
        }
        if ($elapsed -ge $totalSec) { break }
        $due = ([Math]::Floor($elapsed / $IntervalSeconds) + 1) * $IntervalSeconds
        if ($due -gt $totalSec) { $due = $totalSec }
        $waitMs = ($due - $clock.Elapsed.TotalSeconds) * 1000.0
        if ($waitMs -gt 0) { Start-Sleep -Milliseconds ([int][Math]::Ceiling($waitMs)) }
    }
    $script:Completed = $true
} catch {
    $script:Failure = $_.Exception.GetType().FullName + ': ' + $_.Exception.Message
    throw
} finally {
    if (-not $script:Finished) {
        $script:Finished = $true
        if (-not $script:Completed -and $null -ne $script:CsvWriter) {
            try { Invoke-Sample } catch { }
        }
        if ($script:Completed) {
            $status = '已完成'
        } elseif ($script:Failure -and $script:Failure -notmatch 'PipelineStopped') {
            $status = '因错误提前结束：' + $script:Failure
        } else {
            $status = '被中断（Ctrl+C），以下是中断前已有的数据'
        }
        $summaryText = Write-SummaryFile $status
        if ($null -ne $script:CsvWriter) { try { $script:CsvWriter.Dispose() } catch { } }
        if ($null -ne $script:EventsWriter) { try { $script:EventsWriter.Dispose() } catch { } }
        Write-Line ''
        Write-Line $summaryText
        Write-Line ('结果：' + $script:OutDir + '（soak.csv、events.csv、summary.txt）')
    }
}
