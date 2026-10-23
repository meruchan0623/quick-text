#requires -Version 7.0
<#
.SYNOPSIS
  FE-18 / B-52 gate for packaging (Codex review 1): the frontend scanned for the mock backend is
  the frontend the exe embeds, then the scan.

.DESCRIPTION
  tauri::generate_context! compresses build.frontendDist (../dist) into the exe at compile time,
  so the exe itself cannot be scanned for the mock, and the packaging scripts take an exe that was
  built earlier: the dist/ in the tree may have been rebuilt since (for example with VITE_MOCK=1
  or NODE_ENV=development), or the exe built from another one. Scanning that dist/ would prove
  nothing about the exe.

  src-tauri/build.rs records the digest of the directory the same build embedded, as the string
  `quicker-text-frontend-dist-sha256:<64 hex>` in the exe (src-tauri/src/main.rs). This script:
    1. reads that record from the exe; none, `unavailable`, `dev-mode`, or two different values
       fail. `dev-mode` is what build.rs records in Tauri's development mode (built without the
       custom-protocol feature, e.g. --no-default-features): such an exe embeds no frontend and
       loads build.devUrl, so no directory can stand for it;
    2. computes the digest of -Dist the same way and requires it to be equal; otherwise the exe
       embeds another frontend and has to be rebuilt from this one (or this one restored);
    3. runs `node scripts/check-bundle.mjs --scan-only <Dist>` on that directory: no mock chunk,
       no marker string from src/ipc/mock.ts, with its own positive control (a build with the
       mock in it, written to a temporary directory, must be detected).

  Digest (build.rs computes the same thing): for every file under the directory, the line
  `<SHA-256 of the file, lowercase hex>  <path relative to the directory, '/' separators>` and a
  line feed; the lines sorted by path (ordinal, UTF-16 code units); the digest is the SHA-256 of
  the concatenated lines (UTF-8), lowercase hex. Hidden files count. A link anywhere in the
  directory fails, on both sides (Vite writes none; Tauri would follow it). The digest is over
  the files on disk: Tauri adds CSP hashes to HTML as it embeds it.

  Called by scripts/release-binary-checks.ps1 (Assert-ReleaseBinaries) from
  scripts/package-artifacts.ps1 and scripts/package-portable.ps1 before they write anything. Run
  it on its own to check a build without packaging it.

  -SelfTest checks steps 1 and 2 on synthetic files (known digest vector, empty directory, order,
  content, rename, added and hidden files, link, missing, `unavailable`, `dev-mode`, duplicate and
  conflicting records) and that a failing step 3 fails the script (needs node). It prints only failures and a
  summary line; add -Verbose for one line per case.

.PARAMETER Exe
  The exe about to be packaged. Default: target\release\quicker-text.exe.

.PARAMETER Dist
  The frontend directory to scan. Default: dist\ (build.frontendDist).

.PARAMETER SelfTest
  Run the built-in tests and exit.

.EXAMPLE
  ./scripts/check-embedded-frontend.ps1

.EXAMPLE
  ./scripts/check-embedded-frontend.ps1 -SelfTest

.NOTES
  Exit code 0 = pass, 1 = fail.
