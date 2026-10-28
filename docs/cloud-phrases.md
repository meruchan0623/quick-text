# 团队话术（云端快捷话术表）

日期：2026-10-28　状态：实施中

Quicktext 可以订阅 TSim 客服系统维护的一张「团队话术」表。它与本机的「我的指令」完全分开：同步只读下载，不写 `active.json`，个人指令的保存、回滚、导入导出和 Excel 都不包含团队话术。服务端契约见 TSim_CS 仓库 `docs/specs/2026-10-28-quick-text-cloud-phrases.md`。

## 1. 下载内容

`GET {服务器地址}/integrations/quick-text/v1/phrase-table`，请求头 `Authorization: Bearer <同步密钥>`，有缓存时带 `If-None-Match`。

```json
{
  "format": "quicktext-cloud-phrases",
  "schema_version": 1,
  "revision": 12,
  "updated_at": "2026-10-28T08:30:00+00:00",
  "rules": [
    {
      "id": "6f1c2a0e-6c55-4b4e-9d43-0d6a3e1f2b7c",
      "enabled": true,
      "group": "退款",
      "description": "退款到账时间",
      "ignore_case": false,
      "match": { "kind": "literal", "value": "tkdz" },
      "trigger_mode": "terminator",
      "terminator": null,
      "ime_erasure": null,
      "action": { "type": "text", "method": "paste", "source": { "type": "inline", "value": "您好，退款一般 3–5 个工作日原路退回。" } },
      "parameter": { "type": "default_match" },
      "notification": { "enabled": false, "message": "" }
    }
  ]
}
```

- `rules[]` 就是 `Rule` 的序列化格式。读不了的条目（未知动作类型、字段类型错）跳过并计入 `unreadable_count`，不影响其他条目。
- 必须 `format == "quicktext-cloud-phrases"`；`schema_version` 必须是 1 及以下，更高则报 `schema_newer`，保留旧缓存。
- 客户端准入（与服务端校验相同的上限，客户端独立执行，在写缓存和编译之前）：
  - 整表：`rules` 数组超过 2000 项（含读不了的条目）时整表拒绝，报 `too_many_rules`，在读取任何条目之前判定；保留旧缓存与引擎现状。
  - 逐条：`match.kind` 必须是 `literal`，缩写不超过 64 个字符，正文不超过 20000 个字符；不符合的判为 `invalid`（见 §4），不进编译。
  - 启动时读缓存走同一套准入（防止旧版本写下或手改的缓存）。
- 响应：200 正文 + `ETag`；304 表示没变；401 表示密钥无效。

## 2. 本机存放

- `prefs.json` 的 `app.cloud`（不进导出、不进版本历史）：
  ```json
  { "enabled": false, "base_url": "", "key": "", "auto_sync_minutes": 30, "disabled_ids": [] }
  ```
  `auto_sync_minutes` 取 0（仅手动）、15、30、60、120、240；`disabled_ids` 是本机停用的团队话术 id（原始 id，不带前缀）。
- 数据目录 `cloud-phrases.json`：最近一次成功下载的表，`{ "source": "...", "etag": "...", "synced_at": "RFC3339", "table": { …下载内容… } }`，原子写入。启动时先读它，断网也能用。
  - `source` 是这份表的下载地址，按下载时同一规则规范化（`base_url` 去首尾空白和末尾 `/` 后拼接路径，协议和主机名小写），例如 `https://csapi.tsim.tech/integrations/quick-text/v1/phrase-table`。
  - 只有 `source` 与当前 `base_url` 规范化后的下载地址一致时才装载（启动时、修改设置时都核对）。不一致（含没有 `source` 的旧文件、地址为空或非法）的缓存一律不用并尽量删除；删除失败时 `status.last_error` 为 `io`，message 为「旧服务器地址的团队话术缓存（cloud-phrases.json）无法删除，其中的话术不会被使用；请检查数据目录的权限」。因为来源不符的缓存永远不会装载，删除失败也不会让旧服务器的话术在重启后重新生效。
  - `prefs.json` 读不出时本次不启用同步，缓存保持原样（不装载、不删除）。

## 3. 下载规则

