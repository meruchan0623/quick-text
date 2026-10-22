// Pure helpers for the rule list and editor (FE-12). They fix F-04 list items in the 0.4 shell and
// are meant to be reused by the 0.5 components.

/** Name shown for a rule without a group. */
export const DEFAULT_GROUP = '默认';

/** The group a rule is listed under: `""` (or a missing group) is 「默认」, for display and filter. */
export const groupKey = (group: unknown): string =>
  typeof group === 'string' && group !== '' ? group : DEFAULT_GROUP;

/** Rule count per group key, in order of first appearance. */
export function countByGroup(rules: readonly { group?: unknown }[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const rule of rules) {
    const key = groupKey(rule.group);
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }
  return counts;
}

/** Drop selected ids whose rule no longer exists; returns the same array when nothing changed. */
export function pruneSelection(selected: string[], rules: readonly { id?: unknown }[]): string[] {
  const ids = new Set(rules.map((r) => r.id));
  return selected.every((id) => ids.has(id)) ? selected : selected.filter((id) => ids.has(id));
}

const record = (value: unknown): Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
const text = (value: unknown): string => (typeof value === 'string' ? value : '');

function keyName(key: unknown): string {
  const k = record(key);
  if (k.type === 'platform_primary') return 'Ctrl/⌘';
  if (k.type === 'windows_code') return `#${String(k.code)}`;
  return text(k.name) || '?';
}

/**
 * One-line summary of an action for the list's 摘要 column. Unlike the 0.3 fallback chain it does
 * not call a hotkey, 「不执行操作」 or an empty text 「0 个步骤」.
 */
export function summarizeAction(action: unknown): string {
  const a = record(action);
  switch (a.type) {
    case 'text':
    case 'send_keys': {
      const source = record(a.source);
      if (source.type === 'file') return text(source.path) || '（未选择文件）';
      return text(source.value) || '（空文本）';
    }
    case 'html':
      return text(a.html) || text(a.plain_text) || '（空内容）';
    case 'hotkey': {
      const keys = Array.isArray(a.keys) ? a.keys : [];
      return keys.length > 0 ? keys.map(keyName).join(' + ') : '（未设置按键）';
    }
    case 'none':
      return '—';
    case 'steps':
    case 'replay': {
      const steps = record(a.plan).steps;
      return `${String(Array.isArray(steps) ? steps.length : 0)} 个步骤`;
    }
    case 'open':
      return text(a.target) || '（未设置目标）';
    case 'common':
      return text(a.function) || '—';
    case 'reusable':
    case 'quicker':
      return text(a.action_id) || '（未选择）';
    default:
      return '';
  }
}

export interface PathDiagnostic {
  path?: unknown;
}

export interface RuleDiagnostics<T> {
  /** About the action of the edited rule (shown inside the action editor). */
  action: T[];
  /** About other fields of the edited rule. */
  rule: T[];
  /** About other rules or the global settings. */
  other: T[];
}

const fieldNames: [string, string][] = [
  ['.match', '缩写词'],
  ['.id', '指令编号'],
  ['.group', '分组'],
  ['.description', '说明'],
  ['.terminator', '触发键'],
  ['.trigger_mode', '什么时候执行'],
  ['.ime_erasure', '输入法删除方式'],
  ['.parameter', '传给操作的内容'],
  ['.notification', '触发后提示'],
  ['.action', '操作内容'],
];

/** Where a diagnostic points, in words: a field of rule `index`, another rule, or the settings. */
export function diagnosticField(path: unknown, index: number): string {
  const p = text(path);
  const rule = /^rules\[(\d+)\]/.exec(p);
  if (rule) {
    const n = Number(rule[1]);
    if (n !== index) return `第 ${String(n + 1)} 条指令`;
    const rest = p.slice(rule[0].length);
    return fieldNames.find(([prefix]) => rest.startsWith(prefix))?.[1] ?? '这条指令';
  }
  if (p.startsWith('globals')) return '文本指令参数设置';
  return p || '配置';
}

/** Split diagnostics of a whole config by whether they concern rule `index` (`rules[index]…`). */
export function ruleDiagnostics<T extends PathDiagnostic>(
  diagnostics: readonly T[],
  index: number,
): RuleDiagnostics<T> {
  const own = `rules[${String(index)}]`;
  const result: RuleDiagnostics<T> = { action: [], rule: [], other: [] };
  for (const d of diagnostics) {
    const path = text(d.path);
    const rest = path.startsWith(own) ? path.slice(own.length) : null;
    if (rest === null || (rest !== '' && !rest.startsWith('.') && !rest.startsWith('[')))
      result.other.push(d);
    else if (rest.startsWith('.action')) result.action.push(d);
    else result.rule.push(d);
  }
  return result;
}
