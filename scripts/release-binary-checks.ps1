# B-52 checks on the exact binaries a packaging script is about to copy (Codex F3-1), and the
# FE-18 check on the frontend embedded in that exe (Codex review 1).
#
# Dot-sourced by scripts/package-artifacts.ps1 and scripts/package-portable.ps1, which call
# Assert-ReleaseBinaries before they write their first file. The release workflow also runs the
# scanners as steps of their own; these calls make every packaging run, local or in CI, fail closed
# in the same way instead of relying on someone having run them first:
#   1. scripts/check-release-features.ps1: the release configuration of quicker-text enables
#      neither `fault-injection` nor `mock` (cargo tree -e features; needs cargo and this tree).
#   2. The PDB is the exe's own: the GUID in the exe's CodeView debug record (RSDS) must be in the
#      PDB, and the record must name a PDB of that file name. Otherwise a PDB left over from another
#      build could pass step 3 for an exe it does not describe.
#   3. scripts/check-binary-strings.ps1 on that PDB (no `debug_inject_fault`; positive control
#      `quicker_platform`) and on the exe (no `debug_inject_fault`; positive control
#      `com.quickertext.desktop`), the same calls as the release workflow.
#   4. scripts/check-embedded-frontend.ps1 (FE-18, Codex review 1): the exe records the digest of
#      the frontend directory its build embedded (src-tauri/build.rs); dist/ must have that digest,
#      so dist/ is the frontend inside this exe and not one rebuilt since; then
#      `node scripts/check-bundle.mjs --scan-only dist` on it (no mock backend). Needs node.
# The scripts run in their own pwsh process each; a non-zero exit code throws, so the caller
# stops before anything is written.
#
# Not covered: the NSIS installer is compressed, so its copy of the exe cannot be scanned. It comes
# from the same `tauri build` as target\release\quicker-text.exe (release workflow, build-windows.ps1).

function Invoke-ReleaseCheckScript {
    param([Parameter(Mandatory = $true)][string] $Script, [string[]] $Arguments = @())
    $pwsh = [Environment]::ProcessPath
    if (-not $pwsh -or -not (Test-Path -LiteralPath $pwsh -PathType Leaf)) { $pwsh = 'pwsh' }
    & $pwsh -NoProfile -File (Join-Path $PSScriptRoot $Script) @Arguments | Out-Host
    $code = $LASTEXITCODE
    if ($code -ne 0) { throw "Release check failed (exit code $code): scripts/$Script $($Arguments -join ' ')" }
}

function Get-CodeViewRecord {
    <# The CodeView (RSDS) debug record of a PE file: PDB GUID bytes, age and PDB path. Throws when there is none. #>
    param([Parameter(Mandatory = $true)][string] $Path)
    $bytes = [IO.File]::ReadAllBytes($Path)
    function U16([long] $at) { if ($at -lt 0 -or $at + 2 -gt $bytes.Length) { throw "not a PE file: $Path" }; [long][BitConverter]::ToUInt16($bytes, $at) }
    function U32([long] $at) { if ($at -lt 0 -or $at + 4 -gt $bytes.Length) { throw "not a PE file: $Path" }; [long][BitConverter]::ToUInt32($bytes, $at) }
    if ((U16 0) -ne 0x5A4D) { throw "not a PE file (no MZ header): $Path" }
    $pe = U32 0x3C
    if ((U32 $pe) -ne 0x00004550) { throw "not a PE file (no PE signature): $Path" }
    $coff = $pe + 4
    $sectionCount = U16 ($coff + 2)
    $optional = $coff + 20
    $directories = switch (U16 $optional) {
        0x20B { $optional + 112 }  # PE32+
        0x10B { $optional + 96 }   # PE32
        default { throw "unknown optional header in $Path" }
    }
    $debugRva = U32 ($directories + 6 * 8)
    $debugSize = U32 ($directories + 6 * 8 + 4)
    if ($debugRva -eq 0 -or $debugSize -eq 0) { throw "no debug directory in $Path (built without a PDB?)" }
    $sectionTable = $optional + (U16 ($coff + 16))
    $debugOffset = -1
    for ($i = 0; $i -lt $sectionCount; $i++) {
        $section = $sectionTable + 40 * $i
        $virtualSize = U32 ($section + 8); $virtualAddress = U32 ($section + 12)
        $rawSize = U32 ($section + 16); $rawPointer = U32 ($section + 20)
        if ($debugRva -ge $virtualAddress -and $debugRva -lt $virtualAddress + [Math]::Max($virtualSize, $rawSize)) {
            $debugOffset = $rawPointer + ($debugRva - $virtualAddress)
            break
        }
    }
    if ($debugOffset -lt 0) { throw "debug directory of $Path is in no section" }
    for ($entry = $debugOffset; $entry + 28 -le $debugOffset + $debugSize; $entry += 28) {
        if ((U32 ($entry + 12)) -ne 2) { continue }  # IMAGE_DEBUG_TYPE_CODEVIEW
        $dataSize = U32 ($entry + 16); $data = U32 ($entry + 24)
        if ($dataSize -lt 25 -or (U32 $data) -ne 0x53445352) { continue }  # 'RSDS'
        $end = [Array]::IndexOf($bytes, [byte]0, [int]($data + 24))
        if ($end -lt 0) { throw "unterminated PDB path in $Path" }
        return [pscustomobject]@{
            Guid    = [byte[]]$bytes[($data + 4)..($data + 19)]
            Age     = U32 ($data + 20)
            PdbPath = [Text.Encoding]::UTF8.GetString($bytes, $data + 24, $end - ($data + 24))
        }
    }
    throw "no CodeView (RSDS) record in $Path"
}

