# 发布清单

每次发布（含补丁版本）按本清单执行，并把填好的副本和结果存档（发布页面或 `docs/` 下的记录）。分两部分：`release` 工作流（`.github/workflows/release.yml`）自动检查的项目，以及只能由人完成的项目。工作流只构建和检查，不发布任何东西；GitHub Release 由维护者手工创建。

## 0. 触发

- [ ] 工作区里的版本号已统一：`package.json`、`package-lock.json`、`src-tauri/tauri.conf.json`、各 Cargo 包。推送的标签必须是 `v<主>.<次>.<修订>`，并等于这个版本号（工作流第一步检查，不一致立即失败）。
- [ ] 已写好 `docs/windows-<版本>-notes.md`。`scripts/package-artifacts.ps1` 把它作为「先读与测试」放进发布包，缺少时直接报错，避免新版本带着旧版本的说明。
- [ ] 想先看结果、不打标签：在 Actions 页面对要发布的分支手工运行 `release`（`workflow_dispatch`）。它执行同样的构建和检查，产物同样只是工作流附件，发布说明里会写明这是试运行。

## 1. 自动检查（`release` 工作流）

任何一项失败，这个版本就不能发布。

| 检查 | 脚本或命令 | 保证什么 |
| --- | --- | --- |
| 依赖许可证、禁用项、来源 | `licenses` 任务：`cargo deny check bans licenses sources` | 依赖许可证都在 `deny.toml` 的 GPL-3.0-or-later 兼容清单内；没有 `openssl-sys`；没有 crates.io 以外的来源（B-51、S-28） |
| 版本一致 | `scripts/check-release-version.ps1` | 标签、清单文件、Cargo 包版本相同 |
| 扫描器自测 | `scripts/check-binary-strings.ps1 -SelfTest` | 下面的 PDB 检查本身能发现问题 |
| release 没有启用测试特性 | `scripts/check-release-features.ps1` | `cargo tree -e features` 显示 `fault-injection`、`mock` 在发布配置里都没有启用；没有任何 crate 再定义这些特性时也会失败，防止检查变成空检查（B-52）。另有编译期兜底：`debug_assertions` 关闭（release 配置）时启用这两个特性中的任何一个，`src-tauri/src/main.rs` 的 `compile_error!` 让构建直接失败；需要带故障注入的构建只能用 debug 构建或打开了 `debug-assertions` 的配置 |
| 源码白名单覆盖所有 crate | `scripts/check-source-allowlist.ps1 -Strict`（构建前）；对生成的 `Source.zip` 再检查一次 | 发布的源码包没有漏掉工作区文件，也没有带入配置、日志等私有文件（B-51） |
| 钩子模块头部检查 | `scripts/check-hook-lints.ps1` | S-02 |
| 测试 | `cargo test --workspace --locked` | |
| 构建没有改依赖锁文件 | `git diff --exit-code -- Cargo.lock package-lock.json` | 发布的依赖版本就是仓库里固定的版本 |
| PDB 与 exe 中没有故障注入函数 | `scripts/check-binary-strings.ps1`，对归档的 PDB 与 `quicker-text.exe` | 不含 `debug_inject_fault`；同一次扫描要求 PDB 含 `quicker_platform`、exe 含 `com.quickertext.desktop`，空文件或编码不对的文件通不过 |
| 第三方版权与许可证文字 | `scripts/generate-third-party-licenses.ps1`（`notices` 任务在 PR 里也会跑 `-CheckOnly`） | 为实际分发的每个 Rust crate 和 npm 包收集版权与许可证原文，生成 `THIRD-PARTY-LICENSES.txt`；任何一个包找不到许可证文字就失败，不写文件（S-28） |
| 依赖源码包（先于安装包构建） | `scripts/package-dependency-sources.ps1` | 生成 `Quicker-Text-<版本>-Source-deps.zip`：`cargo vendor --locked` 的全部 crate、会进入前端产物的 npm 生产依赖（用 `npm pack` 取的注册表原始 tarball，放在 `npm/tarballs/`，每个都按 `package-lock.json` 的 sha512 `integrity` 校验）、离线重建说明；检查实际分发的每个 crate 都在、数量与 `Cargo.lock` 一致、只靠这些源码 cargo 能离线解析工作区；写完后用门槛函数把成品再验一遍，不通过就删掉这个包（S-28、B-51）。不需要先 `npm ci` |
| 安装包构建的门槛 | `src-tauri/tauri.conf.json` 的 `build.beforeBundleCommand`，即 `scripts/check-bundle-preconditions.ps1` | Tauri 在编译完成、写安装包之前运行它。声明文件是占位文件、没有设置 `QT_DEPENDENCY_SOURCES_ZIP`、依赖源码包与本树锁文件不符或不完整，或平台不是 Windows x64，都使打包失败，不产出安装包（见下面「产出可分发二进制的路径」） |
| 安装包带着生成的声明文件 | `release` 工作流「Installer bundles the generated notice file」 | 构建后 `src-tauri/resources` 与 `target/release` 里的 `THIRD-PARTY-LICENSES.txt` 都是刚生成的那份（哈希相同），不是占位文件；另用 7-Zip 列出安装包内容，列不出只给警告（见第 2 节人工核对） |
| 打包 | `scripts/package-artifacts.ps1` | 安装包、exe、GPL 全文、第三方声明、`THIRD-PARTY-LICENSES.txt`、说明文档、`Source.zip`、`Source-deps.zip`、`SHA256SUMS`。`-DependencySourcesZip` 必填，并按本树的锁文件逐项校验；缺少、损坏、过期或不完整的依赖源码包，或者声明文件是占位文件、不是用本树的锁文件生成的，直接失败且不留输出：没有对应源码的完整版本不能发布。复制之前先对将要打包的那个 exe 重跑 B-52 检查（`scripts/release-binary-checks.ps1`：`check-release-features.ps1`；`target\release\quicker_text.pdb` 必须是这个 exe 的 PDB，按 exe 调试目录里的 GUID 核对；PDB 与 exe 的 `check-binary-strings.ps1`），任何一项失败都以非零退出码终止 |
| 许可证与源码获取文字 | `scripts/write-release-notes.ps1` | 生成 `release-notes.md`，写明文件名、`Source.zip` 与 `Source-deps.zip` 的 SHA256、两者合起来才是完整对应源码，以及版本库位置 |

