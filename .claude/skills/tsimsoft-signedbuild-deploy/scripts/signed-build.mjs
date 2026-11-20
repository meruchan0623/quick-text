#!/usr/bin/env node
// Quicktext（quicker-text）签名打包并发布到 TSim 客服系统更新源（Mac 与 Windows 通用入口）。
//
// 实际的 Tauri/Rust 构建、签名和上传由仓库里的平台脚本完成：
//   macOS (Apple Silicon)  scripts/publish-tsim.sh
//   Windows (x64)          scripts/publish-tsim.ps1（需要 PowerShell 7 / pwsh）
// 本脚本负责：识别平台、发版前检查（不输出任何密钥值）、Windows 依赖源码包准备、调用平台脚本、发布后读回核对。
//
// 用法（在仓库任意目录运行，或用 --repo 指定仓库根目录）：
//   node signed-build.mjs check   [--server test|prod|both]
//   node signed-build.mjs dry-run [--server ...] [--deps <Source-deps.zip>]
//   node signed-build.mjs publish [--server ...] [--draft] [--skip-build] [--deps <Source-deps.zip>]
//   node signed-build.mjs verify  [--server ...] [--version X.Y.Z]
//
// 只依赖 Node 自带模块。

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';

const SERVERS = { prod: 'https://csapi.tsim.tech', test: 'https://test-csapi.tsim.tech' };
const PRODUCT = 'quicktext';
const CHANNEL = 'stable';

// ---------- 参数 ----------
const argv = process.argv.slice(2);
const command = argv[0];
const opt = { server: 'test', draft: false, skipBuild: false, version: null, repo: null, deps: null };
for (let i = 1; i < argv.length; i++) {
  const a = argv[i];
  const val = () => {
    const v = a.includes('=') ? a.slice(a.indexOf('=') + 1) : argv[++i];
    if (!v) die(`${a} 缺少取值`);
    return v;
  };
  if (a.startsWith('--server')) opt.server = val();
  else if (a.startsWith('--version')) opt.version = val();
  else if (a.startsWith('--repo')) opt.repo = val();
  else if (a.startsWith('--deps')) opt.deps = resolve(val());
  else if (a === '--draft') opt.draft = true;
  else if (a === '--skip-build') opt.skipBuild = true;
  else die(`未知参数 ${a}`);
}
if (!['check', 'dry-run', 'publish', 'verify'].includes(command ?? '')) {
  console.log('用法：node signed-build.mjs <check|dry-run|publish|verify> [--server test|prod|both] [--draft] [--skip-build] [--deps zip] [--version X.Y.Z] [--repo 路径]');
  process.exit(command ? 2 : 0);
}
if (!['test', 'prod', 'both'].includes(opt.server)) die('--server 只能是 test、prod 或 both');
const serverNames = opt.server === 'both' ? ['test', 'prod'] : [opt.server];

// ---------- 工具 ----------
function die(msg) { console.error(`错误：${msg}`); process.exit(1); }
function step(msg) { console.log(`\n==> ${msg}`); }
const results = [];
function ok(msg) { results.push(['ok', msg]); console.log(`  ✔ ${msg}`); }
function warn(msg) { results.push(['warn', msg]); console.log(`  ! ${msg}`); }
function fail(msg) { results.push(['fail', msg]); console.log(`  ✘ ${msg}`); }

function run(cmd, args, { cwd, env } = {}) {
  const r = spawnSync(cmd, args, { cwd, env: env ? { ...process.env, ...env } : process.env, encoding: 'utf8', shell: false, windowsHide: true });
  return { code: r.status ?? -1, out: (r.stdout ?? '').trim(), err: (r.stderr ?? '').trim(), missing: r.error?.code === 'ENOENT' };
}

function versionGt(a, b) {
  const pa = a.split('.').map(Number), pb = b.split('.').map(Number);
  for (let i = 0; i < 3; i++) if (pa[i] !== pb[i]) return pa[i] > pb[i];
  return false;
}

async function getJson(url) {
  const r = await fetch(url, { signal: AbortSignal.timeout(15000) });
  if (r.status === 204) return { status: 204, body: null };
  if (!r.ok) return { status: r.status, body: null };
  return { status: r.status, body: await r.json() };
}

// ---------- 平台与仓库 ----------
const platform = process.platform === 'darwin' ? 'macos' : process.platform === 'win32' ? 'windows' : null;
if (!platform) die(`不支持的系统 ${process.platform}：只能在 macOS 或 Windows 上打包`);
const isMac = platform === 'macos';

