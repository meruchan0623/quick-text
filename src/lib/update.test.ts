import { afterEach, describe, expect, it } from 'vitest';
import { MockBackend, createMockTransport } from '../ipc/mock';
import { setTransport } from '../ipc/transport';
import {
  checkUpdate,
  compareSemver,
  downloadUpdate,
  installUpdate,
  isPrerelease,
  onUpdateProgress,
  parseSemver,
  type UpdateProgress,
} from './update';

describe('parseSemver', () => {
  it('splits core, prerelease and build', () => {
    expect(parseSemver('1.2.3')).toEqual({
      major: 1,
      minor: 2,
      patch: 3,
      prerelease: [],
      build: [],
    });
    expect(parseSemver('v0.4.0-beta.11+sha.abc1234')).toEqual({
      major: 0,
      minor: 4,
      patch: 0,
      prerelease: ['beta', 11],
      build: ['sha', 'abc1234'],
    });
    expect(parseSemver('1.0.0-0A.is.legal')?.prerelease).toEqual(['0A', 'is', 'legal']);
    expect(parseSemver('1.0.0-x-y-z.--')?.prerelease).toEqual(['x-y-z', '--']);
  });

  it.each([
    '',
    '1',
    '1.2',
    '1.2.3.4',
    '01.2.3',
    '1.02.3',
    '1.2.03',
    '1.2.3-',
    '1.2.3-01',
    '1.2.3-a..b',
    '1.2.3+',
    '1.2.3+a..b',
    'vv1.2.3',
    ' 1.2.3',
    'test-3-ddff827',
    '99999999999999999999.0.0',
  ])('refuses %j', (v) => {
    expect(parseSemver(v)).toBeNull();
  });
});

describe('compareSemver', () => {
  // The precedence example of semver.org §11.
  const ordered = [
    '1.0.0-alpha',
    '1.0.0-alpha.1',
    '1.0.0-alpha.beta',
    '1.0.0-beta',
    '1.0.0-beta.2',
    '1.0.0-beta.11',
    '1.0.0-rc.1',
    '1.0.0',
    '1.0.1-test.1',
    '1.0.1',
    '1.1.0',
    '2.0.0',
    '2.1.0',
    '2.1.1',
    '10.0.0',
  ];

  it('orders the spec examples', () => {
    for (let i = 0; i < ordered.length; i += 1) {
      for (let j = 0; j < ordered.length; j += 1) {
        const a = ordered[i] ?? '';
        const b = ordered[j] ?? '';
        expect(compareSemver(a, b), `${a} vs ${b}`).toBe(Math.sign(i - j));
      }
    }
  });

  it('ignores build metadata and a leading v', () => {
    expect(compareSemver('1.0.0+a', '1.0.0+b')).toBe(0);
    expect(compareSemver('v1.0.0-rc.1', '1.0.0-rc.1+build.5')).toBe(0);
  });

  it('orders CI test builds by run number', () => {
    expect(compareSemver('0.3.2-test.10', '0.3.2-test.9')).toBe(1);
    expect(compareSemver('0.4.0-beta.1.test.3', '0.4.0-beta.1.test.2')).toBe(1);
    expect(compareSemver('0.4.0-beta.1.test.3', '0.4.0-beta.2')).toBe(-1);
  });

  it('sorts invalid versions below valid ones', () => {
    expect(compareSemver('test-4-eb4eebb', '0.0.1')).toBe(-1);
    expect(compareSemver('0.0.1', 'nope')).toBe(1);
    expect(compareSemver('nope', 'also nope')).toBe(0);
  });
});

describe('isPrerelease', () => {
  it('is true only for a valid version with a prerelease part', () => {
    expect(isPrerelease('0.4.0-test.12')).toBe(true);
    expect(isPrerelease('v1.0.0-rc.1+b')).toBe(true);
    expect(isPrerelease('1.0.0+b')).toBe(false);
    expect(isPrerelease('1.0.0')).toBe(false);
    expect(isPrerelease('test-3-ddff827')).toBe(false);
  });
});

describe('update commands (mock backend)', () => {
  afterEach(() => {
    setTransport(null);
  });

  it('checks, downloads with progress and installs', async () => {
    const backend = new MockBackend({ platform: 'macos', updateDelayMs: 0 });
    setTransport(createMockTransport(backend));

    await expect(installUpdate()).rejects.toMatchObject({
      code: 'update',
      detail: { kind: 'not_ready' },
    });
    const check = await checkUpdate();
    expect(check.update_available).toBe(true);
    expect(check.ready).toBe(false);
    const latest = check.latest;
    expect(latest?.source).toBe('production');
    expect(compareSemver(latest?.version ?? '', check.current)).toBe(1);
    expect(backend.calls.at(-1)).toEqual({ command: 'check_update', args: {} });

    const seen: UpdateProgress[] = [];
    const unlisten = await onUpdateProgress((p) => seen.push(p));
    const result = await downloadUpdate(latest?.version ?? '');
    unlisten();
    expect(result).toEqual({ version: latest?.version });
    expect(seen.length).toBeGreaterThan(1);
    expect(seen.at(-1)?.version).toBe(latest?.version);
    expect(seen.at(-1)?.downloaded).toBe(seen.at(-1)?.total);

    expect((await checkUpdate()).ready).toBe(true);
    await expect(installUpdate()).resolves.toBeUndefined();
    expect(backend.updateInstalls).toBe(1);
  });

  it('refuses a second download while one runs, and a version the check did not find', async () => {
    const backend = new MockBackend({ platform: 'windows', updateDelayMs: 5 });
    setTransport(createMockTransport(backend));
    const { latest } = await checkUpdate();
    const first = downloadUpdate(latest?.version ?? '');
    await expect(downloadUpdate(latest?.version ?? '')).rejects.toMatchObject({
      code: 'update',
      detail: { kind: 'busy' },
    });
    await expect(first).resolves.toEqual({ version: latest?.version });
    await expect(downloadUpdate('9.9.9')).rejects.toMatchObject({
      code: 'update',
      detail: { kind: 'stale' },
    });
  });

  it('reports up to date, unreachable servers and builds without an update channel', async () => {
    setTransport(createMockTransport(new MockBackend({ update: 'none' })));
    expect(await checkUpdate()).toMatchObject({ latest: null, update_available: false });
    setTransport(createMockTransport(new MockBackend({ update: 'error' })));
    await expect(checkUpdate()).rejects.toMatchObject({ detail: { kind: 'network' } });
    setTransport(createMockTransport(new MockBackend({ platform: 'linux' })));
    await expect(checkUpdate()).rejects.toMatchObject({ detail: { kind: 'disabled' } });
  });
});
