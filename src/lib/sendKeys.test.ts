import { describe, expect, it } from 'vitest';
import {
  appendKey,
  appendText,
  itemLabel,
  keyFromEvent,
  parseRecording,
  recordingSource,
  undoLast,
  type KeyPress,
  type Recorded,
} from './sendKeys';

const key = (k: string, mods: ('Ctrl' | 'Shift' | 'Alt')[] = [], repeat = 1): Recorded => ({
  kind: 'key',
  mods,
  key: k,
  repeat,
});
const press = (p: Partial<KeyPress>): KeyPress => ({
  key: '',
  code: '',
  ctrlKey: false,
  shiftKey: false,
  altKey: false,
  metaKey: false,
  ...p,
});

describe('parseRecording', () => {
  it('reads text, keys, counts and placeholders as blocks', () => {
    expect(parseRecording('您好{Enter}{Tab 3}^{A}+{Tab}%{F4}{cliptext}{0:yyyy-MM-dd}')).toEqual([
      { kind: 'text', text: '您好' },
      key('Enter'),
      key('Tab', [], 3),
      key('A', ['Ctrl']),
      key('Tab', ['Shift']),
      key('F4', ['Alt']),
      { kind: 'token', token: 'cliptext' },
      { kind: 'token', token: '0:yyyy-MM-dd' },
    ]);
  });
  it('reads the short forms the backend accepts', () => {
    expect(parseRecording('a~^a^+1')).toEqual([
      { kind: 'text', text: 'a' },
      key('Enter'),
      key('a', ['Ctrl']),
      key('D1', ['Ctrl', 'Shift']),
    ]);
  });
  it('reads escaped symbols as text', () => {
    expect(parseRecording('1{+}1{{}x{}}{%}{~}{(}{)}{[}{]}{^}')).toEqual([
      { kind: 'text', text: '1+1{x}%~()[]^' },
    ]);
  });
  it('gives null for what the blocks cannot show or the backend refuses', () => {
    for (const source of [
      '^(ac)',
      '(ab)',
      '{Enter',
      '{Foo}',
      '{Win}',
      '{F17}',
      '{#13}',
      '{Enter 0}',
      '{Enter x}',
      '^{+}',
      '^,',
      '^',
      '^^a',
      'a)b',
      '^{cliptext}',
    ])
      expect(parseRecording(source), source).toBeNull();
  });
  it('gives no blocks for an empty source', () => {
    expect(parseRecording('')).toEqual([]);
  });
});

describe('recordingSource', () => {
  it('writes blocks back as a source that reads as the same blocks', () => {
    const items: Recorded[] = [
      { kind: 'text', text: '1+1={x}~(a)[b]^%' },
      key('Enter', [], 3),
      key('A', ['Ctrl']),
      key('OEMCOMMA', ['Ctrl', 'Shift']),
      { kind: 'token', token: 'selection' },
      { kind: 'text', text: '好' },
    ];
    const source = recordingSource(items);
    expect(source).toBe(
      '1{+}1={{}x{}}{~}{(}a{)}{[}b{]}{^}{%}{Enter 3}^{A}^+{OEMCOMMA}{selection}好',
    );
    expect(parseRecording(source)).toEqual(items);
  });
});

