# Generates THIRD-PARTY-LICENSES.txt: the copyright notices and license texts of every third-party
# package that is compiled or bundled into a release (GPL-3.0-or-later project; MIT, BSD, Apache-2.0
# and MPL-2.0 dependencies require their notices to travel with the binaries; S-28, B-51).
#
#   pwsh scripts/generate-third-party-licenses.ps1                       # writes the file the installer bundles
#   pwsh scripts/generate-third-party-licenses.ps1 -OutputFile <path>    # elsewhere
#   pwsh scripts/generate-third-party-licenses.ps1 -CheckOnly            # completeness check, writes nothing
#   pwsh scripts/generate-third-party-licenses.ps1 -CheckOnly -NoClarifications
#                                                                        # how many packages ship no license file at all
#
# Package sets: scripts/third-party-set.ps1 (cargo metadata + package-lock.json). Texts come from the
# published source of each package (LICENSE*, COPYING*, NOTICE*, ...; cargo registry sources and
# node_modules, so run `cargo fetch --locked` and `npm ci` first). A package whose published source
# has no license file needs a pinned entry in scripts/third-party-license-clarifications.json.
#
# Fails (throws, exit code 1 with -File; nothing written) when any package has no usable license text, so an incomplete
# notice file can never be packaged. No tool beyond cargo, PowerShell 7 and npm's node_modules is needed.
param(
    [string]$OutputFile,
    [string]$ProjectRoot = (Split-Path -Parent $PSScriptRoot),
    [switch]$CheckOnly,
    [switch]$NoClarifications
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'third-party-set.ps1')

$ProjectRoot = [IO.Path]::GetFullPath($ProjectRoot)
$utf8 = [Text.UTF8Encoding]::new($false)
$utf8Strict = [Text.UTF8Encoding]::new($false, $true)
$version = (Get-Content -LiteralPath (Join-Path $ProjectRoot 'package.json') -Raw | ConvertFrom-Json).version
if (-not $CheckOnly -and -not $OutputFile) { $OutputFile = Join-Path $ProjectRoot 'src-tauri/resources/THIRD-PARTY-LICENSES.txt' }

function Read-LicenseText {
    param([string]$Path)
    $bytes = [IO.File]::ReadAllBytes($Path)
    try { $text = $utf8Strict.GetString($bytes) } catch { $text = [Text.Encoding]::Latin1.GetString($bytes) }
    $text = $text.TrimStart([char]0xFEFF) -replace "`r`n", "`n" -replace "`r", "`n"
    $text = [regex]::Replace($text, '^(\s*\n)+', '')
    return $text.TrimEnd()
}

function Get-TextKind {
    param([string]$FileName)
    $upper = $FileName.ToUpperInvariant()
    if ($upper.StartsWith('NOTICE')) { return 'NOTICE' }
    if ($upper.StartsWith('COPYRIGHT')) { return 'COPYRIGHT' }
    return 'LICENSE'
}

function Get-Sha256 {
    param([string]$Text)
    return [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($utf8.GetBytes($Text)))
}

function Compare-Ordinal {
    param([string]$Left, [string]$Right)
    return [string]::CompareOrdinal($Left, $Right)
}

# --- inputs -----------------------------------------------------------------------------------
$cargoPackages = @(Get-DistributedCargoPackages -ProjectRoot $ProjectRoot)
$npmPackages = @(Get-DistributedNpmPackages -ProjectRoot $ProjectRoot)
$packages = [Collections.Generic.List[object]]::new()
foreach ($p in $cargoPackages) { $packages.Add($p) }
foreach ($p in $npmPackages) { $packages.Add($p) }
$packages.Sort([Comparison[object]] {
    param($a, $b)
    $r = Compare-Ordinal $b.Ecosystem $a.Ecosystem   # rust before npm
    if ($r -eq 0) { $r = Compare-Ordinal $a.Name $b.Name }
    if ($r -eq 0) { $r = Compare-Ordinal $a.Version $b.Version }
    return $r
})

$clarifications = @()
$clarificationFile = Join-Path $PSScriptRoot 'third-party-license-clarifications.json'
if (-not $NoClarifications) {
    $clarifications = @((Get-Content -LiteralPath $clarificationFile -Raw -Encoding utf8 | ConvertFrom-Json).entries)
}
$clarificationUsed = @{}

function Find-Clarification {
    param([string]$Name, [string]$Version)
    $index = 0
    foreach ($entry in $clarifications) {
        foreach ($target in @($entry.packages)) {
            if ($target.name -eq $Name -and $target.version -eq $Version) { return @{ Index = $index; Entry = $entry } }
        }
        $index++
    }
    return $null
}