PDB 检查的做法写在 `scripts/check-binary-strings.ps1` 的头部注释里：对文件做原始字节扫描，同时查 ASCII 与 UTF-16LE。限制：PDB 按 4 KiB 块存放流，正好跨块的名字理论上会漏；同一个符号在多条记录里出现，全部同时跨块的可能性可以忽略。

### 产出可分发二进制的路径（S-28 硬门槛）

原则：任何能产出可分发二进制的路径，在依赖源码缺失时必须失败，不能只给警告。下表是仓库里所有这样的路径；新增脚本或工作流步骤能产出安装包、更新包或 ZIP 时，先把它加进来并接上同样的门槛。门槛的检查逻辑只有一份：`scripts/third-party-set.ps1` 里的 `Assert-DependencySourcesZip`（文件名与版本；在读取或哈希任何条目之前，先对整个 ZIP 的条目名做一次规范性检查，使 Windows 上解压出的文件树与被校验的条目一一对应：条目名不重复，不含仅大小写不同的别名（Windows 上后者会覆盖前者，例如多出一个未被校验的 `CARGO.TOML`），不含反斜杠、冒号、开头的 `/`、空段（`a//b`）、`.` 或 `..` 段、Windows 保留设备名（`CON`、`NUL`、`COM1` 等，带不带扩展名都算）、以点或空格结尾的段，也不含同名的文件与目录，或把文件当目录用；清单里的 `Cargo.lock`、`package-lock.json` 哈希与本树一致；每个 crate 在包里，其 `.cargo-checksum.json` 的 `package` 与 `Cargo.lock` 的校验和相符，并且该文件列出的每一个文件，包括源码、`build.rs`、`Cargo.toml`，都在包里且 SHA256 与列出的一致；每个 npm 包都以注册表原始 tarball 的形式在 `npm/tarballs/` 里，tarball 的 sha512 等于 `package-lock.json` 的 `integrity`；任何不符都抛错）和 `Assert-ThirdPartyLicensesFile`（声明文件不是占位文件，且由本树的锁文件生成）。在当前的 559 个 crate、28312 个文件、5 个 npm tarball 上，一次检查约 3 到 8 秒。