describe('editing blocks', () => {
  it('joins typed text and counts a key pressed again', () => {
    let items = appendText([], '您');
    items = appendText(items, '好');
    items = appendKey(items, key('Enter') as never);
    items = appendKey(items, key('ENTER') as never);
    items = appendKey(items, key('Enter', ['Ctrl']) as never);
    expect(items).toEqual([
      { kind: 'text', text: '您好' },
      key('Enter', [], 2),
      key('Enter', ['Ctrl']),
    ]);
    expect(items.map(itemLabel)).toEqual(['您好', '回车 ×2', 'Ctrl+回车']);
  });
  it('takes back the last character or the last press', () => {
    let items: Recorded[] = [{ kind: 'text', text: '您好' }, key('Enter', [], 2)];
    items = undoLast(items);
    expect(items).toEqual([{ kind: 'text', text: '您好' }, key('Enter')]);
    items = undoLast(undoLast(items));
    expect(items).toEqual([{ kind: 'text', text: '您' }]);
    expect(undoLast(items)).toEqual([]);
    expect(undoLast([])).toEqual([]);
  });
  it('labels placeholders and keys in plain words', () => {
    expect(
      [
        { kind: 'token', token: 'cliptext' },
        { kind: 'token', token: 'param:name' },
        key('Backspace'),
        key('Escape'),
        key('D5', ['Ctrl']),
        key('Left'),
      ].map((i) => itemLabel(i as Recorded)),
    ).toEqual(['已复制的文字', '传入的内容：name', '退格', 'Esc', 'Ctrl+5', '←']);
  });
});

describe('keyFromEvent', () => {
  it('leaves characters to the input box', () => {
    expect(keyFromEvent(press({ key: 'a', code: 'KeyA' }))).toBe('text');
    expect(keyFromEvent(press({ key: 'A', code: 'KeyA', shiftKey: true }))).toBe('text');
    expect(keyFromEvent(press({ key: ' ', code: 'Space' }))).toBe('text');
    expect(keyFromEvent(press({ key: '，', code: 'Comma' }))).toBe('text');
  });
  it('records keys and combinations', () => {
    expect(keyFromEvent(press({ key: 'Enter' }))).toEqual(key('Enter'));
    expect(keyFromEvent(press({ key: 'Backspace' }))).toEqual(key('Backspace'));
    expect(keyFromEvent(press({ key: 'Escape' }))).toEqual(key('Escape'));
    expect(keyFromEvent(press({ key: 'ArrowUp' }))).toEqual(key('Up'));
    expect(keyFromEvent(press({ key: 'Tab', shiftKey: true }))).toEqual(key('Tab', ['Shift']));
    expect(keyFromEvent(press({ key: 'A', code: 'KeyA', ctrlKey: true, shiftKey: true }))).toEqual(
      key('A', ['Ctrl', 'Shift']),
    );
    expect(keyFromEvent(press({ key: ',', code: 'Comma', ctrlKey: true }))).toEqual(
      key('OEMCOMMA', ['Ctrl']),
    );
    expect(keyFromEvent(press({ key: '1', code: 'Digit1', altKey: true }))).toEqual(
      key('D1', ['Alt']),
    );
    expect(keyFromEvent(press({ key: 'F4', altKey: true }))).toEqual(key('F4', ['Alt']));
  });
  it('ignores a modifier alone and refuses keys SendKeys cannot press', () => {
    expect(keyFromEvent(press({ key: 'Control', ctrlKey: true }))).toBeNull();
    expect(keyFromEvent(press({ key: 'Shift', shiftKey: true }))).toBeNull();
    expect(keyFromEvent(press({ key: 'Meta', metaKey: true }))).toBe('unsupported');
    expect(keyFromEvent(press({ key: 'd', code: 'KeyD', metaKey: true }))).toBe('unsupported');
    expect(keyFromEvent(press({ key: 'F17' }))).toBe('unsupported');
    expect(keyFromEvent(press({ key: 'AudioVolumeUp' }))).toBe('unsupported');
  });
  it('records what the backend reads back', () => {
    const keys = [
      press({ key: 'a', code: 'KeyA', ctrlKey: true }),
      press({ key: '.', code: 'Period', ctrlKey: true, shiftKey: true }),
      press({ key: 'PageDown' }),
      press({ key: 'F16' }),
      press({ key: '1', code: 'Numpad1', ctrlKey: true }),
    ].map((p) => keyFromEvent(p) as Recorded);
    expect(parseRecording(recordingSource(keys))).toEqual(keys);
  });
});
