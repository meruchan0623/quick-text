import { describe, expect, it } from 'vitest';
import { withoutKey } from './list';
import {
  DEFAULT_GROUP,
  completeRules,
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
  type ReadableRule,
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
      { group: '工作', match: 42 }, // unreadable: not counted
      rule('d', ''),
    ]);
    expect([...counts]).toEqual([
      ['工作', 1],
      ['默认', 3],
    ]);
  });
});

describe('pruneSelection', () => {
  // (Rust reads `{ id: 'raw' }` alone as a rule with defaults; `match: 42` it cannot read.)
  const rules = [rule('a'), { id: 'raw', match: 42 }, rule('b')];

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

  describe('position-stable write-back', () => {
    const bad = (id: string) => ({ id, match: 42 });
    // Unreadable entries at the start, two in a row between rules, and two at the end.
    const first = bad('first');
    const mid = bad('mid');
    const last = bad('last');
    const mixed: unknown[] = [first, rule('a'), mid, 'mid-2', rule('b'), rule('c'), last, null];
    /** Edit the readable rules the way the list does (on a copy) and write them back. */
    const edit = (fn: (rs: ReadableRule[]) => readonly { id: string }[]) => {
      const result = replaceReadable(mixed, fn(structuredClone(mixed.filter(isReadableRule))));
      // Every unreadable entry is written back as the same value.
      for (const entry of mixed.filter((e) => !isReadableRule(e))) expect(result).toContain(entry);
      return result;
    };

    it('changes nothing when no rule changed', () => {
      expect(edit((rs) => rs)).toEqual(mixed);
    });

    it.each([
      ['a', [first, mid, 'mid-2', rule('b'), rule('c'), last, null]],
      ['b', [first, rule('a'), mid, 'mid-2', rule('c'), last, null]],
      ['c', [first, rule('a'), mid, 'mid-2', rule('b'), last, null]],
    ])('deletes rule %s in place', (id, expected) => {
      expect(edit((rs) => rs.filter((r) => r.id !== id))).toEqual(expected);
    });

    it('deletes several rules in place', () => {
      const ac = new Set(['a', 'c']);
      expect(edit((rs) => rs.filter((r) => !ac.has(r.id)))).toEqual([
        first,
        mid,
        'mid-2',
        rule('b'),
        last,
        null,
      ]);
      expect(edit(() => [])).toEqual([first, mid, 'mid-2', last, null]);
    });

    it('keeps an edited rule where it was', () => {
      const changed = { ...rule('b'), description: '改过', group: '新分组' };
      expect(edit((rs) => rs.map((r) => (r.id === 'b' ? changed : r)))).toEqual([
        first,
        rule('a'),
        mid,
        'mid-2',
        changed,
        rule('c'),
        last,
        null,
      ]);
    });

    it('enables and disables rules in place', () => {
      const off = (id: string) => ({ ...rule(id), enabled: false });
      const disabled = edit((rs) => rs.map((r) => (r.id === 'b' ? r : { ...r, enabled: false })));
      expect(disabled).toEqual([first, off('a'), mid, 'mid-2', rule('b'), off('c'), last, null]);
      const again = replaceReadable(
        disabled,
        disabled.filter(isReadableRule).map((r) => ({ ...r, enabled: true })),
      );
      expect(again).toEqual(mixed);
    });

    it('appends only new rules, after the trailing unreadable entries', () => {
      expect(edit((rs) => [...rs, rule('n')])).toEqual([...mixed, rule('n')]);
      // A new rule listed first is still appended; a deletion in the same edit stays in place.
      expect(edit((rs) => [rule('n'), ...rs.filter((r) => r.id !== 'a')])).toEqual([
        first,
        mid,
        'mid-2',
        rule('b'),
        rule('c'),
        last,
        null,
        rule('n'),
      ]);
      expect(replaceReadable([first], [rule('n')])).toEqual([first, rule('n')]);
    });

    it('keeps entries at their positions even if the edited list comes back reordered', () => {
      expect(edit((rs) => [...rs].reverse())).toEqual(mixed);
    });

    it('pairs rules with a duplicate id with their entries in order', () => {
      const one = { ...rule('d'), description: '1' };
      const two = { ...rule('d'), description: '2' };
      const dup: unknown[] = [one, mid, two];
      expect(replaceReadable(dup, [one, { ...two, enabled: false }])).toEqual([
        one,
        mid,
        { ...two, enabled: false },
      ]);
    });
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
            // No step form: any JSON value, as Rust's serde_json::Value.
            step({ type: 'reusable', action_id: 'a', parameters: { n: { deep: [1, null] } } }),
            step({ type: 'delay', ms: 1 }, { type: 'declared', process: 'notepad.exe' }),
            // Range limits of the Rust types (u16, u64, i32, usize); MAX_DELAY_MS is a validation
            // rule with its own diagnostic, not a reason to stop reading the rule.
            step({ type: 'key_press', key: { type: 'windows_code', code: 0 } }),
            step({ type: 'key_press', key: { type: 'windows_code', code: 65535 } }),
            step({ type: 'delay', ms: 0 }),
            step({ type: 'delay', ms: 3_600_001 }),
            step({ type: 'wheel', axis: 'vertical', amount: -2147483648, unit: 'clicks' }),
            step({ type: 'wheel', axis: 'vertical', amount: 2147483647, unit: 'clicks' }),
            {
              operation: { type: 'delay', ms: 1 },
              span: { line: 0, column: 0, length: 4096 },
              target: { type: 'locked' },
            },
          ),
        }),
        withAction({
          type: 'steps',
          plan: { version: 2, dialect: 'inputscript', source: 'delay:1', steps: [] },
        }),
        withAction({ type: 'steps', plan: { ...plan(), dialect: 'send_keys' } }),
        withAction({ type: 'replay', plan: { ...plan(), dialect: 'replay' }, speed: 1 }),
        withAction({ type: 'replay', plan: plan(), speed: 1.5 }),
        withAction({ type: 'open', target: 'x', args: ['-a'], working_directory: 'C:\\' }),
        withAction({ type: 'open', target: 'x', args: [] }),
        withAction({
          type: 'reusable',
          action_id: 'a',
          parameters: { n: 1, s: 'x', b: false, z: null },
        }),
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
      ['terminator.keys (number)', { ...base, terminator: { keys: 42, consume: true } }],
      ['terminator.keys[0]', { ...base, terminator: { keys: [1], consume: true } }],
      ['terminator.consume', { ...base, terminator: { keys: ['space'], consume: 'yes' } }],
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
      ['open.args (string)', withAction({ type: 'open', target: 'x', args: '-a -b' })],
      [
        'open.working_directory',
        withAction({ type: 'open', target: 'x', args: [], working_directory: 3 }),
      ],
      ['reusable.action_id', withAction({ type: 'reusable', action_id: 1 })],
      ['reusable.parameters', withAction({ type: 'reusable', action_id: 'a', parameters: [] })],
      ['common.function', withAction({ type: 'common', function: null })],
      ['quicker.parameter', withAction({ type: 'quicker', action_id: 'q', parameter: 1 })],
      ['parameter', { ...base, parameter: null }],
      ['parameter.type', { ...base, parameter: { type: 'whatever' } }],
      ['parameter.value', { ...base, parameter: { type: 'fixed', value: 3 } }],
      ['parameter.template', { ...base, parameter: { type: 'capture_template' } }],
      ['notification', { ...base, notification: null }],
      ['notification.enabled', { ...base, notification: { enabled: 'yes', message: '' } }],
      ['notification.message', { ...base, notification: { enabled: true, message: 0 } }],
    ];

    const keptUnreadable = (entry: unknown) => {
      expect(isReadableRule(entry)).toBe(false);
      // …and writes it back unchanged, in place.
      const rules = [rule('a'), entry, rule('b')];
      const next = replaceReadable(rules, [{ ...rule('a'), enabled: false }, rule('b')]);
      expect(next[1]).toBe(entry);
      expect(next).toHaveLength(3);
      expect(completeRules(rules)[1]).toBe(entry);
    };

    it.each(malformed)('treats a malformed %s as unreadable', (_, entry) => {
      keptUnreadable(entry);
    });

    // Below the rule itself an unknown key makes the entry unreadable (serde's
    // deny_unknown_fields): the editor rebuilds these objects and would drop the key on save.
    const future = { future_option: { keep: true } };
    const unknownKeys: [string, unknown][] = [
      ['match', { ...base, match: { ...base.match, ...future } }],
      ['terminator', { ...base, terminator: { keys: ['space'], consume: true, ...future } }],
      ['notification', { ...base, notification: { ...base.notification, ...future } }],
      ['parameter', { ...base, parameter: { type: 'default_match', ...future } }],
      ['parameter (fixed)', { ...base, parameter: { type: 'fixed', value: 'v', ...future } }],
      ['action (none)', withAction({ type: 'none', ...future })],
      ['action (text)', withAction({ ...text, ...future })],
      [
        'send_keys.source (inline)',
        withAction({ type: 'send_keys', source: { type: 'inline', value: 'hello', ...future } }),
      ],
      [
        'text.source (file)',
        withAction({ ...text, source: { type: 'file', path: 'a', encoding: 'utf8', ...future } }),
      ],
      ['hotkey.keys[0]', withAction({ type: 'hotkey', keys: [{ ...named('A'), ...future }] })],
      [
        'hotkey.keys[0] (platform_primary)',
        withAction({ type: 'hotkey', keys: [{ type: 'platform_primary', ...future }] }),
      ],
      ['open', withAction({ type: 'open', target: 'x', args: [], ...future })],
      ['reusable', withAction({ type: 'reusable', action_id: 'a', ...future })],
      ['steps.plan', withAction({ type: 'steps', plan: { ...plan(), ...future } })],
      [
        'step',
        withAction({ type: 'steps', plan: plan({ ...step({ type: 'delay', ms: 1 }), ...future }) }),
      ],
      [
        'step.span',
        withAction({
          type: 'steps',
          plan: plan({
            operation: { type: 'delay', ms: 1 },
            span: { line: 1, column: 1, length: 0, ...future },
          }),
        }),
      ],
      [
        'step.target (locked)',
        withAction({
          type: 'steps',
          plan: plan(step({ type: 'delay', ms: 1 }, { type: 'locked', ...future })),
        }),
      ],
      [
        'step.target (declared)',
        withAction({
          type: 'steps',
          plan: plan(step({ type: 'delay', ms: 1 }, { type: 'declared', process: 'a', ...future })),
        }),
      ],
      [
        'step.operation',
        withAction({ type: 'steps', plan: plan(step({ type: 'delay', ms: 1, ...future })) }),
      ],
      [
        'step key_press.key',
        withAction({
          type: 'steps',
          plan: plan(step({ type: 'key_press', key: { ...named('A'), ...future } })),
        }),
      ],
      ['replay.plan', withAction({ type: 'replay', plan: { ...plan(), ...future }, speed: 1 })],
    ];

    it.each(unknownKeys)('treats an unknown key in %s as unreadable', (_, entry) => {
      keptUnreadable(entry);
    });

    // Values the Rust types reject (Codex's counterexamples first), and `null` for a field that
    // is not an Option.
    const delay = (ms: unknown) =>
      withAction({ type: 'steps', plan: plan(step({ type: 'delay', ms })) });
    const code = (c: unknown) =>
      withAction({ type: 'hotkey', keys: [{ type: 'windows_code', code: c }] });
    const wheel = (amount: unknown) =>
      withAction({
        type: 'steps',
        plan: plan(step({ type: 'wheel', axis: 'vertical', amount, unit: 'clicks' })),
      });
    const withSpan = (span: unknown) =>
      withAction({ type: 'steps', plan: plan({ operation: { type: 'delay', ms: 1 }, span }) });
    const withPlan = (fields: object) =>
      withAction({ type: 'steps', plan: { ...plan(), ...fields } });
    const rustInvalid: [string, unknown][] = [
      ['plan.version (string)', withPlan({ version: 'future' })],
      ['plan.dialect (object)', withPlan({ dialect: { bad: true } })],
      ['plan.source (number)', withPlan({ source: 42 })],
      [
        'plan (version, dialect and source together)',
        withPlan({ version: 'future', dialect: { bad: true }, source: 42 }),
      ],
      ['delay.ms (negative fraction)', delay(-1.5)],
      ['windows_code.code (fraction past u16)', code(65536.5)],
      ['plan.version (negative)', withPlan({ version: -1 })],
      ['plan.version (fraction)', withPlan({ version: 1.5 })],
      ['plan.version (past u32)', withPlan({ version: 2 ** 32 })],
      ['plan.dialect (unknown)', withPlan({ dialect: 'yaml' })],
      ['plan.steps (null)', withPlan({ steps: null })],
      ['plan.version (null)', withPlan({ version: null })],
      ['step.span (null)', withSpan(null)],
      ['step.span (string)', withSpan('1:1')],
      ['step.span.line (negative)', withSpan({ line: -1, column: 1, length: 0 })],
      ['step.span.column (fraction)', withSpan({ line: 1, column: 1.5, length: 0 })],
      ['step.span.length (string)', withSpan({ line: 1, column: 1, length: '0' })],
      [
        'step.target (null)',
        withAction({ type: 'steps', plan: plan(step({ type: 'delay', ms: 1 }, null)) }),
      ],
      ['delay.ms (negative)', delay(-1)],
      ['delay.ms (fraction)', delay(1.5)],
      ['delay.ms (past 2^53)', delay(2 ** 53)],
      ['delay.ms (NaN)', delay(Number.NaN)],
      ['windows_code.code (past u16)', code(65536)],
      ['windows_code.code (negative)', code(-1)],
      ['windows_code.code (fraction)', code(13.5)],
      ['wheel.amount (fraction)', wheel(0.5)],
      ['wheel.amount (past i32)', wheel(2 ** 31)],
      ['wheel.amount (below i32)', wheel(-(2 ** 31) - 1)],
      [
        'mouse_move.y (infinite)',
        withAction({
          type: 'steps',
          plan: plan(step({ type: 'mouse_move', x: 0, y: Number.POSITIVE_INFINITY })),
        }),
      ],
      ['replay.speed (NaN)', withAction({ type: 'replay', plan: plan(), speed: Number.NaN })],
      ['enabled (null)', { ...base, enabled: null }],
      ['terminator.keys (null)', { ...base, terminator: { keys: null } }],
      ['notification.message (null)', { ...base, notification: { enabled: false, message: null } }],
      [
        'text.source.encoding (null)',
        withAction({ ...text, source: { type: 'file', path: 'a', encoding: null } }),
      ],
      [
        'send_keys.syntax_variables (null)',
        withAction({
          type: 'send_keys',
          source: { type: 'inline', value: '' },
          syntax_variables: null,
        }),
      ],
      ['open.args (null)', withAction({ type: 'open', target: 'x', args: null })],
      [
        'step open.args (missing; no serde default there)',
        withAction({
          type: 'steps',
          plan: plan(step({ type: 'open', target: 'x', working_directory: null })),
        }),
      ],
      [
        'step reusable.parameters (missing; no serde default there)',
        withAction({ type: 'steps', plan: plan(step({ type: 'reusable', action_id: 'a' })) }),
      ],
      [
        'quicker.parameter (null)',
        withAction({ type: 'quicker', action_id: 'q', parameter: null }),
      ],
      [
        'reusable.parameters value the form cannot edit',
        withAction({ type: 'reusable', action_id: 'a', parameters: { n: { deep: 1 } } }),
      ],
      ['match.kind (missing; MatchSpec has no default)', { ...base, match: { value: ';x' } }],
    ];

    it.each(rustInvalid)('treats a Rust-invalid %s as unreadable', (_, entry) => {
      keptUnreadable(entry);
    });

    // A field with a serde default may be absent: it is read, and filled in, as Rust reads it.
    const actionOf = (entry: unknown) => (entry as ReadableRule).action;
    const firstStep = (entry: unknown) =>
      (actionOf(entry).plan as { steps: [Record<string, unknown>] }).steps[0];
    const absent: [string, unknown, (filled: unknown) => unknown, unknown][] = [
      [
        'every rule field',
        {},
        (r) => r,
        {
          id: '',
          enabled: true,
          group: '',
          description: '',
          ignore_case: false,
          match: { kind: 'literal', value: '' },
          trigger_mode: 'terminator',
          terminator: null,
          ime_erasure: null,
          action: { type: 'none' },
          parameter: { type: 'default_match' },
          notification: { enabled: false, message: '' },
        },
      ],
      ['terminator', withoutKey(base, 'terminator'), (r) => (r as ReadableRule).terminator, null],
      [
        'ime_erasure',
        withoutKey(base, 'ime_erasure'),
        (r) => (r as ReadableRule).ime_erasure,
        null,
      ],
      [
        'terminator.keys',
        { ...base, terminator: { consume: false } },
        (r) => (r as ReadableRule).terminator,
        { consume: false, keys: ['space', 'tab', 'enter', 'semicolon'] },
      ],
      [
        'terminator.consume',
        { ...base, terminator: { keys: ['space'] } },
        (r) => (r as ReadableRule).terminator,
        { keys: ['space'], consume: true },
      ],
      [
        'notification.message',
        { ...base, notification: { enabled: true } },
        (r) => (r as ReadableRule).notification,
        { enabled: true, message: '' },
      ],
      [
        'notification.enabled',
        { ...base, notification: { message: 'm' } },
        (r) => (r as ReadableRule).notification,
        { message: 'm', enabled: false },
      ],
      [
        'text.source.encoding',
        withAction({ ...text, source: { type: 'file', path: 'a.txt' } }),
        (r) => actionOf(r).source,
        { type: 'file', path: 'a.txt', encoding: 'utf8' },
      ],
      [
        'send_keys.syntax_variables',
        withAction({ type: 'send_keys', source: { type: 'inline', value: '' } }),
        (r) => actionOf(r).syntax_variables,
        [],
      ],
      [
        'open.args and open.working_directory',
        withAction({ type: 'open', target: 'x' }),
        actionOf,
        { type: 'open', target: 'x', args: [], working_directory: null },
      ],
      [
        'reusable.parameters',
        withAction({ type: 'reusable', action_id: 'a' }),
        actionOf,
        { type: 'reusable', action_id: 'a', parameters: {} },
      ],
      [
        'quicker.parameter',
        withAction({ type: 'quicker', action_id: 'q' }),
        actionOf,
        { type: 'quicker', action_id: 'q', parameter: '' },
      ],
      [
        'every plan field',
        withAction({ type: 'steps', plan: {} }),
        (r) => actionOf(r).plan,
        { version: 1, dialect: 'structured', source: null, steps: [] },
      ],
      [
        'step.span and step.target',
        withAction({ type: 'steps', plan: plan({ operation: { type: 'delay', ms: 1 } }) }),
        firstStep,
        {
          operation: { type: 'delay', ms: 1 },
          span: { line: 0, column: 0, length: 0 },
          target: { type: 'locked' },
        },
      ],
      [
        'step.span fields',
        withAction({
          type: 'steps',
          plan: plan({ ...step({ type: 'delay', ms: 1 }), span: { line: 3 } }),
        }),
        (r) => firstStep(r).span,
        { line: 3, column: 0, length: 0 },
      ],
      [
        'mouse_move.relative and percent',
        withAction({ type: 'steps', plan: plan(step({ type: 'mouse_move', x: 1, y: 2 })) }),
        (r) => firstStep(r).operation,
        { type: 'mouse_move', x: 1, y: 2, relative: false, percent: false },
      ],
      [
        'step open.working_directory',
        withAction({ type: 'steps', plan: plan(step({ type: 'open', target: 'x', args: [] })) }),
        (r) => firstStep(r).operation,
        { type: 'open', target: 'x', args: [], working_directory: null },
      ],
    ];

    it.each(absent)('reads an absent %s as its serde default', (_, entry, read, expected) => {
      expect(isReadableRule(entry)).toBe(true);
      const [filled] = completeRules([entry]);
      expect(isReadableRule(filled)).toBe(true);
      expect(read(filled)).toStrictEqual(expected);
      // The entry itself is not changed.
      expect(read(entry)).not.toStrictEqual(expected);
    });

    it('completes nothing when every field is there, keeping arrays and objects', () => {
      const rules: unknown[] = [rule('a'), { id: 'raw', match: 1 }, withAction(text), null];
      expect(completeRules(rules)).toBe(rules);
      const partial = { ...withoutKey(base, 'notification'), future_rule_field: 1 };
      const done = completeRules([rules[0], partial, rules[1]]);
      expect(done[0]).toBe(rules[0]);
      expect(done[2]).toBe(rules[1]);
      // A rule-level key of a newer version keeps its place; filled-in fields are appended.
      expect(Object.keys(done[1] as object)).toEqual([...Object.keys(partial), 'notification']);
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
