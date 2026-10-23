#requires -Version 5.1
<#
.SYNOPSIS
    soak.ps1 的回归用例（不依赖 Pester）。

.DESCRIPTION
    只加载 soak.ps1 的常量和函数（它最后一个函数定义及之前的顶层语句），不跑它的设置和主循环。
    每个用例在 %TEMP% 下自己的临时目录里构造合成日志，直接调用脚本里的函数，检查计数和汇总里的
    自动可疑项。

    不启动应用，不查询、不结束任何进程，不读写 %APPDATA% 下的数据目录，不装钩子，不产生输入。
    结束时删除自己建的临时目录。

    每个用例输出一行 PASS / FAIL；最后一行 RESULT 给出通过和失败的断言数。全部通过时退出码为 0，
    否则为失败的断言数。

.EXAMPLE
    pwsh -NoProfile -File scripts/soak/soak.Tests.ps1

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\soak\soak.Tests.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$soakPath = Join-Path $PSScriptRoot 'soak.ps1'

# ---------------------------------------------------------------------------
# Load soak.ps1's constants and functions only
# ---------------------------------------------------------------------------

$parseTokens = $null
$parseErrors = $null
$soakAst = [System.Management.Automation.Language.Parser]::ParseFile($soakPath, [ref] $parseTokens, [ref] $parseErrors)
if ($parseErrors.Count -gt 0) { throw ('soak.ps1 有语法错误：' + $parseErrors[0].Message) }
$soakStatements = @($soakAst.EndBlock.Statements)
$lastFunction = -1
for ($i = 0; $i -lt $soakStatements.Length; $i++) {
    if ($soakStatements[$i] -is [System.Management.Automation.Language.FunctionDefinitionAst]) { $lastFunction = $i }
}
if ($lastFunction -lt 0) { throw 'soak.ps1 里找不到函数定义' }
$definitions = [System.Text.StringBuilder]::new()
for ($i = 0; $i -le $lastFunction; $i++) {
    $st = $soakStatements[$i]
    # Only constants and functions may come before the setup; anything else
    # (a process query, Add-Type, a file write) must not run here.
    if (-not ($st -is [System.Management.Automation.Language.FunctionDefinitionAst] -or
            $st -is [System.Management.Automation.Language.AssignmentStatementAst])) {
        throw ('soak.ps1 第 ' + $st.Extent.StartLineNumber + ' 行：设置之前出现了常量和函数以外的语句，测试不加载它')
    }
    [void]$definitions.AppendLine($st.Extent.Text)
}
. ([scriptblock]::Create($definitions.ToString()))

# soak.ps1's parameters, as its functions read them.
$Hours = 1.0
$IntervalSeconds = 10
$ProcessName = 'qt-soak-tests-no-such-process'
$DataDirectory = '(测试：不使用数据目录)'
$UnhealthySuspectSeconds = 60
$PauseSuspectSeconds = 300
$ActiveWindowMinutes = 10

$TestRoot = Join-Path ([System.IO.Path]::GetTempPath()) ('qt-soak-tests-' + [guid]::NewGuid().ToString('N'))
if (-not [string]::IsNullOrEmpty($env:APPDATA) -and (Test-PathInside $TestRoot $env:APPDATA)) {
    throw ('临时目录落在 %APPDATA% 里，拒绝运行：' + $TestRoot)
}
[void][System.IO.Directory]::CreateDirectory($TestRoot)

# ---------------------------------------------------------------------------
# Test helpers
# ---------------------------------------------------------------------------

$script:Passed = 0
$script:FailedChecks = 0
$script:CaseName = ''
$script:CaseIndex = 0

function Assert-Equal($Expected, $Actual, [string] $What) {
    if ($Expected -eq $Actual) { $script:Passed++; return }
    $script:FailedChecks++
    Write-Host ('    FAIL ' + $What + '：期望 [' + $Expected + ']，实际 [' + $Actual + ']')
}

function Assert-True([bool] $Condition, [string] $What) {
    if ($Condition) { $script:Passed++; return }
    $script:FailedChecks++
    Write-Host ('    FAIL ' + $What)
}

