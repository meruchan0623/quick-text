import { describe, expect, it } from 'vitest';
import { parseLines, sameItems } from './text';

describe('parseLines', () => {
  it('gives one trimmed entry per non-blank line', () => {
    expect(parseLines('a.exe\n  b.exe  \n\nc.exe')).toEqual(['a.exe', 'b.exe', 'c.exe']);
  });

  it('ignores a trailing newline and accepts CRLF', () => {
    expect(parseLines('a.exe\n')).toEqual(['a.exe']);
    expect(parseLines('a.exe\r\nb.exe\r\n')).toEqual(['a.exe', 'b.exe']);
    expect(parseLines('')).toEqual([]);
  });
});

describe('sameItems', () => {
  it('compares item by item', () => {
    expect(sameItems(['a', 'b'], ['a', 'b'])).toBe(true);
    expect(sameItems(['a', 'b'], ['b', 'a'])).toBe(false);
    expect(sameItems(['a'], ['a', 'b'])).toBe(false);
    expect(sameItems([], [])).toBe(true);
  });
});
