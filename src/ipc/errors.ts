// CommandError (B-45): every command rejects with one of these (docs/ipc-contract-0.4.md §2).
// Errors carry no Chinese text; this module translates `code` + `detail` for display.
import {
  capabilityNames,
  entityNames,
  healthNames,
  ioOpNames,
  label,
  safeModeNames,
} from './labels';
import type { Diagnostic, EntityKind, HealthState, IoOp, SafeModeReason } from './types';

export type CommandError =
  | { code: 'validation'; detail: { diagnostics: Diagnostic[] } }
  | { code: 'rule_conflict'; detail: { id: string; current: unknown } }
  | { code: 'revision_conflict'; detail: { current: number } }
  | { code: 'safe_mode'; detail: { reason: SafeModeReason } }
  | { code: 'io'; detail: { op: IoOp; path: string | null; os_code: number | null } }
  | { code: 'not_found'; detail: { kind: EntityKind; id: string } }
  | { code: 'busy' }
  | { code: 'engine_unavailable'; detail: { health: HealthState } }
  | { code: 'unsupported'; detail: { capability: string } }
  | { code: 'hotkey_conflict'; detail: { combo: string } }
  | { code: 'token_expired' }
  | { code: 'cancelled' }
  | {
      code: 'internal';
      detail: { trace_id: string | null };
      /**
       * Frontend-only: text of a value that was not a CommandError (a 0.3 backend's plain string,
       * a frontend `Error`). Shown instead of the generic text so nothing gets lost.
       */
      message?: string;
    };

export type CommandErrorCode = CommandError['code'];

/** Every code of the contract, in §2 order. */
export const COMMAND_ERROR_CODES: readonly CommandErrorCode[] = [
  'validation',
  'rule_conflict',
  'revision_conflict',
  'safe_mode',
  'io',
  'not_found',
  'busy',
  'engine_unavailable',
  'unsupported',
  'hotkey_conflict',
  'token_expired',
  'cancelled',
  'internal',
];

type Obj = Record<string, unknown>;
const isObj = (v: unknown): v is Obj => typeof v === 'object' && v !== null && !Array.isArray(v);
const str = (v: unknown, fallback = ''): string =>
  typeof v === 'string' ? v : typeof v === 'number' ? String(v) : fallback;
const strOrNull = (v: unknown): string | null => (typeof v === 'string' ? v : null);
const numOrNull = (v: unknown): number | null =>
  typeof v === 'number' && Number.isFinite(v) ? v : null;

const MISSING_COMMAND_TEXT = '当前版本的桌面引擎不支持此功能，请更新到新版本后再试。';

const internal = (message?: string): CommandError =>
  message === undefined
    ? { code: 'internal', detail: { trace_id: null } }
    : { code: 'internal', detail: { trace_id: null }, message };

function toDiagnostic(v: unknown): Diagnostic | null {
  if (!isObj(v) || typeof v.message !== 'string') return null;
  const span = isObj(v.span)
    ? {
        line: numOrNull(v.span.line) ?? 0,
        column: numOrNull(v.span.column) ?? 0,
        length: numOrNull(v.span.length) ?? 0,
      }
    : null;
  return {
    code: str(v.code),
    message: v.message,
    path: str(v.path),
    span,
    severity: v.severity === 'warning' ? 'warning' : 'error',
  };
}

const toDiagnostics = (v: unknown): Diagnostic[] =>
  Array.isArray(v) ? v.map(toDiagnostic).filter((d): d is Diagnostic => d !== null) : [];

function fromObject(value: Obj): CommandError | null {
  if (typeof value.code !== 'string') return null;
  const d: Obj = isObj(value.detail) ? value.detail : {};
  switch (value.code) {
    case 'validation':
      return { code: 'validation', detail: { diagnostics: toDiagnostics(d.diagnostics) } };
    case 'rule_conflict':
      return { code: 'rule_conflict', detail: { id: str(d.id), current: d.current ?? null } };
    case 'revision_conflict':
      return { code: 'revision_conflict', detail: { current: numOrNull(d.current) ?? 0 } };
    case 'safe_mode':
      return { code: 'safe_mode', detail: { reason: str(d.reason) as SafeModeReason } };
    case 'io':
      return {
        code: 'io',
        detail: {
          op: str(d.op, 'other') as IoOp,
          path: strOrNull(d.path),
          os_code: numOrNull(d.os_code),
        },
      };
    case 'not_found':
      return { code: 'not_found', detail: { kind: str(d.kind) as EntityKind, id: str(d.id) } };
    case 'busy':
    case 'token_expired':
    case 'cancelled':
      return { code: value.code };
    case 'engine_unavailable':
      return { code: 'engine_unavailable', detail: { health: str(d.health) as HealthState } };
    case 'unsupported':
      return { code: 'unsupported', detail: { capability: str(d.capability) } };
    case 'hotkey_conflict':
      return { code: 'hotkey_conflict', detail: { combo: str(d.combo) } };
    case 'internal':
      return { code: 'internal', detail: { trace_id: strOrNull(d.trace_id) } };
    default:
      // A code added by a newer backend: keep it visible instead of failing.
      return internal(`操作没有完成（错误代码 ${value.code}）。`);
  }
}

