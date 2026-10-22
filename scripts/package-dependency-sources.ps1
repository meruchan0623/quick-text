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
#   npm/tarballs/        the production npm packages bundled into the frontend, plus the build tool whose
#                        code the bundle embeds (scripts/third-party-set.ps1), as the ORIGINAL registry tarballs
#                        (`npm pack <resolved URL>`), each checked against the sha512 `integrity` of package-lock.json
#   npm/PACKAGES.txt     name, version, resolved URL, integrity and tarball file, from package-lock.json
#   SOURCE-DEPS-MANIFEST.txt, README-OFFLINE-BUILD.md
#
# Prerequisites: cargo and network access for `cargo vendor`; npm and the registry (or an npm cache that has the
# packages) for `npm pack`. Neither `npm ci` nor node_modules is needed: the tarballs come from the registry and
# are bound to package-lock.json, not copied from an install.
# Gates: fails when a distributed crate is not vendored, when the vendored crate count differs from
# Cargo.lock, when cargo cannot resolve the workspace offline from the vendored sources alone, when an npm
# tarball does not have the integrity of package-lock.json, and finally when the finished archive does not pass
# Assert-DependencySourcesZip (scripts/third-party-set.ps1: every file of every vendored crate is in the archive
# with the hash its .cargo-checksum.json lists; every npm tarball has its locked integrity). That check is
# the same one package-portable.ps1, package-artifacts.ps1 and the pre-bundle hook apply. It is aimed at an
# incomplete, stale or mismatched archive, not at someone who edits the files and the checksum file together.
# Output must be a new file; nothing is overwritten. Size: several hundred MB.
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
    # The original registry tarballs, one per package file. `npm pack <tarball URL>` hands out the tarball as the
    # registry serves it (npm cache first, registry otherwise); it is accepted only when its hash is the integrity
    # that package-lock.json records, so the archive holds exactly what `npm ci` installs and is bound to upstream.
    $tarballs = @(Get-DistributedNpmTarballs -ProjectRoot $ProjectRoot)
    if ($tarballs.Count -eq 0) { throw 'No npm packages found; the frontend bundle always contains at least React' }
    $npmRoot = Join-Path $stage 'npm'
    $tarballDir = Join-Path $npmRoot 'tarballs'
    New-Item -ItemType Directory -Path $tarballDir -Force | Out-Null
    $npmExe = if ($IsWindows) { 'npm.cmd' } else { 'npm' }
    $lines = [Collections.Generic.List[string]]::new()
    $lines.Add('# name version | resolved | integrity (package-lock.json) | tarball in this archive')
    foreach ($tarball in $tarballs) {
        $packDir = Join-Path $work ('npm-pack-' + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $packDir | Out-Null
        Push-Location $packDir
        try {
            $null = (& $npmExe pack $tarball.Resolved --pack-destination $packDir --prefer-offline --ignore-scripts --json) | Out-String
            if ($LASTEXITCODE -ne 0) { throw "npm pack failed for $($tarball.Name) $($tarball.Version) (exit code $LASTEXITCODE): $($tarball.Resolved)" }
        } finally { Pop-Location }
        $packed = @(Get-ChildItem -LiteralPath $packDir -File)
        if ($packed.Count -ne 1) { throw "npm pack of $($tarball.Name) $($tarball.Version) produced $($packed.Count) files instead of one tarball" }
        $stream = [IO.File]::OpenRead($packed[0].FullName)
        try { $intact = Test-NpmIntegrity -Integrity $tarball.Integrity -Stream $stream } finally { $stream.Dispose() }
        if (-not $intact) { throw "npm tarball of $($tarball.Name) $($tarball.Version) does not match the integrity in package-lock.json ($($tarball.Resolved))" }
        Move-Item -LiteralPath $packed[0].FullName -Destination (Join-Path $tarballDir $tarball.FileName)
        Remove-Item -LiteralPath $packDir -Recurse -Force
        $lines.Add((Get-NpmPackagesLine -Tarball $tarball))
    }
    Set-Content -LiteralPath (Join-Path $npmRoot 'PACKAGES.txt') -Value $lines -Encoding utf8NoBOM
    Write-Output "npm: $($tarballs.Count) registry tarballs, integrity verified ($((@($tarballs | ForEach-Object { $_.Name })) -join ', '))"

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
        "npm packages: $($tarballs.Count)",
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
| `npm/tarballs/` | the production npm packages bundled into the frontend, plus `vite` (its preload polyfill is embedded in the bundle), as the original registry tarballs (`.tgz`); each one has the sha512 `integrity` that `package-lock.json` records |
| `npm/PACKAGES.txt` | npm name, version, resolved URL, integrity and tarball file from `package-lock.json` |
| `SOURCE-DEPS-MANIFEST.txt` | commit, lock file hashes and tool versions this archive was made with |

## Rebuild from these sources / 重建步骤

Needs Rust (MSVC toolchain), the MSVC C++ build tools with the Windows SDK, Node.js and PowerShell 7.

1. Unpack `Quicker-Text-{{VERSION}}-Source.zip` into an empty directory (`<src>`).
2. Unpack this archive, copy its `cargo-vendor` directory to `<src>\cargo-vendor` and `cargo-config.toml` to
   `<src>\.cargo\config.toml` (create the `.cargo` directory). `<src>\Cargo.lock` stays unchanged.
3. Frontend (`npm`): `npm\tarballs` holds the original registry tarballs (`.tgz`) of the production packages,
   plus `vite` because its preload polyfill is embedded in the bundle. Every file is byte-identical to what npm
   downloads: its SHA-512 hash is the `integrity` value of `package-lock.json`, and `npm\PACKAGES.txt` lists
   name, version, URL, integrity and file of each one. `tar -xzf <file>` unpacks one (the package files are in
   the `package` folder it creates). The other build tools listed under `devDependencies` (TypeScript, the
   React plugin, the Tauri CLI) are not part of the product and are not in this archive. Run `npm ci`: it
   fetches the packages from the npm registry and verifies every one against the same `integrity` values, so
   the production packages it installs have the same content as the tarballs here. Then run `npm run build`.
4. Rust, offline: `cargo build -p quicker-text --release --locked --offline`. Cargo resolves everything from
   `cargo-vendor`; no registry access is needed.
5. Notices: `pwsh scripts/generate-third-party-licenses.ps1` regenerates `THIRD-PARTY-LICENSES.txt` from the
   licenses in `cargo-vendor` (through the cargo configuration of step 2) and in `node_modules`.
   Run it before step 6 so that the installer bundles the file.
6. Installer: `pwsh scripts/build-windows.ps1 -DependencySourcesZip <this archive>` runs `npm ci`, the Rust tests,
   the notice generation of step 5 and the installer build (the Tauri CLI downloads the NSIS toolchain
   itself). A plain `npm run tauri -- build` that bundles an installer is refused by
   `scripts/check-bundle-preconditions.ps1` unless the environment variable `QT_DEPENDENCY_SOURCES_ZIP` names
   this archive and the notices of step 5 are in place; an executable for local use only (not an installer)
   is `npm run tauri -- build --no-bundle`.

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
    # The finished file must pass the gate that package-portable.ps1, package-artifacts.ps1 and the pre-bundle hook
    # apply to it. An archive that fails is removed (it is the file this run just created), so it cannot be picked up.
    $checkTimer = [Diagnostics.Stopwatch]::StartNew()
    try { $verified = Assert-DependencySourcesZip -Path $zipPath -ProjectRoot $ProjectRoot -Version $version } catch {
        Remove-Item -LiteralPath $zipPath -Force -ErrorAction SilentlyContinue
        throw "The archive that was just written fails its own check and was removed: $($_.Exception.Message)"
    }
    Write-Output "Archive check OK: $($verified.Crates) crates ($($verified.RustFiles) files hash-checked against .cargo-checksum.json), $($verified.NpmPackages) npm tarballs matching package-lock.json integrity ($([Math]::Round($checkTimer.Elapsed.TotalSeconds, 1)) s)"
    $hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $megabytes = [Math]::Round((Get-Item -LiteralPath $zipPath).Length / 1MB, 1)
    Write-Output "Wrote $zipPath ($megabytes MB, $actual files, SHA-256 $hash)"
} finally {
    if (-not $KeepWorkDirectory -and (Test-Path -LiteralPath $work)) { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
}
