# Which third-party packages are actually distributed with a release (GPL-3.0-or-later, S-28, B-51).
#
# Dot-sourced by scripts/generate-third-party-licenses.ps1 (notice file) and
# scripts/package-dependency-sources.ps1 (dependency source archive). Keeping the definition
# of "distributed" in one place stops the notice file and the source archive from drifting.
#
# Rust:  `cargo metadata --locked --filter-platform <target>` -> every package reachable from the
#        application crate over normal and build dependencies (dev-dependencies excluded). Build
#        scripts and proc macros are included on purpose: they are needed to rebuild, and
#        including a few extra notices is harmless while omitting one is not.
# npm:   package-lock.json entries that are not dev-only (the production closure of
#        package.json "dependencies"), plus $ThirdPartyNpmExtras (see below).
#
# Workspace crates are the project itself (GPL-3.0-or-later, LICENSE) and are not listed.

$ThirdPartyCargoTarget = 'x86_64-pc-windows-msvc'
$ThirdPartyCargoRoot = 'quicker-text'

# Dev-only npm packages whose code still ends up in the production bundle. Vite injects its
# module-preload polyfill into every production bundle (confirmed in dist/assets/index-*.js),
# so its MIT license text has to travel with the artifacts. Add a package here when a build
# starts embedding code from another build tool.
$ThirdPartyNpmExtras = @('vite')

# License-like files at the top of a package directory. Anything else (docs, sources) is ignored.
$ThirdPartyLicenseFilePattern = '^(licen[sc]e|copying|copyright|notice|unlicense|patents?)([-_. ].*)?$'
$ThirdPartyLicenseFileSkipExtensions = @('.rs', '.toml', '.json', '.py', '.c', '.h', '.js', '.ts', '.yml', '.yaml', '.spdx')

function Get-ThirdPartyLicenseFiles {
    param([Parameter(Mandatory = $true)][string]$Directory)
    if (!(Test-Path -LiteralPath $Directory -PathType Container)) { return @() }
    $found = foreach ($file in (Get-ChildItem -LiteralPath $Directory -File -Force)) {
        if ($file.Name -notmatch $ThirdPartyLicenseFilePattern) { continue }
        if ($ThirdPartyLicenseFileSkipExtensions -contains $file.Extension.ToLowerInvariant()) { continue }
        $file
    }
    return @($found | Sort-Object { $_.Name.ToUpperInvariant() })
}

