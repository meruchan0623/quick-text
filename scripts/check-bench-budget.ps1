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

  The statistic, an approximation of p99:

    criterion does not record single iterations. Each of its samples is the total time of
    iters[i] back-to-back iterations (one iteration = one keystroke through the matcher), in
    <CriterionDir>/<Group>/<bench>/new/sample.json as {"sampling_mode", "iters", "times"}
    (times in nanoseconds). The benchmark uses SamplingMode::Flat, so every sample has the
    same number of iterations; this script refuses other modes, because samples of different
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

  Exit codes: 0 every benchmark within the limit; 1 at least one over it; 2 no or unusable
  results (missing directory, no benchmark, malformed sample.json, wrong sampling mode, too few
  samples). When GITHUB_STEP_SUMMARY is set, the table is appended to it as well.

  Results are taken as they are on disk: run the benchmark first. A benchmark that was renamed
  or removed keeps its old directory until target/criterion is deleted; CI starts from an
  empty one.

.PARAMETER CriterionDir
  criterion's output directory. Default: target/criterion under the repository root.

.PARAMETER Group
  The benchmark group to check. Default: match_1000_rules.

.PARAMETER BudgetMs
  The absolute budget in milliseconds. Default: 1.0 (matching p99 < 1 ms).

.PARAMETER TolerancePercent
  Fail above this percentage of BudgetMs. Default: 120.

.PARAMETER MinSamples
  Fewest samples a benchmark may have. Default: 100.

.EXAMPLE
  cargo bench -p quicker-domain --bench matching
  pwsh -NoProfile -File scripts/check-bench-budget.ps1

.EXAMPLE
  # Prove that the check can fail: a budget of 1 microsecond.
  pwsh -NoProfile -File scripts/check-bench-budget.ps1 -BudgetMs 0.001
#>
[CmdletBinding()]
param(
  [string]$CriterionDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target/criterion'),
  [string]$Group = 'match_1000_rules',
  [double]$BudgetMs = 1.0,
  [double]$TolerancePercent = 120,
  [int]$MinSamples = 100
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Stop-Unusable([string]$Message) {
  Write-Host "check-bench-budget: $Message" -ForegroundColor Red
  exit 2
}

if ($BudgetMs -le 0 -or $TolerancePercent -le 0 -or $MinSamples -lt 1) {
  Stop-Unusable 'BudgetMs and TolerancePercent must be positive, MinSamples at least 1.'
}

$groupDir = Join-Path $CriterionDir $Group
if (-not (Test-Path -LiteralPath $groupDir -PathType Container)) {
  Stop-Unusable "no results in $groupDir. Run: cargo bench -p quicker-domain --bench matching"
}

$limitNs = $BudgetMs * 1e6 * $TolerancePercent / 100
$samples = @(Get-ChildItem -LiteralPath $groupDir -Directory |
  ForEach-Object { Join-Path $_.FullName 'new/sample.json' } |
  Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
  Sort-Object)
if ($samples.Count -eq 0) {
  Stop-Unusable "no <bench>/new/sample.json under $groupDir. Run the benchmark first."
}

function Format-Us([double]$Ns) { '{0:N1}' -f ($Ns / 1000) }

$rows = foreach ($path in $samples) {
  $bench = Split-Path -Leaf (Split-Path -Parent (Split-Path -Parent $path))
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
Write-Host $header
$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host

if ($env:GITHUB_STEP_SUMMARY) {
  $lines = @(
    "### $header", '',
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
