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

# The package-lock.json entries that are distributed: everything that is not dev-only, plus the extras.
# Needs only the lock file (no node_modules), so Assert-DependencySourcesZip can use it too.
function Get-DistributedNpmLockEntries {
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
        $result.Add([pscustomobject]@{ Key = [string]$key; Entry = $entry })
    }
    return @($result)
}

function Get-DistributedNpmPackages {
    param(
        [Parameter(Mandatory = $true)][string]$ProjectRoot,
        [string[]]$Extras = $ThirdPartyNpmExtras
    )
    $result = [Collections.Generic.List[object]]::new()
    foreach ($item in (Get-DistributedNpmLockEntries -ProjectRoot $ProjectRoot -Extras $Extras)) {
        $key = $item.Key
        $entry = $item.Entry
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

# --- dependency source archive (Source-deps.zip) ---------------------------------------------------

function Read-ZipEntryText {
    param([Parameter(Mandatory = $true)]$Entry)
    $reader = [IO.StreamReader]::new($Entry.Open(), [Text.UTF8Encoding]::new($false))
    try { return $reader.ReadToEnd() } finally { $reader.Dispose() }
}

function Get-ZipEntrySha256 {
    param([Parameter(Mandatory = $true)]$Entry)
    $stream = $Entry.Open()
    try { return [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($stream)).ToLowerInvariant() } finally { $stream.Dispose() }
}

# Every crate of Cargo.lock that comes from a registry (name, version, checksum). Workspace members have no
# source line and are skipped. A git or path source cannot be vendored the way the archive is built, so it is an error.
function Get-LockedRegistryCrates {
    param([Parameter(Mandatory = $true)][string]$LockPath)
    $text = (Get-Content -LiteralPath $LockPath -Raw) -replace "`r`n", "`n"
    $pattern = '(?m)^\[\[package\]\]\nname = "(?<name>[^"]+)"\nversion = "(?<version>[^"]+)"\n(?:source = "(?<source>[^"]+)"\n)?(?:checksum = "(?<checksum>[0-9a-f]{64})"\n)?'
    $blocks = ([regex]::Matches($text, '(?m)^\[\[package\]\]$')).Count
    $found = [regex]::Matches($text, $pattern)
    if ($found.Count -ne $blocks) { throw "Cannot parse every [[package]] block of $LockPath" }
    $result = [Collections.Generic.List[object]]::new()
    foreach ($m in $found) {
        if (-not $m.Groups['source'].Success) { continue }
        $source = $m.Groups['source'].Value
        if (-not $source.StartsWith('registry+', [StringComparison]::Ordinal)) { throw "Cargo.lock: $($m.Groups['name'].Value) comes from $source; only registry crates can be vendored" }
        if (-not $m.Groups['checksum'].Success) { throw "Cargo.lock: $($m.Groups['name'].Value) has no checksum" }
        $result.Add([pscustomobject]@{ Name = $m.Groups['name'].Value; Version = $m.Groups['version'].Value; Checksum = $m.Groups['checksum'].Value })
    }
    return @($result)
}

# Hard gate for every path that can produce something distributable (package-portable.ps1, package-artifacts.ps1,
# scripts/check-bundle-preconditions.ps1 for `tauri build`): a binary must never go out without its dependency
# sources (GPL-3.0-or-later; the dependency code is compiled or bundled into it). Throws unless the archive is
#   - named for this version and a readable ZIP with the layout scripts/package-dependency-sources.ps1 writes,
#   - built from THIS tree's Cargo.lock and package-lock.json (hashes in SOURCE-DEPS-MANIFEST.txt),
#   - complete: exactly the crates of Cargo.lock are vendored, each one's .cargo-checksum.json carries the checksum
#     of Cargo.lock and the hash of its own Cargo.toml (so a truncated or edited entry is noticed), and every
#     distributed npm package is present at the version and with the PACKAGES.txt line the lock file asks for.
# Returns Path, Sha256, Crates and NpmPackages. Reads the archive only; it needs neither cargo nor node_modules nor a network.
function Assert-DependencySourcesZip {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ProjectRoot,
        [Parameter(Mandatory = $true)][string]$Version
    )
    $label = 'Dependency source archive'
    if (!(Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$label missing: $Path (build it with scripts/package-dependency-sources.ps1)" }
    $expectedName = "Quicker-Text-$Version-Source-deps.zip"
    if ((Split-Path -Leaf $Path) -ne $expectedName) { throw "$label must be named $expectedName (built for version $Version), got $(Split-Path -Leaf $Path)" }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    try { $archive = [IO.Compression.ZipFile]::OpenRead($Path) } catch { throw "$label is not a readable ZIP file: $Path ($($_.Exception.Message))" }
    try {
        $stage = "Quicker-Text-$Version-Source-deps"
        $entries = @{}
        foreach ($entry in $archive.Entries) { $entries[$entry.FullName] = $entry }
        foreach ($required in @('README-OFFLINE-BUILD.md', 'cargo-config.toml', 'SOURCE-DEPS-MANIFEST.txt', 'npm/PACKAGES.txt')) {
            if (-not $entries.ContainsKey("$stage/$required")) { throw "$label lacks $required" }
        }

        # Manifest: version and the lock files the archive was built from.
        $manifest = Read-ZipEntryText -Entry $entries["$stage/SOURCE-DEPS-MANIFEST.txt"]
        if ($manifest -notmatch "(?m)^Quicker Text $([regex]::Escape($Version)) dependency sources\s*$") { throw "$label manifest is not for version $Version" }
        foreach ($lockName in @('Cargo.lock', 'package-lock.json')) {
            $declaredHash = [regex]::Match($manifest, "(?m)^$([regex]::Escape($lockName)) SHA-256: ([0-9A-Fa-f]{64})\s*$")
            if (-not $declaredHash.Success) { throw "$label manifest has no $lockName hash" }
            $current = Get-ThirdPartyLockfileSha256 -Path (Join-Path $ProjectRoot $lockName)
            if ($declaredHash.Groups[1].Value -ne $current) {
                throw "$label was built from a different $lockName (archive $($declaredHash.Groups[1].Value.ToLowerInvariant()), this tree $($current.ToLowerInvariant())); rebuild it with scripts/package-dependency-sources.ps1"
            }
        }

        # Rust: the vendored directories are exactly the registry crates of Cargo.lock, with the checksums of Cargo.lock.
        $crates = @(Get-LockedRegistryCrates -LockPath (Join-Path $ProjectRoot 'Cargo.lock'))
        if ($crates.Count -lt 1) { throw 'Cargo.lock lists no registry crates' }
        $vendorPrefix = "$stage/cargo-vendor/"
        $vendoredCount = @($entries.Keys | Where-Object {
            $_.StartsWith($vendorPrefix, [StringComparison]::Ordinal) -and $_.EndsWith('/Cargo.toml', [StringComparison]::Ordinal) -and (@($_.Substring($vendorPrefix.Length).Split('/')).Count -eq 2)
        }).Count
        if ($vendoredCount -ne $crates.Count) { throw "$label holds $vendoredCount vendored crates but Cargo.lock lists $($crates.Count)" }
        $declaredCrates = [regex]::Match($manifest, '(?m)^crates vendored: (\d+) ')
        if (-not $declaredCrates.Success -or [int]$declaredCrates.Groups[1].Value -ne $crates.Count) { throw "$label manifest does not declare the $($crates.Count) crates of Cargo.lock" }
        foreach ($crate in $crates) {
            $directory = "$vendorPrefix$($crate.Name)-$($crate.Version)"
            $toml = $entries["$directory/Cargo.toml"]
            $sums = $entries["$directory/.cargo-checksum.json"]
            if (-not $toml -or -not $sums) { throw "$label lacks the source of crate $($crate.Name) $($crate.Version)" }
            $sumText = Read-ZipEntryText -Entry $sums
            $package = [regex]::Match($sumText, '"package":"([0-9a-f]{64})"')
            if (-not $package.Success -or $package.Groups[1].Value -ne $crate.Checksum) { throw "$label`: crate $($crate.Name) $($crate.Version) does not match the checksum in Cargo.lock" }
            $own = [regex]::Match($sumText, '"Cargo\.toml":"([0-9a-f]{64})"')
            if (-not $own.Success -or $own.Groups[1].Value -ne (Get-ZipEntrySha256 -Entry $toml)) { throw "$label`: Cargo.toml of crate $($crate.Name) $($crate.Version) differs from its checksum file (damaged or edited)" }
        }

        # npm: every distributed package of package-lock.json is present, at the locked version, and listed.
        $npmItems = @(Get-DistributedNpmLockEntries -ProjectRoot $ProjectRoot)
        if ($npmItems.Count -lt 1) { throw 'package-lock.json lists no distributed npm packages' }
        $declaredNpm = [regex]::Match($manifest, '(?m)^npm packages: (\d+)\s*$')
        if (-not $declaredNpm.Success -or [int]$declaredNpm.Groups[1].Value -ne $npmItems.Count) { throw "$label manifest does not declare the $($npmItems.Count) npm packages of package-lock.json" }
        $listed = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($line in ((Read-ZipEntryText -Entry $entries["$stage/npm/PACKAGES.txt"]) -split "\r?\n")) {
            if ($line -and -not $line.StartsWith('#', [StringComparison]::Ordinal)) { [void]$listed.Add($line) }
        }
        if ($listed.Count -ne $npmItems.Count) { throw "$label`: npm/PACKAGES.txt lists $($listed.Count) packages but package-lock.json distributes $($npmItems.Count)" }
        foreach ($item in $npmItems) {
            $packageJson = $entries["$stage/npm/$($item.Key)/package.json"]
            if (-not $packageJson) { throw "$label lacks npm package $($item.Key)" }
            $json = Read-ZipEntryText -Entry $packageJson | ConvertFrom-Json -AsHashtable
            if ([string]$json['version'] -ne [string]$item.Entry['version']) { throw "$label holds $($item.Key) $($json['version']) but package-lock.json has $($item.Entry['version'])" }
            $expectedLine = "$($json['name']) $($item.Entry['version']) | $([string]$item.Entry['resolved']) | $([string]$item.Entry['integrity'])"
            if (-not $listed.Contains($expectedLine)) { throw "$label`: npm/PACKAGES.txt has no line for $($item.Key) (expected '$expectedLine')" }
        }
    } finally { $archive.Dispose() }
    return [pscustomobject]@{
        Path        = $Path
        Sha256      = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
        Crates      = $crates.Count
        NpmPackages = $npmItems.Count
    }
}
