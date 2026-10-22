param([Parameter(Mandatory=$true)][string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'source-allowlist.ps1')
$version = (Get-Content -LiteralPath (Join-Path $projectRoot 'package.json') -Raw | ConvertFrom-Json).version
if ($version -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$') { throw 'Package version must use major.minor.patch format' }
$output = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $output -Force | Out-Null
$exe = Join-Path $projectRoot 'target\release\quicker-text.exe'
$installer = Join-Path $projectRoot "target\release\bundle\nsis\Quicker Text_${version}_x64-setup.exe"
foreach ($file in @($exe, $installer)) { if (!(Test-Path -LiteralPath $file -PathType Leaf)) { throw "Missing build: $file" } }
$deliveries = [ordered]@{
    "Quicker-Text-$version-THIRD-PARTY-NOTICES.md" = Join-Path $projectRoot 'THIRD_PARTY_NOTICES.md'
    "Quicker-Text-$version-GPL-3.0.txt" = Join-Path $projectRoot 'LICENSE'
    "Quicker-Text-$version-Windows-x64.exe" = $exe
    "Quicker-Text-$version-Windows-x64-Setup.exe" = $installer
    "Quicker-Text-$version-使用与构建说明.md" = Join-Path $projectRoot 'README.md'
    "Quicker-Text-$version-平台支持与手工测试.md" = Join-Path $projectRoot 'docs\platform-support.md'
    "Quicker-Text-$version-浏览器与IME手测.md" = Join-Path $projectRoot 'docs\browser-ime-manual-tests.md'
    "Quicker-Text-$version-验收矩阵.md" = Join-Path $projectRoot 'docs\acceptance.md'
    "Quicker-Text-$version-构建验证记录.md" = Join-Path $projectRoot 'docs\build-verification.md'
    "Quicker-Text-$version-先读与测试.md" = Join-Path $projectRoot 'docs\windows-0.3.1-notes.md'
    "Quicker-Text-$version-浏览器测试页面.html" = Join-Path $projectRoot 'scripts\fixtures\browser-input.html'
}
$zipName = "Quicker-Text-$version-Source.zip"
$hashName = "Quicker-Text-$version-SHA256SUMS.txt"
foreach ($name in (@($deliveries.Keys) + @($zipName, $hashName))) {
    if (Test-Path -LiteralPath (Join-Path $output $name)) { throw "Output already exists; preserve it: $name" }
}
# Explicit source allowlist, shared with package-portable.ps1 and check-source-allowlist.ps1:
# scripts/source-allowlist.ps1 (update it when a crate or a root-level build file is added, B-51).
# Runtime configuration, imports, logs, build products and private data are never traversed.
$relativeFiles = @(Get-SourceAllowlist -ProjectRoot $projectRoot)
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::Open((Join-Path $output $zipName), [IO.Compression.ZipArchiveMode]::Create)
try {
    foreach ($relative in ($relativeFiles | Sort-Object -Unique)) {
        $path = Join-Path $projectRoot $relative
        if (!(Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing source: $relative" }
        [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $path, $relative.Replace('\','/'), [IO.Compression.CompressionLevel]::Optimal) | Out-Null
    }
} finally { $archive.Dispose() }
foreach ($entry in $deliveries.GetEnumerator()) { Copy-Item -LiteralPath $entry.Value -Destination (Join-Path $output $entry.Key) }
$names = @($deliveries.Keys) + @($zipName)
$hashes = foreach ($name in $names) { $hash = Get-FileHash -LiteralPath (Join-Path $output $name) -Algorithm SHA256; "$($hash.Hash.ToLowerInvariant())  $name" }
Set-Content -LiteralPath (Join-Path $output $hashName) -Value $hashes -Encoding utf8
Write-Output "Packaged version $version; source files: $(($relativeFiles | Sort-Object -Unique).Count)"
Get-ChildItem -LiteralPath $output -File | Where-Object Name -Like "Quicker-Text-$version-*" | Select-Object Name,Length
Get-Content -LiteralPath (Join-Path $output $hashName)