#>
[CmdletBinding(DefaultParameterSetName = 'Check')]
param(
    [Parameter(ParameterSetName = 'Check')]
    [string] $Exe = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\quicker-text.exe'),
    [Parameter(ParameterSetName = 'Check')]
    [string] $Dist = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist'),
    [Parameter(ParameterSetName = 'SelfTest', Mandatory = $true)]
    [switch] $SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RecordPrefix = 'quicker-text-frontend-dist-sha256:'

function ConvertTo-Hex([byte[]] $Bytes) { [Convert]::ToHexString($Bytes).ToLowerInvariant() }

function Get-FrontendDistDigest {
    <# Digest of a frontend directory as defined above, and the number of files. Throws when it cannot be computed. #>
    param([Parameter(Mandatory = $true)][string] $Path)
    if (!(Test-Path -LiteralPath $Path -PathType Container)) { throw "frontend directory not found: $Path" }
    $root = (Get-Item -LiteralPath $Path).FullName
    $options = [IO.EnumerationOptions]::new()
    $options.RecurseSubdirectories = $true
    $options.AttributesToSkip = [IO.FileAttributes]0   # hidden and system files count
    $options.IgnoreInaccessible = $false
    $files = [Collections.Generic.List[object]]::new()
    foreach ($item in [IO.DirectoryInfo]::new($root).EnumerateFileSystemInfos('*', $options)) {
        $relative = [IO.Path]::GetRelativePath($root, $item.FullName).Replace('\', '/')
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "$relative in $root is a link" }
        if ($item -is [IO.FileInfo]) { $files.Add([pscustomobject]@{ Relative = $relative; FullName = $item.FullName }) }
    }
    $files.Sort([Comparison[object]] { param($a, $b) [string]::CompareOrdinal($a.Relative, $b.Relative) })
    $listing = [Text.StringBuilder]::new()
    foreach ($file in $files) {
        $hash = ConvertTo-Hex ([Security.Cryptography.SHA256]::HashData([IO.File]::ReadAllBytes($file.FullName)))
        [void]$listing.Append($hash).Append('  ').Append($file.Relative).Append("`n")
    }
    $digest = ConvertTo-Hex ([Security.Cryptography.SHA256]::HashData([Text.UTF8Encoding]::new($false).GetBytes($listing.ToString())))
    return [pscustomobject]@{ Digest = $digest; Files = $files.Count }
}

function Get-EmbeddedFrontendDigest {
    <# The digest build.rs recorded in the exe. Throws when there is no usable record or more than one value. #>
    param([Parameter(Mandatory = $true)][string] $Path)
    if (!(Test-Path -LiteralPath $Path -PathType Leaf)) { throw "exe not found: $Path" }
    $text = [Text.Encoding]::Latin1.GetString([IO.File]::ReadAllBytes($Path))
    $values = @([regex]::Matches($text, [regex]::Escape($RecordPrefix) + '(?:[0-9a-f]{64}|unavailable|dev-mode)') |
        ForEach-Object { $_.Value.Substring($RecordPrefix.Length) } | Sort-Object -Unique)
    $name = [IO.Path]::GetFileName($Path)
    if ($values.Count -eq 0) { throw "$name has no frontend record ('$RecordPrefix...'): built before the record existed, or not by src-tauri/build.rs; rebuild it" }
    if ($values.Count -gt 1) { throw "$name has $($values.Count) different frontend records ($($values -join ', ')); cannot tell which frontend it embeds" }
    if ($values[0] -eq 'dev-mode') { throw "$name is a Tauri development-mode build and contains no frontend: it was compiled without the custom-protocol feature (for example with --no-default-features) and loads the dev server (build.devUrl) at run time; rebuild it with default features (cargo build -p quicker-text --release)" }
    if ($values[0] -eq 'unavailable') { throw "$name records no frontend digest (build.rs could not hash the frontend directory; see its cargo warning); rebuild it" }
    return $values[0]
}

function Assert-EmbeddedFrontendDigest {
    <# Steps 1 and 2: throws unless the exe records the digest of $Dist. #>
    param([Parameter(Mandatory = $true)][string] $ExePath, [Parameter(Mandatory = $true)][string] $DistPath)
    $recorded = Get-EmbeddedFrontendDigest -Path $ExePath
    $actual = Get-FrontendDistDigest -Path $DistPath
    $name = [IO.Path]::GetFileName($ExePath)
    if ($recorded -ne $actual.Digest) {
        throw "$name embeds another frontend than $DistPath (recorded $recorded, directory $($actual.Digest)); rebuild the exe from this directory (npm run build, then cargo build -p quicker-text --release), or restore the directory it was built from"
    }
    Write-Host "ok:   $name embeds the frontend in $DistPath ($($actual.Files) files, sha256 $recorded)"
}

function Invoke-BundleScan {
    <# Step 3: check-bundle.mjs --scan-only on $DistPath. Throws on a non-zero exit code. #>
    param([Parameter(Mandatory = $true)][string] $DistPath, [switch] $Quiet)
    $node = Get-Command node -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $node) { throw 'node not found: the frontend scan (scripts/check-bundle.mjs) needs Node.js' }
    $full = (Get-Item -LiteralPath $DistPath).FullName
    $script = Join-Path $PSScriptRoot 'check-bundle.mjs'
    if ($Quiet) { & $node.Source $script --scan-only $full *> $null } else { & $node.Source $script --scan-only $full | Out-Host }
    $code = $LASTEXITCODE
    if ($code -ne 0) { throw "frontend scan failed (exit code $code): node scripts/check-bundle.mjs --scan-only $full" }
}

