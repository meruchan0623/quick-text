#!/usr/bin/env node
// Retention of the CI-published releases (docs/auto-update.md).
//
// Every push to main publishes a beta release `vX.Y.Z`, so old ones pile up.
// Clients only ever read the channel manifests (`updater-beta` / `updater-stable` releases,
// `latest.json`) and download the version those point at, so older versioned
// releases are safe to delete. Rules:
//   - only releases whose tag is exactly vX.Y.Z are candidates; the channel
//     releases and anything else are never touched;
//   - beta = prerelease, stable = full release; keep the newest N of each
//     (by semver; stable defaults to keeping all);
//   - a version referenced by any channel's latest.json is always kept;
//   - if a channel manifest exists but cannot be read, nothing is deleted.
//
// Usage (in CI, after latest.json is uploaded):
//   GH_TOKEN=... node scripts/prune-releases.mjs --repo OWNER/REPO \
//        [--keep-beta 10] [--keep-stable all] [--dry-run]
// std-only plus the `gh` CLI. Exit 0 ok, 1 a deletion failed, 2 usage / read error.

import { execFileSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';

export const CHANNELS = ['beta', 'stable'];
const TAG_RE = /^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

function semverKey(tag) {
  const m = TAG_RE.exec(tag);
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
}

function newestFirst(a, b) {
  const x = semverKey(a.tag);
  const y = semverKey(b.tag);
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return y[i] - x[i];
  return 0;
}

/**
 * @param {{ releases: {tag: string, prerelease: boolean, draft?: boolean}[],
 *           pinned: string[], keepBeta: number, keepStable: number }} input
 *   `pinned` are versions (x.y.z) referenced by a channel manifest.
 * @returns {{ remove: string[], keep: string[] }}
 */
export function planPrune({ releases, pinned, keepBeta, keepStable }) {
  for (const [name, n] of [['keepBeta', keepBeta], ['keepStable', keepStable]]) {
    if (!(n === Infinity || (Number.isInteger(n) && n >= 1))) throw new Error(`${name} must be a positive integer or Infinity`);
  }
  const pinnedTags = new Set(pinned.map((v) => `v${v}`));
  const candidates = releases.filter((r) => semverKey(r.tag) && !r.draft);
  const remove = [];
  const keep = [];
  for (const [prerelease, limit] of [[true, keepBeta], [false, keepStable]]) {
    const group = candidates.filter((r) => Boolean(r.prerelease) === prerelease).sort(newestFirst);
    group.forEach((r, i) => (i < limit || pinnedTags.has(r.tag) ? keep : remove).push(r.tag));
  }
  return { remove, keep };
}

/** Versions the channel manifests point at; throws when a manifest exists but is unreadable. */
export function pinnedVersions(manifests) {
  const out = [];
  for (const [channel, m] of Object.entries(manifests)) {
    if (m === null) continue; // channel never published
    if (!m || typeof m.version !== 'string' || !semverKey(`v${m.version}`)) throw new Error(`${channel}/latest.json has no usable version`);
    out.push(m.version);
  }
  return out;
}

function parseArgs(argv) {
  const opts = { keepBeta: 10, keepStable: Infinity, dryRun: false };
  const count = (v, flag) => {
    if (v === 'all') return Infinity;
    const n = Number(v);
    if (!Number.isInteger(n) || n < 1) throw new Error(`${flag} must be a positive integer or "all"`);
    return n;
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const take = () => {
      const v = argv[++i];
      if (v === undefined) throw new Error(`${a} needs a value`);
      return v;
    };
    if (a === '--repo') opts.repo = take();
    else if (a === '--keep-beta') opts.keepBeta = count(take(), a);
    else if (a === '--keep-stable') opts.keepStable = count(take(), a);
    else if (a === '--dry-run') opts.dryRun = true;
    else throw new Error(`unknown argument ${a}`);
  }
  if (!/^[\w.-]+\/[\w.-]+$/.test(opts.repo ?? '')) throw new Error('--repo OWNER/REPO is required');
  return opts;
}

/** Real `gh` access; injectable for tests. */
export function ghClient(
  run = (args) => execFileSync('gh', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }),
  fetchImpl = globalThis.fetch,
) {
  return {
    releases(repo) {
      const out = run(['api', '--paginate', `repos/${repo}/releases?per_page=100`, '--jq', '.[] | {tag: .tag_name, prerelease, draft}']);
      return out.split('\n').filter((l) => l.trim()).map((l) => JSON.parse(l));
    },
    /** Read exactly what clients read; a 404 means the channel was never published. */
    async manifest(repo, channel) {
      const res = await fetchImpl(`https://github.com/${repo}/releases/download/updater-${channel}/latest.json`, { redirect: 'follow' });
      if (res.status === 404) return null;
      if (!res.ok) throw new Error(`${channel}/latest.json: HTTP ${res.status}`);
      return res.json();
    },
    remove(repo, tag) {
      run(['release', 'delete', tag, '--repo', repo, '--yes', '--cleanup-tag']);
    },
  };
}

export async function main(argv, gh = ghClient()) {
  let opts;
  try {
    opts = parseArgs(argv);
  } catch (e) {
    console.error(`prune-releases: ${e.message}`);
    return 2;
  }
  let plan;
  try {
    const manifests = {};
    for (const c of CHANNELS) manifests[c] = await gh.manifest(opts.repo, c);
    const pinned = pinnedVersions(manifests);
    plan = planPrune({ releases: gh.releases(opts.repo), pinned, keepBeta: opts.keepBeta, keepStable: opts.keepStable });
    console.log(`pinned by manifests: ${pinned.join(', ') || '(none)'}; keeping ${plan.keep.length}, removing ${plan.remove.length}`);
  } catch (e) {
    console.error(`prune-releases: cannot read ${opts.repo}, nothing deleted: ${e.message}`);
    return 2;
  }
  let failed = false;
  for (const tag of plan.remove) {
    if (opts.dryRun) {
      console.log(`would delete ${tag}`);
      continue;
    }
    try {
      gh.remove(opts.repo, tag);
      console.log(`deleted ${tag}`);
    } catch (e) {
      console.error(`prune-releases: delete ${tag} failed: ${e.message}`);
      failed = true;
    }
  }
  return failed ? 1 : 0;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) process.exitCode = await main(process.argv.slice(2));
