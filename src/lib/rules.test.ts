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

  describe('structural check of every nested field', () => {
    const base = rule('x');
    const withAction = (action: unknown) => ({ ...base, action });
    const step = (operation: unknown, target: unknown = { type: 'locked' }) => ({
      operation,
      span: { line: 1, column: 1, length: 0 },
      target,
    });
    const plan = (...steps: unknown[]) => ({
      version: 1,
      dialect: 'structured',
      source: null,
      steps,
    });
    const named = (name: string) => ({ type: 'named', name });
    const text = { type: 'text', method: 'keys', source: { type: 'inline', value: 'hi' } };

    it('accepts every action, step and key type in the shape the backend sends', () => {
      const valid: unknown[] = [
        base,
        { ...base, trigger_mode: 'terminator', terminator: { keys: ['space'], consume: true } },
        { ...base, match: { kind: 'regex', value: '^a' }, ime_erasure: 'backspace' },
        { ...base, parameter: { type: 'fixed', value: 'v' } },
        { ...base, parameter: { type: 'capture_template', template: '{1}' } },
        { ...base, parameter: { type: 'first_capture' } },
        { ...base, parameter: { type: 'null' } },
        withAction(text),
        withAction({ type: 'text', method: 'paste', source: { type: 'file', path: 'a.txt' } }),
        withAction({
          type: 'text',
          method: 'keys',
          source: { type: 'file', path: 'a.txt', encoding: 'gbk' },
        }),
        withAction({ type: 'html', html: '<b>x</b>', plain_text: 'x' }),
        withAction({
          type: 'hotkey',
          keys: [{ type: 'platform_primary' }, { type: 'windows_code', code: 91 }, named('S')],
        }),
        withAction({ type: 'send_keys', source: { type: 'inline', value: '{Enter}' } }),
        withAction({
          type: 'send_keys',
          source: { type: 'inline', value: '' },
          syntax_variables: ['cliptext'],
        }),
        withAction({
          type: 'steps',
          plan: plan(
            step({ type: 'text', text: 'a', method: 'paste' }),
            step({ type: 'key_down', key: named('Ctrl') }),
            step({ type: 'key_up', key: named('Ctrl') }),
            step({ type: 'key_press', key: { type: 'windows_code', code: 13 } }),
            step({ type: 'hotkey', keys: [named('Ctrl'), named('C')] }),
            step({ type: 'delay', ms: 100 }),
            step({ type: 'mouse_move', x: 1, y: 2 }),
            step({ type: 'mouse_move', x: 1.5, y: -2, relative: true, percent: false }),
            step({ type: 'mouse_button', button: 'x2', state: 'double_click' }),
            step({ type: 'wheel', axis: 'horizontal', amount: -3, unit: 'delta' }),
            step({ type: 'paste_files', paths: ['a', 'b'] }),
            step({ type: 'paste_image', path: 'a.png' }),
            step({ type: 'html', html: '', plain_text: '' }),
            step({ type: 'open', target: 'x', args: [], working_directory: null }),
            step({ type: 'common', function: 'copy' }),
            step({ type: 'reusable', action_id: 'a', parameters: { n: 1 } }),
            step({ type: 'delay', ms: 1 }, { type: 'declared', process: 'notepad.exe' }),
          ),
        }),
        withAction({ type: 'replay', plan: plan(), speed: 1.5 }),
        withAction({ type: 'open', target: 'x', args: ['-a'], working_directory: 'C:\\' }),
        withAction({ type: 'open', target: 'x', args: [] }),
        withAction({ type: 'reusable', action_id: 'a', parameters: { n: 1 } }),
        withAction({ type: 'reusable', action_id: 'a' }),
        withAction({ type: 'common', function: 'lock_screen' }),
        withAction({ type: 'quicker', action_id: 'q', parameter: '' }),
      ];
      expect(valid.filter((entry) => !isReadableRule(entry))).toEqual([]);
    });

    // One malformed value per nested field the list or the editor reads.
    const malformed: [string, unknown][] = [
      ['id', { ...base, id: 42 }],
      ['enabled', { ...base, enabled: 'yes' }],
      ['group', { ...base, group: null }],
      ['description', { ...base, description: 1 }],
      ['ignore_case', { ...base, ignore_case: 'no' }],
      ['match', { ...base, match: null }],
      ['match.kind', { ...base, match: { kind: 'glob', value: ';x' } }],
      ['match.value', { ...base, match: { kind: 'literal', value: 5 } }],
      ['trigger_mode', { ...base, trigger_mode: 'sometimes' }],
      ['terminator (missing)', { ...base, terminator: undefined }],
      ['terminator.keys (number)', { ...base, terminator: { keys: 42, consume: true } }],
      ['terminator.keys[0]', { ...base, terminator: { keys: [1], consume: true } }],
      ['terminator.consume', { ...base, terminator: { keys: ['space'] } }],
      ['ime_erasure (number)', { ...base, ime_erasure: 7 }],
      ['ime_erasure (unknown)', { ...base, ime_erasure: 'shout' }],
      ['action', { ...base, action: null }],
      ['action.type (number)', withAction({ type: 7 })],
      ['action.type (unknown)', withAction({ type: 'from_the_future' })],
      ['action.type (prototype key)', withAction({ type: 'constructor' })],
      ['text.method', withAction({ ...text, method: 'shout' })],
      ['text.source', withAction({ ...text, source: null })],
      ['text.source.type', withAction({ ...text, source: { type: 'clipboard' } })],
      ['text.source.value', withAction({ ...text, source: { type: 'inline', value: 3 } })],
      ['text.source.path', withAction({ ...text, source: { type: 'file', path: 1 } })],
      [
        'text.source.encoding',
        withAction({ ...text, source: { type: 'file', path: 'a', encoding: 'latin1' } }),
      ],
      ['html.html', withAction({ type: 'html', html: 1, plain_text: '' })],
      ['html.plain_text', withAction({ type: 'html', html: '' })],
      ['hotkey.keys (string)', withAction({ type: 'hotkey', keys: 'ctrl+s' })],
      ['hotkey.keys[0] (string)', withAction({ type: 'hotkey', keys: ['ctrl', 's'] })],
      ['hotkey.keys[0].type', withAction({ type: 'hotkey', keys: [{ type: 'chord' }] })],
      ['hotkey.keys[0].name', withAction({ type: 'hotkey', keys: [{ type: 'named' }] })],
      [
        'hotkey.keys[0].code',
        withAction({ type: 'hotkey', keys: [{ type: 'windows_code', code: '91' }] }),
      ],
      ['send_keys.source', withAction({ type: 'send_keys' })],
      [
        'send_keys.syntax_variables',
        withAction({
          type: 'send_keys',
          source: { type: 'inline', value: '' },
          syntax_variables: 'cliptext',
        }),
      ],
      [
        'send_keys.syntax_variables[0]',
        withAction({
          type: 'send_keys',
          source: { type: 'inline', value: '' },
          syntax_variables: [1],
        }),
      ],
      ['steps.plan', withAction({ type: 'steps', plan: null })],
      ['steps.plan.steps', withAction({ type: 'steps', plan: { steps: 'x' } })],
      ['steps.plan.steps[0]', withAction({ type: 'steps', plan: plan(null) })],
      ['step.operation', withAction({ type: 'steps', plan: plan({ target: { type: 'locked' } }) })],
      [
        'step.operation.type',
        withAction({ type: 'steps', plan: plan(step({ type: 'teleport' })) }),
      ],
      [
        'step.target (missing)',
        withAction({ type: 'steps', plan: plan({ operation: { type: 'delay', ms: 1 } }) }),
      ],
      [
        'step.target.process',
        withAction({
          type: 'steps',
          plan: plan(step({ type: 'delay', ms: 1 }, { type: 'declared' })),
        }),
      ],
      [
        'step text.text',
        withAction({ type: 'steps', plan: plan(step({ type: 'text', text: 1, method: 'keys' })) }),
      ],
      [
        'step delay.ms',
        withAction({ type: 'steps', plan: plan(step({ type: 'delay', ms: '1' })) }),
      ],
      [
        'step key_press.key',
        withAction({ type: 'steps', plan: plan(step({ type: 'key_press', key: 'Enter' })) }),
      ],
      [
        'step hotkey.keys',
        withAction({ type: 'steps', plan: plan(step({ type: 'hotkey', keys: [null] })) }),
      ],
      [
        'step mouse_move.x',
        withAction({ type: 'steps', plan: plan(step({ type: 'mouse_move', x: '1', y: 0 })) }),
      ],
      [
        'step mouse_move.relative',
        withAction({
          type: 'steps',
          plan: plan(step({ type: 'mouse_move', x: 1, y: 0, relative: 'yes' })),
        }),
      ],
      [
        'step mouse_button.button',
        withAction({
          type: 'steps',
          plan: plan(step({ type: 'mouse_button', button: 'left2', state: 'click' })),
        }),
      ],
      [
        'step wheel.unit',
        withAction({
          type: 'steps',
          plan: plan(step({ type: 'wheel', axis: 'vertical', amount: 1, unit: 'pixels' })),
        }),
      ],
      [
        'step paste_files.paths',
        withAction({ type: 'steps', plan: plan(step({ type: 'paste_files', paths: 'a' })) }),
      ],
      [
        'step paste_image.path',
        withAction({ type: 'steps', plan: plan(step({ type: 'paste_image' })) }),
      ],
      ['replay.speed', withAction({ type: 'replay', plan: plan(), speed: '1' })],
      ['replay.plan', withAction({ type: 'replay', speed: 1 })],
      ['open.target', withAction({ type: 'open', target: null, args: [] })],
      ['open.args (missing)', withAction({ type: 'open', target: 'x' })],
      ['open.args (string)', withAction({ type: 'open', target: 'x', args: '-a -b' })],
      [
        'open.working_directory',
        withAction({ type: 'open', target: 'x', args: [], working_directory: 3 }),
      ],
      ['reusable.action_id', withAction({ type: 'reusable', action_id: 1 })],
      ['reusable.parameters', withAction({ type: 'reusable', action_id: 'a', parameters: [] })],
      ['common.function', withAction({ type: 'common', function: null })],
      ['quicker.parameter', withAction({ type: 'quicker', action_id: 'q' })],
      ['parameter', { ...base, parameter: null }],
      ['parameter.type', { ...base, parameter: { type: 'whatever' } }],
      ['parameter.value', { ...base, parameter: { type: 'fixed', value: 3 } }],
      ['parameter.template', { ...base, parameter: { type: 'capture_template' } }],
      ['notification', { ...base, notification: null }],
      ['notification.enabled', { ...base, notification: { enabled: 'yes', message: '' } }],
      ['notification.message', { ...base, notification: { enabled: true } }],
    ];

    it.each(malformed)('treats a malformed %s as unreadable', (_, entry) => {
      expect(isReadableRule(entry)).toBe(false);
      // …and writes it back unchanged, in place.
      const rules = [rule('a'), entry, rule('b')];
      const next = replaceReadable(rules, [{ ...rule('a'), enabled: false }, rule('b')]);
      expect(next[1]).toBe(entry);
      expect(next).toHaveLength(3);
    });
  });

  it('finds the keys of a newer version', () => {
    expect(unknownRuleKeys(rule('a'))).toEqual([]);
    expect(unknownRuleKeys({ ...rule('a'), word_start_only: true, sensitive: false })).toEqual([
      'word_start_only',
      'sensitive',
    ]);
  });
});