这个检查的防护范围：防止不小心发出不完整、过期或不匹配的依赖源码包（复制到一半的文件、另一个提交生成的包、丢了源码文件或 `build.rs`、字节被改动、重新打包时多出一个在 Windows 上会覆盖已校验文件的陈旧别名条目）。它不防有人同时改文件和 `.cargo-checksum.json`：`cargo vendor` 解包之后不留原始 `.crate`，`.cargo-checksum.json` 里的 `package` 只能与 `Cargo.lock` 的校验和比对，不能从包里的文件重新算出；这个包由发布机上的 `scripts/package-dependency-sources.ps1` 生成，不是不受信任的输入。npm 部分比这更强：tarball 直接与锁文件里的 `integrity` 比，绑定到上游发布的那个文件。

| 路径 | 门槛 |
| --- | --- |
| `release` 工作流（标签推送与 `workflow_dispatch` 是同一个任务） | 先生成依赖源码包，再构建安装包（`beforeBundleCommand` 检查），再 `package-artifacts.ps1`（再检查一次）。只上传 `package-artifacts.ps1` 的输出、发布说明与 PDB；没有更新包（未配置 Tauri updater），`ci.yml` 不产出安装包或 ZIP |
| `scripts/package-artifacts.ps1`（发布包） | `-DependencySourcesZip` 必填；B-52 检查（`scripts/release-binary-checks.ps1`）；检查全部通过之后才写第一个文件，中途失败时清理本次写出的文件 |
| `scripts/package-portable.ps1`（绿色包） | `-DependencySourcesZip` 必填；B-52 检查（同上）；检查全部通过之后才写第一个文件；依赖源码包作为附属文件复制到 ZIP 旁边，ZIP 内的 `DEPENDENCY-SOURCES.txt` 写明文件名与 SHA256；中途失败时清理 ZIP、目录和附属文件 |
| `npm run tauri build`、`tauri bundle`、`scripts/build-windows.ps1`（安装包） | `build.beforeBundleCommand`（`scripts/check-bundle-preconditions.ps1`）。`scripts/build-windows.ps1 -DependencySourcesZip <...>` 提前检查并设置 `QT_DEPENDENCY_SOURCES_ZIP`；直接运行 `npm run tauri build` 而没有这个变量时失败 |
| `npm run tauri -- build --no-bundle`、`cargo build --release` | 不打包，只产生 `target\release\quicker-text.exe`，无法挂钩子。这是本机自用的中间产物，不得直接分发；要分发只能经过上面的 `package-artifacts.ps1` 或 `package-portable.ps1` |

发布绿色包：

- [ ] 用 `package-portable.ps1 -OutputDirectory <新目录> -DependencySourcesZip <Source-deps.zip>` 生成（命令见 README「许可与绿色包构建」）。输出目录里有绿色包 ZIP 和 `Quicker-Text-<版本>-Source-deps.zip` 两个文件，一起上传到发布页面；ZIP 里的 `DEPENDENCY-SOURCES.txt` 写的 SHA256 与附属文件一致（`Get-FileHash -Algorithm SHA256`）。

## 2. 许可证核对（S-28，手工）

