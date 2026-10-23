#requires -Version 7.0
<#
.SYNOPSIS
  B-51 / release gate: the corresponding-source allowlist must cover every workspace crate.

.DESCRIPTION
  scripts/source-allowlist.ps1 decides which files go into Source.zip and into the portable
  package's source/ folder. This script proves that nothing a crate needs was left out:

    1. Every workspace member reported by `cargo metadata` (all targets: lib, bin, example,
       test, bench) must have its Cargo.toml, build script and target entry files in the set.
    2. Every git-tracked file below a workspace member directory must be in the set or be
       explicitly excluded (source-allowlist.ps1: $SourceExcluded).
    3. With -Strict, every other tracked file must be in the set or excluded as well, so a
       new root-level build/config file cannot be forgotten. Without -Strict these are only
       reported (as ::warning:: annotations on GitHub Actions) and do not fail the check.
    4. The mandatory legal files (LICENSE, THIRD_PARTY_NOTICES.md, Cargo.lock,
       package-lock.json, vendor/espanso-win32/LICENSE) must be in the set.

  The set is either the allowlist computed from the working tree (default) or, with -Zip,
  the entry names of an archive that package-artifacts.ps1 actually produced. The release
  workflow uses -Zip -Strict, so the check covers what is really delivered.

.PARAMETER Zip
  Check the entries of this ZIP instead of the computed allowlist.

.PARAMETER Strict
  Also fail for unclassified tracked files outside the workspace crates.

.PARAMETER ProjectRoot
  Repository root. Defaults to the parent of this script's directory.

.EXAMPLE
  ./scripts/check-source-allowlist.ps1
  ./scripts/check-source-allowlist.ps1 -Strict -Zip out/Quicktext-0.4.0-Source.zip

.NOTES
  Needs git and cargo on PATH (`cargo metadata --no-deps --locked` needs no network and
  never changes Cargo.lock). Exit code 0 = pass, 1 = failure.
#>
[CmdletBinding()]
param(
    [string] $Zip,
    [switch] $Strict,
    [string] $ProjectRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $ProjectRoot) { $ProjectRoot = Split-Path -Parent $PSScriptRoot }
$root = [IO.Path]::GetFullPath($ProjectRoot)
. (Join-Path $PSScriptRoot 'source-allowlist.ps1')

$mandatory = @(
    'LICENSE',
    'THIRD_PARTY_NOTICES.md',
    'Cargo.lock',
    'package-lock.json',
    'vendor/espanso-win32/LICENSE'
)

function Invoke-Native {
    # Runs a native command in $root and returns its stdout lines; throws on a non-zero exit.
    param([string] $File, [string[]] $Arguments)
    Push-Location -LiteralPath $root
    try {
        $output = & $File @Arguments 2>$null
        if ($LASTEXITCODE -ne 0) { throw "'$File $($Arguments -join ' ')' failed with exit code $LASTEXITCODE" }
        return @($output)
    } finally { Pop-Location }
}

# The set of paths that will be (or were) shipped.
if ($Zip) {
    $zipPath = [IO.Path]::GetFullPath($Zip)
    if (!(Test-Path -LiteralPath $zipPath -PathType Leaf)) { throw "ZIP not found: $zipPath" }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($zipPath)
    try { $shipped = @($archive.Entries | Where-Object { $_.Name } | ForEach-Object { $_.FullName }) }
    finally { $archive.Dispose() }
    $subject = "ZIP $([IO.Path]::GetFileName($zipPath))"
} else {
    $shipped = @(Get-SourceAllowlist -ProjectRoot $root)
    $subject = 'computed allowlist (scripts/source-allowlist.ps1)'
}
$shippedSet = [System.Collections.Generic.HashSet[string]]::new([string[]]$shipped, [StringComparer]::Ordinal)

$tracked = @(Invoke-Native 'git' @('-c', 'core.quotepath=off', 'ls-files'))
if ($tracked.Count -eq 0) { throw 'git ls-files returned nothing: is this a git checkout?' }

$errors = [System.Collections.Generic.List[string]]::new()
$warnings = [System.Collections.Generic.List[string]]::new()

# --- 4. mandatory legal files -----------------------------------------------------------
foreach ($path in $mandatory) {
    if (-not $shippedSet.Contains($path)) { $errors.Add("mandatory file is not in the source set: $path") }
}

# --- 1. workspace members and their targets ---------------------------------------------
$metadata = (Invoke-Native 'cargo' @('metadata', '--format-version', '1', '--no-deps', '--locked')) -join "`n" | ConvertFrom-Json
$memberDirs = [System.Collections.Generic.List[string]]::new()
foreach ($package in $metadata.packages) {
    $manifest = [IO.Path]::GetRelativePath($root, $package.manifest_path).Replace('\', '/')
    $directory = [IO.Path]::GetDirectoryName($manifest.Replace('/', [IO.Path]::DirectorySeparatorChar))
    $directory = if ($directory) { $directory.Replace('\', '/') } else { '' }
    if ($directory) { $memberDirs.Add($directory) }

    $required = [System.Collections.Generic.List[string]]::new()
    $required.Add($manifest)
    foreach ($target in $package.targets) {
        $required.Add(([IO.Path]::GetRelativePath($root, $target.src_path)).Replace('\', '/'))
    }
    foreach ($path in $required | Sort-Object -Unique) {
        if ($path.StartsWith('../', [StringComparison]::Ordinal)) { continue }  # outside the repository
        if (-not $shippedSet.Contains($path)) {
            $errors.Add("crate '$($package.name)': '$path' is not in the source set (add it to scripts/source-allowlist.ps1)")
        }
    }
}

# --- 2 + 3. every tracked file is shipped or explicitly excluded ------------------------
foreach ($path in $tracked) {
    if ($shippedSet.Contains($path)) { continue }
    if ($null -ne (Test-SourceExcluded -RelativePath $path)) { continue }
    $inMember = $false
    foreach ($directory in $memberDirs) {
        if ($path.StartsWith("$directory/", [StringComparison]::Ordinal)) { $inMember = $true; break }
    }
    $message = "tracked file is neither in the source set nor excluded: $path"
    if ($inMember) { $errors.Add($message) }
    elseif ($Strict) { $errors.Add($message) }
    else { $warnings.Add($message) }
}

foreach ($warning in $warnings) {
    if ($env:GITHUB_ACTIONS -eq 'true') { Write-Host "::warning title=source allowlist::$warning" }
    else { Write-Warning $warning }
}
if ($errors.Count -gt 0) {
    foreach ($errorLine in $errors) {
        if ($env:GITHUB_ACTIONS -eq 'true') { Write-Host "::error title=source allowlist::$errorLine" }
        else { Write-Host "FAIL: $errorLine" }
    }
    Write-Host ''
    Write-Host 'Fix: update scripts/source-allowlist.ps1 (list the file or directory, or exclude it with a reason).'
    exit 1
}

Write-Host "ok: $subject covers $($memberDirs.Count) workspace crate(s): $($memberDirs -join ', ')"
Write-Host "    $($shippedSet.Count) source files; $($tracked.Count) tracked files checked$(if ($Strict) { ' (strict)' } else { " ($($warnings.Count) unclassified outside crates)" })"
exit 0
