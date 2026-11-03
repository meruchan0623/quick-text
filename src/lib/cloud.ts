// Team phrases (docs/cloud-phrases.md): display rules for the read-only phrase table synced from the
// TSim 客服系统, as pure functions. The components are in ../CloudPhrases.tsx.
import type { CloudError, CloudRuleEntry, CloudState } from '../ipc/types';
import { actionLabel } from '../model';
import { completeRules, isReadableRule, summarizeAction, type ReadableRule } from './rules';
import { isRecord } from './ruleShape';

/** The engine's rule id of a team phrase is its table id with this prefix (§4). */
export const CLOUD_PREFIX = 'cloud:';

/** The 自动同步间隔 choices (§2), in minutes; 0 is manual only. */
export const AUTO_SYNC_MINUTES: readonly number[] = [0, 15, 30, 60, 120, 240];

export const autoSyncLabel = (minutes: number): string =>
  minutes === 0
    ? '仅手动同步'
    : minutes >= 60 && minutes % 60 === 0
      ? `每 ${minutes / 60} 小时`
      : `每 ${minutes} 分钟`;

/** 状态 column, one per CloudRuleStatus (§4); a newer status reads 「其他（…）」. */
export const cloudStatusNames: Readonly<Record<string, string>> = {
  active: '生效',
  disabled_remote: '云端已停用',
  disabled_local: '本机停用',
  shadowed: '被我的指令覆盖',
  invalid: '无法使用',
};

/** CSS class of a status tag: the status itself, `other` for one newer than this frontend. */
export const cloudStatusClass = (status: string): string =>
  Object.hasOwn(cloudStatusNames, status) ? status : 'other';

/** One sentence per sync error code (§3). */
export const cloudErrorNames: Readonly<Record<string, string>> = {
  not_configured:
    '团队话术同步还没有设置完整：请在「文本指令参数设置」中开启订阅，并填写服务器地址和同步密钥。',
  invalid_url:
    '服务器地址无效：只支持 https:// 开头的地址（本机调试可用 http://localhost）。请在「文本指令参数设置」中修改。',
  unauthorized: '同步密钥无效或已被停用，请向管理员获取新的密钥后在「文本指令参数设置」中替换。',
  network: '连不上服务器，请检查网络连接和服务器地址。',
  timeout: '服务器 15 秒内没有响应，请稍后再试。',
  http_status: '服务器返回了错误，请稍后再试；一直失败时请联系管理员。',
  too_large: '团队话术表超过 2 MiB，已拒绝下载，请联系管理员精简。',
  too_many_rules: '团队话术表超过 2000 条上限，未更新，请联系管理员精简。',
  invalid_format: '下载到的内容不是团队话术表，请检查服务器地址是否填错。',
  schema_newer: '团队话术表使用了更新的格式，需要升级 Quicktext 后才能同步。',
  io: '无法读写本机的团队话术缓存文件，请检查数据目录的权限和剩余空间。',
};

/** Codes fixed in 「团队话术同步」 settings: the error offers a way there. */
export const SETTINGS_ERRORS: readonly string[] = ['not_configured', 'invalid_url', 'unauthorized'];

/**
 * Codes whose backend message is a more precise sentence than ours (the HTTP status and
 * what to check, which cache file failed): it is shown instead of ours.
 */
const DETAILED_ERRORS = new Set(['http_status', 'network', 'io']);

/** The Chinese sentence of a sync error; an unknown code is named, with the backend's message. */
export function cloudErrorText(error: CloudError): string {
  const known = Object.hasOwn(cloudErrorNames, error.code)
    ? cloudErrorNames[error.code]
    : undefined;
  if (known === undefined)
    return `同步失败（错误代码 ${error.code}）${error.message ? `：${error.message}` : '。'}`;
  if (!DETAILED_ERRORS.has(error.code) || !error.message) return known;
  return /[。！？]$/.test(error.message) ? error.message : `${error.message}。`;
}

const pad = (n: number) => String(n).padStart(2, '0');

/**
 * When a sync happened, for 「上次同步 …」: 「今天 16:30」, 「10月27日 16:30」 or, in another year,
 * 「2025年10月27日 16:30」, in local time; null for no time or one that does not parse.
 */
export function syncTime(iso: string | null, now: number = Date.now()): string | null {
  if (iso === null) return null;
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return null;
  const today = new Date(now);
  const time = `${pad(at.getHours())}:${pad(at.getMinutes())}`;
  if (at.toDateString() === today.toDateString()) return `今天 ${time}`;
  const day = `${at.getMonth() + 1}月${at.getDate()}日`;
  return at.getFullYear() === today.getFullYear()
    ? `${day} ${time}`
    : `${at.getFullYear()}年${day} ${time}`;
}

export interface CloudRow {
  entry: CloudRuleEntry;
  /** The rule with its serde defaults filled in; null when this UI cannot read it. */
  rule: ReadableRule | null;
}

/** The table's entries in download order, each with its rule read as the list reads 我的指令. */
export function cloudRows(state: CloudState | null): CloudRow[] {
  const entries = state?.rules ?? [];
  const rules = completeRules(entries.map((e) => e.rule));
  return entries.map((entry, i) => {
    const rule = rules[i];
    return { entry, rule: isReadableRule(rule) ? rule : null };
  });
}

/** 缩写 of a team phrase, else its id. */
const phraseName = (row: CloudRow): string => row.rule?.match.value || row.entry.id;

/**
 * 我的指令 rules that take the place of a team phrase (status `shadowed`), by personal rule id:
 * the names of the phrases each one covers, for 「覆盖团队话术」.
 */
