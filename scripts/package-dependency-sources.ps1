#requires -Version 7.0
# Builds Quicker-Text-<version>-Source-deps.zip: the source of every third-party package the release is
# built from. Together with Quicker-Text-<version>-Source.zip (scripts/package-artifacts.ps1) it is the
# complete corresponding source of the release binaries (GPL-3.0-or-later section 1; the dependency
# code is compiled or bundled into them), so a release must not be published without it (S-28, B-51).
#
#   cargo-vendor/        `cargo vendor --locked --versioned-dirs`: every crate in Cargo.lock (build scripts,
#                        proc macros and crates for other platforms included; cargo needs the complete set
#                        to resolve offline)
#   cargo-config.toml    source replacement for an offline cargo build
#   npm/node_modules/    the production npm packages bundled into the frontend, plus the build tool whose
#                        code the bundle embeds (scripts/third-party-set.ps1), copied from node_modules
#   npm/PACKAGES.txt     name, version, resolved URL and integrity from package-lock.json
#   SOURCE-DEPS-MANIFEST.txt, README-OFFLINE-BUILD.md
#
# Prerequisites: `npm ci` was run in the project root (node_modules is the source of the npm packages;
# npm has verified their integrity against package-lock.json), cargo and network access for `cargo vendor`.
# Gates: fails when a distributed crate is not vendored, when the vendored crate count differs from
# Cargo.lock, when cargo cannot resolve the workspace offline from the vendored sources alone, or when an
# npm package is missing. Output must be a new file; nothing is overwritten. Size: several hundred MB.
#
#   pwsh scripts/package-dependency-sources.ps1 -OutputDirectory <dir>
#   -ProjectRoot <other checkout>    # e.g. the 0.3.1 tree, to publish its dependency sources afterwards
param(
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [string]$ProjectRoot = (Split-Path -Parent $PSScriptRoot),
    [string]$WorkDirectory,
    [switch]$KeepWorkDirectory
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'third-party-set.ps1')

