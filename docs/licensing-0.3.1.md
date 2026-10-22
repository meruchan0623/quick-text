# Quicker Text 0.3.1 的许可证说明与源码获取

状态：草稿，2026-10-22。已核对的内容直接写出；需要维护者对照已发布的 0.3.1 文件确认的内容标为【待核实】，核实后删去标记，再把全文贴到 0.3.1 的发布页面（或随发布页面提供链接）。这份文档不是法律意见。

## 适用范围

Quicker Text 0.3.1，即已经发布的 Windows x64 绿色包 `Quicker-Text-0.3.1-Windows-x64-Portable.zip`，以及用同一份源码构建的其他交付物【待核实：0.3.1 是否另发过单文件 exe 或安装包】。

0.4.0 起的发布按 [发布清单](release-checklist.md) 的许可证核对项执行，每个发布都附带 GPL 全文、第三方声明和 `Quicker-Text-<版本>-Source.zip`；本文只处理已经发出去的 0.3.1。

## 许可证声明

Quicker Text 0.3.1 的整个项目，包括自研代码和静态纳入的 espanso 原生组件，按 GNU 通用公共许可证第 3 版或（由你选择）更新版本，即 GPL-3.0-or-later 授权与分发。许可证全文见仓库根目录的 [LICENSE](../LICENSE)。

依据：

- 0.3.1 的可执行文件静态链接了 espanso v2.4.1 的 Windows 原生检测与注入代码（`vendor/espanso-win32/{detect,inject}/native.{cpp,h}`，版权 Federico Terzi，GPL-3.0-or-later，提交 `a87d1723f47a59e38d370e2f0c636d66d568b8f6`）。这样的组合程序只能按 GPL 分发。
- 项目维护者已确认整个项目采用 GPL-3.0-or-later（开发计划附录 C.1 第 23 项，2026-10-22）。

对 0.3.1 发布包的更正：0.3.1 发布包里的根目录 `LICENSE` 是 MIT 文本，随包的说明文件对自研文件也保留了 MIT 的说法【待核实：对照已发布包内 `LICENSE`、`README.md`、`THIRD_PARTY_NOTICES.md` 的原文，逐条列出】。这与「组合程序按 GPL 分发」不一致。以本文为准：拿到 0.3.1 的人，可以依 GPL-3.0-or-later 使用、复制、修改和再分发整个程序及其对应源码。包内的第三方声明本来就写明了 espanso 部分适用 GPL-3.0-or-later，这一点没有变化。

其他依赖（Rust crates、npm 包）各自保留自己的许可证，不因本声明改变。仓库用 `cargo deny` 对 Rust 依赖的许可证做兼容性检查，说明见 [第三方声明](../THIRD_PARTY_NOTICES.md)。

## 获取 0.3.1 的对应源码

下面三种方式，核实完成前请以第一种为准。

1. 已发布绿色包内的 `source/` 目录。0.3.1 的打包脚本把工作区代码、`vendor/espanso-win32`（含本地补丁）、`Cargo.lock`、`package-lock.json` 和构建、打包脚本放进这个目录，并把每个文件的 SHA256 列在包内的 `SHA256SUMS.txt`。校验方法：解压后对每个文件计算 SHA256，与 `SHA256SUMS.txt` 对照。【待核实：用已发布的 ZIP 实际解压并校验一次】
2. 单独发布的源码包 `Quicker-Text-0.3.1-Source.zip`【待核实：0.3.1 是否单独发布；发布位置；SHA256 `……`】。0.3.1 若没有单独发布源码包，把绿色包内 `source/` 目录另行打成 ZIP 放到发布页面即可，不需要重新构建。
3. 版本库快照：提交 `b359598`（`chore: 0.3.1 baseline`，2026-10-22）的源码树。它与已发布包的差别应当只有 `LICENSE`、`README.md`、`THIRD_PARTY_NOTICES.md` 以及 `package.json` 的 `license` 字段，这几处在 2026-10-22 已改为 GPL-3.0-or-later 口径【待核实：把已发布包的 `source/` 与该提交逐文件比较，记录差异】。版本库目前没有 `v0.3.1` 标签；比较完成后建议在该提交上补打标签，让「0.3.1 的源码」有固定的指向。

依赖的源码：crates.io 与 npm 的依赖没有复制进 `source/`。它们的确切版本和校验和由 `Cargo.lock` 与 `package-lock.json` 固定，构建时从 crates.io 与 npm 下载。要不要把这些依赖的源码也随包提供（例如 `cargo vendor`），是尚未做出的决定，见发布清单的「依赖源码的可获得性」一节。在做出决定之前，本文不声称依赖源码已随包提供。

## 从源码重建 0.3.1

环境：Windows x64、Rust stable（MSVC 工具链）、Node.js、MSVC C++ 生成工具与 Windows SDK、PowerShell 7；运行程序需要 WebView2。0.3.1 没有固定 Rust 与 Node.js 版本【待核实：记录构建 0.3.1 时的 `rustc --version` 与 `node --version`】。

在 `source/` 目录（或版本库快照）中运行：

```
npm ci
npm run build
cargo build -p quicker-text --release
```

得到 `target\release\quicker-text.exe`。重新生成绿色包：`./scripts/package-portable.ps1 -OutputDirectory <新目录>`（输出目录必须是新目录，脚本不会覆盖已有交付包）。更详细的步骤见 `docs/build-verification.md`。

不承诺重建的 exe 与已发布的 exe 逐字节相同：编译器版本、构建路径和依赖下载时间都会影响结果。承诺的是：源码、构建脚本和依赖版本清单足以重新构建出功能相同的程序。

## 待办汇总（维护者）

- 对照已发布的 0.3.1 文件，核实带【待核实】的条目，补上源码包的位置与 SHA256。
- 把本文贴到 0.3.1 的发布页面，或在页面中链接到它。
- 给版本库中对应的提交补打 `v0.3.1` 标签。
- 决定依赖源码是否随发布提供（发布清单「依赖源码的可获得性」）。
