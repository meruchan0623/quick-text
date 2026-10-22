// Pure helpers for the rule list and editor (FE-12). They fix F-04 list items in the 0.4 shell and
// are meant to be reused by the 0.5 components.
import { isRecord, isRuleShape } from './ruleShape';

/** Name shown for a rule without a group. */
export const DEFAULT_GROUP = '默认';

/** The group a rule is listed under: `""` (or a missing group) is 「默认」, for display and filter. */
export const groupKey = (group: unknown): string =>
  typeof group === 'string' && group !== '' ? group : DEFAULT_GROUP;

/** Rule count per group key, in order of first appearance; unreadable entries are not counted. */
export function countByGroup(rules: readonly unknown[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const rule of rules) {
    if (!isReadableRule(rule)) continue;
    const key = groupKey(rule.group);
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }
  return counts;
}

/**
 * Drop selected ids whose rule no longer exists; returns the same array when nothing changed.
 * Only readable rules count: an unreadable entry cannot be selected.
 */
export function pruneSelection(selected: string[], rules: readonly unknown[]): string[] {
  const ids = new Set(rules.filter(isReadableRule).map((r) => r.id));
  return selected.every((id) => ids.has(id)) ? selected : selected.filter((id) => ids.has(id));
}

const record = (value: unknown): Record<string, unknown> => (isRecord(value) ? value : {});
const text = (value: unknown): string => (typeof value === 'string' ? value : '');

// ---- B-43 tolerant reading -----------------------------------------------------------------
// The backend keeps a `rules` entry it cannot read as the raw JSON value (any shape, even not an
// object) and unknown fields as extra keys. The UI lists such an entry read-only and writes every
// entry and key back unchanged.

/**
 * The top-level fields of a readable rule. `isReadableRule` checks every nested field too (see
 * ./ruleShape.ts); the backend sends all of them for a rule it could read.
 */
export interface ReadableRule {
  id: string;
  enabled: boolean;
  group: string;
  description: string;
  ignore_case: boolean;
  match: { kind: 'literal' | 'regex'; value: string };
  trigger_mode: 'terminator' | 'immediate';
  terminator: { keys: string[]; consume: boolean } | null;
  ime_erasure: 'escape' | 'backspace' | null;
  action: Record<string, unknown> & { type: string };
  parameter: Record<string, unknown> & { type: string };
  notification: { enabled: boolean; message: string };
}

/**
 * Whether a `rules` entry is a rule the UI can show and edit, or a raw unreadable entry. Every
 * nested field the list and the editor read is checked, per action, step and key type; an unknown
 * type makes the entry unreadable.
 */
export const isReadableRule = (entry: unknown): entry is ReadableRule => isRuleShape(entry);

/** The id of an unreadable entry, when it has a usable one. */
export function unreadableId(entry: unknown): string | null {
  const id = record(entry).id;
  return typeof id === 'string' && id !== '' ? id : null;
}

export interface UnreadableEntry {
  /** Position in `rules` (diagnostic paths use it). */
  index: number;
  id: string | null;
}

/** The unreadable entries of `rules`, in order. */
export const unreadableEntries = (rules: readonly unknown[]): UnreadableEntry[] =>
  rules.flatMap((entry, index) =>
    isReadableRule(entry) ? [] : [{ index, id: unreadableId(entry) }],
  );

/**
 * Put an edited list of readable rules back into `rules`, position by position. A rule of `next`
 * takes the place of the readable entry it came from (the first one not yet taken with its id), so
 * an edit stays where it was; a readable entry with no rule left in `next` is deleted in place.
 * Unreadable entries stay where they were, every surviving entry keeps its relative order, and only
 * rules that came from no entry (new ones) are appended, in `next` order.
 */
export function replaceReadable(
  rules: readonly unknown[],
  next: readonly { id: string }[],
): unknown[] {
  // Positions in `next` per id, in order; duplicate ids pair up with their entries in order.
  const pending = new Map<string, number[]>();
  next.forEach((rule, position) => {
    const queue = pending.get(rule.id);
    if (queue) queue.push(position);
    else pending.set(rule.id, [position]);
  });
  const placed = new Set<number>();
  const result: unknown[] = [];
  for (const entry of rules) {
    if (!isReadableRule(entry)) {
      result.push(entry);
      continue;
    }
    const position = pending.get(entry.id)?.shift();
    if (position === undefined) continue; // deleted
    placed.add(position);
    result.push(next[position]);
  }
  return [...result, ...next.filter((_, position) => !placed.has(position))];
}

const RULE_KEYS = new Set([
  'id',
  'enabled',
  'group',
  'description',
  'ignore_case',
  'match',
  'trigger_mode',
  'terminator',
  'ime_erasure',
  'action',
  'parameter',
  'notification',
]);

/** Keys of a rule this version does not know (a newer version's fields, kept on save). */
export const unknownRuleKeys = (rule: object): string[] =>
  Object.keys(rule).filter((key) => !RULE_KEYS.has(key));

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