$ProjectRoot = [IO.Path]::GetFullPath($ProjectRoot)
$version = (Get-Content -LiteralPath (Join-Path $ProjectRoot 'package.json') -Raw | ConvertFrom-Json).version
if ($version -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$') { throw 'Package version must use major.minor.patch format' }
$output = [IO.Path]::GetFullPath($OutputDirectory)
$zipName = "Quicker-Text-$version-Source-deps.zip"
$zipPath = Join-Path $output $zipName
if (Test-Path -LiteralPath $zipPath) { throw "Output already exists; preserve it: $zipName" }
New-Item -ItemType Directory -Path $output -Force | Out-Null
if (-not $WorkDirectory) { $WorkDirectory = Join-Path ([IO.Path]::GetTempPath()) ("qt-source-deps-" + [Guid]::NewGuid().ToString('N')) }
$work = [IO.Path]::GetFullPath($WorkDirectory)
if ((Test-Path -LiteralPath $work) -and @(Get-ChildItem -LiteralPath $work -Force).Count -gt 0) { throw "Work directory must be empty: $work" }
$stageName = "Quicker-Text-$version-Source-deps"
$stage = Join-Path $work $stageName
$vendorDir = Join-Path $stage 'cargo-vendor'
New-Item -ItemType Directory -Path $stage -Force | Out-Null
$manifest = Join-Path $ProjectRoot 'Cargo.toml'

try {
    # --- Rust ---------------------------------------------------------------------------------
    Write-Output 'cargo vendor --locked (all crates in Cargo.lock; this takes a minute)'
    $snippet = (& cargo vendor --locked --versioned-dirs --manifest-path $manifest $vendorDir) | Out-String
    if ($LASTEXITCODE -ne 0) { throw "cargo vendor failed (exit code $LASTEXITCODE)" }
    $sourceTables = @([regex]::Matches($snippet, '(?m)^\[source\.[^\]]+\]') | ForEach-Object { $_.Value })
    if (($sourceTables -join ' ') -ne '[source.crates-io] [source.vendored-sources]') {
        throw "Unexpected source replacement from cargo vendor (git dependencies or alternative registries?):`n$snippet"
    }
    $vendored = @(Get-ChildItem -LiteralPath $vendorDir -Directory)
    $lockText = Get-Content -LiteralPath (Join-Path $ProjectRoot 'Cargo.lock') -Raw
    $lockCrates = ([regex]::Matches($lockText, '(?m)^source = "registry\+')).Count
    if ($vendored.Count -ne $lockCrates) { throw "Vendored $($vendored.Count) crates but Cargo.lock lists $lockCrates registry crates" }
    $cargoDistributed = @(Get-DistributedCargoPackages -ProjectRoot $ProjectRoot)
    foreach ($package in $cargoDistributed) {
        $dir = Join-Path $vendorDir "$($package.Name)-$($package.Version)"
        if (!(Test-Path -LiteralPath (Join-Path $dir 'Cargo.toml') -PathType Leaf)) { throw "Distributed crate is not vendored: $($package.Name) $($package.Version)" }
    }
    @'
# Source replacement for an offline build from this archive. Copy it to <source tree>/.cargo/config.toml
# and copy the cargo-vendor directory next to Cargo.toml (see README-OFFLINE-BUILD.md).
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "cargo-vendor"
'@ | Set-Content -LiteralPath (Join-Path $stage 'cargo-config.toml') -Encoding utf8NoBOM

    # Offline resolution against the vendored sources alone (empty CARGO_HOME: no registry cache to lean on).
    $verifyConfig = Join-Path $work 'verify-config.toml'
    $verifyHome = Join-Path $work 'empty-cargo-home'
    New-Item -ItemType Directory -Path $verifyHome | Out-Null
    $vendorToml = $vendorDir -replace '\\', '/'
    "[source.crates-io]`nreplace-with = `"vendored-sources`"`n`n[source.vendored-sources]`ndirectory = `"$vendorToml`"`n" | Set-Content -LiteralPath $verifyConfig -Encoding utf8NoBOM
    $previousHome = $env:CARGO_HOME
    $env:CARGO_HOME = $verifyHome
    try {
        $null = (& cargo metadata --offline --locked --format-version 1 --filter-platform $ThirdPartyCargoTarget --manifest-path $manifest --config $verifyConfig) | Out-String
        if ($LASTEXITCODE -ne 0) { throw "cargo cannot resolve the workspace offline from the vendored sources (exit code $LASTEXITCODE)" }
    } finally {
        if ($null -eq $previousHome) { Remove-Item Env:\CARGO_HOME -ErrorAction SilentlyContinue } else { $env:CARGO_HOME = $previousHome }
    }
    Write-Output "Rust: $($vendored.Count) crates vendored; $($cargoDistributed.Count) distributed crates present; offline resolution OK"

    # --- npm ----------------------------------------------------------------------------------
    $npmPackages = @(Get-DistributedNpmPackages -ProjectRoot $ProjectRoot)
    if ($npmPackages.Count -eq 0) { throw 'No npm packages found; the frontend bundle always contains at least React' }
    $npmRoot = Join-Path $stage 'npm'
    $lines = [Collections.Generic.List[string]]::new()
    $lines.Add('# name version | resolved | integrity (package-lock.json)')
    foreach ($package in $npmPackages) {
        $target = Join-Path $npmRoot ($package.LockKey -replace '/', [IO.Path]::DirectorySeparatorChar)
        New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
        Copy-Item -LiteralPath $package.Directory -Destination $target -Recurse -Force
        $lines.Add("$($package.Name) $($package.Version) | $($package.Source) | $($package.Integrity)")
    }
    Set-Content -LiteralPath (Join-Path $npmRoot 'PACKAGES.txt') -Value $lines -Encoding utf8NoBOM
    Write-Output "npm: $($npmPackages.Count) packages ($((@($npmPackages | ForEach-Object { $_.Name })) -join ', '))"

    # --- manifest and instructions ------------------------------------------------------------
    $commit = ''
    if (Test-Path -LiteralPath (Join-Path $ProjectRoot '.git')) { $commit = ((& git -C $ProjectRoot rev-parse HEAD) | Out-String).Trim() }
    $tool = { param($exe, $argument) try { ((& $exe $argument) | Out-String).Trim() } catch { 'unavailable' } }
    $manifestLines = @(
        "Quicker Text $version dependency sources",
        "source commit: $(if ($commit) { $commit } else { 'unknown (no .git directory)' })",
        "Cargo.lock SHA-256: $(Get-ThirdPartyLockfileSha256 -Path (Join-Path $ProjectRoot 'Cargo.lock'))",
        "package-lock.json SHA-256: $(Get-ThirdPartyLockfileSha256 -Path (Join-Path $ProjectRoot 'package-lock.json'))",
        "crates vendored: $($vendored.Count) (distributed to the Windows x64 release: $($cargoDistributed.Count))",
        "npm packages: $($npmPackages.Count)",
        "cargo: $(& $tool 'cargo' '-V')",
        "rustc: $(& $tool 'rustc' '-V')",
        "node: $(& $tool 'node' '-v')",
        "npm: $(& $tool 'npm' '-v')"
    )
    Set-Content -LiteralPath (Join-Path $stage 'SOURCE-DEPS-MANIFEST.txt') -Value $manifestLines -Encoding utf8NoBOM
    $readme = @'
# Quicker Text {{VERSION}} - dependency sources (Source-deps)

This archive holds the source of the third-party packages that Quicker Text {{VERSION}} is built from.
Together with `Quicker-Text-{{VERSION}}-Source.zip` it is the complete corresponding source of the
release binaries (GPL-3.0-or-later). The licenses of these packages are listed in
`THIRD-PARTY-LICENSES.txt`, which ships with the installer and as a release attachment.

本压缩包包含 Quicker Text {{VERSION}} 所用第三方依赖的源码，与 `Quicker-Text-{{VERSION}}-Source.zip`
合在一起才是发布二进制文件的完整对应源码（GPL-3.0-or-later）。

## Contents / 内容

| Path | What |
| --- | --- |
| `cargo-vendor/` | every crate in `Cargo.lock` (`cargo vendor --locked --versioned-dirs`): {{CRATES}} crates, including build scripts, proc macros and crates for other platforms |
| `cargo-config.toml` | cargo source replacement for an offline build |
| `npm/node_modules/` | the production npm packages bundled into the frontend, plus `vite` (its preload polyfill is embedded in the bundle) |
| `npm/PACKAGES.txt` | npm name, version, resolved URL and integrity from `package-lock.json` |
| `SOURCE-DEPS-MANIFEST.txt` | commit, lock file hashes and tool versions this archive was made with |

## Rebuild from these sources / 重建步骤

Needs Rust (MSVC toolchain), the MSVC C++ build tools with the Windows SDK, Node.js and PowerShell 7.

1. Unpack `Quicker-Text-{{VERSION}}-Source.zip` into an empty directory (`<src>`).
2. Unpack this archive, copy its `cargo-vendor` directory to `<src>\cargo-vendor` and `cargo-config.toml` to
   `<src>\.cargo\config.toml` (create the `.cargo` directory). `<src>\Cargo.lock` stays unchanged.
3. Frontend (`npm`): the production packages are in `npm\node_modules`, plus `vite` because its preload
   polyfill is embedded in the bundle. The other build tools listed under `devDependencies` (TypeScript, the
   React plugin, the Tauri CLI) are not part of the product and are not in this archive; `npm ci` fetches
   them from the npm registry and verifies every package against the `integrity` values in
   `package-lock.json`. After `npm ci`, the folders in `node_modules` that also exist here have identical
   content and can be replaced by the folders from this archive to build against them.
   Then run `npm run build`.
4. Rust, offline: `cargo build -p quicker-text --release --locked --offline`. Cargo resolves everything from
   `cargo-vendor`; no registry access is needed.
5. Notices: `pwsh scripts/generate-third-party-licenses.ps1` regenerates `THIRD-PARTY-LICENSES.txt` from the
   licenses in `cargo-vendor` (through the cargo configuration of step 2) and in `node_modules`.
   Run it before step 6 so that the installer bundles the file.
6. Installer: `npm run tauri -- build --bundles nsis` (the Tauri CLI downloads the NSIS toolchain itself).

The exact build steps used by the release workflow are in `.github/workflows/release.yml` and
`docs/release-checklist.md` inside `Quicker-Text-{{VERSION}}-Source.zip`.
'@
    $readme = $readme.Replace('{{VERSION}}', $version).Replace('{{CRATES}}', [string]$vendored.Count)
    Set-Content -LiteralPath (Join-Path $stage 'README-OFFLINE-BUILD.md') -Value $readme -Encoding utf8NoBOM

    # --- archive ------------------------------------------------------------------------------
    Write-Output "Creating $zipName"
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::CreateFromDirectory($stage, $zipPath, [IO.Compression.CompressionLevel]::Optimal, $true)
    $expected = @(Get-ChildItem -LiteralPath $stage -File -Recurse -Force).Count
    $archive = [IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        $actual = @($archive.Entries | Where-Object { $_.Name }).Count
        $names = @($archive.Entries | ForEach-Object { $_.FullName })
    } finally { $archive.Dispose() }
    if ($actual -ne $expected) { throw "ZIP holds $actual files but $expected were staged" }
    foreach ($required in @("$stageName/README-OFFLINE-BUILD.md", "$stageName/cargo-config.toml", "$stageName/SOURCE-DEPS-MANIFEST.txt", "$stageName/npm/PACKAGES.txt")) {
        if ($names -notcontains $required) { throw "ZIP lacks $required" }
    }
    $hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $megabytes = [Math]::Round((Get-Item -LiteralPath $zipPath).Length / 1MB, 1)
    Write-Output "Wrote $zipPath ($megabytes MB, $actual files, SHA-256 $hash)"
} finally {
    if (-not $KeepWorkDirectory -and (Test-Path -LiteralPath $work)) { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
}
