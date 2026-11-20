---
name: tsimsoft-signedbuild-deploy
description: 在 macOS（Apple Silicon）或 Windows（x64）上把 Quicktext（quicker-text）做 Tauri/Rust 签名打包，发布到 TSim 客服系统更新源（默认测试服 test-csapi.tsim.tech，生产服 csapi.tsim.tech），上架到客服系统下载页（测试服 https://test-agent.tsim.tech/downloads，生产服 https://csagent.tsim.tech/downloads），并读回核对下载链接。用于“打包签名版”“发版到测试服/生产服”“上传 Quicktext 更新包”“发 Mac / Windows 包”“TSim 软件发布”“给我下载链接”“传到下载页 / test-agent.tsim.tech/downloads”。
---

# TSim Quicktext 签名打包与发布

一个入口跑 Mac 和 Windows：`scripts/signed-build.mjs`。它识别平台后调用仓库里的平台脚本
（Mac `scripts/publish-tsim.sh`，Windows `scripts/publish-tsim.ps1`，需要 pwsh 7），前后加上发版前检查和发布后读回。
真正的构建、签名、更新签名、验签、上传逻辑在平台脚本里，**两份平台脚本要同步修改，本 skill 不复制它们的步骤**。
更新源与 VBookingIM（Ctrip-vbk-refact 的 tsimvbk-signedbuild-deploy）相同：`POST <server>/client-updates/v1/publish`，
产品名 `quicktext`。背景见仓库 `docs/auto-update.md`。

## 下载页上架

“传到 https://test-agent.tsim.tech/downloads” 就是 `publish --server test`（生产服下载页 https://csagent.tsim.tech/downloads
对应 `--server prod`）。下载页没有单独的上传入口：它读 `GET <server>/client-updates/v1/quicktext/downloads`，
**只展示已发布（published）的版本**，所以：

- 默认的 `publish`（直接发布）完成后就会出现在下载页；`--draft` 不会，要到管理页「客户端发布」点发布。
- Windows 下载的是 MSI（既是安装包也是更新包）；Mac 下载的是 dmg，`.app.tar.gz` 只给自动更新用。
- 上架成功以 `verify` 为准：下载页数据里的版本等于本次版本，且匿名下载的 sha256 与登记一致；最后把下载页地址和文件直链交给用户。
- 同一服务器同平台只保留最新已发布版本，旧版本会被撤下，下载页随之切换。

## 命令

在仓库内任意目录运行（或加 `--repo <仓库根目录>`）。`<skill>` 指本 skill 目录，仓库内为 `.claude/skills/tsimsoft-signedbuild-deploy`。

```
node <skill>/scripts/signed-build.mjs check   [--server test|prod|both]
node <skill>/scripts/signed-build.mjs dry-run [--server ...] [--deps <Source-deps.zip>]
node <skill>/scripts/signed-build.mjs publish [--server ...] [--draft] [--skip-build] [--deps <Source-deps.zip>]
node <skill>/scripts/signed-build.mjs verify  [--server ...] [--version X.Y.Z]
```

- `--server` 默认 `test`（客服系统测试服）；`both` 先测试服后生产服。
- `publish` 默认直接发布（平台脚本 `--publish` / `-Publish`），完成后自动 `verify`；`--draft` 只建草稿，要到管理页「客户端发布」手动发布。
- `--skip-build` 只用于上传失败后重传上次产物，不重新构建。
- Windows 构建必须带依赖源码包（GPL 门槛）：不给 `--deps` 时用 `target/tsim-publish/<版本>/deps/` 里已有的，没有就先跑
  `scripts/package-dependency-sources.ps1` 生成（数百 MB，需联网）。
- `dry-run` 和 `publish` 构建要 5–15 分钟（Windows 首次生成依赖源码包更久），用后台方式运行，等完成通知，不要轮询。

## 标准流程