if ($SelfTest) {
    $dir = Join-Path ([IO.Path]::GetTempPath()) ('check-embedded-frontend-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $dir | Out-Null
    $script:failures = 0
    $script:cases = 0
    function Report([string] $Name, [bool] $Ok, [string] $Detail) {
        $script:cases++
        if (-not $Ok) { Write-Host "SELFTEST FAIL: $Name ($Detail)"; $script:failures++ }
        elseif ($VerbosePreference -eq 'Continue') { Write-Host "selftest ok: $Name" }
    }
    function Assert-Throws([string] $Name, [scriptblock] $Action, [string] $Pattern) {
        try { & $Action 6> $null; Report $Name $false 'expected a failure, got none' }
        catch { Report $Name ($_.Exception.Message -match $Pattern) "failed with an unexpected message: $($_.Exception.Message)" }
    }
    function Assert-Passes([string] $Name, [scriptblock] $Action) {
        try { & $Action 6> $null; Report $Name $true '' } catch { Report $Name $false "unexpected failure: $($_.Exception.Message)" }
    }
    $utf8 = [Text.UTF8Encoding]::new($false)
    function New-Dist([string] $Name, [Collections.IDictionary] $Files) {
        $path = Join-Path $dir $Name
        New-Item -ItemType Directory -Path $path | Out-Null
        foreach ($relative in $Files.Keys) {
            $file = Join-Path $path $relative
            New-Item -ItemType Directory -Path (Split-Path -Parent $file) -Force | Out-Null
            [IO.File]::WriteAllBytes($file, $utf8.GetBytes($Files[$relative]))
        }
        return $path
    }
    $rng = [Random]::new(1)
    function New-Exe([string] $Name, [string[]] $Records) {
        # Random bytes without printable ASCII around each record, like the .rdata of a real exe.
        function Noise([int] $Length) { $b = [byte[]]::new($Length); $rng.NextBytes($b); for ($i = 0; $i -lt $Length; $i++) { if ($b[$i] -ge 0x20 -and $b[$i] -le 0x7E) { $b[$i] = 0xFF } }; return $b }
        $bytes = [Collections.Generic.List[byte]]::new()
        $bytes.AddRange([byte[]](Noise 300))
        foreach ($record in $Records) { $bytes.AddRange([Text.Encoding]::ASCII.GetBytes($record)); $bytes.AddRange([byte[]](Noise 200)) }
        $path = Join-Path $dir $Name
        [IO.File]::WriteAllBytes($path, $bytes.ToArray())
        return $path
    }
    try {
        $sample = @{ 'index.html' = '<!doctype html><title>t</title>'; 'assets/app.js' = 'console.log(1)'; 'assets/app.css' = 'a{}' }
        $base = New-Dist 'base' $sample
        # Expected value computed independently with coreutils: sha256sum of the three lines
        #   5f546eb4...8419  assets/app.css / 0a286891...c393  assets/app.js / d3d389ab...921c  index.html
        $digest = (Get-FrontendDistDigest -Path $base).Digest
        Report 'known digest vector' ($digest -eq '45cf95364c351a6495a9fc09f18c4a639ac1e41f055b9e2120815060a99d4aeb') "got $digest"
        $empty = New-Dist 'empty' @{}
        Report 'empty directory is the SHA-256 of an empty listing' ((Get-FrontendDistDigest -Path $empty).Digest -eq 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855') 'wrong digest'
        $reordered = New-Dist 'reordered' ([ordered]@{ 'assets/app.css' = 'a{}'; 'index.html' = '<!doctype html><title>t</title>'; 'assets/app.js' = 'console.log(1)' })
        Report 'creation order does not matter' ((Get-FrontendDistDigest -Path $reordered).Digest -eq $digest) 'digest differs'
        foreach ($variant in @(
                @{ Name = 'one byte changed'; Files = @{ 'index.html' = '<!doctype html><title>T</title>'; 'assets/app.js' = 'console.log(1)'; 'assets/app.css' = 'a{}' } },
                @{ Name = 'file renamed'; Files = @{ 'index.html' = '<!doctype html><title>t</title>'; 'assets/app2.js' = 'console.log(1)'; 'assets/app.css' = 'a{}' } },
                @{ Name = 'file moved to another directory'; Files = @{ 'index.html' = '<!doctype html><title>t</title>'; 'app.js' = 'console.log(1)'; 'assets/app.css' = 'a{}' } },
                @{ Name = 'file added'; Files = $sample + @{ 'assets/mock-1.js' = 'x' } }
            )) {
            $path = New-Dist ('v-' + [Guid]::NewGuid().ToString('N')) $variant.Files
            Report "digest changes: $($variant.Name)" ((Get-FrontendDistDigest -Path $path).Digest -ne $digest) 'digest unchanged'
        }
        $hidden = New-Dist 'hidden' $sample
        $hiddenFile = Join-Path $hidden 'assets/.extra.js'
        [IO.File]::WriteAllText($hiddenFile, 'x')
        (Get-Item -LiteralPath $hiddenFile -Force).Attributes = 'Hidden'
        Report 'digest changes: hidden file added' ((Get-FrontendDistDigest -Path $hidden).Digest -ne $digest) 'hidden file ignored'
        $linked = New-Dist 'linked' $sample
        New-Item -ItemType Junction -Path (Join-Path $linked 'more') -Target $empty | Out-Null
        Assert-Throws 'a link in the directory fails' { Get-FrontendDistDigest -Path $linked } 'is a link'
        Assert-Throws 'missing directory fails' { Get-FrontendDistDigest -Path (Join-Path $dir 'nope') } 'not found'

        $good = New-Exe 'good.exe' @("$RecordPrefix$digest")
        Assert-Passes 'exe recording the directory passes' { Assert-EmbeddedFrontendDigest -ExePath $good -DistPath $base }
        $twice = New-Exe 'twice.exe' @("$RecordPrefix$digest", "$RecordPrefix$digest")
        Assert-Passes 'the same record twice passes' { Assert-EmbeddedFrontendDigest -ExePath $twice -DistPath $base }
        $changed = New-Dist 'changed' @{ 'index.html' = '<!doctype html><title>t</title>'; 'assets/app.js' = 'console.log("quickerMock")'; 'assets/app.css' = 'a{}' }
        Assert-Throws 'directory rebuilt after the exe fails' { Assert-EmbeddedFrontendDigest -ExePath $good -DistPath $changed } 'embeds another frontend'
        $none = New-Exe 'none.exe' @()
        Assert-Throws 'exe without a record fails' { Assert-EmbeddedFrontendDigest -ExePath $none -DistPath $base } 'has no frontend record'
        $unavailable = New-Exe 'unavailable.exe' @("${RecordPrefix}unavailable")
        Assert-Throws 'exe recording "unavailable" fails' { Assert-EmbeddedFrontendDigest -ExePath $unavailable -DistPath $base } 'records no frontend digest'
        $devMode = New-Exe 'dev-mode.exe' @("${RecordPrefix}dev-mode")
        Assert-Throws 'exe recording "dev-mode" (Tauri development mode) fails' { Assert-EmbeddedFrontendDigest -ExePath $devMode -DistPath $base } 'Tauri development-mode build and contains no frontend'
        $devAndDigest = New-Exe 'dev-and-digest.exe' @("$RecordPrefix$digest", "${RecordPrefix}dev-mode")
        Assert-Throws 'exe recording both "dev-mode" and a digest fails' { Assert-EmbeddedFrontendDigest -ExePath $devAndDigest -DistPath $base } 'different frontend records'
        $other = (Get-FrontendDistDigest -Path $changed).Digest
        $two = New-Exe 'two.exe' @("$RecordPrefix$digest", "$RecordPrefix$other")
        Assert-Throws 'exe with two different records fails' { Assert-EmbeddedFrontendDigest -ExePath $two -DistPath $base } 'different frontend records'
        $short = New-Exe 'short.exe' @("$RecordPrefix$($digest.Substring(0, 63))")
        Assert-Throws 'truncated record does not count' { Assert-EmbeddedFrontendDigest -ExePath $short -DistPath $base } 'has no frontend record'
        Assert-Throws 'missing exe fails' { Assert-EmbeddedFrontendDigest -ExePath (Join-Path $dir 'nope.exe') -DistPath $base } 'exe not found'
        # Step 3 is wired: check-bundle.mjs rejects a directory without index.html before its control build.
        $noIndex = New-Dist 'no-index' @{ 'assets/app.js' = 'console.log(1)' }
        Assert-Throws 'a failing frontend scan fails' { Invoke-BundleScan -DistPath $noIndex -Quiet } 'frontend scan failed'
    } finally { Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue }
    if ($script:failures -gt 0) { Write-Host "$($script:failures) of $($script:cases) self-test case(s) failed"; exit 1 }
    Write-Host "self-test passed ($($script:cases) cases)"
    exit 0
}

try {
    Assert-EmbeddedFrontendDigest -ExePath $Exe -DistPath $Dist
    Invoke-BundleScan -DistPath $Dist
} catch {
    Write-Host "FAIL: $($_.Exception.Message)"
    exit 1
}
exit 0
