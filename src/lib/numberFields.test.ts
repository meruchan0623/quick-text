import { describe, expect, it } from 'vitest';
import {
  actionDiagnostics,
  actionNumberProblems,
  parseNumber,
  rangeError,
  rangeText,
  ruleEditDiagnostics,
} from './numberFields';
import { MAX_DELAY_MS, ranges } from './ruleShape';

const rule = (action: unknown) => ({
  id: 'r',
  enabled: true,
  group: '默认',
  description: '',
  ignore_case: false,
  match: { kind: 'literal', value: ';r' },
  trigger_mode: 'immediate',
  terminator: null,
  ime_erasure: null,
  action,
  parameter: { type: 'default_match' },
  notification: { enabled: false, message: '' },
});

const step = (operation: unknown) => ({
  operation,
  span: { line: 0, column: 0, length: 0 },
  target: { type: 'locked' },
});

const steps = (...operations: unknown[]) => ({
  type: 'steps',
  plan: { version: 1, dialect: 'structured', source: null, steps: operations.map(step) },
});

describe('parseNumber', () => {
  it('takes a JSON number within the IPC number contract, and no further', () => {
    expect(parseNumber('9007199254740991', ranges.json)).toBe(9007199254740991);
    expect(parseNumber('-9007199254740991', ranges.json)).toBe(-9007199254740991);
    // Converts to 2^53: rounded, so out of range rather than taken as the rounded value.
    expect(parseNumber('9007199254740993', ranges.json)).toBeNull();
    expect(parseNumber('9007199254740992', ranges.json)).toBeNull();
    expect(parseNumber('1e300', ranges.json)).toBeNull();
    expect(parseNumber(' 1.5 ', ranges.json)).toBe(1.5);
    expect(parseNumber('-.5e1', ranges.json)).toBe(-5);
  });

  it('refuses text that is not a number at all', () => {
    for (const text of ['', ' ', '-', '1e', 'abc', '0x10', 'Infinity', 'NaN', '1,000', '1 2'])
      expect(parseNumber(text, ranges.json), text).toBeNull();
  });

  it('takes digits only for an integer field, within its Rust range', () => {
    expect(parseNumber(String(MAX_DELAY_MS), ranges.delayMs)).toBe(MAX_DELAY_MS);
    expect(parseNumber(String(MAX_DELAY_MS + 1), ranges.delayMs)).toBeNull();
    expect(parseNumber('-1', ranges.delayMs)).toBeNull();
    expect(parseNumber('1.5', ranges.delayMs)).toBeNull();
    expect(parseNumber('1e3', ranges.delayMs)).toBeNull();
    expect(parseNumber('+7', ranges.delayMs)).toBe(7);
    expect(parseNumber('255', ranges.windowsKeyCode)).toBe(255);
    expect(parseNumber('0', ranges.windowsKeyCode)).toBeNull();
    expect(parseNumber('256', ranges.windowsKeyCode)).toBeNull();
    expect(parseNumber('70000', ranges.windowsKeyCode)).toBeNull();
    expect(parseNumber('10000', ranges.repeat)).toBe(10000);
    expect(parseNumber('10001', ranges.repeat)).toBeNull();
    expect(parseNumber('-2147483648', ranges.wheelAmount)).toBe(-2147483648);
    expect(parseNumber('2147483648', ranges.wheelAmount)).toBeNull();
  });

  it('checks coordinates against i32, or 0..100 as percentages, and speed as positive', () => {
    expect(parseNumber('2147483647', ranges.coordinate)).toBe(2147483647);
    expect(parseNumber('2147483648', ranges.coordinate)).toBeNull();
    expect(parseNumber('-2147483649', ranges.coordinate)).toBeNull();
    expect(parseNumber('12.5', ranges.coordinate)).toBe(12.5);
    expect(parseNumber('100', ranges.percent)).toBe(100);
    expect(parseNumber('100.5', ranges.percent)).toBeNull();
    expect(parseNumber('0.01', ranges.replaySpeed)).toBe(0.01);
    expect(parseNumber('0', ranges.replaySpeed)).toBeNull();
    expect(parseNumber('-1', ranges.replaySpeed)).toBeNull();
  });
});

describe('rangeText / rangeError', () => {
  it('says the range in words', () => {
    expect(rangeText(ranges.delayMs)).toBe('0 到 3600000 之间的整数');
    expect(rangeText(ranges.json)).toBe('-9007199254740991 到 9007199254740991 之间的数字');
    expect(rangeText(ranges.replaySpeed)).toBe('大于 0 的数字');
    expect(rangeError(ranges.coordinate)).toBe('请输入 -2147483648 到 2147483647 之间的数字。');
    expect(rangeError(ranges.replaySpeed)).toBe('请输入大于 0 的数字。');
  });
});

