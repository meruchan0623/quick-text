import { describe, expect, it } from 'vitest';
import {
  DEFAULT_GROUP,
  countByGroup,
  diagnosticField,
  groupKey,
  isReadableRule,
  pruneSelection,
  replaceReadable,
  ruleDiagnostics,
  summarizeAction,
  unknownRuleKeys,
  unreadableEntries,
  unreadableId,
} from './rules';

/** A readable rule as the backend sends it. */
const rule = (id: string, group = '默认') => ({
  id,
  enabled: true,
  group,
  description: '',
  ignore_case: false,
  match: { kind: 'literal', value: `;${id}` },
  trigger_mode: 'immediate',
  terminator: null,
  ime_erasure: null,
  action: { type: 'none' },
  parameter: { type: 'default_match' },
  notification: { enabled: false, message: '' },
});

describe('groupKey / countByGroup', () => {
  it('lists a rule without a group under 「默认」', () => {
    expect(DEFAULT_GROUP).toBe('默认');
    expect(groupKey('')).toBe('默认');
    expect(groupKey(undefined)).toBe('默认');
    expect(groupKey(3)).toBe('默认');
    expect(groupKey('工作')).toBe('工作');
  });

  it('counts "" and 「默认」 together, in order of first appearance', () => {
    const counts = countByGroup([
      rule('a', '工作'),
      rule('b', ''),
      rule('c', '默认'),
      { group: '工作' }, // unreadable: not counted
      rule('d', ''),
    ]);
    expect([...counts]).toEqual([
      ['工作', 1],
      ['默认', 3],
    ]);
  });
});

describe('pruneSelection', () => {
  const rules = [rule('a'), { id: 'raw' }, rule('b')];

  it('keeps the same array when every selected rule still exists', () => {
    const selected = ['a', 'b'];
    expect(pruneSelection(selected, rules)).toBe(selected);
  });

  it('drops ids of deleted rules', () => {
    expect(pruneSelection(['a', 'gone', 'b'], rules)).toEqual(['a', 'b']);
    expect(pruneSelection(['gone'], [])).toEqual([]);
    // An unreadable entry's id is not selectable.
    expect(pruneSelection(['raw', 'a'], rules)).toEqual(['a']);
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

describe('tolerant reading (B-43)', () => {
  const raw1 = { id: 'broken', match: 42, from_the_future: true };
  const rules: unknown[] = [rule('a'), raw1, rule('b'), 'not a rule', rule('c'), null];

  it('tells readable rules from raw entries', () => {
    expect(rules.map(isReadableRule)).toEqual([true, false, true, false, true, false]);
    expect(isReadableRule({ ...rule('x'), action: { type: 7 } })).toBe(false);
    expect(isReadableRule({ ...rule('x'), notification: null })).toBe(false);
    expect(isReadableRule([rule('x')])).toBe(false);
  });

  it('lists unreadable entries with their position and id, if any', () => {
    expect(unreadableEntries(rules)).toEqual([
      { index: 1, id: 'broken' },
      { index: 3, id: null },
      { index: 5, id: null },
    ]);
    expect(unreadableId({ id: '' })).toBeNull();
    expect(unreadableId({ id: 3 })).toBeNull();
  });

  it('writes edits back without moving or dropping raw entries', () => {
    const edited = { ...rule('b'), enabled: false };
    expect(replaceReadable(rules, [rule('a'), edited, rule('c')])).toEqual([
      rule('a'),
      raw1,
      edited,
      'not a rule',
      rule('c'),
      null,
    ]);
    // Deleting: readable slots close up, raw entries keep their order.
    expect(replaceReadable(rules, [rule('a'), rule('c')])).toEqual([
      rule('a'),
      raw1,
      rule('c'),
      'not a rule',
      null,
    ]);
    // Adding: new rules go to the end.
    expect(replaceReadable([raw1], [rule('n')])).toEqual([raw1, rule('n')]);
    expect(replaceReadable(rules, [])).toEqual([raw1, 'not a rule', null]);
  });

  it('finds the keys of a newer version', () => {
    expect(unknownRuleKeys(rule('a'))).toEqual([]);
    expect(unknownRuleKeys({ ...rule('a'), word_start_only: true, sensitive: false })).toEqual([
      'word_start_only',
      'sensitive',
    ]);
  });
});
