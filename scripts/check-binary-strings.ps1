#requires -Version 7.0
<#
.SYNOPSIS
  B-52 gate: search the archived PDB (and the exe) of a release build for strings that must
  not be there, such as the fault-injection function name `debug_inject_fault`.

.DESCRIPTION
  A raw byte scan; no debugger or PDB library is needed. Every file is read in chunks and
  searched for each string in two encodings, ASCII and UTF-16LE (the Latin-1 view of the
  bytes is searched with an ordinal IndexOf, so the scan runs at memory speed; a chunk
  overlap makes sure a match across a chunk border is found).

  Where the names live:
    * PDB (MSF container): Rust symbols are stored as plain ASCII in the public-symbol and
      procedure-symbol records (`_ZN...debug_inject_fault...` or the demangled path), so a
      function that was linked into the exe is found here. A PDB stores each stream in
      4 KiB blocks that need not be adjacent, so a name that straddles a block border could
      be missed; a name is present in several records (public symbol, procedure symbol,
      line information), all of which would have to straddle a border at once.
    * exe: the release exe is built with strip = "none" and debug = "line-tables-only", so
      its symbols are in the PDB, not in the exe (plan: "checking a stripped exe is
      pointless", section 11 build configuration). Names that are string data in the exe,
      such as a Tauri command name registered with generate_handler!, are found there.

  -RequirePresent is the positive control. A scan that finds nothing in an empty, truncated
  or differently encoded file would prove nothing, so each file must contain the given
  strings, for example `quicker_platform` in the PDB and the app identifier
  `com.quickertext.desktop` in the exe.

  -SelfTest checks the scanner itself on synthetic files (ASCII and UTF-16LE hits, a hit
  across a chunk border at every alignment, absent string, near miss, missing positive control,
  empty file). The release workflow runs it first. It prints only failures and a summary line;
  add -Verbose for one line per case.

.PARAMETER Path
  Files to scan (the PDB, the exe).

.PARAMETER Forbidden
  Strings that must not occur. Default: debug_inject_fault (B-52). ASCII only.

.PARAMETER RequirePresent
  Strings that must occur in every scanned file (positive control). ASCII only.

.PARAMETER ChunkBytes
  Read size. Default 4 MiB. Tests lower it to exercise the chunk border.

.PARAMETER SelfTest
  Run the built-in tests and exit.

.EXAMPLE
  ./scripts/check-binary-strings.ps1 target/release/quicker_text.pdb -RequirePresent quicker_platform

.EXAMPLE
  ./scripts/check-binary-strings.ps1 target/release/quicker-text.exe -RequirePresent com.quickertext.desktop

.NOTES
  Exit code 0 = pass, 1 = a forbidden string was found or a positive control failed.
#>
[CmdletBinding(DefaultParameterSetName = 'Scan')]
param(
    [Parameter(ParameterSetName = 'Scan', Mandatory = $true, Position = 0)]
    [string[]] $Path,
    [Parameter(ParameterSetName = 'Scan')]
    [string[]] $Forbidden = @('debug_inject_fault'),
    [Parameter(ParameterSetName = 'Scan')]
    [string[]] $RequirePresent = @(),
    [int] $ChunkBytes = 4MB,
    [Parameter(ParameterSetName = 'SelfTest', Mandatory = $true)]
    [switch] $SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Find-Strings {
    <# Returns @{ "<needle>|<encoding>" = first byte offset or -1 } for every needle/encoding pair. #>
    param([string] $FilePath, [string[]] $Needles, [int] $ChunkSize)

    $variants = [System.Collections.Generic.List[object]]::new()
    foreach ($needle in $Needles) {
        if ($needle -notmatch '^[\x20-\x7E]+$') { throw "search string must be printable ASCII: '$needle'" }
        $variants.Add([pscustomobject]@{ Key = "$needle|ascii"; Text = $needle; Step = 1 })
        $variants.Add([pscustomobject]@{ Key = "$needle|utf-16le"; Text = ($needle.ToCharArray() -join "`0"); Step = 2 })
    }
    $found = [ordered]@{}
    foreach ($variant in $variants) { $found[$variant.Key] = [long]-1 }

    $overlap = ($variants | ForEach-Object { $_.Text.Length } | Measure-Object -Maximum).Maximum - 1
    $buffer = [byte[]]::new($ChunkSize + $overlap)
    $latin1 = [Text.Encoding]::Latin1
    $stream = [IO.File]::Open($FilePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $carry = 0           # bytes kept from the previous chunk
        $bufferStart = [long]0  # file offset of buffer[0]
        while (($read = $stream.Read($buffer, $carry, $ChunkSize)) -gt 0) {
            $total = $carry + $read
            $text = $latin1.GetString($buffer, 0, $total)
            foreach ($variant in $variants) {
                if ($found[$variant.Key] -ge 0) { continue }
                $index = $text.IndexOf($variant.Text, [StringComparison]::Ordinal)
                if ($index -ge 0) { $found[$variant.Key] = $bufferStart + $index }
            }
            $carry = [Math]::Min($overlap, $total)
            [Buffer]::BlockCopy($buffer, $total - $carry, $buffer, 0, $carry)
            $bufferStart += $total - $carry
        }
    } finally { $stream.Dispose() }
    return $found
}

function Test-File {
    # Returns $true when the file passes; prints one line per finding.
    param([string] $FilePath, [string[]] $ForbiddenStrings, [string[]] $RequiredStrings, [int] $ChunkSize)
    if (!(Test-Path -LiteralPath $FilePath -PathType Leaf)) { Write-Host "FAIL: file not found: $FilePath"; return $false }
    $item = Get-Item -LiteralPath $FilePath
    if ($item.Length -eq 0) { Write-Host "FAIL: file is empty: $FilePath"; return $false }

    $all = @($ForbiddenStrings) + @($RequiredStrings) | Where-Object { $_ }
    $found = Find-Strings -FilePath $item.FullName -Needles $all -ChunkSize $ChunkSize
    $ok = $true
    foreach ($needle in $ForbiddenStrings) {
        foreach ($encoding in 'ascii', 'utf-16le') {
            $offset = $found["$needle|$encoding"]
            if ($offset -ge 0) {
                Write-Host "FAIL: $($item.Name): forbidden string '$needle' ($encoding) at offset $offset"
                $ok = $false
            }
        }
    }
    foreach ($needle in $RequiredStrings) {
        if ($found["$needle|ascii"] -lt 0 -and $found["$needle|utf-16le"] -lt 0) {
            Write-Host "FAIL: $($item.Name): positive control '$needle' not found; the scan proves nothing for this file"
            $ok = $false
        }
    }
    if ($ok) {
        $detail = @()
        if ($ForbiddenStrings) { $detail += "absent: $(($ForbiddenStrings | ForEach-Object { "'$_'" }) -join ', ')" }
        if ($RequiredStrings) { $detail += "positive control present: $(($RequiredStrings | ForEach-Object { "'$_'" }) -join ', ')" }
        Write-Host "ok:   $($item.Name) ($($item.Length) bytes) $($detail -join '; ')"
    }
    return $ok
}

if ($SelfTest) {
    $dir = Join-Path ([IO.Path]::GetTempPath()) ("check-binary-strings-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $dir | Out-Null
    $failures = 0
    $cases = 0
    function Assert-Case {
        param([string] $Name, [byte[]] $Content, [bool] $ExpectPass, [string[]] $Require = @(), [int] $Chunk = 64)
        $file = Join-Path $dir ([Guid]::NewGuid().ToString('N') + '.bin')
        [IO.File]::WriteAllBytes($file, $Content)
        $result = Test-File -FilePath $file -ForbiddenStrings @('debug_inject_fault') -RequiredStrings $Require -ChunkSize $Chunk 6>$null
        $script:cases++
        if ($result -ne $ExpectPass) { Write-Host "SELFTEST FAIL: $Name (expected pass=$ExpectPass)"; $script:failures++ }
        elseif ($VerbosePreference -eq 'Continue') { Write-Host "selftest ok: $Name" }
    }
    try {
        $rng = [Random]::new(1)
        function New-Noise([int] $Length) { $bytes = [byte[]]::new($Length); $rng.NextBytes($bytes); for ($i = 0; $i -lt $Length; $i++) { if ($bytes[$i] -ge 0x20 -and $bytes[$i] -le 0x7E) { $bytes[$i] = 0xFF } }; return $bytes }
        $ascii = [Text.Encoding]::ASCII.GetBytes('_ZN15quicker_platform5fault18debug_inject_fault17h0123456789abcdefE')
        $utf16 = [Text.Encoding]::Unicode.GetBytes('debug_inject_fault')
        $control = [Text.Encoding]::ASCII.GetBytes('quicker_platform')
        Assert-Case 'clean file passes' (New-Noise 500) $true
        Assert-Case 'ASCII hit is found' ((New-Noise 100) + $ascii + (New-Noise 100)) $false
        Assert-Case 'UTF-16LE hit is found' ((New-Noise 100) + $utf16 + (New-Noise 100)) $false
        # Hit placed so that it straddles several 64-byte chunk borders, at every alignment.
        foreach ($shift in 0..70) {
            $content = (New-Noise (50 + $shift)) + $ascii + (New-Noise 30)
            Assert-Case "ASCII hit across chunk border (shift $shift)" $content $false
        }
        foreach ($shift in 0..70) {
            $content = (New-Noise (50 + $shift)) + $utf16 + (New-Noise 30)
            Assert-Case "UTF-16LE hit across chunk border (shift $shift)" $content $false
        }
        Assert-Case 'near miss (one byte short) passes' ((New-Noise 100) + [Text.Encoding]::ASCII.GetBytes('debug_inject_faul') + (New-Noise 100)) $true
        Assert-Case 'positive control present passes' ((New-Noise 100) + $control + (New-Noise 100)) $true -Require @('quicker_platform')
        Assert-Case 'positive control missing fails' (New-Noise 300) $false -Require @('quicker_platform')
        Assert-Case 'empty file fails' ([byte[]]@()) $false
    } finally { Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue }
    if ($failures -gt 0) { Write-Host "$failures of $cases self-test case(s) failed"; exit 1 }
    Write-Host "self-test passed ($cases cases)"
    exit 0
}

$allOk = $true
foreach ($file in $Path) {
    if (-not (Test-File -FilePath $file -ForbiddenStrings $Forbidden -RequiredStrings $RequirePresent -ChunkSize $ChunkBytes)) { $allOk = $false }
}
exit ([int](-not $allOk))
