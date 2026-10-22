<#
.SYNOPSIS
  S-02 / B-02 gate: the hook and RawInput callback modules must enforce the panic-free
  lint set at module level.

.DESCRIPTION
  Each file must open (after comments and other inner attributes) with inner attributes
  that leave all four lints at deny or forbid:

      #![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

  The attribute may be wrapped over several lines, split into several attributes, or use
  forbid instead of deny. A later allow/warn/expect of one of the lints in the header
  cancels the deny and fails the check. Line wrapping, CRLF line endings and a BOM do not
  matter.

  Why a script and not only clippy: CI runs clippy with -A clippy::unwrap_used
  -A clippy::expect_used (plan section 11, until 0.5). Only a module-level #![deny] still
  applies in these modules, and this script makes sure nobody removes it.

  Not checked: #[allow] on individual items further down the file, and blocking locks
  (clippy has no lint for them). Review those in the PR.

.PARAMETER Files
  Files to check. Relative paths are resolved against the repository root. Defaults to
  crates/platform/src/windows/hook.rs and crates/platform/src/windows/rawinput.rs. A
  missing file is an error: those modules are created by the platform split (S-23), and
  the check must not silently pass when they are renamed or moved. When you move one,
  update the default list below and the call in .github/workflows/ci.yml.

.EXAMPLE
  ./scripts/check-hook-lints.ps1
  ./scripts/check-hook-lints.ps1 crates/platform/src/windows/hook.rs

.NOTES
  A bash twin with the same rules lives in scripts/check-hook-lints.sh (needs perl);
  keep the two in sync. Exit code 0 = pass, 1 = failure.
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)]
    [string[]] $Files
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot

$defaultFiles = @(
    'crates/platform/src/windows/hook.rs',
    'crates/platform/src/windows/rawinput.rs'
)
$requiredLints = @(
    'clippy::unwrap_used',
    'clippy::expect_used',
    'clippy::panic',
    'clippy::indexing_slicing'
)

if (-not $Files -or $Files.Count -eq 0) {
    $Files = $defaultFiles
}

$onGitHub = ($env:GITHUB_ACTIONS -eq 'true')
$script:failed = $false

function Write-Failure([string] $File, [string] $Message) {
    $script:failed = $true
    if ($onGitHub) {
        Write-Host "::error file=${File}::${Message}"
    }
    Write-Host "FAIL: ${File}: ${Message}"
}

# Walks the leading comments and inner attributes of a Rust source file and returns the
# final lint level (allow / warn / deny / forbid / expect) of every lint named there.
function Get-HeaderLintLevels([string] $Text) {
    $levels = @{}
    $whitespace = [regex] '\G\s+'
    $lineComment = [regex] '\G//[^\r\n]*'
    $blockComment = [regex] '\G/\*[\s\S]*?\*/'
    $innerAttribute = [regex] '\G#!\[(?:[^\[\]]|\[[^\]]*\])*\]'
    $levelAttribute = [regex] '^#!\[\s*(allow|warn|deny|forbid|expect)\s*\(([\s\S]*)\)\s*\]$'
    $reasonClause = [regex] 'reason\s*=\s*"[^"]*"'
    $innerComment = [regex] '//[^\r\n]*'

    if ($Text.Length -gt 0 -and $Text[0] -eq [char] 0xFEFF) {
        $Text = $Text.Substring(1)
    }

    $pos = 0
    while ($pos -lt $Text.Length) {
        $m = $whitespace.Match($Text, $pos)
        if (-not $m.Success) { $m = $lineComment.Match($Text, $pos) }
        if (-not $m.Success) { $m = $blockComment.Match($Text, $pos) }
        if (-not $m.Success) {
            $m = $innerAttribute.Match($Text, $pos)
            if ($m.Success) {
                $attr = $levelAttribute.Match($m.Value)
                if ($attr.Success) {
                    $level = $attr.Groups[1].Value
                    $list = $attr.Groups[2].Value
                    $list = $reasonClause.Replace($list, '')
                    $list = $innerComment.Replace($list, '')
                    foreach ($item in $list.Split(',')) {
                        $name = $item.Trim()
                        if ($name.Length -eq 0) { continue }
                        # forbid cannot be lowered again, which is why it is sticky.
                        if ($levels.ContainsKey($name) -and $levels[$name] -eq 'forbid') { continue }
                        $levels[$name] = $level
                    }
                }
            }
        }
        if (-not $m.Success -or $m.Length -eq 0) {
            # First token that is neither a comment nor an inner attribute: the header ends.
            break
        }
        $pos = $m.Index + $m.Length
    }
    return $levels
}

foreach ($file in $Files) {
    if ([System.IO.Path]::IsPathRooted($file)) {
        $full = $file
    } else {
        $full = Join-Path $repoRoot $file
    }

    if (-not (Test-Path -LiteralPath $full -PathType Leaf)) {
        $hint = 'The hook and RawInput callback modules are created by the platform split ' +
            '(S-23, B-02). If they were renamed or moved, update the default list in ' +
            'scripts/check-hook-lints.ps1 and scripts/check-hook-lints.sh, and the call in ' +
            '.github/workflows/ci.yml.'
        Write-Failure $file "file not found ($full). $hint"
        continue
    }

    $text = [System.IO.File]::ReadAllText($full)
    $levels = Get-HeaderLintLevels $text

    $bad = @()
    foreach ($lint in $requiredLints) {
        if (-not $levels.ContainsKey($lint)) {
            $bad += "$lint (missing)"
        } elseif ($levels[$lint] -ne 'deny' -and $levels[$lint] -ne 'forbid') {
            $bad += ("{0} (set to {1})" -f $lint, $levels[$lint])
        }
    }

    if ($bad.Count -gt 0) {
        Write-Failure $file ("the module header must contain #![deny({0})] before any item; not enforced: {1}" -f ($requiredLints -join ', '), ($bad -join ', '))
    } else {
        Write-Host "ok:   $file enforces $($requiredLints -join ', ')"
    }
}

if ($script:failed) {
    exit 1
}
exit 0
