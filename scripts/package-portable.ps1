#requires -Version 7.0
# -ThirdPartyLicensesFile: THIRD-PARTY-LICENSES.txt from scripts/generate-third-party-licenses.ps1; defaults to
#   src-tauri\resources\THIRD-PARTY-LICENSES.txt, where the release build has it. The committed placeholder is refused.
# -DependencySourcesZip: Quicker-Text-<version>-Source-deps.zip (scripts/package-dependency-sources.ps1). Optional here
#   because the portable ZIP only carries source\ for the project itself; when given it is copied next to the ZIP.
#   Whoever distributes the portable ZIP must offer Source.zip and Source-deps.zip with it (GPL-3.0-or-later).
param(
    [Parameter(Mandatory=$true)][string]$OutputDirectory,
    [string]$ThirdPartyLicensesFile,
    [string]$DependencySourcesZip
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'source-allowlist.ps1')
. (Join-Path $PSScriptRoot 'third-party-set.ps1')
$version = (Get-Content -LiteralPath (Join-Path $root 'package.json') -Raw -Encoding UTF8 | ConvertFrom-Json).version
$output = [IO.Path]::GetFullPath($OutputDirectory)
$name = "Quicker-Text-$version-Windows-x64-Portable"
$folder = Join-Path $output $name
$zip = Join-Path $output "$name.zip"
if ((Test-Path -LiteralPath $folder) -or (Test-Path -LiteralPath $zip)) { throw 'Preserve existing packages: choose a new output directory.' }
$exe = Join-Path $root 'target\release\quicker-text.exe'
$required = @($exe, (Join-Path $root 'THIRD_PARTY_NOTICES.md'), (Join-Path $root 'docs\windows-0.3.0-refactor.md'), (Join-Path $root 'docs\windows-0.3.1-notes.md'))
foreach ($file in $required) { if (!(Test-Path -LiteralPath $file -PathType Leaf)) { throw "Missing required artifact: $file" } }
# Third-party copyright and license notices travel with the package (S-28); refuse a missing, placeholder or stale file.
if (-not $ThirdPartyLicensesFile) { $ThirdPartyLicensesFile = Join-Path $root 'src-tauri\resources\THIRD-PARTY-LICENSES.txt' }
$licensesFile = [IO.Path]::GetFullPath($ThirdPartyLicensesFile)
Assert-ThirdPartyLicensesFile -Path $licensesFile -ProjectRoot $root
$depsName = "Quicker-Text-$version-Source-deps.zip"
$depsZip = $null
if ($DependencySourcesZip) {
    $depsZip = [IO.Path]::GetFullPath($DependencySourcesZip)
    if (!(Test-Path -LiteralPath $depsZip -PathType Leaf)) { throw "Missing dependency source archive: $depsZip" }
    if ((Split-Path -Leaf $depsZip) -ne $depsName) { throw "Dependency source archive must be named $depsName" }
    if (Test-Path -LiteralPath (Join-Path $output $depsName)) { throw "Output already exists; preserve it: $depsName" }
} else {
    Write-Warning "No -DependencySourcesZip: publish $depsName (scripts/package-dependency-sources.ps1) together with this package and Source.zip"
}
New-Item -ItemType Directory -Path $folder -Force | Out-Null
Copy-Item -LiteralPath $exe -Destination (Join-Path $folder 'quicker-text.exe')
Copy-Item -LiteralPath (Join-Path $root 'README.md'), (Join-Path $root 'LICENSE'), (Join-Path $root 'THIRD_PARTY_NOTICES.md'), (Join-Path $root 'docs\windows-0.3.0-refactor.md'), (Join-Path $root 'docs\windows-0.3.1-notes.md') -Destination $folder
Copy-Item -LiteralPath (Join-Path $root 'vendor\espanso-win32\LICENSE') -Destination (Join-Path $folder 'LICENSE-ESPANSO-GPL-3.0.txt')
Copy-Item -LiteralPath $licensesFile -Destination (Join-Path $folder 'THIRD-PARTY-LICENSES.txt')
# Carry the actual corresponding source, including patched upstream code and build scripts.
# The explicit allowlist excludes user configuration, imports, logs and workspace snapshots.
$source = Join-Path $folder 'source'
New-Item -ItemType Directory -Path $source | Out-Null
# The allowlist is shared with package-artifacts.ps1 and check-source-allowlist.ps1:
# scripts/source-allowlist.ps1 (update it when a crate or a root-level build file is added, B-51).
$files = @(Get-SourceAllowlist -ProjectRoot $root)
foreach ($relative in ($files | Sort-Object -Unique)) {
    $destination = Join-Path $source $relative
    New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $root $relative) -Destination $destination
}
$hashes = foreach ($file in Get-ChildItem -LiteralPath $folder -File -Recurse | Sort-Object FullName) {
    '{0}  {1}' -f (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant(), [IO.Path]::GetRelativePath($folder,$file.FullName).Replace('\','/')
}
Set-Content -LiteralPath (Join-Path $folder 'SHA256SUMS.txt') -Value $hashes -Encoding UTF8
Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::CreateFromDirectory($folder, $zip, [IO.Compression.CompressionLevel]::Optimal, $false)
$archive = [IO.Compression.ZipFile]::OpenRead($zip)
try {
    foreach ($entry in $archive.Entries) {
        if (!$entry.Name) { continue }
        $stream = $entry.Open()
        try { $actual = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($stream)) } finally { $stream.Dispose() }
        $expected = (Get-FileHash -LiteralPath (Join-Path $folder $entry.FullName) -Algorithm SHA256).Hash
        if ($actual -ne $expected) { throw "ZIP entry hash mismatch: $($entry.FullName)" }
    }
    $entryCount = $archive.Entries.Count
} finally { $archive.Dispose() }
if ((Get-FileHash -LiteralPath $exe).Hash -ne (Get-FileHash -LiteralPath (Join-Path $folder 'quicker-text.exe')).Hash) { throw 'Executable hash mismatch' }
if ($depsZip) { Copy-Item -LiteralPath $depsZip -Destination (Join-Path $output $depsName) }
Get-FileHash -LiteralPath $exe,$zip -Algorithm SHA256 | Format-List Path,Hash
Write-Output "Verified $entryCount ZIP entries. Portable package: $zip"