/**
 * Normalise anything an `invoke` can reject with. Values that are not a CommandError become
 * `internal` with an empty trace id (contract §2). A 0.3 backend's validation failure is a JSON
 * string of a Diagnostic array; it is recognised as `validation` so the editor can still point at
 * the offending field.
 */
export function parseCommandError(value: unknown): CommandError {
  if (isMissingCommand(value)) return internal(MISSING_COMMAND_TEXT);
  if (isObj(value) && !(value instanceof Error)) return fromObject(value) ?? internal();
  if (value instanceof Error) return internal(value.message);
  if (typeof value === 'string') {
    const text = value.trim();
    if (text.startsWith('[') || text.startsWith('{')) {
      try {
        const parsed: unknown = JSON.parse(text);
        if (Array.isArray(parsed)) {
          const diagnostics = toDiagnostics(parsed);
          if (diagnostics.length > 0) return { code: 'validation', detail: { diagnostics } };
        } else if (isObj(parsed)) {
          const error = fromObject(parsed);
          if (error) return error;
        }
      } catch {
        // Not JSON: fall through to the plain-text case.
      }
    }
    return internal(value);
  }
  return internal();
}

/** Tauri's rejection for a command the running backend does not register (an older version). */
export function isMissingCommand(value: unknown): boolean {
  const text = value instanceof Error ? value.message : value;
  return typeof text === 'string' && /^Command \S+ not found/.test(text);
}

function describeValidation(diagnostics: Diagnostic[]): string {
  const errors = diagnostics.filter((d) => d.severity === 'error');
  const list = errors.length > 0 ? errors : diagnostics;
  const first = list[0];
  if (!first) return '内容没有通过检查，请修改后再试。';
  return list.length === 1
    ? `内容没有通过检查：${first.message}`
    : `有 ${list.length} 处内容需要修改，第一处：${first.message}`;
}

/** Chinese text for an error. `null` means "show nothing" (the user cancelled). */
export function describeError(error: CommandError): string | null {
  switch (error.code) {
    case 'validation':
      return describeValidation(error.detail.diagnostics);
    case 'rule_conflict':
      return '这条规则已在别处被修改，请刷新后再试。';
    case 'revision_conflict':
      return `设置已在别处更新（当前版本 ${error.detail.current}），请刷新后再试。`;
    case 'safe_mode':
      return `当前处于安全模式（${label(safeModeNames, error.detail.reason)}），暂时不能修改设置。`;
    case 'io': {
      const { op, path, os_code } = error.detail;
      const where = path ? `：${path}` : '';
      const code = os_code === null ? '' : `（系统错误 ${os_code}）`;
      return `${label(ioOpNames, op)}失败${where}${code}。`;
    }
    case 'not_found': {
      const id = error.detail.id ? `「${error.detail.id}」` : '';
      return `找不到${label(entityNames, error.detail.kind)}${id}，可能已被删除。`;
    }
    case 'busy':
      return '上一个动作仍在执行，请稍后再试。';
    case 'engine_unavailable':
      return `输入引擎暂时不可用（${label(healthNames, error.detail.health)}），请稍后再试，或在「运行状态」中修复引擎。`;
    case 'unsupported':
      return `当前平台不支持此功能：${label(capabilityNames, error.detail.capability)}。`;
    case 'hotkey_conflict':
      return `快捷键 ${error.detail.combo} 已被占用，请换一个。`;
    case 'token_expired':
      return '确认已过期，请重新操作。';
    case 'cancelled':
      return null;
    case 'internal': {
      if (error.message !== undefined) return error.message;
      const trace = error.detail.trace_id ? `（追踪编号 ${error.detail.trace_id}）` : '';
      return `操作没有完成，发生了内部错误${trace}。可在「运行状态」中导出诊断包反馈问题。`;
    }
  }
}

/** `describeError(parseCommandError(value))`: the one-liner for catch blocks. */
export const errorText = (value: unknown): string | null => describeError(parseCommandError(value));