- 地址：`base_url` 去掉末尾 `/` 后拼接路径。只允许 `https://`；`http://` 仅允许 `localhost`、`127.0.0.1`、`[::1]`（本地开发）。
- 超时 15 秒；正文上限 2 MiB（超过报 `too_large`）；`User-Agent: Quicktext/<版本>`。
- 时机：启动加载完成后先装载缓存，再在后台拉取一次；之后按 `auto_sync_minutes` 定时（0 只关闭定时器，启动时和修改设置后仍各同步一次）；「立即同步」手动触发；修改同步设置后立即同步一次。同一时间只跑一个同步。
- 不跟随重定向（按 `http_status` 处理）；代理只认环境变量 `HTTP(S)_PROXY` / `NO_PROXY`，不读系统代理设置。
- `base_url` 变化（按上面的规范化比较）时清除内存与磁盘上的缓存和同步记录，直到下次同步成功前团队话术为空（删除失败的处理见 §2）。
- 成功（200）：校验格式 → 写缓存 → 重新装载引擎 → 发 `cloud-state` 事件。304：只更新 `last_success_at`。失败：保留缓存和引擎现状，记录错误。
- 错误码：`not_configured`（未开启或缺地址/密钥）、`invalid_url`、`unauthorized`（401/403）、`network`、`timeout`、`http_status`（其他非 2xx/304）、`too_large`、`too_many_rules`（超过 2000 条，见 §1）、`invalid_format`、`schema_newer`、`io`（缓存读写失败）。
- 成功（200）的处理顺序里，「校验格式」包含 §1 的整表准入；逐条准入在合并时、编译之前进行。

## 4. 与「我的指令」合并

引擎装载的是「生效配置」：以个人配置为底（版本号、总开关、全局设置都取个人配置），规则列表 = 个人规则（原顺序）+ 生效的团队话术（id 加 `cloud:` 前缀，按下载顺序）。没开启订阅或没有团队话术时，生效配置与个人配置完全相同。

每条团队话术的状态：

| 状态 | 条件 | 进引擎 |
| --- | --- | --- |
| `active` | 其余条件都不满足 | 是 |
| `disabled_remote` | 云端 `enabled` 为 false | 否 |
| `disabled_local` | id 在 `disabled_ids` 里 | 否 |
| `shadowed` | 有**已启用**的个人规则与它触发相同：`match.kind` 相同，且 `match.value` 相同（任一方 `ignore_case` 为 true 时按小写比较） | 否，`shadowed_by` 记个人规则 id |
| `invalid` | 编译前的检查：个人规则已占用 `cloud:<id>` 这个 id；动作不是「`text` + `inline` 正文」；`match.kind` 不是 `literal`（`regex_not_allowed`，「团队话术只支持普通缩写，不支持正则」）；缩写超过 64 个字符（`match_too_long`）；正文超过 20000 个字符（`body_too_long`）；正文含 `{cliptext}`、`{selection}` 或 `{param:`（子串判断，偏保守）。通过以上检查的才进宽松编译，编译失败的也判 `invalid`。`problem` 记中文原因 | 否 |

团队话术只接受「粘贴/键入固定文字」：即使服务器被攻破或配置错误，也不会下发打开程序、读取本机文件等动作，正文也不能把本机剪贴板、选中文字或动作参数带进输出（这三种占位符的诊断码为 `local_data_placeholder`，problem「正文含 {cliptext}、{selection} 或 {param:…}，团队话术只输出固定文字」）。日期占位符 `{0:…}` 照常可用。被判 `invalid` 的话术不进生效配置，执行时也就不会为它们读取剪贴板或选区。TSim 服务端保存时同样拒绝这三种占位符。

引擎装载生效配置时，`config` 与 `compiled` 在同时持有两把锁（先 `config` 后 `compiled`，与 macOS 输入回调的顺序一致）的情况下一起替换，读取方不会看到「新缩写 + 旧正文」的组合。候选在匹配时就从同一份快照里取出规则副本（macOS 回调同时持两把锁匹配并按 id 取规则；Windows 从匹配用的 `compiled` 里取规则），执行器执行的是候选自带的 `rule.action`。所以替换之前匹配到的候选按它匹配时的规则执行，与在同步前一刻输入等价。仅团队话术变化的重新装载（同步更新、本机停用或启用、同步设置变化）**不**递增 `engine.generation`、不设置取消：执行器在逐字删除缩写、逐字输出时持续检查代次，递增会让正在执行的指令（包括个人指令）中途停止并留下残缺输入。保存、回滚、导入个人配置时照旧暂停、取消并递增代次。

