#requires -Version 7.0
# Builds the Windows installer (NSIS or MSI) from a clean install of the locked dependencies.
# An installer is a distributable binary, so it is built under the same gate as the release workflow:
#   -DependencySourcesZip (mandatory): Quicktext-<version>-Source-deps.zip from scripts/package-dependency-sources.ps1.
#     It is validated before anything is built and handed to the Tauri hook scripts/check-bundle-preconditions.ps1
#     (QT_DEPENDENCY_SOURCES_ZIP), which refuses to bundle without it.
# The third-party notices are generated here and bundled in place of the committed placeholder
# (src-tauri\resources\THIRD-PARTY-LICENSES.txt); the placeholder is put back afterwards, whether the build worked or not.
param(
    [ValidateSet('nsis','msi')][string]$Bundle = 'nsis',
    [Parameter(Mandatory = $true)][ValidateNotNullOrEmpty()][string]$DependencySourcesZip
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'third-party-set.ps1')
$version = (Get-Content -LiteralPath (Join-Path $projectRoot 'package.json') -Raw -Encoding UTF8 | ConvertFrom-Json).version
$depsZip = [IO.Path]::GetFullPath($DependencySourcesZip)
# Fail before the long steps, not after them.
$null = Assert-DependencySourcesZip -Path $depsZip -ProjectRoot $projectRoot -Version $version
$resource = Join-Path $projectRoot 'src-tauri\resources\THIRD-PARTY-LICENSES.txt'
$placeholder = [IO.File]::ReadAllBytes($resource)
$noticeDir = Join-Path ([IO.Path]::GetTempPath()) ('qt-notices-' + [Guid]::NewGuid().ToString('N'))
$previousDeps = $env:QT_DEPENDENCY_SOURCES_ZIP
Push-Location $projectRoot
try {
    Write-Host 'Installing locked frontend dependencies'
    & npm.cmd ci
    if($LASTEXITCODE -ne 0){throw 'Dependency installation failed'}
    Write-Host 'Running Rust workspace verification'
    & cargo.exe test --workspace
    if($LASTEXITCODE -ne 0){throw 'Rust verification failed'}
    Write-Host 'Generating third-party notices'
    $notices = Join-Path $noticeDir 'THIRD-PARTY-LICENSES.txt'
    & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'generate-third-party-licenses.ps1') -OutputFile $notices
    if($LASTEXITCODE -ne 0){throw 'Third-party notice generation failed'}
    Copy-Item -LiteralPath $notices -Destination $resource -Force
    $env:QT_DEPENDENCY_SOURCES_ZIP = $depsZip
    Write-Host "Building desktop application and $Bundle installer"
    & npm.cmd run tauri -- build --bundles $Bundle
    if($LASTEXITCODE -ne 0){throw 'Desktop build failed'}
} finally {
    [IO.File]::WriteAllBytes($resource, $placeholder)
    if ($null -eq $previousDeps) { Remove-Item Env:\QT_DEPENDENCY_SOURCES_ZIP -ErrorAction SilentlyContinue } else { $env:QT_DEPENDENCY_SOURCES_ZIP = $previousDeps }
    Remove-Item -LiteralPath $noticeDir -Recurse -Force -ErrorAction SilentlyContinue
    Pop-Location
}
