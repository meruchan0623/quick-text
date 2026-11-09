import { afterEach, describe, expect, it } from 'vitest';
import { MockBackend, createMockTransport } from '../ipc/mock';
import { setTransport } from '../ipc/transport';
import {
  checkUpdate,
  compareSemver,
  downloadUpdate,
  isPrerelease,
  onUpdateProgress,
  parseSemver,
  revealUpdate,
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

  it('checks, downloads with progress and reveals', async () => {
    const backend = new MockBackend({ platform: 'macos', updateDelayMs: 0 });
    setTransport(createMockTransport(backend));

    const stable = await checkUpdate(false);
    expect(stable.update_available).toBe(false);
    expect(stable.latest?.version).toBe(stable.current);
    const check = await checkUpdate(true);
    expect(check.update_available).toBe(true);
    const latest = check.latest;
    expect(latest?.prerelease).toBe(true);
    expect(latest?.asset?.name.endsWith('.dmg')).toBe(true);
    expect(backend.calls.at(-1)).toEqual({
      command: 'check_update',
      args: { includePrerelease: true },
    });

    const seen: UpdateProgress[] = [];
    const unlisten = await onUpdateProgress((p) => seen.push(p));
    const result = await downloadUpdate(latest?.tag ?? '');
    unlisten();
    expect(result.verified).toBe(true);
    expect(result.path.endsWith(latest?.asset?.name ?? '?')).toBe(true);
    expect(seen.length).toBeGreaterThan(1);
    expect(seen.at(-1)).toEqual({
      tag: latest?.tag,
      downloaded: latest?.asset?.size,
      total: latest?.asset?.size,
    });

    await expect(revealUpdate(result.path)).resolves.toBeUndefined();
    await expect(revealUpdate('/elsewhere/x.dmg')).rejects.toMatchObject({
      code: 'update',
      detail: { kind: 'not_downloaded' },
    });
  });

  it('refuses a second download while one runs, and an unknown tag', async () => {
    const backend = new MockBackend({ platform: 'windows', updateDelayMs: 5 });
    setTransport(createMockTransport(backend));
    const { latest } = await checkUpdate(true);
    expect(latest?.asset?.name).toMatch(/-Windows-x64-Portable\.zip$/);
    const first = downloadUpdate(latest?.tag ?? '');
    await expect(downloadUpdate(latest?.tag ?? '')).rejects.toMatchObject({
      code: 'update',
      detail: { kind: 'busy' },
    });
    await expect(first).resolves.toMatchObject({ verified: true });
    await expect(downloadUpdate('v9.9.9')).rejects.toMatchObject({
      code: 'update',
      detail: { kind: 'release_not_found' },
    });
  });

  it('has no package for other platforms', async () => {
    setTransport(createMockTransport(new MockBackend({ platform: 'linux' })));
    const { latest } = await checkUpdate(true);
    expect(latest?.asset).toBeNull();
    await expect(downloadUpdate(latest?.tag ?? '')).rejects.toMatchObject({
      code: 'update',
      detail: { kind: 'no_asset' },
    });
  });
});
