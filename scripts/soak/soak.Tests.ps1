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

$Bom = [byte[]](0xEF, 0xBB, 0xBF)
$T0 = [DateTime]::UtcNow.AddHours(-2)

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

try {
    Remove-Item -LiteralPath $TestRoot -Recurse -Force -ErrorAction SilentlyContinue
} catch { }
Write-Host ('RESULT passed=' + $script:Passed + ' failed=' + $script:FailedChecks)
exit $script:FailedChecks
