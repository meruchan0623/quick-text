param([ValidateSet('nsis','msi')][string]$Bundle='nsis')
$ErrorActionPreference='Stop'
$projectRoot=Split-Path -Parent $PSScriptRoot
Push-Location $projectRoot
try {
    Write-Host 'Installing locked frontend dependencies'
    & npm.cmd ci
    if($LASTEXITCODE -ne 0){throw 'Dependency installation failed'}
    Write-Host 'Running Rust workspace verification'
    & cargo.exe test --workspace
    if($LASTEXITCODE -ne 0){throw 'Rust verification failed'}
    Write-Host "Building desktop application and $Bundle installer"
    & npm.cmd run tauri -- build --bundles $Bundle
    if($LASTEXITCODE -ne 0){throw 'Desktop build failed'}
} finally {Pop-Location}
