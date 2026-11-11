# 自动更新与 CI 发布

2026-11-11。参照 Ctrip-vbk-refact 的方案（tauri-plugin-updater + GitHub Releases 上的 `latest.json`）。本仓库是公开仓库，所以安装包和更新清单直接发布在本仓库的 Releases 里，不另建发布仓库。

## 发布（`.github/workflows/release.yml`）

| 触发 | 渠道 | 版本号 | 发布到 |
|---|---|---|---|
| 推送 main（改动了应用源码） | beta | `tauri.conf.json` 的 `major.minor` + `run_number` | pre-release `v<版本>` |
| 推送标签 `vX.Y.Z` | stable | `X.Y.Z` | 正式 release `v<版本>`（latest） |
| 手动运行 | 无 | 清单版本 | 只上传 Actions 产物 |

- 每个版本 release 里有：`Quicktext_<v>_aarch64.dmg`、`Quicktext_<v>_aarch64.app.tar.gz(.sig)`（mac 更新包）、`Quicktext_<v>_x64-setup.exe(.sig)`（Windows 安装器，也是更新包）、Windows 便携版 ZIP、`SHA256SUMS-*.txt`（设置页的手动检查更新使用）。
- 先上传版本 release，最后覆盖渠道清单：`updater-beta` / `updater-stable` 这两个固定 release 里的 `latest.json`。写入前，`scripts/update-manifest.mjs` 会用客户端内嵌的公钥逐个验签，签名必须绑定版本号。
- `scripts/prune-releases.mjs` 只保留最新 10 个 beta，stable 全部保留；任一清单指向的版本永不删除。
- 只改文档或测试的推送不会发布。连续推送时新运行排队、不取消，最多构建两次。
- 打 stable 标签前，先 `npm run version:bump` 把次版本号升上去，否则 stable 版本号可能小于已发出的 beta（beta 号是 `major.minor.run_number`）。

## 客户端（`src-tauri/src/autoupdate.rs`）

- 渠道在编译时由 `QT_UPDATE_CHANNEL` 写入，只有 CI 会设置。本地构建没有渠道，不会访问更新源。
- 启动 60 秒后检查一次，之后每 6 小时检查一次。发现新版本就在后台下载，并验证 minisign 签名（`tauri.conf.json` 的 `requireSignedVersion`：签名必须绑定版本号，防止降级）。
- 验签通过后，托盘菜单顶部出现「重启以更新到 x.y.z」，同时弹出系统通知。不重启也可以：之后从托盘点「退出全部」时会静默安装。关机等其他退出方式不会安装。
- 只有这条流水线发布的安装包具备自动更新，v0.4.0 及更早的版本需要手动装一次。Windows 必须用 setup.exe 安装，便携版 ZIP 不会自动更新。

## 密钥

- 公钥：`src-tauri/update-keys/current.pub`，同时写在 `tauri.conf.json` 的 `plugins.updater.pubkey` 中，两处必须一致。
- 私钥和密码存在仓库 secrets `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 中。设置时用文件重定向（`gh secret set NAME < file`）。不要用 PowerShell 管道，它会加上 BOM，导致 CI 无法解析私钥。
- 私钥丢失后，已安装的客户端将无法再更新，只能手动重装换了公钥的新版本。
- macOS 用固定的自签名身份（secrets `MAC_DEV_*`，见 `scripts/setup-macos-signing.sh`）签名，designated requirement 不变，已授予的权限跨更新保留。首次安装 dmg 需要放行一次，应用内更新不再拦截。