function findRepo() {
  if (opt.repo) return resolve(opt.repo);
  const g = run('git', ['rev-parse', '--show-toplevel']);
  if (g.code === 0 && existsSync(join(g.out, 'scripts', 'publish-tsim.sh'))) return resolve(g.out);
  die('找不到 quicker-text 仓库：请在仓库内运行，或用 --repo 指定根目录');
}
const REPO = findRepo();
const tauriConf = JSON.parse(readFileSync(join(REPO, 'src-tauri', 'tauri.conf.json'), 'utf8'));
const VERSION = opt.version ?? tauriConf.version;
const OUT = join(REPO, 'target', 'tsim-publish', VERSION);

function keyIdOfPub(file) {
  try {
    const text = Buffer.from(readFileSync(file, 'utf8').trim(), 'base64').toString('utf8');
    const raw = Buffer.from(text.split('\n')[1].trim(), 'base64');
    return Buffer.from(raw.subarray(2, 10)).reverse().toString('hex').toUpperCase();
  } catch { return null; }
}

// ---------- 密钥检查：在子进程里加载 env 文件，只回传“有/没有/格式对不对” ----------
const ENV_FILE = process.env.QT_RELEASE_ENV
  ?? (isMac ? join(homedir(), '.quicktext-release.env') : join(homedir(), '.quicktext-release.env.ps1'));
const DEFAULT_KEY = isMac ? join(homedir(), '.config', 'quicktext-signing', 'updater.key') : join(homedir(), '.config', 'quicktext-signing', 'updater.key');