- [ ] `LICENSE` 是未改动的 GPL-3.0 全文（工作流打包的 `Quicker-Text-<版本>-GPL-3.0.txt` 就是它）。
- [ ] 许可证字段一致，都是 `GPL-3.0-or-later`：`package.json` 的 `license`、根 `Cargo.toml` 的 `license`、`crates/domain/Cargo.toml`、`src-tauri/tauri.conf.json` 的 `bundle.license`。`bundle.licenseFile` 指向 `../LICENSE`。
- [ ] `README.md`、`THIRD_PARTY_NOTICES.md` 以及安装包内的说明没有「自研文件保留 MIT」之类的说法。检索：`git grep -n -i "\bMIT\b" -- README.md THIRD_PARTY_NOTICES.md src-tauri/tauri.conf.json`，凡是命中，确认它说的是第三方依赖而不是本项目。历史记录文档（`docs/espanso-source-audit.md`、`docs/build-verification.md`）里的旧口径是当时的记录，不改。
- [ ] 安装向导的许可证页显示的是 GPL 全文，不是空白或其他文本。`licenseFile` 的相对路径依据的是 Tauri 的约定【待核实：第一次构建安装包时实际运行安装程序确认；路径不对就改 `tauri.conf.json`】。
- [ ] 安装包带着第三方声明（S-28，必做）。`tauri.conf.json` 的 `bundle.resources` 把 `THIRD-PARTY-LICENSES.txt`、`THIRD_PARTY_NOTICES.md`、`LICENSE.txt`（GPL 全文）和 `LICENSE-ESPANSO-GPL-3.0.txt` 放到程序目录。在干净机器上装好安装包，到安装目录确认这四个文件都在，`THIRD-PARTY-LICENSES.txt` 是生成的那份（开头写着 `Generated by scripts/generate-third-party-licenses.ps1`，不是占位文字）。【待核实：Tauri 2 的 NSIS 打包把资源放在安装目录根部还是 `resources` 子目录，第一次装完实际看一次；工作流里的 7-Zip 列表只是参考】绿色包（`package-portable.ps1`）里同样要有 `THIRD-PARTY-LICENSES.txt`，输出目录里要有依赖源码包 `Source-deps.zip`，ZIP 里的 `DEPENDENCY-SOURCES.txt` 写着它的文件名和 SHA256。
- [ ] `cargo deny check` 在本地也通过：`bans`、`licenses`、`sources` 必须通过；`advisories` 逐条读一遍，处理不了的写进 `deny.toml` 的 `ignore` 并写明理由和复查日期。`deny.toml` 的 `exceptions` 目前为空，新增任何一条例外都要在提交说明里写理由，并在本次发布记录中列出。
- [ ] 本版新增的依赖，在加入之前已经查过许可证（`cargo deny check licenses` 失败就是提醒）；新增的 `deny.toml` 允许项有理由。
- [ ] 本版新增的第三方代码或数据（例如 OpenCC 词表、英文词表、字体、图标）在 `THIRD_PARTY_NOTICES.md` 里有条目：来源、版本或提交、版权、许可证、本地修改。许可证待核实的先写明待核实，不要沉默。
- [ ] 前端运行时依赖的许可证。`cargo deny` 只管 Rust 依赖，npm 依赖的兼容性靠人：`THIRD-PARTY-LICENSES.txt` 开头的「Package index」列出了每个包的许可证标识（npm 部分只有 `@tauri-apps/api`、`react`、`react-dom`、`scheduler` 以及构建工具 `vite` 的模块预加载代码），逐行确认与 GPL-3.0-or-later 兼容。生成器找不到许可证文字会直接失败，所以「包没有声明许可证」的情况不会静默通过；新增了运行时依赖或改了 `scripts/third-party-set.ps1` 里的 `$ThirdPartyNpmExtras` 时，重新核对。
- [ ] 关于页（U-16）显示 GPL-3.0-or-later、espanso 的来源与许可、源码获取方式。关于页实现之前，本项不适用，把「安装包许可证页 + 发布页面文字」作为替代并在记录中注明。
- [ ] `Source.zip` 抽查：解压后 `LICENSE`、`vendor/espanso-win32/LICENSE`、`Cargo.lock`、`package-lock.json`、`scripts/` 都在，README 里的构建命令能照着执行。`src-tauri/resources/THIRD-PARTY-LICENSES.txt` 在源码包里是占位文件（第一行 `QT-THIRD-PARTY-LICENSES-PLACEHOLDER`），这是预期的：真正的声明文件是单独的发布附件，由脚本生成。
- [ ] 第三方版权与许可证署名（必做，没有可选项）。发布包里必须有 `Quicker-Text-<版本>-THIRD-PARTY-LICENSES.txt`，安装包与绿色包内也各有一份；缺失、是占位文件、或与本树 `Cargo.lock`/`package-lock.json` 不是同一次生成的，打包脚本都会失败。人工抽查：打开附件，文件头写明 Rust crate 与 npm 包的数量；随意挑几个包（建议 `windows`、`serde`、`react`、`vite`）确认版权行和许可证全文在里面；带有 `clarified` 标记的包（上游源码包没带许可证文件，文字来自 `scripts/third-party-license-clarifications.json` 里固定的上游文本）在依赖版本变化后需要重新核对，见下一条。
- [ ] 生成器因某个包没有许可证文字而失败时：到该包的上游仓库找到许可证原文，把逐字文本和来源网址（固定到版本号或提交）加进 `scripts/third-party-license-clarifications.json` 与 `scripts/license-clarifications/`。条目按名称加版本固定，版本一变就失效、要重新核对。不要用「许可证标识加通用模板」凑数：版权行必须是上游的。

