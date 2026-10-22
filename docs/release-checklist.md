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
| release 没有启用测试特性 | `scripts/check-release-features.ps1` | `cargo tree -e features` 显示 `fault-injection`、`mock` 在发布配置里都没有启用；没有任何 crate 再定义这些特性时也会失败，防止检查变成空检查（B-52） |
| 源码白名单覆盖所有 crate | `scripts/check-source-allowlist.ps1 -Strict`（构建前）；对生成的 `Source.zip` 再检查一次 | 发布的源码包没有漏掉工作区文件，也没有带入配置、日志等私有文件（B-51） |
| 钩子模块头部检查 | `scripts/check-hook-lints.ps1` | S-02 |
| 测试 | `cargo test --workspace --locked` | |
| 构建没有改依赖锁文件 | `git diff --exit-code -- Cargo.lock package-lock.json` | 发布的依赖版本就是仓库里固定的版本 |
| PDB 与 exe 中没有故障注入函数 | `scripts/check-binary-strings.ps1`，对归档的 PDB 与 `quicker-text.exe` | 不含 `debug_inject_fault`；同一次扫描要求 PDB 含 `quicker_platform`、exe 含 `com.quickertext.desktop`，空文件或编码不对的文件通不过 |
| 打包 | `scripts/package-artifacts.ps1` | 安装包、exe、GPL 全文、第三方声明、说明文档、`Source.zip`、`SHA256SUMS` |
| 许可证与源码获取文字 | `scripts/write-release-notes.ps1` | 生成 `release-notes.md`，写明文件名、`Source.zip` 的 SHA256 和版本库位置 |

PDB 检查的做法写在 `scripts/check-binary-strings.ps1` 的头部注释里：对文件做原始字节扫描，同时查 ASCII 与 UTF-16LE。限制：PDB 按 4 KiB 块存放流，正好跨块的名字理论上会漏；同一个符号在多条记录里出现，全部同时跨块的可能性可以忽略。

## 2. 许可证核对（S-28，手工）

- [ ] `LICENSE` 是未改动的 GPL-3.0 全文（工作流打包的 `Quicker-Text-<版本>-GPL-3.0.txt` 就是它）。
- [ ] 许可证字段一致，都是 `GPL-3.0-or-later`：`package.json` 的 `license`、根 `Cargo.toml` 的 `license`、`crates/domain/Cargo.toml`、`src-tauri/tauri.conf.json` 的 `bundle.license`。`bundle.licenseFile` 指向 `../LICENSE`。
- [ ] `README.md`、`THIRD_PARTY_NOTICES.md` 以及安装包内的说明没有「自研文件保留 MIT」之类的说法。检索：`git grep -n -i "\bMIT\b" -- README.md THIRD_PARTY_NOTICES.md src-tauri/tauri.conf.json`，凡是命中，确认它说的是第三方依赖而不是本项目。历史记录文档（`docs/espanso-source-audit.md`、`docs/build-verification.md`）里的旧口径是当时的记录，不改。
- [ ] 安装向导的许可证页显示的是 GPL 全文，不是空白或其他文本。`licenseFile` 的相对路径依据的是 Tauri 的约定【待核实：第一次构建安装包时实际运行安装程序确认；路径不对就改 `tauri.conf.json`】。
- [ ] `cargo deny check` 在本地也通过：`bans`、`licenses`、`sources` 必须通过；`advisories` 逐条读一遍，处理不了的写进 `deny.toml` 的 `ignore` 并写明理由和复查日期。`deny.toml` 的 `exceptions` 目前为空，新增任何一条例外都要在提交说明里写理由，并在本次发布记录中列出。
- [ ] 本版新增的依赖，在加入之前已经查过许可证（`cargo deny check licenses` 失败就是提醒）；新增的 `deny.toml` 允许项有理由。
- [ ] 本版新增的第三方代码或数据（例如 OpenCC 词表、英文词表、字体、图标）在 `THIRD_PARTY_NOTICES.md` 里有条目：来源、版本或提交、版权、许可证、本地修改。许可证待核实的先写明待核实，不要沉默。
- [ ] 前端运行时依赖的许可证。在仓库根目录运行（只列会进入打包产物的依赖，`dev` 依赖除外），逐行确认与 GPL-3.0-or-later 兼容：

  ```
  node -e "const l=require('./package-lock.json').packages;console.log(Object.entries(l).filter(([k,v])=>k&&!v.dev).map(([k,v])=>k.replace(/^.*node_modules\//,'')+' '+v.version+' '+(v.license||'?')).join('\n'))"
  ```

  输出里有 `?`（包没有声明许可证）时，打开该包的仓库确认后再发布。
- [ ] 关于页（U-16）显示 GPL-3.0-or-later、espanso 的来源与许可、源码获取方式。关于页实现之前，本项不适用，把「安装包许可证页 + 发布页面文字」作为替代并在记录中注明。
- [ ] `Source.zip` 抽查：解压后 `LICENSE`、`vendor/espanso-win32/LICENSE`、`Cargo.lock`、`package-lock.json`、`scripts/` 都在，README 里的构建命令能照着执行。
- [ ] 逐个 crate 的版权与许可证清单（随包提供的依赖署名列表）尚未做。目前依赖许可证由 `deny.toml` 把关，署名文字在上游源码包里。是否要生成清单（例如 `cargo about`）是遗留决定，发布记录里写明本版的选择。

### 依赖源码的可获得性

GPL 要求提供「对应源码」。目前 `Source.zip` 只含工作区文件：crates.io 与 npm 的依赖没有复制进去，由 `Cargo.lock`、`package-lock.json` 固定版本和校验和，构建时下载；发布说明里如实写明了这一点。这样做的风险：这些注册中心将来下架某个版本时，旧版本的源码就取不到了。更稳妥的做法是随发布提供 `cargo vendor` 和 npm 依赖的副本。这是尚未做出的决定，请维护者在第一次正式发布 0.4.0 之前决定：

- [ ] 选 A：继续只提供锁文件（现状）。在发布记录中写「已知并接受」。
- [ ] 选 B：每个发布另附依赖源码包（`cargo vendor` 的输出，加上 npm 的离线缓存或 `node_modules` 源码）。需要新增打包步骤和白名单规则，范围超出本清单，另开任务。

## 3. 0.3.1 补发许可证说明

0.3.1 已经发出，包内是旧的 MIT `LICENSE` 加 GPL 第三方声明，见 [0.3.1 许可证说明](licensing-0.3.1.md)。首次发布 0.4.0 之前完成：

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

- [ ] 下载 `…-package`、`…-release-notes` 附件，核对 `Quicker-Text-<版本>-SHA256SUMS.txt`（`Get-FileHash -Algorithm SHA256`）。
- [ ] 在干净的 Windows 机器上运行安装包（或绿色版 exe）：能启动；安装向导显示 GPL 许可证页；卸载干净。
- [ ] 按 `docs/acceptance.md` 和发布前手工清单完成实机最小集（记事本、Chrome、微信 PC 版，各配合微软拼音）并存档。真实钩子的注入测试没有自托管 runner 时，在这里以手工记录替代（开发计划 R-10）。
- [ ] 创建 GitHub Release（草稿）：上传 `…-package` 里的全部文件；说明以 `release-notes.md` 的内容开头（许可证与源码获取），之后写更新内容和已知限制；说明平台定位（macOS 未验收）。
- [ ] 对照第 2 节核对完毕后再点发布。