function checkSecrets() {
  existsSync(ENV_FILE) ? ok(`密钥文件 ${ENV_FILE}`) : warn(`没有密钥文件 ${ENV_FILE}：只能靠当前进程环境变量`);
  const publishVars = serverNames.map((s) => (s === 'prod' ? 'TSIM_PUBLISH_KEY_PROD' : 'TSIM_PUBLISH_KEY_TEST'));
  let lines;
  if (isMac) {
    const script = `
      set +u; [[ -f "$1" ]] && { set -a; . "$1" >/dev/null 2>&1 || { echo "fail|加载 $1 失败"; exit 0; }; set +a; }
      dk="$2"
      if [[ -n "\${TAURI_SIGNING_PRIVATE_KEY:-}" ]]; then echo "ok|更新私钥：内容已配置";
      elif [[ -n "\${TAURI_SIGNING_PRIVATE_KEY_PATH:-}" && -f "\$TAURI_SIGNING_PRIVATE_KEY_PATH" ]]; then echo "ok|更新私钥：文件存在";
      elif [[ -z "\${TAURI_SIGNING_PRIVATE_KEY_PATH:-}" && -f "$dk" ]]; then echo "ok|更新私钥：默认文件 $dk 存在";
      else echo "fail|缺少 TAURI_SIGNING_PRIVATE_KEY / TAURI_SIGNING_PRIVATE_KEY_PATH，默认 $dk 也不存在"; fi
      [[ -n "\${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}" ]] && echo "ok|更新私钥密码已配置" || echo "warn|TAURI_SIGNING_PRIVATE_KEY_PASSWORD 为空（私钥无密码时可忽略）"
      id="\${QUICKTEXT_SIGNING_IDENTITY:-Quicktext Local Signing}"
      security find-identity -p codesigning \${QUICKTEXT_SIGNING_KEYCHAIN:+"\$QUICKTEXT_SIGNING_KEYCHAIN"} 2>/dev/null | grep -qF "\\"$id\\"" && echo "ok|codesign 身份 $id 存在" || echo "fail|找不到 codesign 身份 $id（scripts/setup-macos-signing.sh 或导入发版证书）"
      shift 2
      for v in "$@"; do x="\${!v:-}"; [[ "$x" =~ ^tcu_[a-f0-9]{12}\\.[A-Za-z0-9_-]{43}$ ]] && echo "ok|$v 格式正确" || echo "fail|$v 缺失或格式不对（publish 时需要）"; done
    `;
    lines = run('bash', ['-c', script, 'check', ENV_FILE, DEFAULT_KEY, ...publishVars]).out;
  } else {
    const script = `
      [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); $ErrorActionPreference = 'Continue'
      $envFile = $env:QT_CHECK_ENV_FILE
      if (Test-Path -LiteralPath $envFile) { try { . $envFile *> $null } catch { 'fail|加载 ' + $envFile + ' 失败'; return } }
      $dk = $env:QT_CHECK_DEFAULT_KEY
      if ($env:TAURI_SIGNING_PRIVATE_KEY) { 'ok|更新私钥：内容已配置' }
      elseif ($env:TAURI_SIGNING_PRIVATE_KEY_PATH -and (Test-Path -LiteralPath $env:TAURI_SIGNING_PRIVATE_KEY_PATH)) { 'ok|更新私钥：文件存在' }
      elseif (-not $env:TAURI_SIGNING_PRIVATE_KEY_PATH -and (Test-Path -LiteralPath $dk)) { 'ok|更新私钥：默认文件 ' + $dk + ' 存在' }
      else { 'fail|缺少 TAURI_SIGNING_PRIVATE_KEY / TAURI_SIGNING_PRIVATE_KEY_PATH，默认 ' + $dk + ' 也不存在' }
      if ($env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD) { 'ok|更新私钥密码已配置' } else { 'warn|TAURI_SIGNING_PRIVATE_KEY_PASSWORD 为空（私钥无密码时可忽略）' }
      $th = ([string]$env:QT_WIN_CERT_THUMBPRINT -replace '\\s', '').ToUpperInvariant()
      if (-not $th) { 'warn|未设置 QT_WIN_CERT_THUMBPRINT：exe/MSI 不做代码签名，SmartScreen 会提示' }
      elseif ($th -notmatch '^[0-9A-F]{40}$') { 'fail|QT_WIN_CERT_THUMBPRINT 不是 40 位十六进制指纹' }
      else {
        $c = Get-Item -LiteralPath "Cert:\\CurrentUser\\My\\$th" -ErrorAction SilentlyContinue
        if (-not $c) { 'fail|证书库 CurrentUser\\My 里没有该指纹的代码签名证书' }
        elseif (-not $c.HasPrivateKey) { 'fail|代码签名证书没有私钥' }
        else { 'ok|代码签名证书已导入（' + $c.Subject + '）' }
      }
      foreach ($v in ($env:QT_CHECK_PUBLISH_VARS -split ',')) {
        $x = [Environment]::GetEnvironmentVariable($v)
        if ($x -cmatch '^tcu_[a-f0-9]{12}\\.[A-Za-z0-9_-]{43}$') { "ok|$v 格式正确" } else { "fail|$v 缺失或格式不对（publish 时需要）" }
      }
    `;
    lines = run('pwsh', ['-NoProfile', '-Command', `& { ${script} }`], {
      env: { QT_CHECK_ENV_FILE: ENV_FILE, QT_CHECK_DEFAULT_KEY: DEFAULT_KEY, QT_CHECK_PUBLISH_VARS: publishVars.join(',') },
    }).out;
  }
  for (const line of lines.split(/\r?\n/).filter(Boolean)) {
    const [kind, msg] = line.split('|', 2);
    if (kind === 'ok') ok(msg);
    else if (kind === 'warn') warn(msg);
    else if (kind === 'fail') {
      // 发布密钥只在真正上传时需要，check / dry-run 阶段降为提醒。
      if (msg.includes('publish 时需要') && command !== 'publish') warn(msg);
      else fail(msg);
    }
  }
}

