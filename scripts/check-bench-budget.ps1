#requires -Version 7.0
<#
.SYNOPSIS
  B-53 gate: per-keystroke matching with 1000 rules stays within 120% of its absolute budget.

.DESCRIPTION
  Reads what `cargo bench -p quicker-domain --bench matching` (crates/domain/benches/matching.rs)
  left in target/criterion and fails when any benchmark of the group is slower than
  TolerancePercent of BudgetMs. The budget is the development plan's "matching p99 < 1 ms";
  docs/performance-baseline.md, rule 3: CI fails above 120% of the absolute budget, i.e. 1.2 ms.
  Only absolute numbers are compared. Earlier runs (criterion's `base`) are not read, so a
  slower but in-budget run passes; the trend is in the uploaded target/criterion artifact.

  Complete and current results only (Codex review 2). The script carries the list of the seven
  cases of group match_1000_rules (the names in matching.rs):
    char_miss, char_immediate_hit, terminator_miss, terminator_hit, terminator_regex_hit,
    terminator_miss_full_buffer, typing_stream
  and refuses (exit 2):
    * a missing case, naming every missing one: a filtered run such as `-- char_miss` would
      otherwise pass on the cheapest case alone;
    * a result directory with a sample.json for a case not in the list: a renamed or added
      benchmark must be added here, and an old directory must not be judged in its place;
    * results that are not from one run: every sample.json must be written within
      MaxRunMinutes of the newest one (a full run writes all seven within about two minutes),
      so a case filled in by an earlier run fails;
    * with -NotBefore, any sample.json written before that time. CI deletes the group's results
      and records the time before `cargo bench`, then passes it here.
  When a case of matching.rs is renamed, added or removed, update $ExpectedBenches below.

  The statistic, an approximation of p99:

    criterion does not record single iterations. Each of its samples is the total time of
    iters[i] back-to-back iterations (one iteration = one keystroke through the matcher), in
    <CriterionDir>/match_1000_rules/<bench>/new/sample.json as {"sampling_mode", "iters",
    "times"} (times in nanoseconds). The benchmark uses SamplingMode::Flat, so every sample has
    the same number of iterations; this script refuses other modes, because samples of different
    sizes would not be comparable.

      per-sample mean  m[i] = times[i] / iters[i]        (time per keystroke in sample i)
      p99              = nearest-rank 99th percentile of m: sort ascending, take the
                         ceil(0.99 * n)-th value (1-based). With n = 200 samples that is the
                         198th, i.e. the third-slowest sample.

    This is the p99 of per-sample means, not of single keystrokes. A rare slow keystroke is
    averaged with the other iterations of its sample, so the true per-keystroke p99 can be
    higher; a scheduling hiccup on the runner moves one sample rather than one keystroke, so
    the statistic is also steadier than a raw p99 on a shared machine. The 20% tolerance is
    for that noise, not for the averaging. Every single-case benchmark repeats one fixed
    keystroke, so its iterations cost about the same and the averaging hides little.
    typing_stream cycles through a paragraph: its per-sample mean mixes cheap letters with
    expensive terminator keys and says nothing about the slowest keystroke. That is what the
    single-case benchmarks (terminator_miss and terminator_miss_full_buffer above all) are for.

  Following rule 5 of docs/performance-baseline.md (fewer than 100 samples: give a range, not
  a p99), a benchmark with fewer than MinSamples samples fails the check instead of producing a
  p99.

  Exit codes: 0 every case present, current and within the limit; 1 at least one over it; 2 no,
  incomplete, stale or unusable results (missing directory, missing or unexpected case, results
  from different runs or older than -NotBefore, malformed sample.json, wrong sampling mode, too
  few samples). When GITHUB_STEP_SUMMARY is set, the table is appended to it as well.

  -SelfTest runs the script on synthetic criterion directories (complete and in budget, one case
  over, no results, only some cases, one stale case, one case older than -NotBefore, an
  unexpected case, wrong sampling mode, too few samples, malformed JSON) and checks the exit code
  and, where it matters, the message. It prints only failures and a summary line; add -Verbose
  for one line per case.

.PARAMETER CriterionDir
  criterion's output directory. Default: target/criterion under the repository root.

.PARAMETER BudgetMs
  The absolute budget in milliseconds. Default: 1.0 (matching p99 < 1 ms).

.PARAMETER TolerancePercent
  Fail above this percentage of BudgetMs. Default: 120.

.PARAMETER MinSamples
  Fewest samples a benchmark may have. Default: 100.

.PARAMETER NotBefore
  Results written before this time are stale (exit 2). CI passes the time it cleared the
  results, just before `cargo bench`. Default: not checked.

.PARAMETER MaxRunMinutes
  Largest gap allowed between the oldest and the newest sample.json of the seven cases.
  Default: 20.

.PARAMETER SelfTest
  Run the built-in tests and exit.

.EXAMPLE
  cargo bench -p quicker-domain --bench matching
  pwsh -NoProfile -File scripts/check-bench-budget.ps1

.EXAMPLE
  # Prove that the check can fail: a budget of 1 microsecond.
  pwsh -NoProfile -File scripts/check-bench-budget.ps1 -BudgetMs 0.001

.EXAMPLE
  pwsh -NoProfile -File scripts/check-bench-budget.ps1 -SelfTest
#>
[CmdletBinding(DefaultParameterSetName = 'Check')]
param(
  [Parameter(ParameterSetName = 'Check')]
  [string]$CriterionDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target/criterion'),
  [Parameter(ParameterSetName = 'Check')]
  [double]$BudgetMs = 1.0,
  [Parameter(ParameterSetName = 'Check')]
  [double]$TolerancePercent = 120,
  [Parameter(ParameterSetName = 'Check')]
  [int]$MinSamples = 100,
  [Parameter(ParameterSetName = 'Check')]
  [Nullable[datetime]]$NotBefore = $null,
  [Parameter(ParameterSetName = 'Check')]
  [double]$MaxRunMinutes = 20,
  [Parameter(ParameterSetName = 'SelfTest', Mandatory = $true)]
  [switch]$SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Group = 'match_1000_rules'
# The cases of crates/domain/benches/matching.rs, group match_1000_rules. Keep in step with it.
$ExpectedBenches = @(
  'char_miss',
  'char_immediate_hit',
  'terminator_miss',
  'terminator_hit',
  'terminator_regex_hit',
  'terminator_miss_full_buffer',
  'typing_stream'
)

if ($SelfTest) {
  $pwsh = [Environment]::ProcessPath
  if (-not $pwsh -or -not (Test-Path -LiteralPath $pwsh -PathType Leaf)) { $pwsh = 'pwsh' }
  $tmp = Join-Path ([IO.Path]::GetTempPath()) ('check-bench-budget-' + [Guid]::NewGuid().ToString('N'))
  $failures = 0
  $cases = 0
  # Children must not append their tables to the CI step summary.
  $savedSummary = $env:GITHUB_STEP_SUMMARY
  $env:GITHUB_STEP_SUMMARY = $null

  function New-Results([string]$Name, [string[]]$Benches, [double]$MeanNs = 300000, [int]$N = 200, [string]$Mode = 'Flat') {
    $dir = Join-Path $tmp $Name
    foreach ($bench in $Benches) {
      $new = Join-Path $dir "$Group/$bench/new"
      New-Item -ItemType Directory -Path $new -Force | Out-Null
      $iters = @(1..$N | ForEach-Object { 20 })
      $times = @(1..$N | ForEach-Object { 20 * $MeanNs * (1 + ($_ % 10) / 100) })
      @{ sampling_mode = $Mode; iters = $iters; times = $times } | ConvertTo-Json -Compress |
        Set-Content -LiteralPath (Join-Path $new 'sample.json') -Encoding utf8
    }
    return $dir
  }
  function Set-Age([string]$Dir, [string]$Bench, [timespan]$Age) {
    $file = Join-Path $Dir "$Group/$Bench/new/sample.json"
    (Get-Item -LiteralPath $file).LastWriteTimeUtc = [DateTime]::UtcNow - $Age
  }
  function Assert-Run([string]$Name, [int]$ExpectExit, [string[]]$Arguments, [string[]]$ExpectText = @()) {
    $script:cases++
    $output = (& $pwsh -NoProfile -File $PSCommandPath @Arguments *>&1 | Out-String)
    $code = $LASTEXITCODE
    $missing = @($ExpectText | Where-Object { $output -notmatch [regex]::Escape($_) })
    if ($code -ne $ExpectExit -or $missing.Count -gt 0) {
      Write-Host "SELFTEST FAIL: $Name (exit $code, expected $ExpectExit; missing text: $($missing -join ', '))"
      Write-Host ($output.TrimEnd() -replace '(?m)^', '    ')
      $script:failures++
    } elseif ($VerbosePreference -eq 'Continue') {
      Write-Host "selftest ok: $Name (exit $code)"
    }
  }

  try {
    $all = New-Results 'all' $ExpectedBenches
    Assert-Run 'all seven cases within budget pass' 0 @('-CriterionDir', $all) @('7 benchmark(s) within the limit')
    $over = New-Results 'over' $ExpectedBenches
    New-Results 'over' @('terminator_miss') -MeanNs 2000000 | Out-Null
    Assert-Run 'one case over the limit fails' 1 @('-CriterionDir', $over) @('over the limit: terminator_miss')
    Assert-Run 'no results is unusable' 2 @('-CriterionDir', (Join-Path $tmp 'nothing'))
    $partial = New-Results 'partial' @('char_miss')
    Assert-Run 'only some cases (a filtered run) is unusable and names the missing ones' 2 @('-CriterionDir', $partial) `
      @('6 of 7 case(s) missing', 'char_immediate_hit', 'terminator_miss', 'terminator_hit', 'terminator_regex_hit', 'terminator_miss_full_buffer', 'typing_stream')
    $stale = New-Results 'stale' $ExpectedBenches
    Set-Age $stale 'terminator_miss_full_buffer' ([timespan]::FromDays(2))
    Assert-Run 'one case left from an earlier run is unusable' 2 @('-CriterionDir', $stale) @('not from one run', 'terminator_miss_full_buffer')
    $early = New-Results 'early' $ExpectedBenches
    Set-Age $early 'typing_stream' ([timespan]::FromMinutes(5))
    $since = ([DateTime]::UtcNow - [timespan]::FromMinutes(1)).ToString('o')
    Assert-Run 'one case older than -NotBefore is unusable' 2 @('-CriterionDir', $early, '-NotBefore', $since) @('written before', 'typing_stream')
    Assert-Run 'the same results pass without -NotBefore (5 minutes apart is one run)' 0 @('-CriterionDir', $early)
    $extra = New-Results 'extra' ($ExpectedBenches + 'char_miss_old')
    Assert-Run 'an unexpected case is unusable' 2 @('-CriterionDir', $extra) @('not in the case list', 'char_miss_old')
    $linear = New-Results 'linear' $ExpectedBenches -Mode 'Linear'
    Assert-Run 'wrong sampling mode is unusable' 2 @('-CriterionDir', $linear) @("expected 'Flat'")
    $few = New-Results 'few' $ExpectedBenches -N 50
    Assert-Run 'too few samples is unusable' 2 @('-CriterionDir', $few) @('fewer than 100')
    $broken = New-Results 'broken' $ExpectedBenches
    Set-Content -LiteralPath (Join-Path $broken "$Group/char_miss/new/sample.json") -Value '{"sampling_mode": "Flat", "iters": [' -Encoding utf8
    Assert-Run 'malformed sample.json is unusable' 2 @('-CriterionDir', $broken) @('cannot parse')
  } finally {
    $env:GITHUB_STEP_SUMMARY = $savedSummary
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
  if ($failures -gt 0) { Write-Host "$failures of $cases self-test case(s) failed"; exit 1 }
  Write-Host "self-test passed ($cases cases)"
  exit 0
}

function Stop-Unusable([string]$Message) {
  Write-Host "check-bench-budget: $Message" -ForegroundColor Red
  exit 2
}

if ($BudgetMs -le 0 -or $TolerancePercent -le 0 -or $MinSamples -lt 1 -or $MaxRunMinutes -le 0) {
  Stop-Unusable 'BudgetMs, TolerancePercent and MaxRunMinutes must be positive, MinSamples at least 1.'
}

$groupDir = Join-Path $CriterionDir $Group
if (-not (Test-Path -LiteralPath $groupDir -PathType Container)) {
  Stop-Unusable "no results in $groupDir. Run: cargo bench -p quicker-domain --bench matching"
}

$limitNs = $BudgetMs * 1e6 * $TolerancePercent / 100
$found = @{}
foreach ($dir in Get-ChildItem -LiteralPath $groupDir -Directory) {
  $path = Join-Path $dir.FullName 'new/sample.json'
  if (Test-Path -LiteralPath $path -PathType Leaf) { $found[$dir.Name] = Get-Item -LiteralPath $path }
}
$missing = @($ExpectedBenches | Where-Object { -not $found.ContainsKey($_) })
if ($missing.Count -gt 0) {
  Stop-Unusable ("{0} of {1} case(s) missing in {2}: {3}. Run all of them: cargo bench -p quicker-domain --bench matching (no filter)." -f `
      $missing.Count, $ExpectedBenches.Count, $groupDir, ($missing -join ', '))
}
$unexpected = @($found.Keys | Where-Object { $ExpectedBenches -notcontains $_ } | Sort-Object)
if ($unexpected.Count -gt 0) {
  Stop-Unusable ("results for case(s) not in the case list: {0}. A renamed or new benchmark goes into `$ExpectedBenches of this script; an old directory is deleted ({1})." -f `
      ($unexpected -join ', '), $groupDir)
}

$written = @($ExpectedBenches | ForEach-Object { [pscustomobject]@{ Bench = $_; Utc = $found[$_].LastWriteTimeUtc } } | Sort-Object Utc)
$oldest = $written[0]
$newest = $written[-1]
if (($newest.Utc - $oldest.Utc).TotalMinutes -gt $MaxRunMinutes) {
  $stale = @($written | Where-Object { ($newest.Utc - $_.Utc).TotalMinutes -gt $MaxRunMinutes } | ForEach-Object Bench)
  Stop-Unusable ("results are not from one run: {0} written more than {1} min before the newest ({2}, {3:u}). Delete {4} and run the benchmark again." -f `
      ($stale -join ', '), $MaxRunMinutes, $newest.Bench, $newest.Utc, $groupDir)
}
if ($null -ne $NotBefore) {
  # PowerShell hands a set [Nullable[datetime]] over as the [datetime] itself (no .Value).
  $notBeforeUtc = ([datetime]$NotBefore).ToUniversalTime()
  $early = @($written | Where-Object { $_.Utc -lt $notBeforeUtc } | ForEach-Object Bench)
  if ($early.Count -gt 0) {
    Stop-Unusable ("stale results: {0} written before {1:u} (-NotBefore), so not by this run." -f ($early -join ', '), $notBeforeUtc)
  }
}

function Format-Us([double]$Ns) { '{0:N1}' -f ($Ns / 1000) }

$rows = foreach ($bench in $ExpectedBenches) {
  $path = $found[$bench].FullName
  try {
    $data = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
  } catch {
    Stop-Unusable "${bench}: cannot parse ${path}: $($_.Exception.Message)"
  }
  $props = $data.PSObject.Properties.Name
  foreach ($required in 'sampling_mode', 'iters', 'times') {
    if ($props -notcontains $required) { Stop-Unusable "${bench}: $path has no '$required'." }
  }
  if ($data.sampling_mode -ne 'Flat') {
    Stop-Unusable "${bench}: sampling_mode is '$($data.sampling_mode)', expected 'Flat' (see the statistic in this script's help)."
  }
  $iters = @($data.iters | ForEach-Object { [double]$_ })
  $times = @($data.times | ForEach-Object { [double]$_ })
  $n = $times.Count
  if ($iters.Count -ne $n -or $n -eq 0) {
    Stop-Unusable "${bench}: $($iters.Count) iteration counts for $n times in $path."
  }
  if ($n -lt $MinSamples) {
    Stop-Unusable "${bench}: $n samples, fewer than $MinSamples; no p99 is given for so few (performance-baseline.md rule 5)."
  }
  $means = for ($i = 0; $i -lt $n; $i++) {
    if ($iters[$i] -le 0) { Stop-Unusable "${bench}: sample $i has $($iters[$i]) iterations." }
    $times[$i] / $iters[$i]
  }
  $sorted = @($means | Sort-Object)
  $p99 = $sorted[[int][math]::Ceiling(0.99 * $n) - 1]
  $median = if ($n % 2 -eq 1) { $sorted[($n - 1) / 2] } else { ($sorted[$n / 2 - 1] + $sorted[$n / 2]) / 2 }
  [pscustomobject]@{
    Bench           = $bench
    Samples         = $n
    ItersPerSample  = ($iters | Measure-Object -Minimum).Minimum
    MeanUs          = Format-Us (($means | Measure-Object -Average).Average)
    MedianUs        = Format-Us $median
    P99Us           = Format-Us $p99
    MaxUs           = Format-Us $sorted[-1]
    Status          = if ($p99 -le $limitNs) { 'ok' } else { 'OVER' }
  }
}

$header = 'Matching budget ({0}): p99 of per-sample means <= {1:N0} us ({2}% of {3} ms); {4}' -f `
  $Group, ($limitNs / 1000), $TolerancePercent, $BudgetMs, $CriterionDir
$runLine = 'All {0} cases from one run, written {1:u} to {2:u}.' -f $ExpectedBenches.Count, $oldest.Utc, $newest.Utc
Write-Host $header
Write-Host $runLine
$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host

if ($env:GITHUB_STEP_SUMMARY) {
  $lines = @(
    "### $header", '', $runLine, '',
    '| bench | samples | iters/sample | mean (us) | median (us) | p99 (us) | max (us) | status |',
    '| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |'
  )
  $lines += $rows | ForEach-Object {
    "| $($_.Bench) | $($_.Samples) | $($_.ItersPerSample) | $($_.MeanUs) | $($_.MedianUs) | $($_.P99Us) | $($_.MaxUs) | $($_.Status) |"
  }
  Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Value ($lines + '') -Encoding utf8
}

$over = @($rows | Where-Object Status -eq 'OVER')
if ($over.Count -gt 0) {
  Write-Host ("check-bench-budget: over the limit: {0}" -f (($over | ForEach-Object Bench) -join ', ')) -ForegroundColor Red
  exit 1
}
Write-Host "check-bench-budget: $($rows.Count) benchmark(s) within the limit."
exit 0
