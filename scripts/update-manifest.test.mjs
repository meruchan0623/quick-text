import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash, generateKeyPairSync, sign } from 'node:crypto';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { buildManifest, main, parsePubkey, verifyAsset } from './update-manifest.mjs';

// Produced by `tauri signer generate` + `tauri signer sign --app-version 0.1.42` (CLI 2.12.4)
// over the five bytes "hello", with a throwaway key. Public data only.
const REAL = {
  pubkey: Buffer.from('untrusted comment: minisign public key: 9D0048ED9393A4E7\nRWTnpJOT7UgAnaa5PbhedLcq+ym2lsvmmQpqjWpiBmXw9ajLnmsJszSn\n').toString('base64'),
  signature: Buffer.from(
    'untrusted comment: signature from tauri secret key\n'
    + 'RUTnpJOT7UgAnX0YXU67WEWFW7WeuwrthZ2k55E12foBk0zyXNRQcWFZxbKDKj5Chi0bVDDRHWkjBwCquMNrMDmdyST2OyI/lgQ=\n'
    + 'trusted comment: timestamp:1794389598\tfile:a.bin\tversion:0.1.42\n'
    + 'aqWpllh91dZsKdrem47nVsO+J/2e8KVwkIIRKzGGWuomCL2ePvlLocPwSsdLyTI2o9IZddnav/dmVBARDfSCBw==\n',
  ).toString('base64'),
  data: Buffer.from('hello'),
};

/** A minisign-compatible signer built on node:crypto, for negative cases. */
function keypair(id) {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519');
  const raw = Buffer.from(publicKey.export({ format: 'jwk' }).x, 'base64url');
  const idBytes = Buffer.alloc(8);
  idBytes.writeBigUInt64LE(id);
  const pub = Buffer.concat([Buffer.from('Ed'), idBytes, raw]);
  const pubkey = Buffer.from(`untrusted comment: minisign public key\n${pub.toString('base64')}\n`).toString('base64');
  const signFile = (data, trusted) => {
    const sig = sign(null, createHash('blake2b512').update(data).digest(), privateKey);
    const global = sign(null, Buffer.concat([sig, Buffer.from(trusted)]), privateKey);
    const line = Buffer.concat([Buffer.from('ED'), idBytes, sig]).toString('base64');
    return Buffer.from(`untrusted comment: sig\n${line}\ntrusted comment: ${trusted}\n${global.toString('base64')}\n`).toString('base64');
  };
  return { pubkey, signFile };
}

test('verifies a signature produced by the tauri CLI', () => {
  assert.equal(parsePubkey(REAL.pubkey).keyId, '9D0048ED9393A4E7');
  assert.deepEqual(verifyAsset({ ...REAL, version: '0.1.42' }), { ok: true, keyId: '9D0048ED9393A4E7' });
});

test('rejects tampering, version mismatch and a foreign key', () => {
  assert.equal(verifyAsset({ ...REAL, data: Buffer.from('hellO'), version: '0.1.42' }).ok, false);
  assert.match(verifyAsset({ ...REAL, version: '0.1.43' }).reason, /bound to version 0\.1\.42/);
  const other = keypair(7n);
  assert.match(verifyAsset({ ...REAL, pubkey: other.pubkey, version: '0.1.42' }).reason, /signed by key 9D0048ED9393A4E7/);
});

test('rejects a forged trusted comment', () => {
  const k = keypair(1n);
  const data = Buffer.from('payload');
  const good = k.signFile(data, 'timestamp:1\tfile:x\tversion:1.2.3');
  assert.equal(verifyAsset({ pubkey: k.pubkey, signature: good, data, version: '1.2.3' }).ok, true);
  // Same file signature, trusted comment edited to another version: the global signature breaks.
  const forged = Buffer.from(Buffer.from(good, 'base64').toString('utf8').replace('version:1.2.3', 'version:9.9.9')).toString('base64');
  assert.match(verifyAsset({ pubkey: k.pubkey, signature: forged, data, version: '9.9.9' }).reason, /trusted comment/);
  const unbound = k.signFile(data, 'timestamp:1\tfile:x');
  assert.match(verifyAsset({ pubkey: k.pubkey, signature: unbound, data, version: '1.2.3' }).reason, /\(none\)/);
});

test('manifest shape matches the tauri updater format', () => {
  const m = buildManifest({
    version: '0.1.42',
    notes: '',
    pubDate: '2026-11-11T00:00:00Z',
    baseUrl: 'https://github.com/o/r/releases/download/v0.1.42',
    assets: [
      { platform: 'darwin-aarch64', name: 'VBookingIM_0.1.42_aarch64.app.tar.gz', signature: 'c2ln\n' },
      { platform: 'windows-x86_64', name: 'VBookingIM_0.1.42_x64-setup.exe', signature: 'c2ln' },
    ],
  });
  assert.equal(m.platforms['darwin-aarch64'].url, 'https://github.com/o/r/releases/download/v0.1.42/VBookingIM_0.1.42_aarch64.app.tar.gz');
  assert.equal(m.platforms['darwin-aarch64'].signature, 'c2ln');
  assert.deepEqual(Object.keys(m).sort(), ['notes', 'platforms', 'pub_date', 'version']);
  const ok = { version: '0.1.42', pubDate: 'x', baseUrl: 'https://h/p', assets: [{ platform: 'darwin-aarch64', name: 'a', signature: 's' }] };
  assert.throws(() => buildManifest({ ...ok, version: '0.1.42-beta' }), /x\.y\.z/);
  assert.throws(() => buildManifest({ ...ok, baseUrl: 'http://h/p' }), /https/);
  assert.throws(() => buildManifest({ ...ok, baseUrl: 'https://h/p/' }), /trailing/);
  assert.throws(() => buildManifest({ ...ok, assets: [{ ...ok.assets[0], platform: 'linux-x86_64' }] }), /unknown platform/);
  assert.throws(() => buildManifest({ ...ok, assets: [...ok.assets, ...ok.assets] }), /duplicate/);
  assert.throws(() => buildManifest({ ...ok, assets: [] }), /no assets/);
});

test('cli writes the manifest only when every asset verifies', () => {
  const dir = mkdtempSync(join(tmpdir(), 'vbk-manifest-'));
  const pub = join(dir, 'current.pub');
  writeFileSync(pub, REAL.pubkey);
  const asset = join(dir, 'a.bin');
  writeFileSync(asset, REAL.data);
  writeFileSync(`${asset}.sig`, REAL.signature);
  const out = join(dir, 'latest.json');
  const args = ['--version', '0.1.42', '--pubkey', pub, '--base-url', 'https://h/d/v0.1.42', '--pub-date', 'now', '--out', out];
  assert.equal(main([...args, `darwin-aarch64=${asset}`]), 0);
  assert.equal(JSON.parse(readFileSync(out, 'utf8')).platforms['darwin-aarch64'].url, 'https://h/d/v0.1.42/a.bin');
  // Platform names carry underscores (x86_64); the CLI must accept them.
  assert.equal(main([...args, `windows-x86_64=${asset}`]), 0);
  assert.ok(JSON.parse(readFileSync(out, 'utf8')).platforms['windows-x86_64']);

  const out2 = join(dir, 'latest2.json');
  writeFileSync(asset, 'tampered');
  assert.equal(main([...args.slice(0, -1), out2, `darwin-aarch64=${asset}`]), 1);
  assert.throws(() => readFileSync(out2));
  assert.equal(main(['--version', '0.1.42']), 2);
  writeFileSync(pub, '\n');
  assert.equal(main([...args, `darwin-aarch64=${asset}`]), 2);
});
