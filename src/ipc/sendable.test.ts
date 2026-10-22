import { describe, expect, it } from 'vitest';
import { invalidNumber } from '../lib/numberFields';
import { unsendablePath } from './sendable';

describe('unsendablePath', () => {
  it('passes anything JSON carries as it is', () => {
    expect(unsendablePath({ a: [1, -0.5, 9007199254740991, 'NaN', null, true, { b: {} }] })).toBe(
      null,
    );
    expect(unsendablePath(undefined)).toBe(null);
    expect(unsendablePath('text')).toBe(null);
  });

  it('finds NaN, ±Infinity and the invalid-number marker anywhere, first in document order', () => {
    expect(unsendablePath(NaN)).toBe('');
    expect(unsendablePath({ a: [0, Infinity] })).toBe('a[1]');
    expect(unsendablePath({ a: { b: -Infinity }, c: NaN })).toBe('a.b');
    expect(unsendablePath([{ ok: 1 }, { n: invalidNumber('300') }], 'args')).toBe('args[1].n.nan');
    // Even when JSON.stringify would let it through as an ordinary object.
    const marker = invalidNumber('3600001');
    expect(JSON.parse(JSON.stringify({ n: marker }))).toEqual({
      n: { invalidNumber: '3600001', nan: null },
    });
    expect(unsendablePath({ action: { parameters: { n: marker } } })).toBe(
      'action.parameters.n.nan',
    );
  });

  it('walks arrays and objects of any width without a call per element (Codex round 6)', () => {
    // Wide enough that `push(...items)` throws a RangeError; JSON.stringify writes it fine.
    const wide = (): unknown[] => new Array<number>(200_000).fill(0);
    const config = {
      config: { rules: [{ future: wide() }, { id: 'raw', match: 42, future: wide() }] },
    };
    expect(JSON.stringify(config).length).toBeGreaterThan(400_000);
    expect(unsendablePath(config)).toBe(null);

    const last = wide();
    last[199_999] = NaN;
    expect(unsendablePath({ config: { rules: [{ future: last }] } })).toBe(
      'config.rules[0].future[199999]',
    );
    const keys = Object.fromEntries(wide().map((n, i) => [`k${String(i)}`, n]));
    keys.k199999 = Infinity;
    expect(unsendablePath({ keys })).toBe('keys.k199999');
  });

  it('ends on cyclic values', () => {
    const cyclic: Record<string, unknown> = { a: 1 };
    cyclic.self = cyclic;
    expect(unsendablePath(cyclic)).toBe(null);
    cyclic.bad = NaN;
    expect(unsendablePath(cyclic)).toBe('bad');
  });
});