describe('actionNumberProblems / ruleEditDiagnostics', () => {
  it('finds nothing in an action the editor wrote', () => {
    const action = steps(
      { type: 'delay', ms: MAX_DELAY_MS },
      { type: 'mouse_move', x: -5, y: 2147483647, relative: false, percent: false },
      { type: 'mouse_move', x: 0, y: 100, relative: false, percent: true },
      { type: 'wheel', axis: 'vertical', amount: -3, unit: 'clicks' },
      { type: 'key_press', key: { type: 'windows_code', code: 13 } },
      {
        type: 'hotkey',
        keys: [
          { type: 'named', name: 'Ctrl' },
          { type: 'windows_code', code: 1 },
        ],
      },
    );
    expect(actionNumberProblems(action)).toEqual([]);
    expect(ruleEditDiagnostics(rule(action), 0)).toEqual([]);
    const reusable = { type: 'reusable', action_id: 'x', parameters: { n: 9007199254740991 } };
    expect(ruleEditDiagnostics(rule(reusable), 0)).toEqual([]);
    expect(actionDiagnostics(reusable)).toEqual([]);
  });

  it('flags a reusable parameter past the IPC number contract, or held as NaN', () => {
    for (const n of [9007199254740992, NaN, Infinity]) {
      const action = { type: 'reusable', action_id: 'x', parameters: { text: 'a', n } };
      expect(ruleEditDiagnostics(rule(action), 4)).toEqual([
        {
          code: 'number_range',
          message: '参数「n」的值应为 -9007199254740991 到 9007199254740991 之间的数字。',
          path: 'rules[4].action.parameters.n',
          span: { line: 0, column: 0, length: 0 },
          severity: 'error',
        },
      ]);
    }
  });

  it('flags each step number past its Rust range, with a path to the step', () => {
    const action = steps(
      { type: 'text', text: 'a', method: 'keys' },
      { type: 'delay', ms: MAX_DELAY_MS + 1 },
      { type: 'mouse_move', x: 2147483648, y: NaN, relative: false, percent: false },
      { type: 'mouse_move', x: 50, y: 101, relative: false, percent: true },
      { type: 'wheel', axis: 'vertical', amount: 1.5, unit: 'clicks' },
      { type: 'key_down', key: { type: 'windows_code', code: 300 } },
      {
        type: 'hotkey',
        keys: [
          { type: 'named', name: 'Ctrl' },
          { type: 'windows_code', code: NaN },
        ],
      },
    );
    expect(ruleEditDiagnostics(rule(action), 1).map((d) => [d.path, d.message])).toEqual([
      [
        'rules[1].action.plan.steps[1].operation.ms',
        '等待时间（毫秒）应为 0 到 3600000 之间的整数。',
      ],
      [
        'rules[1].action.plan.steps[2].operation.x',
        '左右位置 X应为 -2147483648 到 2147483647 之间的数字。',
      ],
      [
        'rules[1].action.plan.steps[2].operation.y',
        '上下位置 Y应为 -2147483648 到 2147483647 之间的数字。',
      ],
      ['rules[1].action.plan.steps[3].operation.y', '上下位置 Y应为 0 到 100 之间的数字。'],
      [
        'rules[1].action.plan.steps[4].operation.amount',
        '滚动量应为 -2147483648 到 2147483647 之间的整数。',
      ],
      ['rules[1].action.plan.steps[5].operation.key.code', '键码应为 1 到 255 之间的整数。'],
      ['rules[1].action.plan.steps[6].operation.keys[1].code', '键码应为 1 到 255 之间的整数。'],
    ]);
    // The same checks for 「测试动作」, with paths from the action.
    expect(actionDiagnostics(action).map((d) => d.path)[0]).toBe('plan.steps[1].operation.ms');
  });

  it('flags a replay speed that is not positive, and hotkey action codes', () => {
    const replay = { ...steps(), type: 'replay', speed: 0 };
    expect(actionDiagnostics(replay).map((d) => d.message)).toEqual(['倍速应为大于 0 的数字。']);
    const hotkey = { type: 'hotkey', keys: [{ type: 'windows_code', code: 0 }] };
    expect(actionDiagnostics(hotkey).map((d) => d.path)).toEqual(['keys[0].code']);
  });

  it('refuses any other shape the backend would not read as meant', () => {
    const odd = rule({ type: 'text', method: 'keys', source: { type: 'inline', value: 1 } });
    expect(ruleEditDiagnostics(odd, 2)).toEqual([
      {
        code: 'not_savable',
        message: '含有无法识别的内容，请检查后再试。',
        path: 'rules[2]',
        span: { line: 0, column: 0, length: 0 },
        severity: 'error',
      },
    ]);
    expect(actionDiagnostics({ type: 'unknown' }).map((d) => d.path)).toEqual(['']);
  });
});
