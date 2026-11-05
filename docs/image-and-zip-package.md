# 粘贴预设图片与 ZIP 配置包（跨项目契约）

日期：2026-11-04　状态：实施中

本文件是 Quicktext（本仓库）与 TSim_CS 共用的契约。团队话术的其余规则见 [团队话术](cloud-phrases.md)，服务端见 TSim_CS `docs/specs/2026-10-28-quick-text-cloud-phrases.md`。

## 1. 「粘贴图片」动作

```json
{ "type": "image", "asset": "<sha256 小写十六进制>.png", "name": "二维码.png" }
```

- `asset`：`^[a-f0-9]{64}\.(png|jpg|gif|bmp|webp)$`。哈希是文件字节的 SHA-256，扩展名由文件签名决定（不看原文件名），`.jpeg` 一律写成 `.jpg`。
- `name`：原文件名，只用于显示，缺省为空串，≤ 200 字符。
- 图片 ≤ 10 MiB，签名须是 PNG / JPEG / GIF / BMP / WebP。选图时复制进资源库，之后不再依赖原文件。
- 执行：删除缩写 → 图片写入剪贴板 → 发送粘贴键 → 按现有规则恢复剪贴板。**不判断目标文本框是否支持图片**：能贴上就贴上，贴不上不算错误，结果照常报告 `unknown`。资源文件缺失或损坏时在删除缩写之前报错。
- 旧版客户端把未知动作计入 `unreadable`，所以 `schema_version` 不变。

## 2. Quicktext 资源库

数据目录下 `assets/<asset>`，内容寻址，个人指令和团队话术共用同一目录。历史版本回滚仍可能引用旧图片，因此删除指令不删文件。

## 3. ZIP 配置包

```
package.json            UTF-8；以下两种之一
assets/<asset>          package.json 引用的图片，可有零到多个
```

- `package.json` 的 `format`：
  - `quicker-text`：Quicktext 配置包（`{format, version, config, unresolved}`，现有格式不变）。
  - `quicktext-cloud-phrases`：TSim 话术表（`{format, schema_version, revision, updated_at, rules}`，现有格式不变）。
- 两边都能读两种 `format`。旧的裸 JSON 文件继续可以导入；导出一律是 ZIP。
- 读取时的安全规则（两边相同，不符合则整包拒绝，不写入任何内容）：
  - 只取 `package.json` 与匹配 `assets/` + §1 正则的条目，其他条目（如 `__MACOSX/`）忽略且绝不解压到磁盘；路径穿越的名字因此不可能被写出。
  - 条目 ≤ 2100 个；`package.json` ≤ 16 MiB；单张图片 ≤ 10 MiB；解压总量 ≤ 128 MiB。按实际读出的字节数限制，不信任 ZIP 头里声明的大小。拒绝加密条目。
  - 每张图片校验：文件名中的哈希等于内容 SHA-256、扩展名与签名一致。
- 规则引用的图片在包内和资源库都没有时：该规则仍导入，但禁用并给出提示（Quicktext）；TSim 导入预览时将其列为跳过，原因「图片不在配置包中」。

## 4. 操作类型的记录与执行

TSim_CS 记录 Quicktext 的**全部**操作类型：`none`、`text`、`html`、`hotkey`、`send_keys`、`steps`、`replay`、`open`、`reusable`、`common`、`quicker`、`image`。TSim_CS 不解析执行，只校验结构后原样保存，导入导出无损。

通过同步进入引擎的团队话术只放行以下类型，其余判 `invalid`（诊断码 `automation_not_allowed`），不下发执行：

| 类型 | 同步后能否运行 |
| --- | --- |
| `text`、`send_keys`（`inline` 正文，规则不变） | 能 |
| `html`、`hotkey`、`image`、`open` | 能 |
| `steps`、`replay` | 能，但步骤里不能有 `common`、`reusable`（`automation_not_allowed`）和 `paste_files`、`paste_image`（`action_not_allowed`） |
| `none`、`reusable`、`common`、`quicker` | 否：`problem` 为「这类操作不通过团队话术同步运行；可导出配置包后导入「我的指令」，审阅后启用」 |

风险：`open`、`steps`、`replay` 同步后不经坐席确认就可运行，同步密钥或服务器一旦泄露，等于能在所有坐席电脑上运行程序。ZIP 导入「我的指令」时沿用现有审阅流程：自动化动作导入后禁用，用户审阅后手动启用。`html` 正文、`plain_text` 和步骤里的文字同样不得含 `{cliptext}`、`{selection}`、`{param:`。

## 5. 同步图片

`GET /integrations/quick-text/v1/phrase-assets/{asset}`，`Authorization: Bearer <同步密钥>`：200 图片字节（`Cache-Control: private, max-age=31536000, immutable`）；401 同话术表；404 `asset_not_found`。

客户端：话术表 200 或 304 后，下载表中 `image` 动作缺失的资源（缺失时 304 也要补）。超时 15 秒、不跟随重定向、≤ 10 MiB，校验 SHA-256 与签名后原子写入 `assets/`。单张失败不影响其他话术，该话术判 `invalid`（`asset_missing`，「图片尚未下载，将在下次同步时重试」），并在 `last_error` 记 `asset_download`。

## 6. TSim_CS 管理接口（前缀 `/agent/settings/quick-text-phrases`）

| 接口 | 权限 | 说明 |
| --- | --- | --- |
| `POST /assets`（multipart `file`） | 编辑 | 校验后保存，返回 `{ "asset": "...", "name": "...", "size": 123 }` |
| `GET /assets/{asset}` | 查看 | 图片字节，用于预览 |
| `POST /import-package`（multipart `file`） | 编辑 | 按 §3 解析，保存图片，返回 `{ "package": <package.json 内容>, "assets": ["..."] }`；不改话术表，由前端走现有预览与保存。错误：422 `invalid_package` + 中文 `message` |
| `GET /export-package` | 查看 | `application/zip`，文件名 `quicktext-cloud-phrases-v<revision>.zip` |

`PUT` 保存时，`image` 动作的 `asset` 必须已存在，否则 422（`action.asset`，「图片不存在，请重新上传」）。

## 7. Quicktext IPC

| 命令 | 参数 | 返回 |
| --- | --- | --- |
| `import_image_asset` | `{ path }` | `{ asset, name }`，校验后复制进资源库 |
| `preview_image_asset` | `{ asset }` | 图片 data URL，同现有 `preview_image` |
| `export_config` | 同现有 | 写 ZIP，默认文件名 `quicktext-config.zip` |
| `import_config` | 同现有 | 另接受 `.zip`，且 `format` 可为 `quicktext-cloud-phrases` |

`choose_file("import")` 过滤器为 `json`、`xlsx`、`zip`。
