#requires -Version 7.0
# Hard gate in front of every installer or app bundle that Tauri builds (S-28, B-51).
#
# src-tauri/tauri.conf.json runs this as `build.beforeBundleCommand`, i.e. after the exe is compiled and before the
# bundler writes an installer. It exits non-zero (and the bundling is aborted, so no installer is produced) unless:
#   - the target is Windows x64, the only platform with a notice file and a dependency source archive (the
#     notice generator and the archive follow the x86_64-pc-windows-msvc dependency set);
#   - src-tauri\resources\THIRD-PARTY-LICENSES.txt, which the bundle puts into the installer, is the generated file
#     (not the committed placeholder) and was generated from this tree's Cargo.lock and package-lock.json;
#   - $env:QT_DEPENDENCY_SOURCES_ZIP names Quicktext-<version>-Source-deps.zip, built from the same lock files
#     and complete (Assert-DependencySourcesZip). An installer is a distributable binary: the dependency code compiled into
#     it needs its corresponding source (GPL-3.0-or-later), so it is not built without that archive.
# scripts/build-windows.ps1 and .github/workflows/release.yml set the variable; a bare `npm run tauri build` fails here.
# For a local build that is only to be run, not distributed: `npm run tauri -- build --no-bundle` (exe only).
#
# The raw exe in target\release is an intermediate, not a delivery: it is only distributed through
# scripts/package-artifacts.ps1 or scripts/package-portable.ps1, which apply the same checks.
param()
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
try {
    . (Join-Path $PSScriptRoot 'third-party-set.ps1')
    $platform = $env:TAURI_ENV_PLATFORM
    if (-not $platform) { $platform = if ($IsWindows) { 'windows' } elseif ($IsMacOS) { 'darwin' } else { 'linux' } }
    $arch = $env:TAURI_ENV_ARCH
    if ($platform -ne 'windows' -or ($arch -and $arch -ne 'x86_64')) {
        throw "Only the Windows x64 release flow has third-party notices and dependency sources; refusing to bundle for $platform/$(if ($arch) { $arch } else { 'unknown arch' }). For a local build that is not distributed use: npm run tauri -- build --no-bundle"
    }
    $version = (Get-Content -LiteralPath (Join-Path $root 'package.json') -Raw -Encoding UTF8 | ConvertFrom-Json).version
    $notices = Join-Path $root 'src-tauri\resources\THIRD-PARTY-LICENSES.txt'
    Assert-ThirdPartyLicensesFile -Path $notices -ProjectRoot $root
    $depsPath = $env:QT_DEPENDENCY_SOURCES_ZIP
    if (-not $depsPath) {
        throw "QT_DEPENDENCY_SOURCES_ZIP is not set. Build Quicktext-$version-Source-deps.zip with scripts/package-dependency-sources.ps1 and point the variable at it (scripts/build-windows.ps1 -DependencySourcesZip does this); an installer must not be built without its dependency sources."
    }
    $deps = Assert-DependencySourcesZip -Path ([IO.Path]::GetFullPath($depsPath)) -ProjectRoot $root -Version $version
    Write-Output "bundle gate OK: generated notice file; dependency sources $($deps.Crates) crates + $($deps.NpmPackages) npm packages (SHA-256 $($deps.Sha256))"
} catch {
    [Console]::Error.WriteLine("bundle gate FAILED: $($_.Exception.Message)")
    exit 1
}
