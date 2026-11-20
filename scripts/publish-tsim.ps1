#requires -Version 7.0
# Builds, signs and uploads the Windows x64 release (per-user MSI) to the TSim update servers
# (docs/auto-update.md; contract: client-updates CONTRACT.md §3 and §6). Runs on the packaging PC only; no GitHub
# involved. The MSI is both the installer and the update package.
#
#   1. version from the manifests (scripts/version.mjs check), update notes from -Notes, -NotesFile or
#      release-notes\<version>.txt (plain text, 1-4000 characters)
#   2. build through scripts/build-windows.ps1 -Bundle msi -DependencySourcesZip <...>: the same gate as every
#      distributable build (generated third-party notices, verified dependency source archive), stable update
#      channel, optional code signing (-CertificateThumbprint). -SkipBuild reuses the MSI already in
#      target\release\bundle\msi (which only that gated build produces).
#   3. `tauri signer sign --app-version` on the MSI, verified against src-tauri\update-keys\current.pub
#   4. multipart upload to POST <server>/client-updates/v1/publish (draft, or published with -Publish)
#
# Usage:
#   pwsh -NoProfile -File scripts/publish-tsim.ps1 [-Server test|prod|both] `
#        -DependencySourcesZip <dir>\Quicktext-<version>-Source-deps.zip [-Publish] [-SkipBuild] [-DryRun] `
#        [-Notes <text> | -NotesFile <file>] [-CertificateThumbprint <thumbprint>]
#   -Server   default test (the customer-service test server); both = test first, then prod
#   -DryRun   build, sign and verify only; nothing is uploaded and no publish key is needed
#
# Environment (never committed, never printed). Dot-sourced first from $env:QT_RELEASE_ENV, default
# %USERPROFILE%\.quicktext-release.env.ps1, when that file exists ($env:NAME = '...' lines):
#   TSIM_PUBLISH_KEY_PROD / TSIM_PUBLISH_KEY_TEST   publish keys of the servers uploaded to
#   TAURI_SIGNING_PRIVATE_KEY (content) or TAURI_SIGNING_PRIVATE_KEY_PATH (file; default
#     %USERPROFILE%\.config\quicktext-signing\updater.key when it exists), TAURI_SIGNING_PRIVATE_KEY_PASSWORD
# Distribute the Source.zip / Source-deps.zip of this version alongside (docs/release-checklist.md).
param(
    [ValidateSet('prod', 'test', 'both')][string]$Server = 'test',
    [switch]$Publish,
    [switch]$DryRun,
    [switch]$SkipBuild,
    [string]$Notes = '',
    [string]$NotesFile = '',
    [string]$DependencySourcesZip = '',
    [string]$CertificateThumbprint = ''
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$product = 'quicktext'
$bases = @{ prod = 'https://csapi.tsim.tech'; test = 'https://test-csapi.tsim.tech' }
$servers = if ($Server -eq 'both') { @('test', 'prod') } else { @($Server) }
$releaseEnv = if ($env:QT_RELEASE_ENV) { $env:QT_RELEASE_ENV } else { Join-Path $HOME '.quicktext-release.env.ps1' }
if (Test-Path -LiteralPath $releaseEnv -PathType Leaf) { . $releaseEnv }
if (-not $CertificateThumbprint -and $env:QT_WIN_CERT_THUMBPRINT) { $CertificateThumbprint = $env:QT_WIN_CERT_THUMBPRINT }

Push-Location $projectRoot
try {
    # 1. Version, notes and keys: everything checked before the long build.
    $check = & node scripts/version.mjs check
    if ($LASTEXITCODE -ne 0 -or "$check" -notmatch '^version (\S+) ') { throw 'version manifests disagree (node scripts/version.mjs check)' }
    $version = $Matches[1]
    if (-not $Notes) {
        if (-not $NotesFile) { $NotesFile = Join-Path 'release-notes' "$version.txt" }
        if (-not (Test-Path -LiteralPath $NotesFile -PathType Leaf)) { throw "no update notes: pass -Notes, -NotesFile or create $NotesFile" }
        $Notes = Get-Content -LiteralPath $NotesFile -Raw -Encoding UTF8
    }
    $Notes = $Notes.Trim()
    $length = [Globalization.StringInfo]::new($Notes).LengthInTextElements
    if ($length -lt 1 -or $length -gt 4000) { throw "update notes must be 1-4000 characters (got $length)" }
    if (-not $DryRun) { foreach ($s in $servers) {
        if (-not [Environment]::GetEnvironmentVariable("TSIM_PUBLISH_KEY_$($s.ToUpper())")) { throw "TSIM_PUBLISH_KEY_$($s.ToUpper()) is not set" }
    } }
    if (-not $env:TAURI_SIGNING_PRIVATE_KEY -and -not $env:TAURI_SIGNING_PRIVATE_KEY_PATH) {
        $defaultKey = Join-Path $HOME '.config\quicktext-signing\updater.key'
        if (-not (Test-Path -LiteralPath $defaultKey -PathType Leaf)) {
            throw "set TAURI_SIGNING_PRIVATE_KEY or TAURI_SIGNING_PRIVATE_KEY_PATH (or put the key at $defaultKey)"
        }
        $env:TAURI_SIGNING_PRIVATE_KEY_PATH = $defaultKey
    }

    # 2. Build (gated, see scripts/build-windows.ps1).
    $built = Join-Path $projectRoot "target\release\bundle\msi\Quicktext_${version}_x64_zh-CN.msi"
    if (-not $SkipBuild) {
        if (-not $DependencySourcesZip) { throw '-DependencySourcesZip is required to build (scripts/package-dependency-sources.ps1); -SkipBuild reuses an MSI already built' }
        $buildArgs = @{ Bundle = 'msi'; DependencySourcesZip = $DependencySourcesZip }
        if ($CertificateThumbprint) { $buildArgs.CertificateThumbprint = $CertificateThumbprint }
        else { Write-Warning 'no -CertificateThumbprint: exe and MSI are not code-signed (SmartScreen warns on first install)' }
        # build-windows.ps1 keeps a QT_UPDATE_CHANNEL that is already set (e.g. a leftover `beta`); a published build
        # must read the stable feed, or it can never update again.
        $previousChannel = $env:QT_UPDATE_CHANNEL
        $env:QT_UPDATE_CHANNEL = 'stable'
        try {
            & (Join-Path $PSScriptRoot 'build-windows.ps1') @buildArgs
        } finally {
            if ($null -eq $previousChannel) { Remove-Item Env:\QT_UPDATE_CHANNEL -ErrorAction SilentlyContinue } else { $env:QT_UPDATE_CHANNEL = $previousChannel }
        }
    }
    if (-not (Test-Path -LiteralPath $built -PathType Leaf)) { throw "MSI not found: $built" }

    # 3. Sign the update package and verify it the way the client will.
    $out = Join-Path $projectRoot "target\tsim-publish\$version\windows"
    if (Test-Path -LiteralPath $out) { Remove-Item -LiteralPath $out -Recurse -Force }
    New-Item -ItemType Directory -Path $out | Out-Null
    $msi = Join-Path $out (Split-Path -Leaf $built)
    Copy-Item -LiteralPath $built -Destination $msi
    & npx.cmd tauri signer sign --app-version $version $msi | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'tauri signer sign failed' }
    & node scripts/update-manifest.mjs --version $version --pubkey src-tauri/update-keys/current.pub `
        --base-url https://example.invalid/check --out (Join-Path $out 'check.json') "windows-x86_64=$msi" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'the update signature does not verify against src-tauri/update-keys/current.pub' }
    Remove-Item -LiteralPath (Join-Path $out 'check.json')
    $signature = (Get-Content -LiteralPath "$msi.sig" -Raw).Trim()
    Write-Host "packaged and verified: $msi"
    if ($DryRun) { Write-Host 'dry run: nothing uploaded'; return }

    # 4. Upload.
    $failed = @()
    foreach ($s in $servers) {
        $form = @{
            product      = $product
            platform     = 'windows'
            version      = $version
            notes        = $Notes
            signature    = $signature
            updater_file = Get-Item -LiteralPath $msi
        }
        if ($Publish) { $form.publish = 'true' }
        $headers = @{ 'X-Client-Publish-Key' = [Environment]::GetEnvironmentVariable("TSIM_PUBLISH_KEY_$($s.ToUpper())") }
        Write-Host "uploading $version to $s ($($bases[$s]))..."
        try {
            $response = Invoke-WebRequest -Method Post -Uri "$($bases[$s])/client-updates/v1/publish" -Headers $headers `
                -Form $form -SkipHttpErrorCheck -TimeoutSec 600
            if ($response.StatusCode -ge 200 -and $response.StatusCode -lt 300) {
                Write-Host "${s}: HTTP $($response.StatusCode) $($response.Content)"
            } else {
                Write-Warning "${s}: upload FAILED (HTTP $($response.StatusCode)) $($response.Content)"
                $failed += $s
            }
        } catch {
            Write-Warning "${s}: upload FAILED ($($_.Exception.Message))"
            $failed += $s
        }
    }
    if ($failed.Count -gt 0) { throw "not every upload succeeded ($($failed -join ', ')); rerun with -SkipBuild -Server <the failed one>" }
    Write-Host ("done: $version " + $(if ($Publish) { 'published' } else { 'uploaded as draft (publish it in the admin page)' }))
} finally {
    Pop-Location
}
