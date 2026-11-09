#!/usr/bin/env node
// The app's version (SemVer 2.0.0, https://semver.org) lives in several manifests that must agree:
//
//   package.json                    "version"
//   package-lock.json               "version" and packages[""].version
//   src-tauri/tauri.conf.json       "version" (what the binary reports: app.package_info().version)
//   Cargo.toml                      [workspace.package] version (every workspace crate inherits it)
//   Cargo.lock                      the entries of the workspace crates
//
// Usage (no dependencies, Node 18+):
//
//   node scripts/version.mjs check
//       Fails unless every manifest holds the same valid SemVer version. Prints it.
//
//   node scripts/version.mjs bump <major|minor|patch|premajor|preminor|prepatch|prerelease> [id]
//   node scripts/version.mjs bump <X.Y.Z[-pre][+build]> [--allow-downgrade]
//       Rewrites every manifest to the next version (npm's `semver.inc` rules; build metadata is
//       dropped by the keywords). An explicit version must be greater than the current one unless
//       --allow-downgrade is given. Nothing is written when any manifest disagrees beforehand.
//
//   node scripts/version.mjs compare <a> <b>
//       Prints -1, 0 or 1 by SemVer precedence (build metadata ignored).
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

// The regular expression suggested by semver.org (numbered groups).
const SEMVER =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$/;

export function parse(version) {
  const m = SEMVER.exec(String(version));
  if (!m) return null;
  return {
    major: Number(m[1]),
    minor: Number(m[2]),
    patch: Number(m[3]),
    pre: m[4] === undefined ? [] : m[4].split('.').map((x) => (/^\d+$/.test(x) ? Number(x) : x)),
    build: m[5] === undefined ? [] : m[5].split('.'),
  };
}

export function format(v) {
  const core = `${v.major}.${v.minor}.${v.patch}`;
  const pre = v.pre.length ? `-${v.pre.join('.')}` : '';
  const build = v.build?.length ? `+${v.build.join('.')}` : '';
  return core + pre + build;
}

function cmpId(a, b) {
  const an = typeof a === 'number';
  const bn = typeof b === 'number';
  if (an && bn) return a === b ? 0 : a < b ? -1 : 1;
  if (an) return -1;
  if (bn) return 1;
  return a === b ? 0 : a < b ? -1 : 1;
}

/** SemVer 2.0.0 §11 precedence; build metadata is ignored. */
export function compare(a, b) {
  const x = typeof a === 'string' ? parse(a) : a;
  const y = typeof b === 'string' ? parse(b) : b;
  if (!x || !y) throw new Error(`not a SemVer version: ${!x ? a : b}`);
  for (const k of ['major', 'minor', 'patch']) if (x[k] !== y[k]) return x[k] < y[k] ? -1 : 1;
  if (!x.pre.length || !y.pre.length) return x.pre.length === y.pre.length ? 0 : x.pre.length ? -1 : 1;
  for (let i = 0; i < Math.max(x.pre.length, y.pre.length); i += 1) {
    if (i >= x.pre.length) return -1;
    if (i >= y.pre.length) return 1;
    const c = cmpId(x.pre[i], y.pre[i]);
    if (c !== 0) return c;
  }
  return 0;
}

/** npm `semver.inc` for the release keywords. */
export function bump(current, kind, id) {
  const v = parse(current);
  if (!v) throw new Error(`not a SemVer version: ${current}`);
  const next = { major: v.major, minor: v.minor, patch: v.patch, pre: [], build: [] };
  const preOf = () => (id ? [id, 0] : [0]);
  switch (kind) {
    case 'major':
      if (!(v.pre.length && v.minor === 0 && v.patch === 0)) next.major += 1;
      next.minor = 0;
      next.patch = 0;
      break;
    case 'minor':
      if (!(v.pre.length && v.patch === 0)) next.minor += 1;
      next.patch = 0;
      break;
    case 'patch':
      if (!v.pre.length) next.patch += 1;
      break;
    case 'premajor':
      Object.assign(next, { major: v.major + 1, minor: 0, patch: 0, pre: preOf() });
      break;
    case 'preminor':
      Object.assign(next, { minor: v.minor + 1, patch: 0, pre: preOf() });
      break;
    case 'prepatch':
      Object.assign(next, { patch: v.patch + 1, pre: preOf() });
      break;
    case 'prerelease':
      if (!v.pre.length) {
        Object.assign(next, { patch: v.patch + 1, pre: preOf() });
      } else if (id && v.pre[0] !== id) {
        next.pre = [id, 0];
      } else {
        const pre = [...v.pre];
        let i = pre.length - 1;
        while (i >= 0 && typeof pre[i] !== 'number') i -= 1;
        if (i < 0) pre.push(0);
        else pre[i] += 1;
        next.pre = pre;
      }
      break;
    default:
      throw new Error(`unknown release kind: ${kind}`);
  }
  if (id !== undefined && !/^[0-9A-Za-z-]+$/.test(id)) throw new Error(`bad prerelease id: ${id}`);
  const text = format(next);
  if (!parse(text)) throw new Error(`bump produced an invalid version: ${text}`);
  return text;
}

// ---- manifests ----

const read = (rel) => readFileSync(join(ROOT, rel), 'utf8');