function Assert-AnyContains([string[]] $Items, [string] $Needle, [string] $What) {
    foreach ($i in $Items) { if ($i.Contains($Needle)) { $script:Passed++; return } }
    $script:FailedChecks++
    Write-Host ('    FAIL ' + $What + '：没有一项包含「' + $Needle + '」；共 ' + $Items.Length + ' 项：')
    foreach ($i in $Items) { Write-Host ('         - ' + $i) }
}

function Assert-NoneContains([string[]] $Items, [string] $Needle, [string] $What) {
    foreach ($i in $Items) {
        if ($i.Contains($Needle)) {
            $script:FailedChecks++
            Write-Host ('    FAIL ' + $What + '：不应出现「' + $Needle + '」：' + $i)
            return
        }
    }
    $script:Passed++
}

# A fresh run state and an empty log directory of the case's own.
function Reset-TestState {
    Initialize-SoakState
    $script:CaseIndex++
    $caseDir = Join-Path $TestRoot ('case' + $script:CaseIndex)
    $script:LogDirectory = Join-Path $caseDir 'logs'
    $script:OutDir = Join-Path $caseDir 'out'
    [void][System.IO.Directory]::CreateDirectory($script:LogDirectory)
    $script:RunStart = [DateTime]::Now.AddHours(-3)
    $script:NativeOk = $false
    $script:NativeError = '测试中不加载'
}

function Invoke-TestCase([string] $Name, [scriptblock] $Body) {
    $script:CaseName = $Name
    $before = $script:FailedChecks
    Reset-TestState
    try {
        & $Body
    } catch {
        $script:FailedChecks++
        Write-Host ('    FAIL 异常：' + $_.Exception.Message + ' ' + $_.InvocationInfo.PositionMessage)
    }
    if ($script:FailedChecks -eq $before) { Write-Host ('PASS  ' + $Name) } else { Write-Host ('FAIL  ' + $Name) }
}

# One line in tracing_subscriber's default format.
function New-TestLine([DateTime] $Utc, [string] $Level, [string] $Event, [string] $Fields) {
    $line = $Utc.ToString("yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'", $script:Inv) + ' ' + $Level.PadLeft(5) +
        ' main quicker_text::soak_test: synthetic'
    if ($Event) { $line += ' event="' + $Event + '"' }
    if ($Fields) { $line += ' ' + $Fields }
    return $line
}

function ConvertTo-TestBytes([string[]] $Lines) {
    return $script:Utf8NoBom.GetBytes([string]::Join("`n", $Lines) + "`n")
}