1. **check**：全部 ✔ 才继续。`✘` 按提示修本机环境；不要绕过脏工作区。
2. **版本**：服务器已在服务同一版本或更高版本时，先用 `scripts/version.mjs` 升版本（各清单一致），并写
   `release-notes/<版本>.txt`（客服看得懂的中文，逐条写可见变化和操作提醒，1–4000 字），一起提交。
3. **dry-run**：必须看到 `packaged and verified` 和 `dry run: nothing uploaded`。
4. **publish**：测试服可直接执行。**生产服（`--server prod` 或 `both`）必须先得到用户在对话里的明确同意**：
   发布即推送给所有装了客户端的人，且会撤下更低版本。
5. **verify**：确认 `latest.json` 版本、下载页数据、匿名下载 sha256 一致，把下载页和文件直链交给用户。

## 汇报

版本、平台、服务器、发布状态（published / 草稿）、更新签名 key id、文件名与大小、sha256、下载页与直链。
写明未验证的部分（例如没有在干净机器上实际安装、没测自动更新）。不要把 dry-run 通过说成已发布。

对外分发时提醒：Mac 包只支持 Apple Silicon，自签名未经 Apple 公证，首次打开需右键“打开”或在
「系统设置 › 隐私与安全性」点“仍要打开”；Windows 未配代码签名证书时 SmartScreen 会提示，选“仍要运行”。

## 密钥与安全边界

- 密钥只在仓库外：Mac `~/.quicktext-release.env`（`KEY=value` 行），Windows `%USERPROFILE%\.quicktext-release.env.ps1`
  （`$env:KEY = '...'` 行），可用 `QT_RELEASE_ENV` 改路径。平台脚本启动时自动加载。内容：
  - `TSIM_PUBLISH_KEY_TEST` / `TSIM_PUBLISH_KEY_PROD`：只在对应服务器有效，在管理页「客户端发布」生成（和 VBK 用的是同一台服务器）。
  - `TAURI_SIGNING_PRIVATE_KEY[_PATH]`、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`：Quicktext 自己的更新私钥，默认
    `~/.config/quicktext-signing/updater.key`。**不要用 VBK 的私钥**，必须与 `src-tauri/update-keys/current.pub` 配对。
  - Windows 可选 `QT_WIN_CERT_THUMBPRINT`（代码签名证书指纹，等同 `-CertificateThumbprint`）；Mac 可选
    `QUICKTEXT_SIGNING_IDENTITY` / `QUICKTEXT_SIGNING_KEYCHAIN`（默认 `Quicktext Local Signing`，每次发版用同一张证书）。
- `check` 只在子进程里加载并报告“有/格式对”，**任何时候都不要打印、复制或在对话里转述密钥值**。
- 服务端受信公钥：TSim 后端只接受为 `quicktext` 配置的更新公钥 key id；上传报“签名公钥不在受信列表中”时需要改服务器
  profile 并重建后端（属于 TSim_CS 部署，按 deploy-expert skill 执行，先征得用户同意）。

## 常见失败

| 现象 | 处理 |
| --- | --- |
| `工作区有未提交改动` | 提交或确认后再来 |
| `已在服务 X，... 不更高` | 升版本号并补更新说明 |
| `the update signature does not verify` | 私钥与 `current.pub` 不是一对：检查 env 文件里的 `TAURI_SIGNING_PRIVATE_KEY[_PATH]` |
| 上传 401 `发布密钥无效` | 发布密钥错或不属于这台服务器 |
| 上传 422 `签名公钥 ... 不在受信列表中` | 服务端没配置该 key id，见上文 |
| 上传 409 `version_not_newer` | 服务器已有该版本或更高版本 |
| 网络中断导致上传失败 | `publish --skip-build --server <失败那台>` 重传 |
| Windows 依赖源码包生成失败 | 检查网络与 cargo/npm；也可先手动生成后用 `--deps` 指定 |

Linux 不出包；Intel Mac 包不在本流程内。
