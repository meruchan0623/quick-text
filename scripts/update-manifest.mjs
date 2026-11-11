#!/usr/bin/env node
// Updater manifest (`latest.json`) generator for the release pipeline (docs/auto-update.md).
//
// Every asset is verified against the public key that the client embeds
// (`src-tauri/update-keys/current.pub`) BEFORE the manifest is written:
// a wrong TAURI_SIGNING_PRIVATE_KEY secret, a tampered file or a signature
// bound to another version fails here instead of on every client.
//
// Usage:
//   node scripts/update-manifest.mjs --version 0.1.42 \
//        --pubkey src-tauri/update-keys/current.pub \
//        --base-url https://github.com/OWNER/REPO/releases/download/v0.1.42 \
//        [--notes "text"] [--pub-date 2026-11-11T00:00:00Z] --out latest.json \
//        darwin-aarch64=path/app.tar.gz windows-x86_64=path/setup.exe ...
//
// Each asset needs `<file>.sig` next to it (written by `tauri build` /
// `tauri signer sign --app-version`). std-only. Exit 0 ok, 1 verification
// failure, 2 usage / IO error.

import { createHash, createPublicKey, verify } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { basename } from 'node:path';
import { pathToFileURL } from 'node:url';

export const PLATFORMS = ['darwin-aarch64', 'darwin-x86_64', 'windows-x86_64'];
const VERSION_RE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

/** Tauri wraps the whole minisign text in one more base64 layer. */
function unwrap(tauriB64) {
  const text = Buffer.from(String(tauriB64).trim(), 'base64').toString('utf8');
  return text.split(/\r?\n/).map((l) => l.trim());
}

function keyIdHex(bytes) {
  return Buffer.from(bytes.subarray(2, 10)).reverse().toString('hex').toUpperCase();
}

export function parsePubkey(tauriB64) {
  const line = unwrap(tauriB64).find((l) => l && !l.startsWith('untrusted comment:'));
  const bin = line ? Buffer.from(line, 'base64') : Buffer.alloc(0);
  if (bin.length !== 42 || bin.toString('latin1', 0, 2) !== 'Ed') throw new Error('not a minisign public key');
  return { keyId: keyIdHex(bin), key: bin.subarray(10) };
}

export function parseSignature(tauriB64) {
  const lines = unwrap(tauriB64);
  const i = lines.findIndex((l) => l.startsWith('untrusted comment:'));
  const sigLine = lines[i + 1] ?? '';
  const trusted = lines[i + 2] ?? '';
  const globalLine = lines[i + 3] ?? '';
  const bin = Buffer.from(sigLine, 'base64');
  const alg = bin.toString('latin1', 0, 2);
  if (i < 0 || bin.length !== 74 || (alg !== 'ED' && alg !== 'Ed')) throw new Error('not a minisign signature');
  if (!trusted.startsWith('trusted comment: ')) throw new Error('signature has no trusted comment');
  return {
    prehashed: alg === 'ED',
    keyId: keyIdHex(bin),
    sig: bin.subarray(10),
    trusted: trusted.slice('trusted comment: '.length),
    globalSig: Buffer.from(globalLine, 'base64'),
  };
}

function ed25519(raw) {
  return createPublicKey({ key: { kty: 'OKP', crv: 'Ed25519', x: Buffer.from(raw).toString('base64url') }, format: 'jwk' });
}

/** Full minisign verification plus tauri's `version:` binding (requireSignedVersion). */
export function verifyAsset({ pubkey, signature, data, version }) {
  const pk = parsePubkey(pubkey);
  const s = parseSignature(signature);
  if (s.keyId !== pk.keyId) return { ok: false, reason: `signed by key ${s.keyId}, expected ${pk.keyId}` };
  const key = ed25519(pk.key);
  const msg = s.prehashed ? createHash('blake2b512').update(data).digest() : data;
  if (!verify(null, msg, key, s.sig)) return { ok: false, reason: 'file signature does not verify' };
  if (s.globalSig.length !== 64 || !verify(null, Buffer.concat([s.sig, Buffer.from(s.trusted, 'utf8')]), key, s.globalSig)) {
    return { ok: false, reason: 'trusted comment signature does not verify' };
  }
  const bound = s.trusted.split('\t').find((p) => p.startsWith('version:'))?.slice('version:'.length);
  if (bound !== version) return { ok: false, reason: `signature bound to version ${bound ?? '(none)'}, expected ${version}` };
  return { ok: true, keyId: pk.keyId };
}

export function buildManifest({ version, notes, pubDate, baseUrl, assets }) {
  if (!VERSION_RE.test(version)) throw new Error(`version must be x.y.z: ${version}`);
  if (!/^https:\/\/[^\s?#]+[^/]$/.test(baseUrl)) throw new Error('base url must be https without a trailing slash');
  const platforms = {};
  for (const a of assets) {
    if (!PLATFORMS.includes(a.platform)) throw new Error(`unknown platform ${a.platform}`);
    if (platforms[a.platform]) throw new Error(`duplicate platform ${a.platform}`);
    platforms[a.platform] = { signature: a.signature.trim(), url: `${baseUrl}/${encodeURIComponent(a.name)}` };
  }
  if (Object.keys(platforms).length === 0) throw new Error('no assets');
  return { version, notes: notes ?? '', pub_date: pubDate, platforms };
}

function parseArgs(argv) {
  const opts = { assets: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const take = () => {
      const v = argv[++i];
      if (v === undefined) throw new Error(`${a} needs a value`);
      return v;
    };
    if (a === '--version') opts.version = take();
    else if (a === '--pubkey') opts.pubkey = take();
    else if (a === '--base-url') opts.baseUrl = take();
    else if (a === '--notes') opts.notes = take();
    else if (a === '--pub-date') opts.pubDate = take();
    else if (a === '--out') opts.out = take();
    else if (/^[a-z0-9_-]+=/.test(a)) {
      const at = a.indexOf('=');
      opts.assets.push({ platform: a.slice(0, at), path: a.slice(at + 1) });
    } else throw new Error(`unknown argument ${a}`);
  }
  for (const k of ['version', 'pubkey', 'baseUrl', 'out']) if (!opts[k]) throw new Error(`missing --${k.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`)}`);
  return opts;
}

export function main(argv) {
  let opts;
  try {
    opts = parseArgs(argv);
  } catch (e) {
    console.error(`update-manifest: ${e.message}`);
    return 2;
  }
  const pubkey = readFileSync(opts.pubkey, 'utf8');
  if (!pubkey.trim()) {
    console.error('update-manifest: public key file is empty');
    return 2;
  }
  const assets = [];
  let failed = false;
  for (const a of opts.assets) {
    const signature = readFileSync(`${a.path}.sig`, 'utf8');
    const r = verifyAsset({ pubkey, signature, data: readFileSync(a.path), version: opts.version });
    if (!r.ok) {
      console.error(`update-manifest: ${a.platform} ${basename(a.path)}: ${r.reason}`);
      failed = true;
      continue;
    }
    console.log(`verified ${a.platform} ${basename(a.path)} (key ${r.keyId})`);
    assets.push({ platform: a.platform, name: basename(a.path), signature });
  }
  if (failed) return 1;
  try {
    const manifest = buildManifest({ version: opts.version, notes: opts.notes, pubDate: opts.pubDate ?? new Date().toISOString(), baseUrl: opts.baseUrl, assets });
    writeFileSync(opts.out, `${JSON.stringify(manifest, null, 2)}\n`);
  } catch (e) {
    console.error(`update-manifest: ${e.message}`);
    return 2;
  }
  return 0;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) process.exitCode = main(process.argv.slice(2));
