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
$ThirdPartyCargoRoot = 'quicktext'

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

# The distributed npm packages as registry tarballs: one entry per package file (name, version, the tarball URL and
# the integrity of package-lock.json, and the file name the tarball has inside the archive: npm/tarballs/<FileName>).
# Needs only package-lock.json. The archive generator and Assert-DependencySourcesZip both derive from this, so they agree.
function Get-NpmLockPackageName {
    param([Parameter(Mandatory = $true)][string]$Key, [Parameter(Mandatory = $true)]$Entry)
    if ($Entry.ContainsKey('name') -and $Entry['name']) { return [string]$Entry['name'] }
    return $Key.Substring($Key.LastIndexOf('node_modules/', [StringComparison]::Ordinal) + 'node_modules/'.Length)
}

function Get-DistributedNpmTarballs {
    param([Parameter(Mandatory = $true)][string]$ProjectRoot)
    $byFile = [ordered]@{}
    foreach ($item in (Get-DistributedNpmLockEntries -ProjectRoot $ProjectRoot)) {
        $entry = $item.Entry
        $name = Get-NpmLockPackageName -Key $item.Key -Entry $entry
        $version = [string]$entry['version']
        $resolved = if ($entry.ContainsKey('resolved')) { [string]$entry['resolved'] } else { '' }
        $integrity = if ($entry.ContainsKey('integrity')) { [string]$entry['integrity'] } else { '' }
        if (-not $resolved -or -not $integrity) { throw "package-lock.json: $($item.Key) has no resolved URL or integrity, so its tarball cannot be bound to the registry" }
        $fileName = ($name -replace '/', '__') + "-$version.tgz"
        if ($byFile.Contains($fileName)) {
            if ($byFile[$fileName].Integrity -ne $integrity) { throw "package-lock.json: $name $version appears with two different integrity values" }
            continue
        }
        $byFile[$fileName] = [pscustomobject]@{ Name = $name; Version = $version; Resolved = $resolved; Integrity = $integrity; FileName = $fileName }
    }
    return @($byFile.Values | Sort-Object FileName)
}

# The line npm/PACKAGES.txt has for a tarball: name, version, resolved URL, integrity (package-lock.json) and the tarball's path.
function Get-NpmPackagesLine {
    param([Parameter(Mandatory = $true)]$Tarball)
    return "$($Tarball.Name) $($Tarball.Version) | $($Tarball.Resolved) | $($Tarball.Integrity) | tarballs/$($Tarball.FileName)"
}

# True when the stream has the npm `integrity` value (Subresource Integrity: "sha512-<base64>", possibly several
# values; the strongest of sha512, sha384, sha256 counts). Throws for a value it cannot check.
function Test-NpmIntegrity {
    param([Parameter(Mandatory = $true)][string]$Integrity, [Parameter(Mandatory = $true)][IO.Stream]$Stream)
    $tokens = @($Integrity -split '\s+' | Where-Object { $_ })
    $chosen = $null
    foreach ($algorithm in @('sha512', 'sha384', 'sha256')) {
        $chosen = $tokens | Where-Object { $_.StartsWith("$algorithm-", [StringComparison]::Ordinal) } | Select-Object -First 1
        if ($chosen) { break }
    }
    if (-not $chosen) { throw "Unsupported npm integrity value '$Integrity' (need sha512, sha384 or sha256)" }
    $algorithm, $expected = $chosen.Split('-', 2)
    $expected = ($expected -split '\?')[0]
    if ($algorithm -eq 'sha512') { $hash = [Security.Cryptography.SHA512]::HashData($Stream) }
    elseif ($algorithm -eq 'sha384') { $hash = [Security.Cryptography.SHA384]::HashData($Stream) }
    else { $hash = [Security.Cryptography.SHA256]::HashData($Stream) }
    return [Convert]::ToBase64String($hash) -ceq $expected
}