团队话术的执行不计入个人配置的「最近一次可用版本」（last-known-good）：只有规则 id 属于当前个人 `active` 配置的成功执行才算（按 id 是否在个人规则中判断，不看 `cloud:` 前缀；合并已保证个人规则占用 `cloud:<id>` 时对应团队话术判为 `invalid`）。

编译前就判为 `invalid` 的话术不会被编译；进入编译的最多 2000 条、都是不超过 64 个字符的普通缩写，编译开销有上限。安全模式下团队话术不进引擎，也不编译：状态只来自编译前的检查（只有编译才会发现的问题此时仍显示为 `active`），离开安全模式后才合并并编译。

判定顺序：`disabled_remote` → `disabled_local` → `shadowed` → `invalid`。长短不同的重叠缩写沿用现有优先级（字面先于正则、长的先于短的、同级时个人在前）。个人配置保存、启动加载、同步完成、本机停用变化后都重新合并并装载引擎。`disabled_ids` 中对不上任何团队话术的 id 在下次成功同步后清理。

## 5. IPC 契约（新增）

命令：

| 命令 | 参数 | 返回 |
| --- | --- | --- |
| `get_cloud_state` | 无 | `CloudState` |
| `set_cloud_settings` | `{ settings: { enabled, base_url, key?, auto_sync_minutes } }`，`key` 省略或为 null 表示不改，空串表示清除；`base_url`、`key` 先去首尾空白 | `CloudState`。不等待同步：开启时在后台同步一次。设置非法时报 `validation`，诊断码为 `invalid_url`（`settings.base_url`）、`invalid_key`（`settings.key`）、`invalid_interval`（`settings.auto_sync_minutes`），message 为中文。开启但地址或密钥为空不算非法，同步时报 `not_configured` |
| `sync_cloud_now` | 无 | `CloudState`（等同步结束；同步失败不报错，看 `status.last_error`） |
| `set_cloud_rules_disabled` | `{ ids: string[], disabled: boolean }` | `CloudState` |

事件 `cloud-state`：载荷为 `CloudState`，在同步开始/结束、设置变化、本机停用变化、个人配置保存后发送。

```ts
interface CloudState {
  settings: { enabled: boolean; base_url: string; has_key: boolean; auto_sync_minutes: number };
  status: {
    syncing: boolean;
    last_attempt_at: string | null;   // RFC3339
    last_success_at: string | null;
    last_error: { code: string; message: string } | null;
    revision: number | null;          // 当前缓存表的 revision
    updated_at: string | null;        // 当前缓存表的 updated_at
    unreadable_count: number;
  };
  rules: Array<{
    id: string;                        // 原始 id（不带 cloud: 前缀）
    rule: Rule;                        // 云端原样规则
    status: 'active' | 'disabled_remote' | 'disabled_local' | 'shadowed' | 'invalid';
    disabled_local: boolean;           // id 是否在 disabled_ids 里（与 status 无关）
    shadowed_by?: string;              // 个人规则 id
    problem?: string;                  // 中文、面向用户
  }>;
}
```

- 密钥只写不读：`CloudState` 里只有 `has_key`。
- `last_error.message` 与 `problem` 会原样显示给用户，必须是中文、用户能看懂的文本；开发细节只进日志。
- `sync_cloud_now` 遇到正在进行的同步时等待并返回那次的结果，不再跑第二次。
- 安全模式下设置、同步、本机停用都放行（不写 `active.json`），引擎照常处于暂停。

## 6. 界面

- 「文本指令管理」顶部切换「我的指令 (N) | 团队话术 (M)」。
- 团队话术视图：同样的搜索、分组页签和表格列；「停用」列换成「本机停用」；多一列「状态」（生效、云端已停用、本机停用、被我的指令覆盖、无法使用）；行操作「查看」（只读详情）、「复制到我的指令」（新 id，其余字段照抄，经现有保存流程写入个人表，之后副本覆盖云端那条）；批量「本机启用 / 本机停用」；没有新增、编辑、删除、导入。顶部显示来源、版本、上次同步时间和「立即同步」，失败时显示错误。
- 「我的指令」里覆盖了团队话术的规则显示「覆盖团队话术」标记。
- 「参数设置」新增「团队话术同步」：开关、服务器地址、同步密钥（只写，已设置时显示「已设置」并可替换或清除）、自动同步间隔、「保存并同步」。
- 运行记录中 `cloud:` 开头的规则 id 用团队话术表查名称。
- 总开关「启用文本指令」同时控制两张表。
