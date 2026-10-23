#requires -Version 7.0
<#
.SYNOPSIS
  Release gate: the version in the git tag, package.json, package-lock.json,
  src-tauri/tauri.conf.json and every Cargo workspace package must be the same.

.DESCRIPTION
  scripts/package-artifacts.ps1 names the deliverables after package.json's version and looks
  for the installer `Quicktext_<version>_x64-setup.exe`, which tauri names after
  tauri.conf.json's version. If the tag, the manifests and the installer disagree, the
  package would carry the wrong version or fail late, after a long build. This script fails
  early.

  The version must be plain major.minor.patch (package-artifacts.ps1 refuses anything else).
  With -Tag the tag must be `v<version>`. When $env:GITHUB_OUTPUT is set the version is
  written to it as `version=<version>` for later workflow steps.

.PARAMETER Tag
  The pushed tag name, e.g. v0.4.0. Leave empty for a manual (workflow_dispatch) dry run,
  which only checks that the manifests agree with each other.

.PARAMETER ProjectRoot
  Repository root. Defaults to the parent of this script's directory.

.EXAMPLE
  ./scripts/check-release-version.ps1 -Tag v0.4.0

.NOTES
  Needs cargo on PATH (`cargo metadata --no-deps --locked`: no network, no lockfile change).
  Exit code 0 = pass, 1 = mismatch.
#>
[CmdletBinding()]
param(
    [string] $Tag = '',
    [string] $ProjectRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $ProjectRoot) { $ProjectRoot = Split-Path -Parent $PSScriptRoot }
$root = [IO.Path]::GetFullPath($ProjectRoot)

function Read-Json([string] $RelativePath) {
    # -AsHashtable: package-lock.json has a package named '' (the root), which a
    # PSCustomObject cannot hold.
    Get-Content -LiteralPath (Join-Path $root $RelativePath) -Raw -Encoding utf8 | ConvertFrom-Json -AsHashtable
}

$found = [ordered]@{}
$found['package.json'] = (Read-Json 'package.json')['version']
$lock = Read-Json 'package-lock.json'
$found['package-lock.json'] = $lock['version']
$found['package-lock.json (root package)'] = $lock['packages']['']['version']
$found['src-tauri/tauri.conf.json'] = (Read-Json 'src-tauri/tauri.conf.json')['version']

Push-Location -LiteralPath $root
try {
    $output = & cargo metadata --format-version 1 --no-deps --locked 2>$null
    if ($LASTEXITCODE -ne 0) { throw "'cargo metadata' failed with exit code $LASTEXITCODE" }
} finally { Pop-Location }
foreach ($package in ($output -join "`n" | ConvertFrom-Json).packages) {
    $found["Cargo package $($package.name)"] = $package.version
}

$expected = $null
if ($Tag) {
    if ($Tag -notmatch '^v(?<version>(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*))$') {
        Write-Host "FAIL: tag '$Tag' is not of the form v<major>.<minor>.<patch>"
        exit 1
    }
    $expected = $Matches['version']
} else {
    $expected = $found['package.json']
}

$failed = $false
if ($expected -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$') {
    Write-Host "FAIL: version '$expected' is not plain major.minor.patch (scripts/package-artifacts.ps1 requires it)"
    exit 1
}
foreach ($entry in $found.GetEnumerator()) {
    if ($entry.Value -ceq $expected) { Write-Host ('ok:   {0,-44} {1}' -f $entry.Key, $entry.Value) }
    else { Write-Host ('FAIL: {0,-44} {1} (expected {2})' -f $entry.Key, $entry.Value, $expected); $failed = $true }
}
if ($failed) {
    Write-Host ''
    $origin = if ($Tag) { "tag $Tag" } else { 'package.json' }
    Write-Host "Fix: make every version above equal to the version of $origin."
    exit 1
}

if ($env:GITHUB_OUTPUT) { Add-Content -LiteralPath $env:GITHUB_OUTPUT -Value "version=$expected" -Encoding utf8 }
Write-Host "release version: $expected"
exit 0