### 依赖源码的可获得性（S-28，必做）

GPL-3.0-or-later 第 1 节的「对应源码」包括编进二进制文件的第三方代码，第 6 条（d）要求同等的获取途径。所以每个发布必须同时提供两个源码包，缺一不可，不存在「只给锁文件、已知并接受」的选项（注册中心将来下架某个版本，旧版本就取不到了）：

- `Quicker-Text-<版本>-Source.zip`：工作区代码、`vendor/espanso-win32`、锁文件、构建和打包脚本。
- `Quicker-Text-<版本>-Source-deps.zip`：`cargo vendor --locked` 的全部 crate、会打进前端产物的 npm 生产依赖（注册表原始 tarball，`npm/tarballs/*.tgz`）、`README-OFFLINE-BUILD.md`（离线重建步骤）。两个包合在一起才是完整对应源码，发布说明里也这样写。

由 `release` 工作流生成并作为前置条件检查：依赖源码包缺失、命名不对、与本树锁文件不符、里面缺 crate 或 npm 包，或者 crate 缺了 `.cargo-checksum.json` 列出的文件（源码、`build.rs`）、文件内容被改过、npm tarball 与锁文件的 `integrity` 不符，安装包构建和打包步骤都会失败。绿色包同理：`package-portable.ps1` 的 `-DependencySourcesZip` 必填，依赖源码包作为附属文件放在绿色包 ZIP 旁边（不放进 ZIP，约 100 MB 对约 5 MB 的下载太大，与安装包发布时两个源码包并列的方式一致），发布时两个文件一起上传。

- [ ] 发布附件里有 `Source-deps.zip`，`SHA256SUMS` 里有它的哈希，发布说明写着两个包合在一起是完整对应源码。
- [ ] 抽查 `Source-deps.zip`（约 100 MB，解压后近 800 MB）：`SOURCE-DEPS-MANIFEST.txt` 里的 `Cargo.lock` 与 `package-lock.json` 哈希与本版相同；按 `README-OFFLINE-BUILD.md` 在断网的机器上把两个包解到同一目录，`cargo metadata --offline --locked` 能通过（脚本在打包时已验证过一次，这里用另一台机器再做一次）。门槛只能挡住意外（防护范围见上面「产出可分发二进制的路径」一节开头），这次抽查是第二道：在发布机以外的机器上打开 `npm/tarballs/`，任选一个 `.tgz` 用 `tar -xzf` 解开，确认 `package/` 里有实现文件，不是空壳。
- [ ] 依赖增长到接近 GitHub 单个附件的上限（2 GiB）时，改用多个 ZIP 分卷并更新发布说明的写法，不要因此退回只提供锁文件。

## 3. 0.3.1 补发许可证说明

0.3.1 已经发出，包内是旧的 MIT `LICENSE` 加 GPL 第三方声明，见 [0.3.1 许可证说明](licensing-0.3.1.md)。首次发布 0.4.0 之前完成：

