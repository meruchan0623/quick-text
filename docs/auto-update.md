# 自动更新与发布

2026-11-19 起，更新源从 GitHub Releases 改为 TSim 的两台服务器。安装包在本机打包签名后上传，不再经过 GitHub Actions。三方契约见 `client-updates/CONTRACT.md`（服务端为 TSim_CS，客户端部分在 §6）。

## 客户端（`src-tauri/src/autoupdate.rs`、`src-tauri/src/update.rs`）

- 更新地址编译在程序里，远端不能改写：
  - 生产：`https://csapi.tsim.tech/client-updates/v1/quicktext/stable/{macos|windows}/latest.json`
  - 测试：`https://test-csapi.tsim.tech/client-updates/v1/quicktext/stable/{macos|windows}/latest.json`
- 每次检查同时请求两份清单，取版本较新的那份；版本相同时用生产服。一台失败时只用另一台，两台都失败才算检查失败；两台都返回 204 或旧版本，就是已是最新。
- 渠道在编译时由 `QT_UPDATE_CHANNEL` 写入，只认 `stable`。发布脚本和 `scripts/build-*.{sh,ps1}` 默认都设成 `stable`。直接运行 `tauri build` 或 `tauri dev` 时没有渠道，不访问更新服务器，设置页的「检查更新」也会提示本机构建不能检查。
- 后台：启动 60 秒后检查一次，之后每 6 小时检查一次。发现新版本就在后台下载，用内嵌公钥验证 minisign 签名（`requireSignedVersion`：签名必须绑定版本号，防止降级）。验证通过后，托盘菜单顶部出现「重启以更新到 x.y.z」，同时弹出系统通知。不重启也可以：之后从托盘点「退出全部」时会静默安装。关机等其他退出方式不会安装。
- 设置页「检查更新」走同一套逻辑：显示版本、日期、更新说明，来自测试服时加「测试服」标记 →「下载更新」（有进度条，下载后验签）→「重启并更新」。不再把安装包下载到「下载」文件夹，也不再使用 GitHub API 和 SHA256SUMS。
- 下载好的包会记住提供它的清单地址。退出时安装要恢复这次更新，用的就是这个地址。
- Windows 只有 MSI 安装版能自动更新，便携版 ZIP 不会。

## 过渡

已安装的 0.5.0 及更早版本仍然从 GitHub 的 `updater-stable/latest.json` 检查更新。所以带新地址的第一个版本还要在 GitHub 上发布一次：在 Actions 里手动运行 `release` 工作流，并勾选 `github_bridge`，它会按旧流程发布 GitHub Release 和 `updater-stable`。这一步由维护者执行。之后的版本只发布到 TSim。工作流现在只能手动触发，推送 main 不会再自动发布。

## 发布（本机脚本）

先在 `release-notes/<版本>.txt` 写好给用户看的更新说明（纯文本，1–4000 字），或者用 `--notes` / `--notes-file` 传入。版本号仍用 `node scripts/version.mjs bump …` 修改。服务端只接受比当前已有版本更高的版本号。

| 平台 | 命令 | 产物 |
|---|---|---|
| macOS（Apple silicon） | `scripts/publish-tsim.sh --server test\|prod\|both [--publish] [--skip-build]` | `Quicktext_<v>_aarch64.app.tar.gz`（既是更新包也是安装包） |
| Windows x64 | `pwsh -File scripts/publish-tsim.ps1 -Server test\|prod\|both -DependencySourcesZip <…Source-deps.zip> [-Publish] [-SkipBuild] [-CertificateThumbprint …]` | `Quicktext_<v>_x64_zh-CN.msi`（既是安装包也是更新包） |

- 脚本依次执行：检查版本和说明 → 构建 → 系统签名 → 用 `tauri signer sign --app-version` 签更新包，并用 `src-tauri/update-keys/current.pub` 验签 → multipart 上传到 `POST {服务器}/client-updates/v1/publish`。不带 `--publish` / `-Publish` 时上传为草稿，到管理页「客户端发布」里发布。
- `--skip-build` / `-SkipBuild` 复用已构建的 .app 或 MSI，只重新打包、签名、上传。某台服务器上传失败时，用它加 `--server <那一台>` 重传。
- Windows 构建走 `scripts/build-windows.ps1`，和其他可分发构建一样，必须带第三方声明和依赖源码包（`-DependencySourcesZip`），缺了直接失败。macOS 沿用原 CI 的做法：跳过这道只针对 Windows 的检查，因为 macOS 的声明和源码包流程还没有。
- 每个平台服务器上只保留一个包，自动更新和首次下载共用，不再上传 dmg。Mac 首次安装：下载 `.app.tar.gz`，双击解压，把 Quicktext.app 拖进「应用程序」。新版本发布后，服务器会撤回同平台的旧版本并删除其文件。
- 建议先传 `test`，装机验证后再传 `prod`。两台的版本相同时，客户端使用生产服的包。

## 密钥（只放在打包机上，不进仓库，也不上传 TSim）

- 发布密钥：环境变量 `TSIM_PUBLISH_KEY_PROD` / `TSIM_PUBLISH_KEY_TEST`，在 TSim 管理页生成。脚本通过请求头 `X-Client-Publish-Key` 发送，不会打印出来。
- 更新签名私钥：`TAURI_SIGNING_PRIVATE_KEY`（内容）或 `TAURI_SIGNING_PRIVATE_KEY_PATH`（文件，默认 `~/.config/quicktext-signing/updater.key`），密码放在 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。对应的公钥是 `src-tauri/update-keys/current.pub`，也写在 `tauri.conf.json` 的 `plugins.updater.pubkey` 中，两处必须一致；TSim 服务端也内置了这把公钥（key id `9F53EFA021ACF8E7`）。私钥丢失后，已安装的客户端将无法再更新。
- macOS 系统签名：固定的自签名身份（`QUICKTEXT_SIGNING_IDENTITY`，默认 `Quicktext Local Signing`，见 `scripts/setup-macos-signing.sh`）。没有找到这个身份时脚本直接失败，不会退回 ad-hoc 签名。macOS 按证书记住用户授予的权限，所以每次发布都要用同一张证书。要和以前 CI 发布的版本衔接，就在打包机上导入 CI 用的那张证书（secret `MAC_DEV_CERTIFICATE`），并把 `QUICKTEXT_SIGNING_IDENTITY` 设成 `MAC_DEV_SIGNING_IDENTITY`。换了证书，用户更新后需要重新授权一次。
- Windows 代码签名证书是可选的（`-CertificateThumbprint`）。不签名时，SmartScreen 会在首次安装时警告。