function Assert-PdbMatchesExe {
    <# Throws unless $Pdb is the PDB the exe's CodeView record points to (same file name, GUID present in the PDB). #>
    param([Parameter(Mandatory = $true)][string] $Exe, [Parameter(Mandatory = $true)][string] $Pdb)
    $record = Get-CodeViewRecord -Path $Exe
    $expectedName = [IO.Path]::GetFileName($record.PdbPath.Replace('/', '\'))
    if ($expectedName -ine [IO.Path]::GetFileName($Pdb)) { throw "$([IO.Path]::GetFileName($Exe)) names PDB '$expectedName', not '$([IO.Path]::GetFileName($Pdb))'" }
    $pdbBytes = [IO.File]::ReadAllBytes($Pdb)
    $latin1 = [Text.Encoding]::Latin1
    if (-not $latin1.GetString($pdbBytes, 0, [Math]::Min(32, $pdbBytes.Length)).StartsWith("Microsoft C/C++ MSF 7.00`r`n", [StringComparison]::Ordinal)) {
        throw "not a PDB (MSF 7.00) file: $Pdb"
    }
    # The PDB info stream holds the GUID as 16 raw bytes (it never straddles a 4 KiB block: the stream starts a block).
    if ($latin1.GetString($pdbBytes).IndexOf($latin1.GetString($record.Guid), [StringComparison]::Ordinal) -lt 0) {
        $guid = [Guid]::new([byte[]]$record.Guid)
        throw "$Pdb is not the PDB of $([IO.Path]::GetFileName($Exe)) (GUID $guid, age $($record.Age) not found): rebuild, or package the PDB of this build"
    }
    Write-Host "ok:   $([IO.Path]::GetFileName($Pdb)) is the PDB of $([IO.Path]::GetFileName($Exe)) (GUID $([Guid]::new([byte[]]$record.Guid)))"
}

function Assert-ReleaseBinaries {
    <# Runs the checks described at the top of this file on the exe and PDB about to be packaged and the frontend the exe embeds. Throws on any failure. #>
    param(
        [Parameter(Mandatory = $true)][string] $Exe,
        [Parameter(Mandatory = $true)][string] $Pdb,
        [string] $Dist = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist')
    )
    foreach ($file in @($Exe, $Pdb)) {
        if (!(Test-Path -LiteralPath $file -PathType Leaf)) { throw "Missing build output for the B-52 checks: $file" }
    }
    Invoke-ReleaseCheckScript 'check-release-features.ps1'
    Assert-PdbMatchesExe -Exe $Exe -Pdb $Pdb
    Invoke-ReleaseCheckScript 'check-binary-strings.ps1' @($Pdb, '-RequirePresent', 'quicker_platform')
    Invoke-ReleaseCheckScript 'check-binary-strings.ps1' @($Exe, '-RequirePresent', 'com.quickertext.desktop')
    Invoke-ReleaseCheckScript 'check-embedded-frontend.ps1' @('-Exe', $Exe, '-Dist', $Dist)
}
