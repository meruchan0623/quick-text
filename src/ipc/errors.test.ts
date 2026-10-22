import { describe, expect, it } from 'vitest';
import {
  COMMAND_ERROR_CODES,
  describeError,
  errorText,
  isMissingCommand,
  parseCommandError,
  type CommandError,
} from './errors';
import type { Diagnostic } from './types';

const diagnostic: Diagnostic = {
  code: 'empty_match',
  message: '缩写不能为空',
  path: 'rules[0].match.value',
  span: { line: 1, column: 2, length: 3 },
  severity: 'error',
};

// One wire value per code, exactly as the backend serialises it (docs/ipc-contract-0.4.md §2).
const wire: Record<CommandError['code'], unknown> = {
  validation: { code: 'validation', detail: { diagnostics: [diagnostic] } },
  rule_conflict: { code: 'rule_conflict', detail: { id: 'r1', current: { revision: 3 } } },
  revision_conflict: { code: 'revision_conflict', detail: { current: 7 } },
  safe_mode: { code: 'safe_mode', detail: { reason: 'crash_loop' } },
  io: { code: 'io', detail: { op: 'write', path: 'active.json', os_code: 32 } },
  not_found: { code: 'not_found', detail: { kind: 'rule', id: 'r9' } },
  busy: { code: 'busy' },
  engine_unavailable: { code: 'engine_unavailable', detail: { health: 'failed' } },
  unsupported: { code: 'unsupported', detail: { capability: 'recording' } },
  hotkey_conflict: { code: 'hotkey_conflict', detail: { combo: 'Ctrl+Alt+Q' } },
  token_expired: { code: 'token_expired' },
  cancelled: { code: 'cancelled' },
  internal: { code: 'internal', detail: { trace_id: '6f1c' } },
};

const expected: Record<CommandError['code'], CommandError> = {
  validation: { code: 'validation', detail: { diagnostics: [diagnostic] } },
  rule_conflict: { code: 'rule_conflict', detail: { id: 'r1', current: { revision: 3 } } },
  revision_conflict: { code: 'revision_conflict', detail: { current: 7 } },
  safe_mode: { code: 'safe_mode', detail: { reason: 'crash_loop' } },
  io: { code: 'io', detail: { op: 'write', path: 'active.json', os_code: 32 } },
  not_found: { code: 'not_found', detail: { kind: 'rule', id: 'r9' } },
  busy: { code: 'busy' },
  engine_unavailable: { code: 'engine_unavailable', detail: { health: 'failed' } },
  unsupported: { code: 'unsupported', detail: { capability: 'recording' } },
  hotkey_conflict: { code: 'hotkey_conflict', detail: { combo: 'Ctrl+Alt+Q' } },
  token_expired: { code: 'token_expired' },
  cancelled: { code: 'cancelled' },
  internal: { code: 'internal', detail: { trace_id: '6f1c' } },
};

const texts: Record<CommandError['code'], string | null> = {
  validation: '内容没有通过检查：缩写不能为空',
  rule_conflict: '这条规则已在别处被修改，请刷新后再试。',
  revision_conflict: '设置已在别处更新（当前版本 7），请刷新后再试。',
  safe_mode: '当前处于安全模式（连续多次启动失败），暂时不能修改设置。',
  io: '写入文件失败：active.json（系统错误 32）。',
  not_found: '找不到规则「r9」，可能已被删除。',
  busy: '上一个动作仍在执行，请稍后再试。',
  engine_unavailable: '输入引擎暂时不可用（故障），请稍后再试，或在「运行状态」中修复引擎。',
  unsupported: '当前平台不支持此功能：键鼠录制。',
  hotkey_conflict: '快捷键 Ctrl+Alt+Q 已被占用，请换一个。',
  token_expired: '确认已过期，请重新操作。',
  cancelled: null,
  internal: '操作没有完成，发生了内部错误（追踪编号 6f1c）。可在「运行状态」中导出诊断包反馈问题。',
};

