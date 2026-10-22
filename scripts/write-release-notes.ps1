#requires -Version 7.0
<#
.SYNOPSIS
  Writes the license and source-acquisition section of the release description (S-28, GPL).

.DESCRIPTION
  GPL-3.0-or-later requires that everyone who receives the installer can also get the license
  text and the complete corresponding source. The release workflow calls this script after
  scripts/package-artifacts.ps1 and uses the result twice: as the job summary and as
  `release-notes.md` (a workflow artifact), which the maintainer pastes into the description
  of the GitHub Release. The text names the files that are attached to the release, so it only
  makes sense for that package set; the SHA256 of both source archives is computed here from
  the real files.

  The text says that the source ZIP and the dependency source ZIP together are the complete
  corresponding source: the crates.io and npm dependencies are shipped in the dependency ZIP
  (scripts/package-dependency-sources.ps1; see docs/release-checklist.md).

.PARAMETER Version
  major.minor.patch, as in the package file names.

.PARAMETER SourceZip
  The Quicker-Text-<version>-Source.zip that package-artifacts.ps1 produced.

.PARAMETER DependencySourcesZip
  The Quicker-Text-<version>-Source-deps.zip that package-dependency-sources.ps1 produced and
  package-artifacts.ps1 attached. A release without it is not allowed, so it is mandatory.

.PARAMETER RepositoryUrl
  Repository web address, e.g. https://github.com/<owner>/<repo>.

.PARAMETER Ref
  Tag name (release) or branch name (dry run) that was built.

.PARAMETER CommitSha
  Full commit hash that was built.

.PARAMETER IsTag
  The ref is a release tag. Without it the text says the build is a dry run.

.PARAMETER OutputFile
  Where to write the Markdown. Must not exist yet.

.EXAMPLE
  ./scripts/write-release-notes.ps1 -Version 0.4.0 -SourceZip out/Quicker-Text-0.4.0-Source.zip `
    -DependencySourcesZip out/Quicker-Text-0.4.0-Source-deps.zip `
    -RepositoryUrl https://github.com/owner/repo -Ref v0.4.0 -CommitSha 0123abc -IsTag -OutputFile out/release-notes.md

.NOTES
  Exit code 0 = written.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidatePattern('^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$')][string] $Version,
    [Parameter(Mandatory = $true)][string] $SourceZip,
    [Parameter(Mandatory = $true)][string] $DependencySourcesZip,
    [Parameter(Mandatory = $true)][string] $RepositoryUrl,
    [Parameter(Mandatory = $true)][string] $Ref,
    [Parameter(Mandatory = $true)][string] $CommitSha,
    [switch] $IsTag,
    [Parameter(Mandatory = $true)][string] $OutputFile
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (!(Test-Path -LiteralPath $SourceZip -PathType Leaf)) { throw "Source ZIP not found: $SourceZip" }
if (!(Test-Path -LiteralPath $DependencySourcesZip -PathType Leaf)) { throw "Dependency source ZIP not found: $DependencySourcesZip (a release without it is not allowed)" }
if (Test-Path -LiteralPath $OutputFile) { throw "Output already exists; preserve it: $OutputFile" }
$zipSha256 = (Get-FileHash -LiteralPath $SourceZip -Algorithm SHA256).Hash.ToLowerInvariant()
$zipName = [IO.Path]::GetFileName($SourceZip)
$depsSha256 = (Get-FileHash -LiteralPath $DependencySourcesZip -Algorithm SHA256).Hash.ToLowerInvariant()
$depsName = [IO.Path]::GetFileName($DependencySourcesZip)
$repository = $RepositoryUrl.TrimEnd('/')

$origin = if ($IsTag) { "标签 ``$Ref``（提交 ``$CommitSha``）：$repository/tree/$Ref" }
          else { "提交 ``$CommitSha``（分支 ``$Ref``，试运行，不是正式发布）：$repository/commit/$CommitSha" }

$text = @"
## 许可证与源码获取（GPL-3.0-or-later）

Quicker Text $Version 的整个项目（自研代码与静态纳入的 espanso 组件）按 GPL-3.0-or-later 授权与分发。

- 许可证全文：``Quicker-Text-$Version-GPL-3.0.txt``（与仓库根目录的 ``LICENSE`` 相同）。
- 第三方声明：``Quicker-Text-$Version-THIRD-PARTY-NOTICES.md``（espanso 的版权、许可和本地修改说明）。
- 第三方依赖的版权声明与许可证全文：``Quicker-Text-$Version-THIRD-PARTY-LICENSES.txt``（由实际分发的 Rust crate 与 npm 包生成；安装包和绿色包内也带有同一份文件）。
- 完整对应源码由下面两个文件**合在一起**构成，缺一不可：
  - ``$zipName``（SHA256 ``$zipSha256``）：全部工作区代码、``vendor/espanso-win32`` 原生组件、``Cargo.lock``、``package-lock.json`` 以及构建与打包脚本。
  - ``$depsName``（SHA256 ``$depsSha256``）：全部第三方依赖的源码，即 ``cargo vendor --locked`` 的 Rust crate 和打包进前端的 npm 包，附离线重建说明 ``README-OFFLINE-BUILD.md``。
  - 两个文件的校验和同时列在 ``Quicker-Text-$Version-SHA256SUMS.txt``。
- 版本库：$origin
- 构建方法：源码包内 ``README.md`` 的「许可与绿色包构建」一节和 ``docs/build-verification.md``。
- 依赖：Rust 与前端依赖的源码已随本次发布提供（``$depsName``），版本和校验和由 ``Cargo.lock`` 与 ``package-lock.json`` 固定；Rust 依赖的许可证由 ``deny.toml`` 对照与 GPL-3.0-or-later 兼容的清单检查。前端构建工具（``devDependencies``，如 TypeScript）不属于产品，仍在构建时由 npm 获取。
- 已发布的 0.3.1 的许可证说明和源码获取方式：源码包内 ``docs/licensing-0.3.1.md``。

PDB 符号文件不随安装包分发，由维护者按版本另行归档，用于离线符号化崩溃记录。
"@

New-Item -ItemType Directory -Path (Split-Path -Parent ([IO.Path]::GetFullPath($OutputFile))) -Force | Out-Null
Set-Content -LiteralPath $OutputFile -Value $text -Encoding utf8
Write-Host "wrote $OutputFile"