- [ ] 补发 0.3.1 的依赖源码包和第三方许可证文字。两个脚本都有 `-ProjectRoot` 参数，可以对 0.3.1 的版本库快照（`b359598`）运行：`git archive b359598` 解到一个新目录，在里面 `npm ci`（生成声明文件要用 `node_modules`；依赖源码包本身不需要，它的 npm 部分直接取注册表的 tarball）、`cargo fetch --locked`，再运行 `pwsh scripts/generate-third-party-licenses.ps1 -ProjectRoot <该目录> -OutputFile <文件>` 和 `pwsh scripts/package-dependency-sources.ps1 -ProjectRoot <该目录> -OutputDirectory <新目录>`。用当前仓库的脚本即可，不要往 0.3.1 的快照里加东西。2026-10-22 对 `b359598` 的快照试过两个脚本：声明文件覆盖 286 个 crate 加 5 个 npm 包，没有缺许可证文字的包；依赖源码包含 513 个 crate（其中 286 个实际分发）和 5 个 npm 包，约 104 MB，离线解析通过。两个文件随 0.3.1 的发布页面一起提供。

- [ ] 对照已发布的 0.3.1 文件，核实 `docs/licensing-0.3.1.md` 里所有【待核实】项，并删去标记。
- [ ] 把已发布 ZIP 的 `source/` 与版本库提交 `b359598` 逐文件比较，记录差异（预期只有 `LICENSE`、`README.md`、`THIRD_PARTY_NOTICES.md`、`package.json` 的 `license`）。
- [ ] 补上 0.3.1 源码包的位置和 SHA256；0.3.1 没有单独的源码包时，用绿色包内的 `source/` 打包发布。
- [ ] 把说明贴到 0.3.1 的发布页面，或在页面中放链接。
- [ ] 在 `b359598` 上补打 `v0.3.1` 标签（核实无误后）。

## 4. 构建与符号（B-52）

自动检查通过之后，这几项仍需人工：

- [ ] 下载 `…-symbols` 附件，**永久归档**（按版本，与发布包的 SHA256 放在一起）。GitHub 的工作流附件最长保留 90 天，过期后这个版本的崩溃记录就无法符号化。附件里的 `.pdb.txt` 记录了提交、rustc 版本以及 exe 和 PDB 的 SHA256。PDB 不放进安装包，也不放到发布页面。
- [ ] 确认 PDB 与 exe 的签名（GUID 和 age）匹配，并用一份测试崩溃记录符号化成功（开发计划 §7.5「构建配置」）。这一步目前没有自动化，依赖崩溃记录格式（S-08、B-09）定稿后再补脚本；在那之前由人工完成并记录结果。
- [ ] exe 体积与上一版比较；增加超过 5% 时按开发计划「构建配置」一节改为只在 CI 的带符号构建中生成 PDB。

## 5. 发布

- [ ] 下载 `…-package`、`…-release-notes` 附件，核对 `Quicker-Text-<版本>-SHA256SUMS.txt`（`Get-FileHash -Algorithm SHA256`）。`…-package` 里要有 `Source.zip`、`Source-deps.zip` 和 `THIRD-PARTY-LICENSES.txt`。发布绿色包时，绿色包 ZIP 与 `Source-deps.zip` 一起核对、一起上传。
- [ ] 在干净的 Windows 机器上运行安装包（或绿色版 exe）：能启动；安装向导显示 GPL 许可证页；程序目录里有 `THIRD-PARTY-LICENSES.txt`（第 2 节）；卸载干净。
- [ ] 按 `docs/acceptance.md` 和发布前手工清单完成实机最小集（记事本、Chrome、微信 PC 版，各配合微软拼音）并存档。真实钩子的注入测试没有自托管 runner 时，在这里以手工记录替代（开发计划 R-10）。
- [ ] 创建 GitHub Release（草稿）：上传 `…-package` 里的全部文件，包括 `Source.zip`、`Source-deps.zip`（体积大，上传需要一些时间）和 `THIRD-PARTY-LICENSES.txt`，一个都不能少；说明以 `release-notes.md` 的内容开头（许可证与源码获取），之后写更新内容和已知限制；说明平台定位（macOS 未验收）。
- [ ] 对照第 2 节核对完毕后再点发布。
