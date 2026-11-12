# 自动更新与 CI 发布

2026-11-11。参照 Ctrip-vbk-refact 的方案（tauri-plugin-updater + GitHub Releases 上的 `latest.json`）。本仓库是公开仓库，所以安装包和更新清单直接发布在本仓库的 Releases 里，不另建发布仓库。

## 发布（`.github/workflows/release.yml`）

2026-11-12 起只发 stable，不再发 beta；只有版本号上调才构建。

| 触发 | 渠道 | 版本号 | 发布到 |
|---|---|---|---|
| 推送 main，且 `tauri.conf.json` 的 `version` 高于 `updater-stable` 已发布的版本 | stable | `version` 原样 | 正式 release `v<版本>`（latest） |
| 推送 main，版本号没升 | 无 | 无 | 只跑几秒的检查，不构建 |
| 手动运行 | 无 | 清单版本 | 只上传 Actions 产物 |

- 发版：`node scripts/version.mjs bump <patch|minor|x.y.z>`，提交后推送到 main。
- 安装包只有两个：`Quicktext_<v>_aarch64.dmg`（签名的 arm64），以及 `Quicktext_<v>_x64_zh-CN.msi`（按用户安装，也是 Windows 的更新包）。另外还会附带 mac 更新包 `Quicktext_<v>_aarch64.app.tar.gz(.sig)`、Windows 便携版 ZIP 和 `SHA256SUMS-*.txt`（设置页的手动检查更新用）。不再构建 NSIS 和 Intel Mac 包。
- MSI 用自定义 WiX 模板 `src-tauri/wix/main.wxs`（复制自 tauri-cli 2.12.2 的上游模板）。安装位置是 `%LOCALAPPDATA%\Programs\Quicktext`（`ALLUSERS=2` + `MSIINSTALLPERUSER=1`），安装和更新都不弹 UAC。如果检测到旧的全机安装版，会拦下安装并提示先卸载。升级 tauri-cli 时要同步更新这个模板。
- 先上传版本 release，最后覆盖 `updater-stable` 里的 `latest.json`。写入前，`scripts/update-manifest.mjs` 会用客户端内嵌的公钥逐个验签，签名必须绑定版本号。stable 的历史版本全部保留。
- 旧的 beta 渠道只发过 0.4.4（NSIS 安装）。每次发 stable 时，`updater-beta/latest.json` 会被改写成只含 mac 条目，这样 mac 上的 0.4.4 会自动升到 stable。Windows 上的 0.4.4 要先手动卸载，再安装 MSI；两种安装方式互不识别，直接装会并存两份。
- 连续推送时，新运行排队、不取消。

## 客户端（`src-tauri/src/autoupdate.rs`）

- 渠道在编译时由 `QT_UPDATE_CHANNEL` 写入。CI 和本地打包脚本（`scripts/build-windows.ps1`、`scripts/build-macos.sh`）默认都是 `stable`，所以本地打出的包也会从 CI 发布的 stable 更新。直接运行 `tauri build` 或 `tauri dev` 时没有渠道，不访问更新源。
- 启动 60 秒后检查一次，之后每 6 小时检查一次。发现新版本就在后台下载，并验证 minisign 签名（`tauri.conf.json` 的 `requireSignedVersion`：签名必须绑定版本号，防止降级）。
- 验签通过后，托盘菜单顶部出现「重启以更新到 x.y.z」，同时弹出系统通知。不重启也可以：之后从托盘点「退出全部」时会静默安装。关机等其他退出方式不会安装。
- 只有这条流水线发布的安装包具备自动更新，v0.4.0 及更早的版本需要手动装一次。Windows 必须用 MSI 安装，便携版 ZIP 不会自动更新。

## 密钥

- 公钥：`src-tauri/update-keys/current.pub`，同时写在 `tauri.conf.json` 的 `plugins.updater.pubkey` 中，两处必须一致。
- 私钥和密码存在仓库 secrets `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 中。设置时用文件重定向（`gh secret set NAME < file`）。不要用 PowerShell 管道，它会加上 BOM，导致 CI 无法解析私钥。
- 私钥丢失后，已安装的客户端将无法再更新，只能手动重装换了公钥的新版本。
- macOS 用固定的自签名身份（secrets `MAC_DEV_*`，见 `scripts/setup-macos-signing.sh`）签名，designated requirement 不变，已授予的权限跨更新保留。首次安装 dmg 需要放行一次，应用内更新不再拦截。
