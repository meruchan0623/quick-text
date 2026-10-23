#requires -Version 7.0
# -ThirdPartyLicensesFile: THIRD-PARTY-LICENSES.txt from scripts/generate-third-party-licenses.ps1 (the file the
#   installer bundles; the committed placeholder is refused).
# -DependencySourcesZip: Quicker-Text-<version>-Source-deps.zip from scripts/package-dependency-sources.ps1.
#   Source.zip and Source-deps.zip together are the complete corresponding source (GPL-3.0-or-later), so a
#   package without the dependency archive is refused. The archive is validated against this tree's
#   Cargo.lock and package-lock.json (Assert-DependencySourcesZip): a stale, damaged or incomplete one fails.
# Everything is checked before the first file is written. When a later step fails, every file this run created
# is removed again, so no half-finished package is left in the output directory.
# B-52 (scripts/release-binary-checks.ps1): before anything is copied, the release configuration must enable no
# fault-injection / mock feature, target\release\quicker_text.pdb must be the PDB of the exe, and neither may
# contain debug_inject_fault. Any failure stops the run with a non-zero exit code.
param(
    [Parameter(Mandatory=$true)][string]$OutputDirectory,
    [Parameter(Mandatory=$true)][string]$ThirdPartyLicensesFile,
    [Parameter(Mandatory=$true)][string]$DependencySourcesZip
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'source-allowlist.ps1')
. (Join-Path $PSScriptRoot 'third-party-set.ps1')
. (Join-Path $PSScriptRoot 'release-binary-checks.ps1')
$version = (Get-Content -LiteralPath (Join-Path $projectRoot 'package.json') -Raw | ConvertFrom-Json).version
if ($version -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$') { throw 'Package version must use major.minor.patch format' }
$output = [IO.Path]::GetFullPath($OutputDirectory)
$exe = Join-Path $projectRoot 'target\release\quicker-text.exe'
$installer = Join-Path $projectRoot "target\release\bundle\nsis\Quicker Text_${version}_x64-setup.exe"
$pdb = Join-Path $projectRoot 'target\release\quicker_text.pdb'
foreach ($file in @($exe, $installer, $pdb)) { if (!(Test-Path -LiteralPath $file -PathType Leaf)) { throw "Missing build: $file" } }
# B-52 on the exact exe that is copied below, and its PDB. Throws on any failure.
Assert-ReleaseBinaries -Exe $exe -Pdb $pdb
# The read-me-first notes belong to one version: a later release must not ship another version's notes
# under its own file name (0.4+ writes docs\windows-<version>-notes.md before it is packaged).
$notes = Join-Path $projectRoot "docs\windows-$version-notes.md"
if (!(Test-Path -LiteralPath $notes -PathType Leaf)) { throw "Missing release notes: docs\windows-$version-notes.md (write it before packaging)" }
# Third-party copyright and license notices travel with every artifact (S-28): the generated file must
# describe this tree's lock files, and the dependency source archive must be the one built for this version
# from those lock files, and complete.
$licensesFile = [IO.Path]::GetFullPath($ThirdPartyLicensesFile)
Assert-ThirdPartyLicensesFile -Path $licensesFile -ProjectRoot $projectRoot
$depsZip = [IO.Path]::GetFullPath($DependencySourcesZip)
$depsName = "Quicker-Text-$version-Source-deps.zip"
$deps = Assert-DependencySourcesZip -Path $depsZip -ProjectRoot $projectRoot -Version $version
$deliveries = [ordered]@{
    "Quicker-Text-$version-THIRD-PARTY-NOTICES.md" = Join-Path $projectRoot 'THIRD_PARTY_NOTICES.md'
    "Quicker-Text-$version-THIRD-PARTY-LICENSES.txt" = $licensesFile
    "Quicker-Text-$version-GPL-3.0.txt" = Join-Path $projectRoot 'LICENSE'
    "Quicker-Text-$version-Windows-x64.exe" = $exe
    "Quicker-Text-$version-Windows-x64-Setup.exe" = $installer
    "Quicker-Text-$version-使用与构建说明.md" = Join-Path $projectRoot 'README.md'
    "Quicker-Text-$version-平台支持与手工测试.md" = Join-Path $projectRoot 'docs\platform-support.md'
    "Quicker-Text-$version-浏览器与IME手测.md" = Join-Path $projectRoot 'docs\browser-ime-manual-tests.md'
    "Quicker-Text-$version-验收矩阵.md" = Join-Path $projectRoot 'docs\acceptance.md'
    "Quicker-Text-$version-构建验证记录.md" = Join-Path $projectRoot 'docs\build-verification.md'
    "Quicker-Text-$version-先读与测试.md" = $notes
    "Quicker-Text-$version-浏览器测试页面.html" = Join-Path $projectRoot 'scripts\fixtures\browser-input.html'
}
$zipName = "Quicker-Text-$version-Source.zip"
$hashName = "Quicker-Text-$version-SHA256SUMS.txt"
foreach ($name in (@($deliveries.Keys) + @($zipName, $depsName, $hashName))) {
    if (Test-Path -LiteralPath (Join-Path $output $name)) { throw "Output already exists; preserve it: $name" }
}
# Explicit source allowlist, shared with package-portable.ps1 and check-source-allowlist.ps1:
# scripts/source-allowlist.ps1 (update it when a crate or a root-level build file is added, B-51).
# Runtime configuration, imports, logs, build products and private data are never traversed.
$relativeFiles = @(Get-SourceAllowlist -ProjectRoot $projectRoot)
foreach ($relative in ($relativeFiles | Sort-Object -Unique)) {
    if (!(Test-Path -LiteralPath (Join-Path $projectRoot $relative) -PathType Leaf)) { throw "Missing source: $relative" }
}
# From here on files are written. None of these names existed (checked above), so on failure they are all ours to remove.
$createdOutput = -not (Test-Path -LiteralPath $output)
$written = [Collections.Generic.List[string]]::new()
try {
    New-Item -ItemType Directory -Path $output -Force | Out-Null
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $written.Add((Join-Path $output $zipName))
    $archive = [IO.Compression.ZipFile]::Open((Join-Path $output $zipName), [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($relative in ($relativeFiles | Sort-Object -Unique)) {
            $path = Join-Path $projectRoot $relative
            [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $path, $relative.Replace('\','/'), [IO.Compression.CompressionLevel]::Optimal) | Out-Null
        }
    } finally { $archive.Dispose() }
    foreach ($entry in $deliveries.GetEnumerator()) {
        $written.Add((Join-Path $output $entry.Key))
        Copy-Item -LiteralPath $entry.Value -Destination (Join-Path $output $entry.Key)
    }
    $written.Add((Join-Path $output $depsName))
    Copy-Item -LiteralPath $depsZip -Destination (Join-Path $output $depsName)
    if ((Get-FileHash -LiteralPath (Join-Path $output $depsName) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $deps.Sha256) { throw "Copied $depsName differs from the validated archive" }
    $names = @($deliveries.Keys) + @($zipName, $depsName)
    $hashes = foreach ($name in $names) { $hash = Get-FileHash -LiteralPath (Join-Path $output $name) -Algorithm SHA256; "$($hash.Hash.ToLowerInvariant())  $name" }
    $written.Add((Join-Path $output $hashName))
    Set-Content -LiteralPath (Join-Path $output $hashName) -Value $hashes -Encoding utf8
} catch {
    $failure = $_
    foreach ($leftover in $written) { Remove-Item -LiteralPath $leftover -Force -ErrorAction SilentlyContinue }
    if ($createdOutput -and (Test-Path -LiteralPath $output) -and -not (Get-ChildItem -LiteralPath $output -Force)) { Remove-Item -LiteralPath $output -Force -ErrorAction SilentlyContinue }
    throw $failure
}
Write-Output "Packaged version $version; source files: $(($relativeFiles | Sort-Object -Unique).Count)"
Get-ChildItem -LiteralPath $output -File | Where-Object Name -Like "Quicker-Text-$version-*" | Select-Object Name,Length
Get-Content -LiteralPath (Join-Path $output $hashName)