# Rejects the whole archive when its entry names are not canonical, before any entry is read or hashed: the tree that
# unpacking gives must be exactly the set of entries that are verified. The archive is meant to be unpacked on
# Windows, where the file system is case-insensitive and rewrites or refuses some names: two entries that collide
# there overwrite one another (an unchecked stale `CARGO.TOML` replaces the verified `Cargo.toml`), and a name that
# is rewritten (trailing dot or space) or refused (device names, `:`, `\`, `..`) is not the file that was hashed.
# Rejected:
#   - exact duplicate names (a later entry silently shadows an earlier one);
#   - names that collide when case is folded (the later entry overwrites the earlier one), and a directory whose
#     spelling differs only by case between entries (`SERDE-1.0/x` next to `serde-1.0/y`);
#   - a directory entry and a file entry with the same name, or a file that is also used as a directory (`a/b` and `a/b/c`);
#   - names that do not unpack unchanged on Windows: a backslash, colon or another character Windows rejects
#     (< > " | ? * and control characters), a leading slash, an empty segment (`a//b`), a `.` or `..` segment, a
#     segment that ends with a space or dot, a reserved device name with or without extension (CON, PRN, AUX, NUL,
#     COM1-9, LPT1-9). A directory entry's trailing slash is not an empty segment.
function Assert-ZipEntryNames {
    param(
        [Parameter(Mandatory = $true)]$Archive,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $names = [string[]]$Archive.Entries.FullName
    if ($null -eq $names -or $names.Length -eq 0) { return }
    $slash = [char]'/'
    # A per-entry PowerShell loop over some 29000 entries takes more than a second, a third of the whole validation, so
    # the names are handled as text: one string, one name per line, and a few passes of the regex engine and of
    # HashSet, which do the work in one call each. (A name that contains a line break shifts the lines: see step 2.)
    $multiline = [Text.RegularExpressions.RegexOptions]::Multiline -bor [Text.RegularExpressions.RegexOptions]::Compiled
    $text = [string]::Join("`n", $names)

    # 1. Names that do not unpack unchanged on Windows. `^$` is the empty name; the character class leaves out
    #    the line break itself, which is caught by the line count in step 2.
    $problem = [regex]::new('^$|[\\:<>"|?*\x00-\x09\x0b-\x1f]|^/|//|(?:^|/)\.{1,2}(?:/|$)|[ .](?:/|$)|(?:^|/)(?:con|prn|aux|nul|com[1-9]|lpt[1-9]) *(?:\.[^/\n]*)?(?:/|$)', $multiline -bor [Text.RegularExpressions.RegexOptions]::IgnoreCase -bor [Text.RegularExpressions.RegexOptions]::CultureInvariant)
    $found = $problem.Match($text)
    if ($found.Success) {
        $from = if ($found.Index -eq 0) { 0 } else { $text.LastIndexOf([char]"`n", $found.Index - 1) + 1 }
        $to = $text.IndexOf([char]"`n", $found.Index)
        if ($to -lt 0) { $to = $text.Length }
        throw "$Label has an entry name that does not unpack unchanged on Windows: '$($text.Substring($from, $to - $from))' (empty, backslash, colon, leading or double slash, '.' or '..' segment, trailing dot or space, reserved device name or a character Windows rejects); rebuild it with scripts/package-dependency-sources.ps1"
    }

    # 2. The path of every entry: its name without the trailing slash of a directory entry (safe: `//` is rejected).
    $hasDirectoryEntries = $text.EndsWith($slash) -or $text.Contains("/`n")
    $pathText = if ($hasDirectoryEntries) { [regex]::Replace($text, '/$', '', $multiline) } else { $text }
    $paths = $pathText.Split([char]"`n")
    if ($paths.Length -ne $names.Length) {
        throw "$Label has an entry name with a line break in it: '$(($names | Where-Object { $_.Contains([char]"`n") } | Select-Object -First 1))'"
    }

    # 3. Duplicates, and names that are the same path on Windows (case folded, a file next to a directory entry).
    $folded = [Collections.Generic.HashSet[string]]::new($paths, [StringComparer]::OrdinalIgnoreCase)
    if ($folded.Count -ne $paths.Length) {
        # Only to say which: the decision was made above.
        $first = [Collections.Generic.Dictionary[string, string]]::new([StringComparer]::OrdinalIgnoreCase)
        for ($i = 0; $i -lt $paths.Length; $i++) {
            if ($first.TryAdd($paths[$i], $names[$i])) { continue }
            $previous = $first[$paths[$i]]
            if ([string]::Equals($previous, $names[$i], [StringComparison]::Ordinal)) { throw "$Label has the entry '$previous' more than once" }
            throw "$Label has entries '$previous' and '$($names[$i])' that are the same path on Windows (they differ only by case or by being a file and a directory); unpacking would let one overwrite the other"
        }
    }

    # 4. The directories: those that entries are in, and the directory entries. Each one and each of its ancestors,
    #    once: one spelling only (`SERDE-1.0/x` next to `serde-1.0/y` is one directory on Windows), and not also a file.
    $dirs = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $dirEntries = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $parents = [regex]::Matches($pathText, '^.*(?=/[^/\n]*$)', $multiline).Value
    if ($null -ne $parents) { $dirs.UnionWith([string[]]$parents) }
    if ($hasDirectoryEntries) {
        $own = [regex]::Matches($text, '^.*(?=/$)', $multiline).Value
        if ($null -ne $own) { $dirEntries.UnionWith([string[]]$own); $dirs.UnionWith([string[]]$own) }
    }
    # $spelling: case-folded directory path -> the one spelling in use. A directory seen before in the same spelling
    # has had all its ancestors checked too, so the walk up stops there.
    $spelling = [Collections.Generic.Dictionary[string, string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($dir in $dirs) {
        $current = $dir
        while ($true) {
            if (-not $spelling.TryAdd($current, $current)) {
                $known = $spelling[$current]
                if ([string]::Equals($known, $current, [StringComparison]::Ordinal)) { break }
                throw "$Label uses the directory '$known' and '$current', which are the same directory on Windows (they differ only by case)"
            }
            if ($folded.Contains($current) -and -not $dirEntries.Contains($current)) {
                throw "$Label has a file named '$current' and also entries below '$current/'; a path cannot be a file and a directory"
            }
            $cut = $current.LastIndexOf($slash)
            if ($cut -lt 0) { break }
            $current = $current.Substring(0, $cut)
        }
    }
}

# Hard gate for every path that can produce something distributable (package-portable.ps1, package-artifacts.ps1,
# scripts/check-bundle-preconditions.ps1 for `tauri build`): a binary must never go out without its dependency
# sources (GPL-3.0-or-later; the dependency code is compiled or bundled into it). Throws unless the archive is
#   - named for this version and a readable ZIP with the layout scripts/package-dependency-sources.ps1 writes,
#   - made of canonical entry names, checked first and for the whole archive (Assert-ZipEntryNames): unique, and
#     unpacking on Windows gives back exactly the entries that are verified (no case aliases, no `\`, `..`, device names),
#   - built from THIS tree's Cargo.lock and package-lock.json (hashes in SOURCE-DEPS-MANIFEST.txt),
#   - complete, including the payload and not only the metadata:
#       Rust: exactly the crates of Cargo.lock are vendored; each crate's .cargo-checksum.json carries the checksum of
#             Cargo.lock, and EVERY file it lists (sources, build.rs, Cargo.toml, licenses, ...) is in the archive with
#             the listed SHA-256, so a deleted or altered source file is noticed;
#       npm:  every distributed package of package-lock.json is in the archive as the original registry tarball, and
#             the tarball's hash equals the `integrity` of package-lock.json (not a value the archive declares itself),
#             so the payload is bound to the upstream package.
# Threat model: it stops an incomplete, stale or mismatched archive from being shipped by accident (a truncated copy,
# an archive of another commit, files lost on the way). It does not defend against somebody who deliberately edits
# the files AND the .cargo-checksum.json that sits next to them: the `package` checksum there is the hash of the
# original .crate file, which `cargo vendor` unpacked and discarded, so it can only be compared with Cargo.lock, not
# recomputed from the files. The archive is made by scripts/package-dependency-sources.ps1 on the release machine.
# Returns Path, Sha256, Crates, RustFiles and NpmPackages. Reads the archive only; it needs neither cargo nor
# node_modules nor a network.
function Assert-DependencySourcesZip {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ProjectRoot,
        [Parameter(Mandatory = $true)][string]$Version
    )
    $label = 'Dependency source archive'
    if (!(Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$label missing: $Path (build it with scripts/package-dependency-sources.ps1)" }
    $expectedName = "Quicktext-$Version-Source-deps.zip"
    if ((Split-Path -Leaf $Path) -ne $expectedName) { throw "$label must be named $expectedName (built for version $Version), got $(Split-Path -Leaf $Path)" }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    try { $archive = [IO.Compression.ZipFile]::OpenRead($Path) } catch { throw "$label is not a readable ZIP file: $Path ($($_.Exception.Message))" }
    try {
        # Before anything is read or hashed: every check below looks entries up by exact name, so an alias that
        # unpacks onto a verified file (or a duplicate) would otherwise slip through unchecked.
        Assert-ZipEntryNames -Archive $archive -Label $label
        $stage = "Quicktext-$Version-Source-deps"
        # Case-sensitive lookup; after the check above no two entries differ only by case.
        $entries = [Collections.Generic.Dictionary[string, object]]::new([StringComparer]::Ordinal)
        foreach ($entry in $archive.Entries) { $entries[$entry.FullName] = $entry }
        foreach ($required in @('README-OFFLINE-BUILD.md', 'cargo-config.toml', 'SOURCE-DEPS-MANIFEST.txt', 'npm/PACKAGES.txt')) {
            if (-not $entries.ContainsKey("$stage/$required")) { throw "$label lacks $required" }
        }

        # Manifest: version and the lock files the archive was built from.
        $manifest = Read-ZipEntryText -Entry $entries["$stage/SOURCE-DEPS-MANIFEST.txt"]
        if ($manifest -notmatch "(?m)^Quicktext $([regex]::Escape($Version)) dependency sources\s*$") { throw "$label manifest is not for version $Version" }
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
        # Every file the crate's .cargo-checksum.json lists must be in the archive with the listed SHA-256. The hash is
        # computed straight from the ZIP entry stream (no extraction, no process per file).
        $rustFiles = 0
        foreach ($crate in $crates) {
            $directory = "$vendorPrefix$($crate.Name)-$($crate.Version)"
            $sums = $null
            if (-not $entries.TryGetValue("$directory/.cargo-checksum.json", [ref]$sums) -or -not $entries.ContainsKey("$directory/Cargo.toml")) {
                throw "$label lacks the source of crate $($crate.Name) $($crate.Version)"
            }
            $document = $null
            try { $document = [Text.Json.JsonDocument]::Parse((Read-ZipEntryText -Entry $sums)) } catch { throw "$label`: .cargo-checksum.json of crate $($crate.Name) $($crate.Version) is not valid JSON" }
            try {
                $package = $null
                $fileTable = $null
                foreach ($property in $document.RootElement.EnumerateObject()) {
                    if ($property.Name -ceq 'package') { $package = $property.Value.GetString() }
                    elseif ($property.Name -ceq 'files') { $fileTable = $property.Value }
                }
                if (-not $package -or $package -ne $crate.Checksum) { throw "$label`: crate $($crate.Name) $($crate.Version) does not match the checksum in Cargo.lock" }
                if ($null -eq $fileTable -or $fileTable.ValueKind -ne [Text.Json.JsonValueKind]::Object) { throw "$label`: .cargo-checksum.json of crate $($crate.Name) $($crate.Version) has no file table" }
                $listsCargoToml = $false
                foreach ($listed in $fileTable.EnumerateObject()) {
                    $zipEntry = $null
                    if (-not $entries.TryGetValue("$directory/$($listed.Name)", [ref]$zipEntry)) {
                        throw "$label`: crate $($crate.Name) $($crate.Version) lacks $($listed.Name) (listed in its .cargo-checksum.json; the source is incomplete)"
                    }
                    $stream = $zipEntry.Open()
                    try { $actual = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($stream)) } finally { $stream.Dispose() }
                    if (-not [string]::Equals($actual, $listed.Value.GetString(), [StringComparison]::OrdinalIgnoreCase)) {
                        throw "$label`: $($listed.Name) of crate $($crate.Name) $($crate.Version) differs from its .cargo-checksum.json (damaged or edited)"
                    }
                    if ($listed.Name -ceq 'Cargo.toml') { $listsCargoToml = $true }
                    $rustFiles++
                }
                if (-not $listsCargoToml) { throw "$label`: .cargo-checksum.json of crate $($crate.Name) $($crate.Version) does not list Cargo.toml" }
            } finally { $document.Dispose() }
        }

        # npm: every distributed package of package-lock.json is in the archive as the registry tarball, and the
        # tarball has the integrity that package-lock.json (this tree, not the archive) asks for.
        $tarballs = @(Get-DistributedNpmTarballs -ProjectRoot $ProjectRoot)
        if ($tarballs.Count -lt 1) { throw 'package-lock.json lists no distributed npm packages' }
        $declaredNpm = [regex]::Match($manifest, '(?m)^npm packages: (\d+)\s*$')
        if (-not $declaredNpm.Success -or [int]$declaredNpm.Groups[1].Value -ne $tarballs.Count) { throw "$label manifest does not declare the $($tarballs.Count) npm packages of package-lock.json" }
        $listedLines = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($line in ((Read-ZipEntryText -Entry $entries["$stage/npm/PACKAGES.txt"]) -split "\r?\n")) {
            if ($line -and -not $line.StartsWith('#', [StringComparison]::Ordinal)) { [void]$listedLines.Add($line) }
        }
        if ($listedLines.Count -ne $tarballs.Count) { throw "$label`: npm/PACKAGES.txt lists $($listedLines.Count) packages but package-lock.json distributes $($tarballs.Count)" }
        foreach ($tarball in $tarballs) {
            $expectedLine = Get-NpmPackagesLine -Tarball $tarball
            if (-not $listedLines.Contains($expectedLine)) { throw "$label`: npm/PACKAGES.txt has no line for $($tarball.Name) $($tarball.Version) (expected '$expectedLine')" }
            $zipEntry = $null
            if (-not $entries.TryGetValue("$stage/npm/tarballs/$($tarball.FileName)", [ref]$zipEntry)) {
                throw "$label lacks the npm tarball of $($tarball.Name) $($tarball.Version) (npm/tarballs/$($tarball.FileName))"
            }
            $stream = $zipEntry.Open()
            try { $intact = Test-NpmIntegrity -Integrity $tarball.Integrity -Stream $stream } finally { $stream.Dispose() }
            if (-not $intact) { throw "$label`: npm tarball of $($tarball.Name) $($tarball.Version) does not match the integrity in package-lock.json (damaged, edited or another build)" }
        }
    } finally { $archive.Dispose() }
    return [pscustomobject]@{
        Path        = $Path
        Sha256      = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
        Crates      = $crates.Count
        RustFiles   = $rustFiles
        NpmPackages = $tarballs.Count
    }
}