describe('parseCommandError', () => {
  it('covers every code of the contract', () => {
    expect(Object.keys(wire).sort()).toEqual([...COMMAND_ERROR_CODES].sort());
  });

  it.each(COMMAND_ERROR_CODES)('parses %s', (code) => {
    expect(parseCommandError(wire[code])).toEqual(expected[code]);
  });

  it.each(COMMAND_ERROR_CODES)('describes %s in Chinese', (code) => {
    expect(describeError(parseCommandError(wire[code]))).toBe(texts[code]);
  });

  it('fills missing or malformed detail fields instead of throwing', () => {
    expect(parseCommandError({ code: 'io' })).toEqual({
      code: 'io',
      detail: { op: 'other', path: null, os_code: null },
    });
    expect(parseCommandError({ code: 'validation', detail: { diagnostics: 'x' } })).toEqual({
      code: 'validation',
      detail: { diagnostics: [] },
    });
    expect(parseCommandError({ code: 'revision_conflict', detail: null })).toEqual({
      code: 'revision_conflict',
      detail: { current: 0 },
    });
    expect(describeError(parseCommandError({ code: 'validation' }))).toBe(
      '内容没有通过检查，请修改后再试。',
    );
  });

  it('shows unknown enum values verbatim', () => {
    const text = errorText({ code: 'safe_mode', detail: { reason: 'cosmic_ray' } });
    expect(text).toBe('当前处于安全模式（cosmic_ray），暂时不能修改设置。');
  });

  it('keeps an unknown code visible as internal', () => {
    expect(parseCommandError({ code: 'quota_exceeded', detail: {} })).toEqual({
      code: 'internal',
      detail: { trace_id: null },
      message: '操作没有完成（错误代码 quota_exceeded）。',
    });
  });

  it('treats a legacy plain string as internal and keeps its text', () => {
    const error = parseCommandError('Store lock');
    expect(error).toEqual({ code: 'internal', detail: { trace_id: null }, message: 'Store lock' });
    expect(describeError(error)).toBe('Store lock');
  });

  it('recognises a legacy JSON diagnostics string as validation', () => {
    const error = parseCommandError(
      JSON.stringify([diagnostic, { ...diagnostic, message: '第二处' }]),
    );
    expect(error.code).toBe('validation');
    expect(describeError(error)).toBe('有 2 处内容需要修改，第一处：缩写不能为空');
  });

  it('counts only errors when warnings are mixed in', () => {
    const error = parseCommandError({
      code: 'validation',
      detail: {
        diagnostics: [{ ...diagnostic, severity: 'warning', message: '提示' }, diagnostic],
      },
    });
    expect(describeError(error)).toBe('内容没有通过检查：缩写不能为空');
  });

  it('handles frontend errors and junk', () => {
    expect(errorText(new Error('本地错误'))).toBe('本地错误');
    for (const junk of [undefined, null, 42, [1, 2], '{not json'])
      expect(parseCommandError(junk).code).toBe('internal');
    expect(errorText(null)).toBe(
      '操作没有完成，发生了内部错误。可在「运行状态」中导出诊断包反馈问题。',
    );
  });
});

describe('isMissingCommand', () => {
  it('matches the Tauri rejection for an unregistered command only', () => {
    expect(isMissingCommand('Command get_runtime_snapshot not found')).toBe(true);
    expect(isMissingCommand(new Error('Command list_history not found'))).toBe(true);
    expect(isMissingCommand('Store lock')).toBe(false);
    expect(isMissingCommand({ code: 'not_found', detail: { kind: 'rule', id: 'x' } })).toBe(false);
  });

  it('explains a missing command in Chinese instead of showing the English rejection', () => {
    expect(errorText('Command export_diagnostics not found')).toBe(
      '当前版本的桌面引擎不支持此功能，请更新到新版本后再试。',
    );
  });
});
