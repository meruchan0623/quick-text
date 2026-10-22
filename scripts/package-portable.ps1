#requires -Version 7.0
param([Parameter(Mandatory=$true)][string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$version = (Get-Content -LiteralPath (Join-Path $root 'package.json') -Raw -Encoding UTF8 | ConvertFrom-Json).version
$output = [IO.Path]::GetFullPath($OutputDirectory)
$name = "Quicker-Text-$version-Windows-x64-Portable"
$folder = Join-Path $output $name
$zip = Join-Path $output "$name.zip"
if ((Test-Path -LiteralPath $folder) -or (Test-Path -LiteralPath $zip)) { throw 'Preserve existing packages: choose a new output directory.' }
$exe = Join-Path $root 'target\release\quicker-text.exe'
$required = @($exe, (Join-Path $root 'THIRD_PARTY_NOTICES.md'), (Join-Path $root 'docs\windows-0.3.0-refactor.md'), (Join-Path $root 'docs\windows-0.3.1-notes.md'))
foreach ($file in $required) { if (!(Test-Path -LiteralPath $file -PathType Leaf)) { throw "Missing required artifact: $file" } }
New-Item -ItemType Directory -Path $folder -Force | Out-Null
Copy-Item -LiteralPath $exe -Destination (Join-Path $folder 'quicker-text.exe')
Copy-Item -LiteralPath (Join-Path $root 'README.md'), (Join-Path $root 'LICENSE'), (Join-Path $root 'THIRD_PARTY_NOTICES.md'), (Join-Path $root 'docs\windows-0.3.0-refactor.md'), (Join-Path $root 'docs\windows-0.3.1-notes.md') -Destination $folder
Copy-Item -LiteralPath (Join-Path $root 'vendor\espanso-win32\LICENSE') -Destination (Join-Path $folder 'LICENSE-ESPANSO-GPL-3.0.txt')
# Carry the actual corresponding source, including patched upstream code and build scripts.
# Explicit allowlist excludes user configuration, imports, logs and workspace snapshots.
$source = Join-Path $folder 'source'
New-Item -ItemType Directory -Path $source | Out-Null
$files = @('Cargo.toml','Cargo.lock','package.json','package-lock.json','index.html','tsconfig.json','vite.config.ts','.gitignore','LICENSE','README.md','THIRD_PARTY_NOTICES.md','src-tauri\Cargo.toml','src-tauri\build.rs','src-tauri\tauri.conf.json')
$dirs = @('crates','vendor','src','scripts','docs','src-tauri\src','src-tauri\icons','src-tauri\capabilities')
foreach ($dir in $dirs) {
    $path = Join-Path $root $dir
    if (Test-Path -LiteralPath $path) {
        foreach ($file in Get-ChildItem -LiteralPath $path -File -Recurse) {
            $relative = [IO.Path]::GetRelativePath($root, $file.FullName)
            if ($relative -match '(^|[\\/])(target|node_modules|\.git)([\\/]|$)') { continue }
            $files += $relative
        }
    }
}
foreach ($relative in ($files | Sort-Object -Unique)) {
    $destination = Join-Path $source $relative
    New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $root $relative) -Destination $destination
}
$hashes = foreach ($file in Get-ChildItem -LiteralPath $folder -File -Recurse | Sort-Object FullName) {
    '{0}  {1}' -f (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant(), [IO.Path]::GetRelativePath($folder,$file.FullName).Replace('\','/')
}
Set-Content -LiteralPath (Join-Path $folder 'SHA256SUMS.txt') -Value $hashes -Encoding UTF8
Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::CreateFromDirectory($folder, $zip, [IO.Compression.CompressionLevel]::Optimal, $false)
$archive = [IO.Compression.ZipFile]::OpenRead($zip)
try {
    foreach ($entry in $archive.Entries) {
        if (!$entry.Name) { continue }
        $stream = $entry.Open()
        try { $actual = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($stream)) } finally { $stream.Dispose() }
        $expected = (Get-FileHash -LiteralPath (Join-Path $folder $entry.FullName) -Algorithm SHA256).Hash
        if ($actual -ne $expected) { throw "ZIP entry hash mismatch: $($entry.FullName)" }
    }
    $entryCount = $archive.Entries.Count
} finally { $archive.Dispose() }
if ((Get-FileHash -LiteralPath $exe).Hash -ne (Get-FileHash -LiteralPath (Join-Path $folder 'quicker-text.exe')).Hash) { throw 'Executable hash mismatch' }
Get-FileHash -LiteralPath $exe,$zip -Algorithm SHA256 | Format-List Path,Hash
Write-Output "Verified $entryCount ZIP entries. Portable package: $zip"
