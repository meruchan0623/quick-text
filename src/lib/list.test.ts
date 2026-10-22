import { describe, expect, it } from 'vitest';
import { moveItem, withoutKey } from './list';

describe('moveItem', () => {
  it('moves an item forwards and backwards', () => {
    expect(moveItem(['a', 'b', 'c', 'd'], 0, 2)).toEqual(['b', 'c', 'a', 'd']);
    expect(moveItem(['a', 'b', 'c', 'd'], 3, 1)).toEqual(['a', 'd', 'b', 'c']);
  });

  it('swaps neighbours like the step 上移 / 下移 buttons', () => {
    expect(moveItem(['a', 'b', 'c'], 1, 0)).toEqual(['b', 'a', 'c']);
    expect(moveItem(['a', 'b', 'c'], 1, 2)).toEqual(['a', 'c', 'b']);
  });

  it('does not mutate the input and ignores an out-of-range source', () => {
    const xs = ['a', 'b'];
    expect(moveItem(xs, 5, 0)).toEqual(['a', 'b']);
    expect(moveItem(xs, 0, 1)).not.toBe(xs);
    expect(xs).toEqual(['a', 'b']);
  });
});

describe('withoutKey', () => {
  it('removes one key and keeps the order of the rest', () => {
    const r = withoutKey({ a: 1, b: 2, c: 3 }, 'b');
    expect(Object.keys(r)).toEqual(['a', 'c']);
  });

  it('returns an equal copy when the key is absent', () => {
    const src = { a: 1 };
    const r = withoutKey(src, 'x');
    expect(r).toEqual(src);
    expect(r).not.toBe(src);
  });
});
