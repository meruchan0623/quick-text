import type { SafeModeReason } from './ipc/types';
import { completeRules, hasUnsafeNumber, isReadableRule } from './lib/rules';
import { DEFAULT_TERMINATORS } from './lib/terminators';
export type Data = Record<string, any>;
export interface Config extends Data {
  version: number;
  revision: number;
  enabled: boolean;
  globals: Data;
  /** Rules, and raw entries the backend could not read (B-43): narrow with `isRule`. */
  rules: unknown[];
}
export interface Rule extends Data {
  id: string;
  enabled: boolean;
  group: string;
  description: string;
  ignore_case: boolean;
  match: { kind: string; value: string };
  trigger_mode: string;
  terminator: null | Data;
  ime_erasure: string | null;
  action: Data;
  parameter: Data;
  notification: { enabled: boolean; message: string };
}
/** A `rules` entry the UI can show and edit (B-43). */
export const isRule = (entry: unknown): entry is Rule => isReadableRule(entry);
/**
 * `config` with the serde defaults of absent rule fields filled in (see completeRules), as the UI
 * reads it; the same object when nothing was absent.
 */
export function completeConfig<T extends Config>(config: T): T {
  const rules = completeRules(config.rules);
  return rules === config.rules ? config : { ...config, rules };
}
/**
 * Whether the config outside `rules` (its own keys and the globals', known or of a newer version)
 * holds a number past the IPC number contract (docs/ipc-contract-0.4.md §6), which saving may
 * rewrite. Rules are left out: such a rule is unreadable and the list says so.
 */
export const settingsHaveUnsafeNumber = (config: Config): boolean =>
  hasUnsafeNumber(
    Object.entries(config).flatMap(([key, value]) => (key === 'rules' ? [] : [value])),
  );
export interface State extends Data {
  active: Config;
  revision: number;
  paused: boolean;
  engine_status: string;
  last_result: any;
  capabilities: any;
  /** S-09 「启动时自动开始监听」; absent on a 0.3 backend. */
  auto_listen_on_start?: boolean;
  /** Absent on a 0.3 backend. */
  safe_mode?: SafeModeReason | null;
}
export const emptyConfig = (): Config => ({
  version: 1,
  revision: 0,
  enabled: true,
  globals: {
    terminators: [...DEFAULT_TERMINATORS],
    word_separators: ' \t\n',
    blacklist: [],
    ime_erasure: 'escape',
  },
  rules: [],
});
export const newRule = (platform?: string): Rule => ({
  id: crypto.randomUUID(),
  enabled: true,
  group: '默认',
  description: '',
  ignore_case: false,
  match: { kind: 'literal', value: '' },
  trigger_mode: platform === 'windows' ? 'immediate' : 'terminator',
  terminator: null,
  ime_erasure: null,
  action: { type: 'text', method: 'paste', source: { type: 'inline', value: '' } },
  parameter: { type: 'default_match' },
  notification: { enabled: false, message: '' },
});
export const actionNames: Record<string, string> = {
  none: '不执行操作',
  text: '纯文本',
  text_paste: '粘贴纯文本',
  text_keys: '键入纯文本',
  html: '粘贴带格式的文字',
  hotkey: '按快捷键',
  send_keys: '文字与按键组合',
  steps: '按顺序执行多个操作',
  replay: '重放鼠标和键盘操作',
  open: '打开网页、文件或程序',
  common: '常用电脑操作',
  reusable: '执行另一条文本指令',
  quicker: '保留 Quicker 动作信息（暂不能执行）',
};
// These choices are UI labels; persisted text actions retain the existing type and method.
export const actionChoice = (action: Data): string =>
  action.type === 'text' ? (action.method === 'paste' ? 'text_paste' : 'text_keys') : action.type;
export const actionLabel = (action: Data): string =>
  actionNames[actionChoice(action)] || action.type;
export const actionOptions = Object.entries(actionNames).filter(([type]) => type !== 'text');
/**
 * The choices offered for a new action: what everyday replies need. The other types stay readable,
 * runnable and selectable on a rule that already has one (an import, an older version), but are
 * not offered for new rules.
 */
export const offeredChoices: ReadonlySet<string> = new Set([
  'text_paste',
  'text_keys',
  'send_keys',
  'hotkey',
  'steps',
  'open',
]);
export const clone = <T>(x: T): T => structuredClone(x);
export const keys = ['space', 'tab', 'enter', 'semicolon'];
export const keyNames: Record<string, string> = {
  space: '空格',
  tab: 'Tab',
  enter: '回车',
  semicolon: '分号',
};