export function shadowedPhrases(state: CloudState | null): Map<string, string[]> {
  const result = new Map<string, string[]>();
  for (const row of cloudRows(state)) {
    const by = row.entry.shadowed_by;
    if (row.entry.status !== 'shadowed' || !by) continue;
    result.set(by, [...(result.get(by) ?? []), phraseName(row)]);
  }
  return result;
}

/**
 * The name of an engine rule id `cloud:<id>` in the history: 「缩写（团队话术：说明）」. Null for any
 * other id, and for a phrase no longer in the table (the caller shows the id).
 */
export function cloudRuleName(id: string, state: CloudState | null): string | null {
  if (!id.startsWith(CLOUD_PREFIX)) return null;
  const raw = id.slice(CLOUD_PREFIX.length);
  const row = cloudRows(state).find((r) => r.entry.id === raw);
  if (!row) return null;
  const description = row.rule?.description;
  return `${phraseName(row)}（团队话术${description ? `：${description}` : ''}）`;
}

/** 触发方式 of the detail view. */
export function triggerText(rule: ReadableRule): string {
  const how =
    rule.trigger_mode === 'immediate' ? '缩写输入完成后立即执行' : '输入缩写后，再按触发键执行';
  const notes = [
    rule.match.kind === 'regex' ? '按正则表达式匹配' : null,
    rule.ignore_case ? '英文不区分大小写' : null,
  ].filter((note) => note !== null);
  return notes.length > 0 ? `${how}（${notes.join('，')}）` : how;
}

const str = (value: unknown): string => (typeof value === 'string' ? value : '');

/** 正文 of the detail view: the whole text a text action outputs, else the list's summary. */
export function actionBody(action: unknown): string {
  const a = isRecord(action) ? action : {};
  if (a.type === 'text' || a.type === 'send_keys') {
    const source = isRecord(a.source) ? a.source : {};
    if (source.type === 'file') return `读取文件：${str(source.path) || '（未选择文件）'}`;
    return str(source.value) || '（空文本）';
  }
  if (a.type === 'html') return str(a.html) || str(a.plain_text) || '（空内容）';
  return summarizeAction(action) || '—';
}

// ---- 「复制到我的指令」 ----
// A team phrase comes from a server; a copy becomes a rule of 我的指令 that runs as the user's own,
// enabled, outside any check the table's merge made. So nothing of the phrase is copied as it is:
// the copy is a new rule with its defaults, and only the fields below, rebuilt one by one from what
// passed, are taken over. Only fixed text qualifies. The status alone does not tell, since
// disabled_remote, disabled_local and shadowed are decided before the action is ever checked (§4).

/** Placeholders the engine fills from this computer: the clipboard, the selection, a parameter. */
export const LOCAL_INPUT_PLACEHOLDERS: readonly string[] = ['{cliptext}', '{selection}', '{param:'];

/** Trigger keys a copy keeps; any other is dropped. */
export const COPY_TERMINATOR_KEYS: readonly string[] = ['space', 'semicolon'];

/** What a copy takes from a team phrase; every other field is a new rule's default. */
export interface PhraseCopy {
  match: { kind: 'literal'; value: string };
  ignore_case: boolean;
  trigger_mode: 'terminator' | 'immediate';
  terminator: { keys: string[]; consume: boolean } | null;
  group: string;
  description: string;
  action: { type: 'text'; method: 'paste' | 'keys'; source: { type: 'inline'; value: string } };
}

export type PhraseCopyCheck = { copy: PhraseCopy; reason: null } | { copy: null; reason: string };

const refuse = (reason: string): PhraseCopyCheck => ({ copy: null, reason });

/**
 * The copy 「复制到我的指令」 makes of a team phrase, or why it makes none: the phrase is unreadable
 * or `invalid`, matches by regex, does something other than output fixed text inline, or reads
 * this computer's clipboard, selection or a parameter. A terminator keeps 空格 and 分号 only; with
 * neither left, or triggered at once, the copy uses the default keys.
 */
export function phraseCopy(row: CloudRow): PhraseCopyCheck {
  const { entry, rule } = row;
  if (rule === null) return refuse('当前版本无法读取这条团队话术，不能复制。');
  if (entry.status === 'invalid') return refuse('这条团队话术无法使用，不能复制。');
  if (rule.match.kind !== 'literal') return refuse('这条按正则表达式匹配；只能复制普通缩写。');
  if (rule.match.value === '') return refuse('这条团队话术没有缩写，不能复制。');
  const { action } = rule;
  if (action.type !== 'text')
    return refuse(
      `这条的操作是「${actionLabel(action)}」；团队话术只能复制输出固定文字的（纯文本）。`,
    );
  const method = action.method === 'paste' ? 'paste' : action.method === 'keys' ? 'keys' : null;
  const source = isRecord(action.source) ? action.source : {};
  if (method === null || source.type !== 'inline' || typeof source.value !== 'string')
    return refuse('这条的正文不是直接写在话术里的文字；团队话术只能复制固定文字。');
  const value = source.value;
  if (LOCAL_INPUT_PLACEHOLDERS.some((placeholder) => value.includes(placeholder)))
    return refuse(
      '正文会读取本机的剪贴板、选中文字或参数（{cliptext}、{selection}、{param:…}）；团队话术只能复制固定文字。',
    );
  const keys = [...new Set(rule.terminator?.keys ?? [])].filter((k) =>
    COPY_TERMINATOR_KEYS.includes(k),
  );
  return {
    copy: {
      match: { kind: 'literal', value: rule.match.value },
      ignore_case: rule.ignore_case,
      trigger_mode: rule.trigger_mode,
      terminator:
        rule.trigger_mode === 'terminator' && rule.terminator && keys.length > 0
          ? { keys, consume: rule.terminator.consume }
          : null,
      group: rule.group,
      description: rule.description,
      action: { type: 'text', method, source: { type: 'inline', value } },
    },
    reason: null,
  };
}