# --- collect texts ----------------------------------------------------------------------------
$problems = [Collections.Generic.List[string]]::new()
$records = [Collections.Generic.List[object]]::new()
$ownCount = 0
$clarifiedCount = 0
foreach ($package in $packages) {
    $label = "$($package.Ecosystem) $($package.Name) $($package.Version)"
    $texts = [Collections.Generic.List[object]]::new()
    $files = [Collections.Generic.List[object]]::new()
    foreach ($f in (Get-ThirdPartyLicenseFiles -Directory $package.Directory)) { $files.Add($f) }
    if ($package.LicenseFileField) {
        $declared = Join-Path $package.Directory $package.LicenseFileField
        if ((Test-Path -LiteralPath $declared -PathType Leaf) -and -not ($files | Where-Object { $_.FullName -eq (Get-Item -LiteralPath $declared).FullName })) {
            $files.Add((Get-Item -LiteralPath $declared))
        }
    }
    foreach ($f in $files) { $texts.Add(@{ Kind = (Get-TextKind $f.Name); Text = (Read-LicenseText $f.FullName) }) }

    $clarified = $null
    $match = Find-Clarification -Name $package.Name -Version $package.Version
    if ($match) {
        if ($texts.Count -gt 0) {
            Write-Warning "Clarification for $label is redundant: the package ships its own license files; ignoring the clarification"
        } else {
            $clarificationUsed[$match.Index] = $true
            $entry = $match.Entry
            $origin = ''
            if ($entry.PSObject.Properties.Name -contains 'texts') {
                foreach ($t in @($entry.texts)) {
                    $path = Join-Path $PSScriptRoot $t.file
                    if (!(Test-Path -LiteralPath $path -PathType Leaf)) { $problems.Add("$label : clarification text file missing: scripts/$($t.file)"); continue }
                    $texts.Add(@{ Kind = (Get-TextKind (Split-Path -Leaf $t.file)); Text = (Read-LicenseText $path) })
                    $origin += "$($t.source) "
                }
            }
            if ($entry.PSObject.Properties.Name -contains 'fromPackage') {
                $sibling = $packages | Where-Object { $_.Name -eq $entry.fromPackage.name -and $_.Version -eq $entry.fromPackage.version } | Select-Object -First 1
                if (-not $sibling) { $problems.Add("$label : clarification refers to $($entry.fromPackage.name) $($entry.fromPackage.version), which is not a distributed package") }
                else {
                    foreach ($name in @($entry.fromPackage.files)) {
                        $path = Join-Path $sibling.Directory $name
                        if (!(Test-Path -LiteralPath $path -PathType Leaf)) { $problems.Add("$label : $($entry.fromPackage.name) $($entry.fromPackage.version) has no file $name"); continue }
                        $texts.Add(@{ Kind = (Get-TextKind $name); Text = (Read-LicenseText $path) })
                    }
                    $origin += "license files of $($entry.fromPackage.name) $($entry.fromPackage.version)"
                }
            }
            $clarified = $origin.Trim()
            $clarifiedCount++
        }
    } elseif ($texts.Count -gt 0) { $ownCount++ }

    if ($texts.Count -eq 0) {
        $problems.Add("$label ($(if ($package.License) { $package.License } else { 'no license declared' })) : no license text in the published package and no pinned clarification [$($package.Directory)]")
        continue
    }
    $usable = 0
    foreach ($t in $texts) { if (($t.Text -replace '\s', '').Length -ge 50) { $usable++ } }
    if ($usable -eq 0) { $problems.Add("$label : license files exist but contain no usable text (under 50 characters)"); continue }
    if (-not $package.License -and -not $package.LicenseFileField) { $problems.Add("$label : the package declares neither a license expression nor a license file") }
    $records.Add([pscustomobject]@{ Package = $package; Texts = @($texts); Clarified = $clarified })
}
for ($i = 0; $i -lt $clarifications.Count; $i++) {
    if (-not $clarificationUsed.ContainsKey($i)) {
        $names = (@($clarifications[$i].packages) | ForEach-Object { "$($_.name) $($_.version)" }) -join ', '
        Write-Warning "Clarification entry not used (package not distributed, version changed or now ships its own license): $names"
    }
}

Write-Output "Rust crates: $($cargoPackages.Count), npm packages: $($npmPackages.Count)"
Write-Output "License text from the package itself: $ownCount; from pinned clarifications: $clarifiedCount; without license text: $($problems.Count)"
if ($problems.Count -gt 0) {
    foreach ($p in $problems) { [Console]::Error.WriteLine("MISSING  $p") }
    [Console]::Error.WriteLine("Notice file incomplete. Add a pinned entry (verbatim upstream text plus its source) to scripts/third-party-license-clarifications.json, or drop the dependency.")
    throw "Third-party notice file is incomplete: $($problems.Count) package(s) without license text"
}
if ($CheckOnly) { Write-Output 'Check only: complete, nothing written'; return }

