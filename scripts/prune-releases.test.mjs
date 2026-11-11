import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ghClient, main, pinnedVersions, planPrune } from './prune-releases.mjs';

const beta = (v) => ({ tag: `v${v}`, prerelease: true });
const stable = (v) => ({ tag: `v${v}`, prerelease: false });

test('keeps the newest N betas by semver, not by string order', () => {
  const releases = ['0.1.9', '0.1.10', '0.1.11', '0.1.8', '0.2.1'].map(beta);
  const { remove, keep } = planPrune({ releases, pinned: [], keepBeta: 3, keepStable: Infinity });
  assert.deepEqual(keep, ['v0.2.1', 'v0.1.11', 'v0.1.10']);
  assert.deepEqual(remove, ['v0.1.9', 'v0.1.8']);
});

test('never touches channel releases, drafts or non-version tags; stable kept by default', () => {
  const releases = [
    { tag: 'beta', prerelease: true },
    { tag: 'stable', prerelease: true },
    { tag: 'v0.1.1-rc1', prerelease: true },
    { tag: 'test-12-abc', prerelease: true },
    { tag: 'v0.1.2', prerelease: true, draft: true },
    beta('0.1.3'),
    beta('0.1.4'),
    stable('1.0.0'),
    stable('1.0.1'),
  ];
  const { remove, keep } = planPrune({ releases, pinned: [], keepBeta: 1, keepStable: Infinity });
  assert.deepEqual(remove, ['v0.1.3']);
  assert.deepEqual(keep.sort(), ['v0.1.4', 'v1.0.0', 'v1.0.1']);
});

test('a version a channel manifest points at survives even past the limit', () => {
  const releases = [beta('0.1.5'), beta('0.1.6'), beta('0.1.7'), stable('1.0.0'), stable('1.1.0')];
  const { remove } = planPrune({ releases, pinned: ['0.1.5', '1.0.0'], keepBeta: 1, keepStable: 1 });
  assert.deepEqual(remove, ['v0.1.6']);
});

test('rejects bad limits and unreadable manifests', () => {
  assert.throws(() => planPrune({ releases: [], pinned: [], keepBeta: 0, keepStable: Infinity }), /keepBeta/);
  assert.deepEqual(pinnedVersions({ beta: { version: '0.1.5' }, stable: null }), ['0.1.5']);
  assert.throws(() => pinnedVersions({ beta: { version: 'x' } }), /beta/);
  assert.throws(() => pinnedVersions({ beta: {} }), /beta/);
});

function fakeGh({ releases, manifests, failDelete = [] }) {
  const deleted = [];
  return {
    deleted,
    releases: () => releases,
    manifest: async (_repo, c) => {
      const m = manifests[c];
      if (m instanceof Error) throw m;
      return m;
    },
    remove: (_repo, tag) => {
      if (failDelete.includes(tag)) throw new Error('boom');
      deleted.push(tag);
    },
  };
}

test('cli deletes only what the plan says; dry-run deletes nothing', async () => {
  const setup = () => fakeGh({ releases: ['0.1.1', '0.1.2', '0.1.3'].map(beta), manifests: { beta: { version: '0.1.3' }, stable: null } });
  const gh = setup();
  assert.equal(await main(['--repo', 'o/r', '--keep-beta', '1'], gh), 0);
  assert.deepEqual(gh.deleted, ['v0.1.2', 'v0.1.1']);
  const dry = setup();
  assert.equal(await main(['--repo', 'o/r', '--keep-beta', '1', '--dry-run'], dry), 0);
  assert.deepEqual(dry.deleted, []);
});

test('cli deletes nothing when a manifest cannot be read, and reports failed deletions', async () => {
  const broken = fakeGh({ releases: ['0.1.1', '0.1.2'].map(beta), manifests: { beta: new Error('HTTP 502'), stable: null } });
  assert.equal(await main(['--repo', 'o/r', '--keep-beta', '1'], broken), 2);
  assert.deepEqual(broken.deleted, []);
  const partial = fakeGh({ releases: ['0.1.1', '0.1.2', '0.1.3'].map(beta), manifests: { beta: null, stable: null }, failDelete: ['v0.1.2'] });
  assert.equal(await main(['--repo', 'o/r', '--keep-beta', '1'], partial), 1);
  assert.deepEqual(partial.deleted, ['v0.1.1']);
  assert.equal(await main(['--keep-beta', '1']), 2);
  assert.equal(await main(['--repo', 'o/r', '--keep-beta', '0']), 2);
});

test('gh client: 404 manifest means unpublished, other HTTP errors throw, releases parse', async () => {
  const calls = [];
  const run = (args) => {
    calls.push(args);
    return '{"tag":"v0.1.1","prerelease":true,"draft":false}\n{"tag":"beta","prerelease":true,"draft":false}\n';
  };
  const responses = { beta: { status: 404, ok: false }, stable: { status: 502, ok: false } };
  const fetchImpl = async (url) => responses[url.includes('/updater-beta/') ? 'beta' : 'stable'];
  const gh = ghClient(run, fetchImpl);
  assert.equal(await gh.manifest('o/r', 'beta'), null);
  await assert.rejects(gh.manifest('o/r', 'stable'), /HTTP 502/);
  assert.equal(gh.releases('o/r').length, 2);
  gh.remove('o/r', 'v0.1.1');
  assert.deepEqual(calls.at(-1), ['release', 'delete', 'v0.1.1', '--repo', 'o/r', '--yes', '--cleanup-tag']);
});