// ---------- check ----------
async function check() {
  step(`环境（${platform}，仓库 ${REPO}）`);
  ok(`Node ${process.versions.node}`);
  if (isMac) (process.arch === 'arm64' ? ok('Apple Silicon') : fail('只能在 Apple Silicon Mac 上打包'));
  else {
    process.arch === 'x64' ? ok('Windows x64') : fail(`只能在 x64 Windows 上打包（本机 ${process.arch}）`);
    const p = run('pwsh', ['-NoProfile', '-Command', '$PSVersionTable.PSVersion.Major']);
    (!p.missing && Number(p.out) >= 7) ? ok(`PowerShell ${p.out}`) : fail('缺少 PowerShell 7（pwsh）：publish-tsim.ps1 需要');
  }
  const rustc = run('rustc', ['--version']);
  rustc.missing ? fail('没有 rustc') : ok(rustc.out);
  const target = isMac ? 'aarch64-apple-darwin' : 'x86_64-pc-windows-msvc';
  run('rustup', ['target', 'list', '--installed']).out.split(/\r?\n/).includes(target) ? ok(`Rust target ${target}`) : fail(`缺少 Rust target：rustup target add ${target}`);
  existsSync(join(REPO, 'node_modules', '@tauri-apps', 'cli')) ? ok('node_modules') : fail('缺少 node_modules：在仓库根目录 npm ci');

  step('代码');
  const branch = run('git', ['rev-parse', '--abbrev-ref', 'HEAD'], { cwd: REPO }).out;
  branch === 'main' ? ok('分支 main') : warn(`当前分支 ${branch}，正式发版应在 main`);
  const dirty = run('git', ['status', '--porcelain'], { cwd: REPO }).out;
  dirty ? fail(`工作区有未提交改动（${dirty.split(/\r?\n/).length} 个文件）：先提交`) : ok('工作区干净');
  if (run('git', ['fetch', '--quiet', 'origin'], { cwd: REPO }).code === 0) {
    const counts = run('git', ['rev-list', '--left-right', '--count', 'HEAD...@{upstream}'], { cwd: REPO }).out.split(/\s+/).map(Number);
    if (counts.length === 2 && !Number.isNaN(counts[0])) {
      if (counts[1] > 0) fail(`落后远端 ${counts[1]} 个提交：先 git pull`);
      else if (counts[0] > 0) warn(`有 ${counts[0]} 个提交未推送：发布的提交应能在远端找到`);
      else ok('与远端一致');
    }
  } else warn('git fetch 失败，没核对远端');
  const head = run('git', ['rev-parse', '--short', 'HEAD'], { cwd: REPO }).out;

  step(`版本 ${VERSION}（提交 ${head}）`);
  const vc = run('node', ['scripts/version.mjs', 'check'], { cwd: REPO });
  vc.code === 0 ? ok('各清单版本一致（scripts/version.mjs check）') : fail('版本清单不一致：node scripts/version.mjs check');
  const notesFile = join(REPO, 'release-notes', `${VERSION}.txt`);
  if (!existsSync(notesFile)) fail(`缺少更新说明 release-notes/${VERSION}.txt`);
  else {
    const n = readFileSync(notesFile, 'utf8').replace(/\r\n?/g, '\n').trim();
    (n && [...n].length <= 4000) ? ok(`更新说明（${[...n].length} 字）`) : fail('更新说明必须是 1–4000 字');
  }
  const cur = keyIdOfPub(join(REPO, 'src-tauri', 'update-keys', 'current.pub'));
  cur ? ok(`更新公钥 current=${cur}（私钥是否配对由 dry-run 验签确认）`) : fail('src-tauri/update-keys/current.pub 为空或无法解析');
  if (!isMac && !opt.skipBuild) {
    const deps = findDeps();
    deps ? ok(`依赖源码包 ${deps}`) : warn('还没有依赖源码包：dry-run/publish 会先用 scripts/package-dependency-sources.ps1 生成（数百 MB）');
  }

  step('服务器');
  for (const s of serverNames) {
    try {
      const { status, body } = await getJson(`${SERVERS[s]}/client-updates/v1/${PRODUCT}/${CHANNEL}/${platform}/latest.json`);
      if (status === 204) ok(`${s} 还没有 ${platform} 版本，本次 ${VERSION}`);
      else if (!body) warn(`${s} 读 latest.json 返回 HTTP ${status}`);
      else if (body.version === VERSION) warn(`${s} 已在服务 ${VERSION}：上传会被拒（409）`);
      else if (versionGt(VERSION, body.version)) ok(`${s} 当前 ${body.version}，本次 ${VERSION}`);
      else fail(`${s} 已在服务 ${body.version}，${VERSION} 不更高：先升版本号并补更新说明`);
    } catch (e) { warn(`${s} 读不到：${e.message}`); }
  }

  step('密钥（只检查是否存在与格式，不输出值）');
  checkSecrets();

  const fails = results.filter(([k]) => k === 'fail').length;
  const warns = results.filter(([k]) => k === 'warn').length;
  console.log(`\n检查结果：${fails} 项不通过，${warns} 项提醒。`);
  return fails === 0;
}

// ---------- Windows 依赖源码包 ----------
function findDeps() {
  if (opt.deps) return existsSync(opt.deps) ? opt.deps : null;
  const dir = join(OUT, 'deps');
  if (!existsSync(dir)) return null;
  const f = readdirSync(dir).find((n) => n.endsWith(`-${VERSION}-Source-deps.zip`));
  return f ? join(dir, f) : null;
}
function ensureDeps() {
  const found = findDeps();
  if (found) return found;
  if (opt.deps) die(`--deps 指定的文件不存在：${opt.deps}`);
  step('生成依赖源码包（scripts/package-dependency-sources.ps1）');
  const r = spawnSync('pwsh', ['-NoProfile', '-File', join(REPO, 'scripts', 'package-dependency-sources.ps1'), '-OutputDirectory', join(OUT, 'deps')], { cwd: REPO, stdio: 'inherit' });
  if (r.status !== 0) die(`生成依赖源码包失败（退出码 ${r.status}）`);
  return findDeps() ?? die('生成后仍找不到 Source-deps.zip');
}

