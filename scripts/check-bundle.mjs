// Release-bundle guard (FE-18): the in-memory mock backend (src/ipc/mock.ts) must never reach a
// production build, whatever VITE_MOCK says. Usage: `npm run check:bundle`.
//
// 1. Runs `npm run build` plainly and with VITE_MOCK=1, and after each one scans every file in
//    dist/ for a mock chunk and for markers that only exist in mock.ts. Any hit fails the check.
// 2. Control: builds once more with NODE_ENV=development (import.meta.env.DEV = true, so the mock
//    is bundled) into a temporary directory, and requires the scan to find it there. This proves
//    the markers survive minification, so zero hits above means "no mock", not "scan is blind".
import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const mockSource = join(root, 'src', 'ipc', 'mock.ts');

// String literals and property names from mock.ts (these survive minification), plus a few
// identifiers that only survive unminified (harmless extra coverage). `createPreviewTransport` is
// not one: transport.ts names it at the (dev-only) import site; the chunk check covers that import.
const MARKERS = [
  'quickerMock',
  'mock-rule-',
  'mock-unreadable-',
  'mock-exec-',
  'mock-import-',
  'mock-legacy',
  'mock-health',
  'mock-safe-mode',
  'mock-platform',
  'mock-dialog',
  'addTolerantSample',
  'recordExecution',
  'MockBackend',
  'createMockTransport',
];

function filesUnder(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    return entry.isDirectory() ? filesUnder(path) : [path];
  });
}

function fail(message) {
  console.error(`check:bundle FAILED: ${message}`);
  process.exit(1);
}

// The markers must still identify the mock: present in mock.ts, absent from other app sources.
const mockText = readFileSync(mockSource, 'utf8');
const stale = MARKERS.filter((marker) => !mockText.includes(marker));
if (stale.length > 0) fail(`markers no longer in src/ipc/mock.ts: ${stale.join(', ')}`);
for (const file of filesUnder(join(root, 'src'))) {
  if (file === mockSource || /\.test\.tsx?$/.test(file)) continue;
  const text = readFileSync(file, 'utf8');
  const shared = MARKERS.filter((marker) => text.includes(marker));
  if (shared.length > 0) {
    fail(`markers also used by ${relative(root, file)}: ${shared.join(', ')}`);
  }
}

/** Mock chunks and marker hits in a build output directory. */
function scan(dir) {
  if (!existsSync(join(dir, 'index.html'))) fail(`${dir} has no index.html; did the build run?`);
  const files = filesUnder(dir);
  const chunks = files.filter((file) => /mock/i.test(basename(file)));
  const hits = new Map();
  for (const file of files) {
    const text = readFileSync(file, 'utf8');
    for (const marker of MARKERS) {
      const count = text.split(marker).length - 1;
      if (count > 0) hits.set(marker, (hits.get(marker) ?? 0) + count);
    }
  }
  return { files: files.length, chunks: chunks.map((file) => relative(dir, file)), hits };
}

function describe({ files, chunks, hits }) {
  const total = [...hits.values()].reduce((sum, n) => sum + n, 0);
  const list = [...hits].map(([marker, n]) => `${marker}×${n}`).join(', ');
  return (
    `${files} files, mock chunks: ${chunks.length > 0 ? chunks.join(', ') : 'none'}, ` +
    `marker hits: ${total}${list ? ` (${list})` : ''}`
  );
}

function run(command, args, env, shell) {
  const result = spawnSync(command, args, { cwd: root, env, stdio: 'inherit', shell });
  if (result.status !== 0) fail(`${[command, ...args].join(' ')} exited with ${result.status}`);
}

const { VITE_MOCK: _ignored, ...baseEnv } = process.env;
const results = [];

for (const [name, env] of [
  ['npm run build', { ...baseEnv, NODE_ENV: 'production' }],
  ['VITE_MOCK=1 npm run build', { ...baseEnv, NODE_ENV: 'production', VITE_MOCK: '1' }],
]) {
  console.log(`\n== ${name}`);
  run('npm run build', [], env, true);
  const found = scan(join(root, 'dist'));
  console.log(`-- ${name}: ${describe(found)}`);
  results.push({ name, found });
}

console.log('\n== control: NODE_ENV=development vite build (mock bundled on purpose)');
const controlDir = mkdtempSync(join(tmpdir(), 'qt-check-bundle-'));
try {
  const vite = join(root, 'node_modules', 'vite', 'bin', 'vite.js');
  const args = [vite, 'build', '--outDir', controlDir, '--emptyOutDir', '--logLevel', 'warn'];
  run(process.execPath, args, { ...baseEnv, NODE_ENV: 'development' }, false);
  const control = scan(controlDir);
  console.log(`-- control: ${describe(control)}`);
  if (control.chunks.length === 0 || control.hits.size === 0) {
    fail('the control build with the mock bundled was not detected; the scan is blind');
  }
} finally {
  rmSync(controlDir, { recursive: true, force: true });
}

const leaks = results.filter(({ found }) => found.chunks.length > 0 || found.hits.size > 0);
if (leaks.length > 0) fail(`mock code in: ${leaks.map(({ name }) => name).join('; ')}`);
console.log('\ncheck:bundle passed: no mock code in either production build.');
