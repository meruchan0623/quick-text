#requires -Version 7.0
<#
.SYNOPSIS
  B-52 gate: a release build must not enable the `fault-injection` or `mock` features.

.DESCRIPTION
  Asks cargo which features are enabled when the release package is built, without building
  anything:

      cargo tree -p <Package> -e normal,build,features -i <crate> --locked

  `-e features` lists a node `<crate> feature "<name>"` for every enabled feature; the
  inverted tree (-i) of a crate also contains the features of the crates that depend on it,
  such as `quicktext feature "fault-injection"`, which enables the one in quicker-platform.
  Dev-dependencies are not part of the check (`cargo build --release` does not compile
  them); only the host target is resolved, like the real build.

  The check runs for every workspace crate that defines a forbidden feature and is part of
  the package's dependency graph. It fails when
    * a forbidden feature is enabled in any of them, or
    * a forbidden feature is defined by no crate in the graph. Then the check could never
      fail, so the feature was probably renamed: update -Forbidden (and the call in
      .github/workflows/release.yml).

  This covers the Cargo.toml side (a `default = [...]` that pulls in a test feature). The
  other half of B-52 is scripts/check-binary-strings.ps1 on the archived PDB and the exe.

.PARAMETER Package
  The package the installer is built from. Default: quicktext (src-tauri).

.PARAMETER Forbidden
  Feature names that must not be enabled. Default: fault-injection, mock.

.PARAMETER CargoArgs
  Extra arguments for `cargo tree`, e.g. --features fault-injection. The release workflow
  passes none. They exist to prove that the check can fail (see .EXAMPLE).

.EXAMPLE
  ./scripts/check-release-features.ps1

.EXAMPLE
  # Self-test from a pwsh prompt (an array does not survive `pwsh -File`): must exit 1.
  ./scripts/check-release-features.ps1 -CargoArgs '--features','fault-injection'

.NOTES
  Exit code 0 = pass, 1 = a forbidden feature is enabled or the check is vacuous.
#>
[CmdletBinding()]
param(
    [string] $Package = 'quicktext',
    [string[]] $Forbidden = @('fault-injection', 'mock'),
    [string[]] $CargoArgs = @()
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot

function Invoke-Cargo {
    # Returns stdout lines of a cargo command run in the repository root; throws on failure.
    param([string[]] $Arguments)
    Push-Location -LiteralPath $repoRoot
    try {
        $output = & cargo @Arguments 2>$null
        if ($LASTEXITCODE -ne 0) { throw "'cargo $($Arguments -join ' ')' failed with exit code $LASTEXITCODE" }
        return @($output)
    } finally { Pop-Location }
}

# Crates that exist in the package's dependency graph (first token of each line).
$graph = Invoke-Cargo (@('tree', '-p', $Package, '--locked', '-e', 'normal,build', '--prefix', 'none', '--format', '{p}') + $CargoArgs)
$inGraph = [System.Collections.Generic.HashSet[string]]::new([string[]]@($graph | ForEach-Object { ($_ -split ' ')[0] }), [StringComparer]::Ordinal)
if (-not $inGraph.Contains($Package)) { throw "package '$Package' is not part of the workspace graph" }

# Workspace crates that define at least one forbidden feature.
$metadata = (Invoke-Cargo @('metadata', '--format-version', '1', '--no-deps', '--locked')) -join "`n" | ConvertFrom-Json
$definesFeature = @{}
$candidates = [System.Collections.Generic.List[string]]::new()
foreach ($member in $metadata.packages) {
    $defined = @($member.features.PSObject.Properties | ForEach-Object { $_.Name } | Where-Object { $_ -in $Forbidden })
    if ($defined.Count -eq 0 -or -not $inGraph.Contains($member.name)) { continue }
    $candidates.Add($member.name)
    foreach ($feature in $defined) { $definesFeature[$feature] = $true }
}
$undefined = @($Forbidden | Where-Object { -not $definesFeature.ContainsKey($_) })
if ($undefined.Count -gt 0) {
    Write-Host "FAIL: no crate in the '$Package' graph defines the feature(s): $($undefined -join ', ')"
    Write-Host '      The check would pass vacuously. Was a feature renamed or removed? Update -Forbidden'
    Write-Host '      and the call in .github/workflows/release.yml.'
    exit 1
}

$pattern = '^(?<crate>\S+) feature "(?<feature>' + (($Forbidden | ForEach-Object { [regex]::Escape($_) }) -join '|') + ')"'
$hits = [System.Collections.Generic.SortedSet[string]]::new([StringComparer]::Ordinal)
foreach ($crate in $candidates) {
    $tree = Invoke-Cargo (@('tree', '-p', $Package, '--locked', '-e', 'normal,build,features', '-i', $crate, '--prefix', 'none') + $CargoArgs)
    foreach ($line in $tree) {
        if ($line -match $pattern) { [void]$hits.Add("$($Matches['crate']) feature `"$($Matches['feature'])`"") }
    }
}

if ($hits.Count -gt 0) {
    Write-Host "FAIL: the release configuration of '$Package' enables forbidden feature(s):"
    foreach ($hit in $hits) { Write-Host "  $hit" }
    Write-Host "  find out why: cargo tree -p $Package -e normal,build,features -i <crate>"
    exit 1
}

Write-Host "ok: '$Package' enables none of [$($Forbidden -join ', ')] (checked $($candidates -join ', '))"
exit 0