// ---------- 调用平台脚本 ----------
function runPlatformScript(mode) {
  let cmd, args;
  if (isMac) {
    cmd = 'bash';
    args = [join(REPO, 'scripts', 'publish-tsim.sh'), '--server', opt.server];
    if (mode === 'dry-run') args.push('--dry-run');
    if (mode === 'publish' && !opt.draft) args.push('--publish');
    if (opt.skipBuild) args.push('--skip-build');
  } else {
    cmd = 'pwsh';
    args = ['-NoProfile', '-File', join(REPO, 'scripts', 'publish-tsim.ps1'), '-Server', opt.server];
    if (mode === 'dry-run') args.push('-DryRun');
    if (mode === 'publish' && !opt.draft) args.push('-Publish');
    if (opt.skipBuild) args.push('-SkipBuild');
    else args.push('-DependencySourcesZip', ensureDeps());
  }
  step(`运行 ${isMac ? 'publish-tsim.sh' : 'publish-tsim.ps1'}（${mode}${mode === 'publish' ? (opt.draft ? '，草稿' : '，直接发布') : ''}）`);
  const r = spawnSync(cmd, args, { cwd: REPO, stdio: 'inherit', windowsHide: false });
  if (r.status !== 0) die(`平台脚本退出码 ${r.status}`);
}

// ---------- verify ----------
async function verify() {
  let allOk = true;
  for (const s of serverNames) {
    step(`读回 ${s}（${SERVERS[s]}）${platform} ${VERSION}`);
    const latest = await getJson(`${SERVERS[s]}/client-updates/v1/${PRODUCT}/${CHANNEL}/${platform}/latest.json`);
    if (latest.body?.version === VERSION) ok(`latest.json 版本 ${VERSION}`);
    else { fail(`latest.json 版本是 ${latest.body?.version ?? `HTTP ${latest.status}`}，不是 ${VERSION}（草稿不会出现在这里）`); allOk = false; }

    const dl = await getJson(`${SERVERS[s]}/client-updates/v1/${PRODUCT}/downloads`);
    const entry = dl.body?.platforms?.[platform];
    if (!entry?.installer?.url || entry.version !== VERSION) { fail('下载页数据里没有这个版本'); allOk = false; continue; }
    ok(`下载页数据：${entry.installer.name}（${(entry.installer.size / 1048576).toFixed(1)} MiB）`);

    const r = await fetch(entry.installer.url, { signal: AbortSignal.timeout(300000) });
    if (!r.ok) { fail(`匿名下载失败 HTTP ${r.status}`); allOk = false; continue; }
    const hash = createHash('sha256');
    let size = 0;
    for await (const chunk of r.body) { hash.update(chunk); size += chunk.length; }
    const sha = hash.digest('hex');
    if (sha === entry.installer.sha256 && size === entry.installer.size) ok(`匿名下载 sha256 一致：${sha}`);
    else { fail(`下载内容与登记不一致（sha256 ${sha}，大小 ${size}）`); allOk = false; }

    const page = s === 'prod' ? 'https://csagent.tsim.tech/downloads' : 'https://test-agent.tsim.tech/downloads';
    console.log(`\n  下载页：${page}\n  文件直链：${entry.installer.url}`);
  }
  return allOk;
}

// ---------- 主流程 ----------
if (command === 'check') {
  process.exit((await check()) ? 0 : 1);
} else if (command === 'dry-run') {
  if (!(await check())) die('检查未通过，先处理上面标 ✘ 的项');
  runPlatformScript('dry-run');
  console.log('\n演练完成：构建、签名与验签通过，未上传。');
} else if (command === 'publish') {
  if (!(await check())) die('检查未通过，先处理上面标 ✘ 的项');
  runPlatformScript('publish');
  if (opt.draft) console.log('\n已上传为草稿：到管理页「客户端发布」发布后再运行 verify。');
  else process.exit((await verify()) ? 0 : 1);
} else if (command === 'verify') {
  process.exit((await verify()) ? 0 : 1);
}