/** Workspace members' package names, from the root Cargo.toml (members = [...]). */
function workspaceCrates(cargoToml) {
  const members = /^\s*members\s*=\s*\[([^\]]*)\]/m.exec(cargoToml);
  if (!members) throw new Error('Cargo.toml: no [workspace] members');
  return [...members[1].matchAll(/"([^"]+)"/g)].map(([, dir]) => {
    const toml = read(join(dir, 'Cargo.toml'));
    const name = /^\s*name\s*=\s*"([^"]+)"/m.exec(toml)?.[1];
    if (!name) throw new Error(`${dir}/Cargo.toml: no package name`);
    if (!/^\s*version\.workspace\s*=\s*true/m.test(toml))
      throw new Error(`${dir}/Cargo.toml: version must be \`version.workspace = true\``);
    return name;
  });
}

const WORKSPACE_VERSION = /(\[workspace\.package\][^[]*?^\s*version\s*=\s*")([^"]*)(")/m;

/** `[[package]]` blocks of path (workspace) crates: no `source` line. */
function lockEntry(name) {
  const escaped = name.replace(/[.*+?^${}()|[\]\\-]/g, '\\$&');
  return new RegExp(`(\\[\\[package\\]\\]\\r?\\nname = "${escaped}"\\r?\\nversion = ")([^"]*)(")`, 'g');
}

function collect() {
  const found = [];
  const pkg = JSON.parse(read('package.json'));
  found.push(['package.json', pkg.version]);
  const lock = JSON.parse(read('package-lock.json'));
  found.push(['package-lock.json', lock.version]);
  found.push(['package-lock.json packages[""]', lock.packages?.['']?.version]);
  const tauri = JSON.parse(read('src-tauri/tauri.conf.json'));
  found.push(['src-tauri/tauri.conf.json', tauri.version]);
  const cargo = read('Cargo.toml');
  found.push(['Cargo.toml [workspace.package]', WORKSPACE_VERSION.exec(cargo)?.[2]]);
  const cargoLock = read('Cargo.lock');
  for (const name of workspaceCrates(cargo)) {
    const entries = [...cargoLock.matchAll(lockEntry(name))];
    if (entries.length !== 1) found.push([`Cargo.lock ${name}`, undefined]);
    else found.push([`Cargo.lock ${name}`, entries[0][2]]);
  }
  return found;
}

function check() {
  const found = collect();
  const problems = [];
  for (const [where, version] of found) {
    if (version === undefined) problems.push(`${where}: version not found`);
    else if (!parse(version)) problems.push(`${where}: "${version}" is not a SemVer 2.0.0 version`);
  }
  const versions = new Set(found.map(([, v]) => v));
  if (versions.size > 1) {
    problems.push('the manifests disagree:');
    for (const [where, version] of found) problems.push(`  ${where}: ${version}`);
  }
  if (problems.length) throw new Error(problems.join('\n'));
  return found[0][1];
}

/** Replaces the first `count` `"version": "…"` values of a JSON text, then checks the result. */
function replaceJsonVersions(rel, count, next, verify) {
  const text = read(rel);
  let left = count;
  const out = text.replace(/("version"\s*:\s*")([^"]*)(")/g, (all, a, _old, c) => {
    if (left === 0) return all;
    left -= 1;
    return a + next + c;
  });
  if (left !== 0) throw new Error(`${rel}: fewer than ${count} version fields`);
  const before = JSON.parse(text);
  const after = JSON.parse(out);
  verify(before, next);
  if (JSON.stringify(before) !== JSON.stringify(after))
    throw new Error(`${rel}: rewriting the version would change other fields`);
  return out;
}

function write(next) {
  const current = check();
  const files = {};
  files['package.json'] = replaceJsonVersions('package.json', 1, next, (o, v) => {
    o.version = v;
  });
  files['package-lock.json'] = replaceJsonVersions('package-lock.json', 2, next, (o, v) => {
    o.version = v;
    o.packages[''].version = v;
  });
  files['src-tauri/tauri.conf.json'] = replaceJsonVersions(
    'src-tauri/tauri.conf.json',
    1,
    next,
    (o, v) => {
      o.version = v;
    },
  );
  const cargo = read('Cargo.toml');
  files['Cargo.toml'] = cargo.replace(WORKSPACE_VERSION, `$1${next}$3`);
  let cargoLock = read('Cargo.lock');
  for (const name of workspaceCrates(cargo)) cargoLock = cargoLock.replace(lockEntry(name), `$1${next}$3`);
  files['Cargo.lock'] = cargoLock;
  for (const [rel, text] of Object.entries(files)) writeFileSync(join(ROOT, rel), text);
  const now = check();
  if (now !== next) throw new Error(`after the rewrite the manifests say ${now}, not ${next}`);
  return current;
}

const KINDS = ['major', 'minor', 'patch', 'premajor', 'preminor', 'prepatch', 'prerelease'];

function main(argv) {
  const [command, ...args] = argv;
  switch (command) {
    case 'check': {
      console.log(`version ${check()} (all manifests agree)`);
      return;
    }
    case 'bump': {
      const [target, id] = args.filter((a) => !a.startsWith('--'));
      if (!target) throw new Error(`usage: bump <${KINDS.join('|')}> [id] | bump <X.Y.Z[-pre]>`);
      const current = check();
      let next;
      if (KINDS.includes(target)) next = bump(current, target, id);
      else {
        if (!parse(target)) throw new Error(`"${target}" is not a SemVer 2.0.0 version`);
        if (compare(target, current) <= 0 && !args.includes('--allow-downgrade'))
          throw new Error(`${target} is not greater than ${current} (pass --allow-downgrade to force)`);
        next = target;
      }
      write(next);
      console.log(`${current} -> ${next}`);
      return;
    }
    case 'compare':
      console.log(String(compare(args[0], args[1])));
      return;
    default:
      throw new Error('usage: version.mjs <check|bump|compare> ...');
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(`version: ${error.message}`);
    process.exit(1);
  }
}