# --- compose ----------------------------------------------------------------------------------
$groups = [ordered]@{}
foreach ($record in $records) {
    $key = ($record.Texts | ForEach-Object { "$($_.Kind):$(Get-Sha256 $_.Text)" }) -join '|'
    if (-not $groups.Contains($key)) { $groups[$key] = [Collections.Generic.List[object]]::new() }
    $groups[$key].Add($record)
}
$orderedGroups = [Collections.Generic.List[object]]::new()
foreach ($key in $groups.Keys) { $orderedGroups.Add($groups[$key]) }
$orderedGroups.Sort([Comparison[object]] {
    param($a, $b)
    $r = Compare-Ordinal $a[0].Package.Name $b[0].Package.Name
    if ($r -eq 0) { $r = Compare-Ordinal $a[0].Package.Version $b[0].Package.Version }
    return $r
})

$cargoLock = Get-ThirdPartyLockfileSha256 -Path (Join-Path $ProjectRoot 'Cargo.lock')
$npmLock = Get-ThirdPartyLockfileSha256 -Path (Join-Path $ProjectRoot 'package-lock.json')
$sb = [Text.StringBuilder]::new()
$rule = '=' * 78
$thin = '-' * 78
[void]$sb.Append("Quicker Text $version - third-party licenses`n$rule`n`n")
[void]$sb.Append("Quicker Text itself is licensed GPL-3.0-or-later (see the GPL-3.0 text shipped with this`n")
[void]$sb.Append("release). This file reproduces the copyright notices and license texts of the third-party`n")
[void]$sb.Append("packages that are compiled or bundled into the release artifacts. Other notices: the`n")
[void]$sb.Append("THIRD_PARTY_NOTICES file shipped with this release.`n`n")
[void]$sb.Append("Generated by scripts/generate-third-party-licenses.ps1.`n")
[void]$sb.Append("  Rust: $($cargoPackages.Count) crates. Normal and build dependencies of the quicker-text application crate for`n")
[void]$sb.Append("        $ThirdPartyCargoTarget (cargo metadata --locked), Cargo.lock SHA-256 $cargoLock`n")
[void]$sb.Append("  npm:  $($npmPackages.Count) packages. Production dependencies from package-lock.json plus the build tool code the`n")
[void]$sb.Append("        production bundle embeds ($($ThirdPartyNpmExtras -join ', ')), package-lock.json SHA-256 $npmLock`n")
[void]$sb.Append("Texts are copied from the files in each package's published source (LICENSE*, COPYING*, NOTICE*, ...).`n")
[void]$sb.Append("Identical texts are printed once, followed by every package that uses them. Packages marked`n")
[void]$sb.Append("'clarified' publish no license file; their text is the upstream project's, pinned in`n")
[void]$sb.Append("scripts/third-party-license-clarifications.json.`n")
[void]$sb.Append("The source code of these packages is published with each release as Source-deps.zip.`n`n")
[void]$sb.Append("$rule`n1. Package index`n$rule`n`n")
foreach ($package in $packages) {
    $license = if ($package.License) { $package.License } else { "see file $($package.LicenseFileField)" }
    [void]$sb.Append(("{0,-5} {1} {2}  [{3}]`n" -f $package.Ecosystem, $package.Name, $package.Version, $license))
}
[void]$sb.Append("`n$rule`n2. License texts ($($orderedGroups.Count) distinct)`n$rule`n")
$number = 0
foreach ($group in $orderedGroups) {
    $number++
    [void]$sb.Append("`n$thin`nText set $number of $($orderedGroups.Count). Used by:`n")
    foreach ($record in $group) {
        $p = $record.Package
        $license = if ($p.License) { $p.License } else { "see file $($p.LicenseFileField)" }
        [void]$sb.Append("  * $($p.Ecosystem) $($p.Name) $($p.Version)  [$license]`n")
        if ($p.Authors.Count -gt 0) { [void]$sb.Append("      authors: $($p.Authors -join '; ')`n") }
        if ($p.Repository) { [void]$sb.Append("      repository: $($p.Repository)`n") }
        if ($record.Clarified) { [void]$sb.Append("      clarified, text from: $($record.Clarified)`n") }
    }
    [void]$sb.Append("$thin`n")
    $first = $group[0]
    $textNumber = 0
    foreach ($t in $first.Texts) {
        $textNumber++
        [void]$sb.Append("`n[$($t.Kind) $textNumber of $($first.Texts.Count)]`n$($t.Text)`n")
    }
}
$output = [IO.Path]::GetFullPath($OutputFile)
New-Item -ItemType Directory -Path (Split-Path -Parent $output) -Force | Out-Null
[IO.File]::WriteAllText($output, $sb.ToString(), $utf8)
$sha = (Get-FileHash -LiteralPath $output -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Output "Wrote $output ($((Get-Item -LiteralPath $output).Length) bytes, $($orderedGroups.Count) distinct text sets, SHA-256 $sha)"