# Appends to the file (creating it), as the app does.
function Add-TestBytes([string] $Path, [byte[]] $Bytes) {
    $fs = [System.IO.FileStream]::new($Path, [System.IO.FileMode]::Append, [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete)
    try { $fs.Write($Bytes, 0, $Bytes.Length) } finally { $fs.Dispose() }
}

# Truncates the same file in place (same creation time) and writes $Bytes.
function Set-TestBytesInPlace([string] $Path, [byte[]] $Bytes) {
    $fs = [System.IO.FileStream]::new($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete)
    try { $fs.SetLength(0); $fs.Write($Bytes, 0, $Bytes.Length) } finally { $fs.Dispose() }
}

# The 「自动可疑项」 of a summary.
function Get-AutoItems([string] $Text) {
    $items = [System.Collections.Generic.List[string]]::new()
    $inside = $false
    foreach ($raw in $Text.Split([char]10)) {
        $t = $raw.TrimEnd([char]13)
        if ($t.StartsWith('自动可疑项（')) { $inside = $true; continue }
        if ($t.StartsWith('人工复核项（')) { break }
        if ($inside -and $t.StartsWith('  - ')) { $items.Add($t.Substring(4)) }
    }
    return , $items.ToArray()
}

# A sample with the properties Invoke-Sample gives it.
function New-FakeSample([DateTime] $Time, [bool] $Exists, $ProcId) {
    $count = 0
    if ($Exists) { $count = 1 }
    return [pscustomobject]@{
        Time = $Time; Exists = $Exists; ProcId = $ProcId; PidChanged = $false; Count = $count
        WS = $null; PB = $null; Handles = $null; Threads = $null; Gdi = $null; User = $null
        CpuMs = $null; CpuPct = $null; IdleSec = $null
        Health = $script:Health; Listen = $script:Listen; Safe = $false; Lost = ''; Unavail = ''
        Lines = 0; Events = 0; Errors = 0; LogFound = $false
    }
}

# Stand-ins for System.Diagnostics.Process: Select-TrackedProcess reads only
# .Id and .StartTime.
function New-FakeProcess([int] $Id, [DateTime] $Start) {
    return [pscustomobject]@{ Id = $Id; StartTime = $Start }
}

$Bom = [byte[]](0xEF, 0xBB, 0xBF)
$T0 = [DateTime]::UtcNow.AddHours(-2)
# A fixed UTC noon, so grace checks do not depend on when the tests run.
$Noon = [DateTime]::SpecifyKind([DateTime]::new(2026, 10, 23, 12, 0, 0), [DateTimeKind]::Utc)
$Midnight = [DateTime]::SpecifyKind([DateTime]::new(2026, 10, 23, 0, 0, 0), [DateTimeKind]::Utc)

# ---------------------------------------------------------------------------
# Item 5: a UTF-8 BOM at the start of a file
# ---------------------------------------------------------------------------

Invoke-TestCase '5a 文件开头带 UTF-8 BOM：第一条事件不丢' {
    $p = Join-Path $script:LogDirectory 'bom.log'
    $body = ConvertTo-TestBytes @(
        (New-TestLine $T0 'INFO' 'app.start' 'version="0.4.0" debug_build=false'),
        (New-TestLine $T0.AddSeconds(1) 'INFO' 'self_test.startup' 'health=Healthy outcome="listening"'))
    [System.IO.File]::WriteAllBytes($p, [byte[]]($Bom + $body))
    $status = Invoke-LogRead $p ([DateTime]::Now)
    Assert-Equal 'ok' $status '读取状态'
    Assert-Equal 1 (Get-EventCount 'app.start') 'app.start 次数'
    Assert-Equal 2 $script:LogLines '日志行数'
    Assert-Equal 0 $script:ContinuationLines '续行数（BOM 没剥掉时第一行会被当成续行）'
    Assert-Equal 'healthy' $script:Health '推断的健康'
}

Invoke-TestCase '5b 只在偏移 0 处剥 BOM；游标复位后照样剥' {
    $p = Join-Path $script:LogDirectory 'bom-reset.log'
    [System.IO.File]::WriteAllBytes($p, (ConvertTo-TestBytes @(
                (New-TestLine $T0 'INFO' 'app.start' 'version="0.4.0"'),
                (New-TestLine $T0.AddSeconds(1) 'INFO' 'app.exit' ''),
                (New-TestLine $T0.AddSeconds(2) 'INFO' 'app.start' 'version="0.4.0"'))))
    $null = Invoke-LogRead $p ([DateTime]::Now)
    Assert-Equal 2 (Get-EventCount 'app.start') '第一次读：app.start 次数'
    # Truncated in place and written again, shorter, starting with a BOM.
    Set-TestBytesInPlace $p ([byte[]]($Bom + (ConvertTo-TestBytes @((New-TestLine $T0.AddSeconds(3) 'WARN' 'input.queue_full' 'source="hook"')))))
    $null = Invoke-LogRead $p ([DateTime]::Now)
    Assert-Equal 1 $script:LogResets '复位次数'
    Assert-Equal 1 (Get-EventCount 'input.queue_full') '复位后第一条事件'
    Assert-Equal 0 $script:ContinuationLines '复位后续行数'
    # Appended after the BOM line: read from a non-zero offset, nothing skipped.
    Add-TestBytes $p (ConvertTo-TestBytes @((New-TestLine $T0.AddSeconds(4) 'WARN' 'input.queue_full' 'source="raw"')))
    $null = Invoke-LogRead $p ([DateTime]::Now)
    Assert-Equal 2 (Get-EventCount 'input.queue_full') '偏移非 0 时不剥字节'
    Assert-Equal 0 $script:ContinuationLines '偏移非 0 时续行数'
    Assert-Equal 1 $script:LogResets '追加不复位'
}

# ---------------------------------------------------------------------------
# Item 2: the same path truncated and grown back, or replaced, between reads
# ---------------------------------------------------------------------------

Invoke-TestCase '2a 只追加：不误判复位，指纹和身份都记下' {
    $p = Join-Path $script:LogDirectory 'append.log'
    Add-TestBytes $p (ConvertTo-TestBytes @((New-TestLine $T0 'INFO' 'test.one' '')))
    $null = Invoke-LogRead $p ([DateTime]::Now)
    $c = $script:Cursors[$p]
    Assert-True ($null -ne $c.Identity) '记下了文件身份（创建时间）'
    Assert-True ($null -ne $c.Tail) '记下了偏移前的指纹'
    Add-TestBytes $p (ConvertTo-TestBytes @((New-TestLine $T0.AddSeconds(1) 'INFO' 'test.two' '')))
    $null = Invoke-LogRead $p ([DateTime]::Now)
    Add-TestBytes $p (ConvertTo-TestBytes @((New-TestLine $T0.AddSeconds(2) 'INFO' 'test.three' '')))
    $null = Invoke-LogRead $p ([DateTime]::Now)
    Assert-Equal 0 $script:LogResets '复位次数'
    Assert-Equal 1 (Get-EventCount 'test.one') 'test.one'
    Assert-Equal 1 (Get-EventCount 'test.two') 'test.two'
    Assert-Equal 1 (Get-EventCount 'test.three') 'test.three'
    Assert-Equal ([System.IO.FileInfo]::new($p)).Length $script:Cursors[$p].Offset '游标在文件末尾'
}

Invoke-TestCase '2b 截断后在两次读取之间长回原长度以上：按「内容变了」从头重读，不漏事件' {
    $p = Join-Path $script:LogDirectory 'regrow.log'
    Add-TestBytes $p (ConvertTo-TestBytes @(
            (New-TestLine $T0 'INFO' 'test.one' ''),
            (New-TestLine $T0.AddSeconds(1) 'INFO' 'test.two' ''),
            (New-TestLine $T0.AddSeconds(2) 'INFO' 'test.three' '')))
    $null = Invoke-LogRead $p ([DateTime]::Now)
    $oldOffset = $script:Cursors[$p].Offset
    $identityBefore = [System.IO.File]::GetCreationTimeUtc($p).Ticks
    Set-TestBytesInPlace $p (ConvertTo-TestBytes @(
            (New-TestLine $T0.AddMinutes(5) 'WARN' 'test.x1' ''),
            (New-TestLine $T0.AddMinutes(5).AddSeconds(1) 'WARN' 'test.x2' ''),
            (New-TestLine $T0.AddMinutes(5).AddSeconds(2) 'WARN' 'test.x3' ''),
            (New-TestLine $T0.AddMinutes(5).AddSeconds(3) 'WARN' 'test.x4' '')))
    Assert-Equal $identityBefore ([System.IO.File]::GetCreationTimeUtc($p).Ticks) '前提：同一个文件（创建时间没变）'
    Assert-True (([System.IO.FileInfo]::new($p)).Length -gt $oldOffset) '前提：新长度超过旧偏移'
    $null = Invoke-LogRead $p ([DateTime]::Now)
    Assert-Equal 1 $script:LogResets '复位次数'
    Assert-Equal 1 $script:LogResetKinds['rewritten'] '复位原因：偏移前的内容变了'
    foreach ($e in @('test.x1', 'test.x2', 'test.x3', 'test.x4')) { Assert-Equal 1 (Get-EventCount $e) ($e + ' 次数') }
    Assert-Equal 0 $script:ContinuationLines '没有半行被当成续行'
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-AnyContains $auto '从头重读 1 次' '汇总把复位列为自动可疑项'
}

Invoke-TestCase '2c 同路径换成另一个文件（创建时间不同、旧偏移前的字节相同）：按「被替换」从头重读' {
    $p = Join-Path $script:LogDirectory 'replaced.log'
    $head = @((New-TestLine $T0 'INFO' 'test.one' ''), (New-TestLine $T0.AddSeconds(1) 'INFO' 'test.two' ''))
    Add-TestBytes $p (ConvertTo-TestBytes $head)
    $null = Invoke-LogRead $p ([DateTime]::Now)
    $created = [System.IO.File]::GetCreationTimeUtc($p)
    [System.IO.File]::Delete($p)
    [System.IO.File]::WriteAllBytes($p, (ConvertTo-TestBytes ($head + @((New-TestLine $T0.AddSeconds(2) 'INFO' 'test.three' '')))))
    # NTFS tunnelling may hand a file recreated within 15 s the old creation
    # time; set a different one explicitly so the case does not depend on it.
    [System.IO.File]::SetCreationTimeUtc($p, $created.AddDays(1))
    $null = Invoke-LogRead $p ([DateTime]::Now)
    Assert-Equal 1 $script:LogResets '复位次数'
    Assert-Equal 1 $script:LogResetKinds['replaced'] '复位原因：被替换'
    Assert-Equal 1 (Get-EventCount 'test.three') '新文件的新增事件'
    Assert-Equal 2 (Get-EventCount 'test.one') '从头重读：新文件开头再计一次'
}

Invoke-TestCase '2d 文件变短：按「变短」从头重读' {
    $p = Join-Path $script:LogDirectory 'shrunk.log'
    Add-TestBytes $p (ConvertTo-TestBytes @(
            (New-TestLine $T0 'INFO' 'test.one' ''),
            (New-TestLine $T0.AddSeconds(1) 'INFO' 'test.two' '')))
    $null = Invoke-LogRead $p ([DateTime]::Now)
    Set-TestBytesInPlace $p (ConvertTo-TestBytes @((New-TestLine $T0.AddSeconds(2) 'INFO' 'test.y' '')))
    $null = Invoke-LogRead $p ([DateTime]::Now)
    Assert-Equal 1 $script:LogResetKinds['shrunk'] '复位原因：变短'
    Assert-Equal 1 (Get-EventCount 'test.y') '变短后的事件'
}

# ---------------------------------------------------------------------------
# Item 1: the day's log file missing while the process runs
# ---------------------------------------------------------------------------

Invoke-TestCase '1a 当天启动的进程：日志文件缺失超过 120 秒列 A 类，宽限内不列' {
    $p = Get-LogPathForUtc $Noon
    $start = $Noon.AddSeconds(-10)
    Assert-Equal $false (Update-LogCoverage $Noon.ToLocalTime() $p $true $start) '启动后 10 秒'
    Assert-Equal $false (Update-LogCoverage $Noon.AddSeconds(100).ToLocalTime() $p $true $start) '启动后 110 秒'
    Assert-Equal 0 @(Get-LogGapFindings).Length '宽限内没有可疑项'
    Assert-Equal $true (Update-LogCoverage $Noon.AddSeconds(115).ToLocalTime() $p $true $start) '启动后 125 秒：第一次超过宽限'
    Assert-Equal $false (Update-LogCoverage $Noon.AddSeconds(130).ToLocalTime() $p $true $start) '只在第一次超过时返回 true'
    Assert-Equal 'startup' $script:LogGap.Kind '宽限种类'
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-AnyContains $auto ('当天日志文件 ' + [System.IO.Path]::GetFileName($p) + ' 超过 120 秒仍不存在') '汇总列出'
    Assert-AnyContains $auto '到结束仍不存在' '到结束仍缺失'
    # The file appears: the stretch closes and stays listed.
    Assert-Equal $false (Update-LogCoverage $Noon.AddSeconds(140).ToLocalTime() $p $false $start) '文件出现'
    Assert-True ($null -eq $script:LogGap) '缺失段已结束'
    Assert-Equal 1 $script:LogGaps.Count '记下一段'
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-AnyContains $auto '超过 120 秒仍不存在' '结束后的缺失段照样列出'
    Assert-NoneContains $auto '到结束仍不存在' '已结束的段不写「到结束」'
}

Invoke-TestCase '1b 宽限内文件出现：不列' {
    $p = Get-LogPathForUtc $Noon
    $start = $Noon.AddSeconds(-5)
    $null = Update-LogCoverage $Noon.ToLocalTime() $p $true $start
    $null = Update-LogCoverage $Noon.AddSeconds(60).ToLocalTime() $p $true $start
    $null = Update-LogCoverage $Noon.AddSeconds(70).ToLocalTime() $p $false $start
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-NoneContains $auto '当天日志文件' '宽限内的缺失不列'
}

Invoke-TestCase '1c 跨日：从 UTC 零点算 60 分钟，50 分钟不列，61 分钟列' {
    $p = Get-LogPathForUtc $Midnight
    $start = $Midnight.AddDays(-1)
    Assert-Equal $false (Update-LogCoverage $Midnight.AddMinutes(30).ToLocalTime() $p $true $start) '零点后 30 分钟'
    Assert-Equal $false (Update-LogCoverage $Midnight.AddMinutes(50).ToLocalTime() $p $true $start) '零点后 50 分钟'
    Assert-Equal 'rollover' $script:LogGap.Kind '宽限种类'
    Assert-Equal 0 @(Get-LogGapFindings).Length '50 分钟时没有可疑项'
    Assert-Equal $true (Update-LogCoverage $Midnight.AddMinutes(61).ToLocalTime() $p $true $start) '零点后 61 分钟'
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-AnyContains $auto '超过 60 分钟仍不存在' '汇总列出'
    Assert-AnyContains $auto 'UTC 零点' '说明是跨日'
}

Invoke-TestCase '1d 读不到进程启动时间：从第一次发现算跨日宽限' {
    $p = Get-LogPathForUtc $Noon
    Assert-Equal $false (Update-LogCoverage $Noon.ToLocalTime() $p $true $null) '第一次发现'
    Assert-Equal $false (Update-LogCoverage $Noon.AddMinutes(59).ToLocalTime() $p $true $null) '59 分钟'
    Assert-Equal $true (Update-LogCoverage $Noon.AddMinutes(61).ToLocalTime() $p $true $null) '61 分钟'
    Assert-Equal 'unknown' $script:LogGap.Kind '宽限种类'
}

Invoke-TestCase '1e 换了日志文件：上一段结束，新文件另起一段' {
    $a = Get-LogPathForUtc $Midnight.AddDays(-1)
    $b = Get-LogPathForUtc $Midnight
    $start = $Midnight.AddDays(-2)
    $null = Update-LogCoverage $Midnight.AddMinutes(-1).ToLocalTime() $a $true $start
    $null = Update-LogCoverage $Midnight.AddMinutes(1).ToLocalTime() $b $true $start
    Assert-Equal 1 $script:LogGaps.Count '上一段结束'
    Assert-Equal $b $script:LogGap.Path '新的一段对应新文件'
}

Invoke-TestCase '1f 整轮没有读到任何日志：列 A 类；读到过一次就不列' {
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-AnyContains $auto '整轮没有读到任何日志文件' '没读到时列出'
    $p = Join-Path $script:LogDirectory 'present.log'
    Assert-Equal 'missing' (Invoke-LogRead $p ([DateTime]::Now)) '不存在的文件'
    Assert-Equal 0 $script:LogReadableCount '不存在的文件不算读到'
    Add-TestBytes $p (ConvertTo-TestBytes @((New-TestLine $T0 'INFO' 'test.one' '')))
    $null = Invoke-LogRead $p ([DateTime]::Now)
    Assert-Equal 1 $script:LogReadableCount '读到一次'
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-NoneContains $auto '整轮没有读到任何日志文件' '读到过就不列'
}

# ---------------------------------------------------------------------------
# Item 3: a process gone before the first sample
# ---------------------------------------------------------------------------

Invoke-TestCase '3a 启动检查看到、第一次采样前就没了：算进程不存在，结束时没有 app.exit 列 A 类' {
    $now = [DateTime]::Now
    $procs = @((New-FakeProcess 4243 $now.AddMinutes(-3)), (New-FakeProcess 4242 $now.AddMinutes(-5)))
    Assert-Equal $true (Register-StartupProcess $procs) '启动检查看到进程'
    Assert-Equal $true $script:ProcessEverSeen '算作已见过'
    Assert-Equal 4242 $script:TrackedPid '跟踪最早启动的那个'
    $script:Health = 'healthy'
    $script:LastProcessEvent = 'start'
    Set-ProcessAbsent $now
    $script:Samples.Add((New-FakeSample $now $false $null))
    Assert-True $script:AbsentTracker.IsOpen '进程不存在的时段已开始'
    Assert-Equal 'exited' $script:Health '健康改为已退出'
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-AnyContains $auto '进程不存在 1 段' '列出进程不存在'
    Assert-AnyContains $auto '结束时进程 ' '列出结束时不在且没有 app.exit'
}

Invoke-TestCase '3b 进程正常退出（读到 app.exit）：仍列进程不存在，但不列「没有正常退出」' {
    $now = [DateTime]::Now
    $null = Register-StartupProcess @((New-FakeProcess 4242 $now.AddMinutes(-5)))
    $script:Health = 'healthy'
    $script:LastProcessEvent = 'start'
    $p = Join-Path $script:LogDirectory 'exit.log'
    Add-TestBytes $p (ConvertTo-TestBytes @((New-TestLine $now.ToUniversalTime() 'INFO' 'app.exit' '')))
    $null = Invoke-LogRead $p $now
    Assert-Equal 'exit' $script:LastProcessEvent '读到 app.exit'
    Set-ProcessAbsent $now
    $script:Samples.Add((New-FakeSample $now $false $null))
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-AnyContains $auto '进程不存在 1 段' '进程不存在照样列出'
    Assert-NoneContains $auto '结束时进程 ' '正常退出不列'
}

Invoke-TestCase '3c 整轮没有看到进程：列 A 类' {
    Assert-Equal $false (Register-StartupProcess @()) '启动检查没看到'
    Set-ProcessAbsent ([DateTime]::Now)
    $script:Samples.Add((New-FakeSample ([DateTime]::Now) $false $null))
    Assert-Equal $false $script:AbsentTracker.IsOpen '没见过就不记不存在的时段'
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-AnyContains $auto '整轮没有看到进程' '列出'
}

Invoke-TestCase '3d Get-EndProcessFinding 与启动检查后的 PID 判定' {
    Assert-Equal '' (Get-EndProcessFinding $true $true 'start') '结束时在运行'
    Assert-True ((Get-EndProcessFinding $false $true 'start') -ne '') '不在且最后是 app.start'
    Assert-True ((Get-EndProcessFinding $false $true '') -ne '') '不在且日志里没有记录'
    Assert-Equal '' (Get-EndProcessFinding $false $true 'exit') '不在但正常退出'
    Assert-True ((Get-EndProcessFinding $false $false '').Contains('整轮没有看到进程')) '从没见过'
    $null = Register-StartupProcess @((New-FakeProcess 4242 ([DateTime]::Now)))
    Assert-Equal $false (Register-ProcessSeen 4242 ([DateTime]::Now)) '第一次采样是同一个 PID：不算重启'
    Assert-Equal $true (Register-ProcessSeen 4244 ([DateTime]::Now)) '换了 PID：算重启'
    Assert-Equal 2 $script:SeenPids.Count '两个 PID'
}

# ---------------------------------------------------------------------------
# Item 4: read errors during the start-up replay
# ---------------------------------------------------------------------------

Invoke-TestCase '4a 回放时今天的日志被独占：重试一次仍失败，计数、标「基线不完整」、列 A 类，监视从文件末尾开始' {
    $utc = [DateTime]::UtcNow
    $yesterday = Get-LogPathForUtc $utc.AddDays(-1)
    $today = Get-LogPathForUtc $utc
    Add-TestBytes $yesterday (ConvertTo-TestBytes @(
            (New-TestLine $utc.AddHours(-20) 'INFO' 'app.start' 'version="0.4.0"'),
            (New-TestLine $utc.AddHours(-20).AddSeconds(1) 'INFO' 'self_test.startup' 'health=Healthy outcome="listening"')))
    Add-TestBytes $today (ConvertTo-TestBytes @(
            (New-TestLine $utc.AddMinutes(-30) 'WARN' 'test.history' ''),
            (New-TestLine $utc.AddMinutes(-29) 'INFO' 'supervisor.recovering' '')))
    $todayLen = ([System.IO.FileInfo]::new($today)).Length
    $lock = [System.IO.FileStream]::new($today, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
    try {
        Invoke-Baseline $true
    } finally {
        $lock.Dispose()
    }
    Assert-Equal 1 $script:BaselineReadErrors '回放出错次数'
    Assert-Equal $true $script:BaselineIncomplete '基线不完整'
    Assert-Equal ([System.IO.Path]::GetFileName($today)) ($script:BaselineErrorFiles -join ',') '出错的文件'
    Assert-True ($script:BaselineHealthVia.Contains('基线不完整')) '起始依据标了基线不完整'
    Assert-Equal 'healthy' $script:Health '昨天的日志照样回放'
    Assert-Equal $todayLen $script:Cursors[$today].Offset '今天的游标在启动时的文件末尾'
    Assert-Equal 1 $script:LogReadableCount '昨天的文件算读到'
    Add-TestBytes $today (ConvertTo-TestBytes @((New-TestLine $utc 'WARN' 'input.queue_full' 'source="hook"')))
    $null = Invoke-LogRead $today ([DateTime]::Now)
    Assert-Equal 1 (Get-EventCount 'input.queue_full') '监视期间的新增照样计数'
    Assert-Equal 0 (Get-EventCount 'test.history') '启动前的历史不计入'
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-AnyContains $auto '启动时回放日志出错 1 次' '汇总列出'
    $text = Build-Summary '测试'
    Assert-True ($text.Contains('基线不完整')) '「健康」一节的起始依据带标记'
}

Invoke-TestCase '4b 回放第一次读失败、重试成功：不算出错' {
    $utc = [DateTime]::UtcNow
    $today = Get-LogPathForUtc $utc
    Add-TestBytes $today (ConvertTo-TestBytes @(
            (New-TestLine $utc.AddMinutes(-10) 'INFO' 'app.start' 'version="0.4.0"'),
            (New-TestLine $utc.AddMinutes(-10).AddSeconds(1) 'INFO' 'self_test.startup' 'health=Healthy outcome="listening"')))
    $todayLen = ([System.IO.FileInfo]::new($today)).Length
    $real = ${function:Read-LogChunk}
    $script:FailNext = 1
    $script:FailName = [System.IO.Path]::GetFileName($today)
    # Shadows Read-LogChunk for calls made from this case's scope (PowerShell
    # resolves commands dynamically); gone when the case returns.
    ${function:Read-LogChunk} = {
        param([string] $Path, $Cursor, [long] $StopAt)
        if ($script:FailNext -gt 0 -and $Path.EndsWith($script:FailName)) {
            $script:FailNext--
            return [pscustomobject]@{
                Status = 'error'; Offset = [long]0; Reset = ''; Bytes = [long]0; Cursor = (New-LogCursor 0)
                Lines  = ([System.Collections.Generic.List[string]]::new())
            }
        }
        return (& $real $Path $Cursor $StopAt)
    }
    Invoke-Baseline $true
    Assert-Equal 0 $script:FailNext '第一次读确实失败过'
    Assert-Equal 0 $script:BaselineReadErrors '重试成功不计出错'
    Assert-Equal $false $script:BaselineIncomplete '基线完整'
    Assert-Equal 'healthy' $script:Health '重试读到的内容照样回放'
    Assert-Equal $todayLen $script:Cursors[$today].Offset '游标在文件末尾'
    $auto = Get-AutoItems (Build-Summary '测试')
    Assert-NoneContains $auto '启动时回放日志出错' '不列'
}

# ---------------------------------------------------------------------------

try {
    Remove-Item -LiteralPath $TestRoot -Recurse -Force -ErrorAction SilentlyContinue
} catch { }
Write-Host ('RESULT passed=' + $script:Passed + ' failed=' + $script:FailedChecks)
exit $script:FailedChecks
