#requires -Version 7.0
# Builds the portable (no-install) ZIP of the release exe. This is a distributable binary, so it is gated the
# same way as scripts/package-artifacts.ps1: no ZIP is produced unless the complete corresponding source and the
# third-party notices are available (GPL-3.0-or-later; the dependency code is compiled into quicktext.exe).
#
# -ThirdPartyLicensesFile: THIRD-PARTY-LICENSES.txt from scripts/generate-third-party-licenses.ps1; defaults to
#   src-tauri\resources\THIRD-PARTY-LICENSES.txt, where the release build has it. The committed placeholder is refused.
# -DependencySourcesZip (mandatory): Quicktext-<version>-Source-deps.zip from scripts/package-dependency-sources.ps1.
#   It is validated against this tree's Cargo.lock and package-lock.json (Assert-DependencySourcesZip) and then
#   placed next to the ZIP in the output directory as a companion file; the ZIP carries DEPENDENCY-SOURCES.txt, which
#   names it and states its SHA-256. (A copy inside the ZIP would add ~100 MB to a ~5 MB download; the companion
#   file is the same arrangement as the installer package, where Source.zip and Source-deps.zip sit beside the
#   installer.) Whoever distributes the portable ZIP must offer both files together.
#
# Everything is checked before the first file is written. When a later step fails, the folder, the ZIP and the
# companion copy created by this run are removed again (a file you passed in is never touched, and an archive that
# already sits in the output directory under its own name is used in place).
# B-52 (scripts/release-binary-checks.ps1): before anything is copied, the release configuration must enable no
# fault-injection / mock feature, target\release\quicktext.pdb must be the PDB of the exe, and neither may
# contain debug_inject_fault. FE-18 (same file, Codex review 1): dist\ must be the frontend the exe embeds (the
# digest its build recorded, src-tauri/build.rs), then scripts/check-bundle.mjs --scan-only dist finds no mock
# backend in it; a dist\ rebuilt after the exe, or an exe built from a mock frontend, fails. Needs node. Any
# failure stops the run with a non-zero exit code. To check a build without packaging it:
#   pwsh -NoProfile -File scripts/check-embedded-frontend.ps1
#
#   pwsh -NoProfile -File scripts/package-portable.ps1 -OutputDirectory <new dir> `
#        -ThirdPartyLicensesFile <THIRD-PARTY-LICENSES.txt> -DependencySourcesZip <...-Source-deps.zip>
param(
    [Parameter(Mandatory=$true)][string]$OutputDirectory,
    [string]$ThirdPartyLicensesFile,
    [Parameter(Mandatory=$true)][ValidateNotNullOrEmpty()][string]$DependencySourcesZip
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'source-allowlist.ps1')
. (Join-Path $PSScriptRoot 'third-party-set.ps1')
. (Join-Path $PSScriptRoot 'release-binary-checks.ps1')
$version = (Get-Content -LiteralPath (Join-Path $root 'package.json') -Raw -Encoding UTF8 | ConvertFrom-Json).version
$output = [IO.Path]::GetFullPath($OutputDirectory)
$name = "Quicktext-$version-Windows-x64-Portable"
$folder = Join-Path $output $name
$zip = Join-Path $output "$name.zip"
$depsName = "Quicktext-$version-Source-deps.zip"
$depsCopy = Join-Path $output $depsName
$depsZip = [IO.Path]::GetFullPath($DependencySourcesZip)
# The archive may already sit in the output directory under its proper name: then it is the companion file as it is.
$depsInPlace = $depsZip -ieq [IO.Path]::GetFullPath($depsCopy)
if ((Test-Path -LiteralPath $folder) -or (Test-Path -LiteralPath $zip)) { throw 'Preserve existing packages: choose a new output directory.' }
if ((Test-Path -LiteralPath $depsCopy) -and -not $depsInPlace) { throw "Output already exists; preserve it: $depsName (choose a new output directory)" }
$exe = Join-Path $root 'target\release\quicktext.exe'
$required = @($exe, (Join-Path $root 'THIRD_PARTY_NOTICES.md'), (Join-Path $root 'docs\windows-0.3.0-refactor.md'), (Join-Path $root 'docs\windows-0.3.1-notes.md'))
foreach ($file in $required) { if (!(Test-Path -LiteralPath $file -PathType Leaf)) { throw "Missing required artifact: $file" } }
# B-52 on the exact exe that is copied below and its PDB; FE-18 on the frontend it embeds. Throws on any failure.
Assert-ReleaseBinaries -Exe $exe -Pdb (Join-Path $root 'target\release\quicktext.pdb')
# Third-party copyright and license notices travel with the package (S-28); refuse a missing, placeholder or stale file.
if (-not $ThirdPartyLicensesFile) { $ThirdPartyLicensesFile = Join-Path $root 'src-tauri\resources\THIRD-PARTY-LICENSES.txt' }
$licensesFile = [IO.Path]::GetFullPath($ThirdPartyLicensesFile)
Assert-ThirdPartyLicensesFile -Path $licensesFile -ProjectRoot $root
# The dependency sources: right version, built from this tree's lock files, complete. Throws otherwise.
$deps = Assert-DependencySourcesZip -Path $depsZip -ProjectRoot $root -Version $version
$files = @(Get-SourceAllowlist -ProjectRoot $root)
$createdOutput = -not (Test-Path -LiteralPath $output)
$copyStarted = $false
try {
    New-Item -ItemType Directory -Path $folder -Force | Out-Null
    Copy-Item -LiteralPath $exe -Destination (Join-Path $folder 'quicktext.exe')
    Copy-Item -LiteralPath (Join-Path $root 'README.md'), (Join-Path $root 'LICENSE'), (Join-Path $root 'THIRD_PARTY_NOTICES.md'), (Join-Path $root 'docs\windows-0.3.0-refactor.md'), (Join-Path $root 'docs\windows-0.3.1-notes.md') -Destination $folder
    Copy-Item -LiteralPath (Join-Path $root 'vendor\espanso-win32\LICENSE') -Destination (Join-Path $folder 'LICENSE-ESPANSO-GPL-3.0.txt')
    Copy-Item -LiteralPath $licensesFile -Destination (Join-Path $folder 'THIRD-PARTY-LICENSES.txt')
    # Where the dependency sources are, with the hash to check them by (the archive itself is a companion file).
    $depsSize = (Get-Item -LiteralPath $depsZip).Length
    $depsNote = @"
Quicktext $version portable package - dependency sources / 第三方依赖源码

This ZIP contains the source of Quicktext itself in source\ (GPL-3.0-or-later). The source of the third-party
Rust crates and npm packages that are compiled or bundled into quicktext.exe is NOT inside this ZIP. It is the
companion file

    $depsName
    SHA-256: $($deps.Sha256)
    size:    $depsSize bytes ($($deps.Crates) crates, $($deps.NpmPackages) npm packages)

which is published together with this ZIP. Quicktext-$version-Windows-x64-Portable.zip and $depsName
together are the complete corresponding source of quicktext.exe. Whoever passes this ZIP on must offer both files
from the same place. Check the companion file with:  Get-FileHash <file> -Algorithm SHA256

本压缩包的 source\ 目录只含 Quicktext 自身的源码。编译进 quicktext.exe 的第三方 Rust crate 与 npm 包的源码不在本压缩包内，
而在与它一同发布的附属文件 $depsName（SHA-256 见上）中。两者合在一起才是 quicktext.exe 的完整对应源码；
转发本压缩包的人必须在同一位置同时提供这两个文件。
Rebuild instructions are in README-OFFLINE-BUILD.md inside the companion file; their licenses are in THIRD-PARTY-LICENSES.txt.
"@
    Set-Content -LiteralPath (Join-Path $folder 'DEPENDENCY-SOURCES.txt') -Value $depsNote -Encoding utf8
    # Carry the actual corresponding source, including patched upstream code and build scripts.
    # The explicit allowlist excludes user configuration, imports, logs and workspace snapshots.
    $source = Join-Path $folder 'source'
    New-Item -ItemType Directory -Path $source | Out-Null
    # The allowlist is shared with package-artifacts.ps1 and check-source-allowlist.ps1:
    # scripts/source-allowlist.ps1 (update it when a crate or a root-level build file is added, B-51).
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
        if (-not ($archive.Entries | Where-Object FullName -eq 'DEPENDENCY-SOURCES.txt')) { throw 'ZIP lacks DEPENDENCY-SOURCES.txt' }
    } finally { $archive.Dispose() }
    if ((Get-FileHash -LiteralPath $exe).Hash -ne (Get-FileHash -LiteralPath (Join-Path $folder 'quicktext.exe')).Hash) { throw 'Executable hash mismatch' }
    # The companion file goes into the output directory last; its hash must be the one the ZIP states.
    if (-not $depsInPlace) {
        $copyStarted = $true
        Copy-Item -LiteralPath $depsZip -Destination $depsCopy
    }
    if ((Get-FileHash -LiteralPath $depsCopy -Algorithm SHA256).Hash.ToLowerInvariant() -ne $deps.Sha256) { throw "Companion file $depsName does not match the SHA-256 stated in DEPENDENCY-SOURCES.txt" }
} catch {
    $failure = $_
    foreach ($leftover in @($zip, $folder)) { Remove-Item -LiteralPath $leftover -Recurse -Force -ErrorAction SilentlyContinue }
    if ($copyStarted) { Remove-Item -LiteralPath $depsCopy -Force -ErrorAction SilentlyContinue }
    if ($createdOutput -and (Test-Path -LiteralPath $output) -and -not (Get-ChildItem -LiteralPath $output -Force)) { Remove-Item -LiteralPath $output -Force -ErrorAction SilentlyContinue }
    throw $failure
}
Get-FileHash -LiteralPath $exe,$zip,$depsCopy -Algorithm SHA256 | Format-List Path,Hash
Write-Output "Verified $entryCount ZIP entries. Portable package: $zip"
Write-Output "Companion file (publish together with the ZIP): $depsCopy"