function Get-DistributedCargoPackages {
    param(
        [Parameter(Mandatory = $true)][string]$ProjectRoot,
        [string]$Target = $ThirdPartyCargoTarget,
        [string]$RootPackage = $ThirdPartyCargoRoot
    )
    $manifest = Join-Path $ProjectRoot 'Cargo.toml'
    # cargo prints UTF-8 (author names); the console code page would garble them.
    $previousEncoding = [Console]::OutputEncoding
    [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
    try {
        $raw = (& cargo metadata --format-version 1 --locked --filter-platform $Target --manifest-path $manifest) | Out-String
        if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed (exit code $LASTEXITCODE); run 'cargo fetch --locked' first" }
    } finally { [Console]::OutputEncoding = $previousEncoding }
    $meta = $raw | ConvertFrom-Json -AsHashtable

    $byId = @{}
    foreach ($package in $meta['packages']) { $byId[$package['id']] = $package }
    $nodes = @{}
    foreach ($node in $meta['resolve']['nodes']) { $nodes[$node['id']] = $node }
    $workspace = [Collections.Generic.HashSet[string]]::new([string[]]@($meta['workspace_members']))

    $rootId = $null
    foreach ($id in $workspace) { if ($byId[$id]['name'] -eq $RootPackage) { $rootId = $id } }
    if (-not $rootId) { throw "Root package '$RootPackage' not found in the workspace" }

    $seen = [Collections.Generic.HashSet[string]]::new()
    $queue = [Collections.Generic.Queue[string]]::new()
    [void]$seen.Add($rootId)
    $queue.Enqueue($rootId)
    while ($queue.Count -gt 0) {
        $current = $queue.Dequeue()
        foreach ($dep in $nodes[$current]['deps']) {
            $distributed = $false
            foreach ($kind in $dep['dep_kinds']) { if ($kind['kind'] -ne 'dev') { $distributed = $true } }
            if ($distributed -and $seen.Add($dep['pkg'])) { $queue.Enqueue($dep['pkg']) }
        }
    }

    $result = [Collections.Generic.List[object]]::new()
    foreach ($id in $seen) {
        if ($workspace.Contains($id)) { continue }
        $package = $byId[$id]
        $result.Add([pscustomobject]@{
            Ecosystem  = 'rust'
            Name       = [string]$package['name']
            Version    = [string]$package['version']
            License    = [string]$package['license']
            LicenseFileField = [string]$package['license_file']
            Authors    = @($package['authors'])
            Repository = [string]$package['repository']
            Source     = [string]$package['source']
            Directory  = Split-Path -Parent ([string]$package['manifest_path'])
        })
    }
    return @($result | Sort-Object Name, Version)
}

function Get-DistributedNpmPackages {
    param(
        [Parameter(Mandatory = $true)][string]$ProjectRoot,
        [string[]]$Extras = $ThirdPartyNpmExtras
    )
    $lockPath = Join-Path $ProjectRoot 'package-lock.json'
    $lock = Get-Content -LiteralPath $lockPath -Raw | ConvertFrom-Json -AsHashtable
    if ([int]$lock['lockfileVersion'] -lt 2) { throw 'package-lock.json must use lockfileVersion 2 or newer (a "packages" map)' }
    $extraKeys = @($Extras | ForEach-Object { "node_modules/$_" })
    foreach ($extraKey in $extraKeys) {
        if (!$lock['packages'].ContainsKey($extraKey)) { throw "Extra npm package $extraKey is not in package-lock.json" }
    }
    $result = [Collections.Generic.List[object]]::new()
    foreach ($key in $lock['packages'].Keys) {
        if ($key -eq '') { continue }
        $entry = $lock['packages'][$key]
        $devOnly = $entry.ContainsKey('dev') -and [bool]$entry['dev']
        if ($devOnly -and ($extraKeys -notcontains $key)) { continue }
        if ($entry.ContainsKey('link') -and [bool]$entry['link']) { continue }
        $directory = Join-Path $ProjectRoot ($key -replace '/', [IO.Path]::DirectorySeparatorChar)
        $packageJson = Join-Path $directory 'package.json'
        if (!(Test-Path -LiteralPath $packageJson -PathType Leaf)) {
            throw "npm package $key is not installed under $ProjectRoot (run 'npm ci' first)"
        }
        $json = Get-Content -LiteralPath $packageJson -Raw | ConvertFrom-Json -AsHashtable
        $name = [string]$json['name']
        $version = [string]$json['version']
        if ($version -ne [string]$entry['version']) { throw "npm package $key is $version on disk but $($entry['version']) in package-lock.json (run 'npm ci')" }
        $license = ''
        if ($json.ContainsKey('license') -and $json['license'] -is [string]) { $license = $json['license'] }
        elseif ($json.ContainsKey('license') -and $json['license'] -is [hashtable] -and $json['license'].ContainsKey('type')) { $license = [string]$json['license']['type'] }
        elseif ($entry.ContainsKey('license')) { $license = [string]$entry['license'] }
        $repository = ''
        if ($json.ContainsKey('repository')) {
            if ($json['repository'] -is [string]) { $repository = $json['repository'] }
            elseif ($json['repository'] -is [hashtable] -and $json['repository'].ContainsKey('url')) { $repository = [string]$json['repository']['url'] }
        }
        $authors = @()
        if ($json.ContainsKey('author')) {
            if ($json['author'] -is [string]) { $authors = @($json['author']) }
            elseif ($json['author'] -is [hashtable] -and $json['author'].ContainsKey('name')) { $authors = @([string]$json['author']['name']) }
        }
        $result.Add([pscustomobject]@{
            Ecosystem  = 'npm'
            Name       = $name
            Version    = $version
            License    = $license
            LicenseFileField = ''
            Authors    = $authors
            Repository = $repository
            Source     = [string]$entry['resolved']
            Directory  = $directory
            LockKey    = $key
            Integrity  = [string]$entry['integrity']
        })
    }
    return @($result | Sort-Object Name, Version)
}

# Lock files are hashed with LF line endings so that a CRLF checkout (core.autocrlf) gives the same value.
function Get-ThirdPartyLockfileSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)
    $text = (Get-Content -LiteralPath $Path -Raw) -replace "`r`n", "`n"
    return [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.UTF8Encoding]::new($false).GetBytes($text)))
}

# Used by the packaging scripts: the notice file that is shipped must be the generated one (not the
# committed placeholder) and must have been generated from the lock files of this very tree.
function Assert-ThirdPartyLicensesFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ProjectRoot
    )
    if (!(Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Missing third-party license file: $Path (run scripts/generate-third-party-licenses.ps1 before packaging)"
    }
    $text = [IO.File]::ReadAllText($Path, [Text.UTF8Encoding]::new($false))
    if ($text.Contains('QT-THIRD-PARTY-LICENSES-PLACEHOLDER')) {
        throw "$Path is still the committed placeholder; run scripts/generate-third-party-licenses.ps1 before building and packaging"
    }
    if ($text.Length -lt 10000 -or -not $text.Contains('Generated by scripts/generate-third-party-licenses.ps1')) {
        throw "$Path was not produced by scripts/generate-third-party-licenses.ps1"
    }
    $cargo = Get-ThirdPartyLockfileSha256 -Path (Join-Path $ProjectRoot 'Cargo.lock')
    $npm = Get-ThirdPartyLockfileSha256 -Path (Join-Path $ProjectRoot 'package-lock.json')
    if (-not $text.Contains("Cargo.lock SHA-256 $cargo")) { throw "$Path was generated from a different Cargo.lock; regenerate it" }
    if (-not $text.Contains("package-lock.json SHA-256 $npm")) { throw "$Path was generated from a different package-lock.json; regenerate it" }
}
