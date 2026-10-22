import { describe, expect, it } from 'vitest';
import {
  DEFAULT_GROUP,
  countByGroup,
  diagnosticField,
  groupKey,
  pruneSelection,
  ruleDiagnostics,
  summarizeAction,
} from './rules';

describe('groupKey / countByGroup', () => {
  it('lists a rule without a group under 「默认」', () => {
    expect(DEFAULT_GROUP).toBe('默认');
    expect(groupKey('')).toBe('默认');
    expect(groupKey(undefined)).toBe('默认');
    expect(groupKey(3)).toBe('默认');
    expect(groupKey('工作')).toBe('工作');
  });

  it('counts "" and 「默认」 together, in order of first appearance', () => {
    const counts = countByGroup([{ group: '工作' }, { group: '' }, { group: '默认' }, {}]);
    expect([...counts]).toEqual([
      ['工作', 1],
      ['默认', 3],
    ]);
  });
});

describe('pruneSelection', () => {
  const rules = [{ id: 'a' }, { id: 'b' }];

  it('keeps the same array when every selected rule still exists', () => {
    const selected = ['a', 'b'];
    expect(pruneSelection(selected, rules)).toBe(selected);
  });

  it('drops ids of deleted rules', () => {
    expect(pruneSelection(['a', 'gone', 'b'], rules)).toEqual(['a', 'b']);
    expect(pruneSelection(['gone'], [])).toEqual([]);
  });
});

describe('summarizeAction', () => {
  it('summarises text, files and html', () => {
    expect(summarizeAction({ type: 'text', source: { type: 'inline', value: '你好' } })).toBe(
      '你好',
    );
    expect(summarizeAction({ type: 'text', source: { type: 'inline', value: '' } })).toBe(
      '（空文本）',
    );
    expect(
      summarizeAction({ type: 'send_keys', source: { type: 'file', path: 'C:\\a.txt' } }),
    ).toBe('C:\\a.txt');
    expect(summarizeAction({ type: 'text', source: { type: 'file', path: '' } })).toBe(
      '（未选择文件）',
    );
    expect(summarizeAction({ type: 'html', html: '', plain_text: '纯文本' })).toBe('纯文本');
    expect(summarizeAction({ type: 'html' })).toBe('（空内容）');
  });

  it('does not call a hotkey or 「不执行操作」 a step count', () => {
    expect(
      summarizeAction({
        type: 'hotkey',
        keys: [{ type: 'platform_primary' }, { type: 'named', name: 'S' }],
      }),
    ).toBe('Ctrl/⌘ + S');
    expect(summarizeAction({ type: 'hotkey', keys: [{ type: 'windows_code', code: 91 }] })).toBe(
      '#91',
    );
    expect(summarizeAction({ type: 'hotkey', keys: [] })).toBe('（未设置按键）');
    expect(summarizeAction({ type: 'none' })).toBe('—');
  });

  it('summarises steps, open, common and actions', () => {
    expect(summarizeAction({ type: 'steps', plan: { steps: [{}, {}] } })).toBe('2 个步骤');
    expect(summarizeAction({ type: 'replay', plan: {} })).toBe('0 个步骤');
    expect(summarizeAction({ type: 'open', target: 'https://example.com' })).toBe(
      'https://example.com',
    );
    expect(summarizeAction({ type: 'open', target: '' })).toBe('（未设置目标）');
    expect(summarizeAction({ type: 'common', function: 'lock_screen' })).toBe('lock_screen');
    expect(summarizeAction({ type: 'quicker', action_id: '' })).toBe('（未选择）');
    expect(summarizeAction({ type: 'reusable', action_id: 'act-1' })).toBe('act-1');
  });

  it('returns an empty summary for junk instead of throwing', () => {
    for (const junk of [null, undefined, 42, 'text', [], { type: 'from_the_future' }])
      expect(summarizeAction(junk)).toBe('');
  });
});

describe('ruleDiagnostics / diagnosticField', () => {
  const d = (path: string) => ({ path, message: path });

  it('splits diagnostics into this rule\u2019s action, its other fields and the rest', () => {
    const split = ruleDiagnostics(
      [
        d('rules[2].action.source'),
        d('rules[2].match'),
        d('rules[2]'),
        d('rules[20].match'),
        d('rules[1].id'),
        d('globals.terminators'),
        d(''),
      ],
      2,
    );
    expect(split.action.map((x) => x.path)).toEqual(['rules[2].action.source']);
    expect(split.rule.map((x) => x.path)).toEqual(['rules[2].match', 'rules[2]']);
    expect(split.other.map((x) => x.path)).toEqual([
      'rules[20].match',
      'rules[1].id',
      'globals.terminators',
      '',
    ]);
  });

  it('names the field in words', () => {
    expect(diagnosticField('rules[0].match.text', 0)).toBe('缩写词');
    expect(diagnosticField('rules[0].terminator.keys', 0)).toBe('触发键');
    expect(diagnosticField('rules[0]', 0)).toBe('这条指令');
    expect(diagnosticField('rules[4].id', 0)).toBe('第 5 条指令');
    expect(diagnosticField('globals.blacklist', 0)).toBe('文本指令参数设置');
    expect(diagnosticField('', 0)).toBe('配置');
    expect(diagnosticField(undefined, 0)).toBe('配置');
  });
});
