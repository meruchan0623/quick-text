// In-app updates (src-tauri/src/update.rs): the newest GitHub release of meruchan0623/quick-text,
// downloaded into the Downloads folder and checked against the release's SHA256SUMS file.
// Release tags are `v<SemVer 2.0.0>` (scripts/version.mjs); a tag with a `-` part is a prerelease.
import { call, listen, type Unlisten } from '../ipc/transport';

export type { Unlisten };

export interface UpdateAsset {
  name: string;
  size: number;
}

export interface UpdateRelease {
  /** SemVer, without the tag's `v`. */
  version: string;
  tag: string;
  prerelease: boolean;
  /** RFC 3339. */
  published_at: string | null;
  /** Markdown, as on the release page. */
  notes: string;
  html_url: string;
  /** This platform's package; null when the release has none for it. */
  asset: UpdateAsset | null;
}

export interface UpdateCheck {
  current: string;
  latest: UpdateRelease | null;
  update_available: boolean;
}

export interface UpdateProgress {
  tag: string;
  downloaded: number;
  total: number | null;
}

export interface UpdateDownload {
  path: string;
  sha256: string;
  /** False when the release lists no checksum for the package (the file is kept). */
  verified: boolean;
}

export const UPDATE_PROGRESS_EVENT = 'update-download-progress';

/** This build's version, as the backend reports it. */
export function appVersion(): Promise<string> {
  return call<string>('app_version');
}

/**
 * Rejects with a CommandError; update failures are `{ code: 'update', detail: { kind, message } }`
 * with a Chinese `message` (`errorText` shows it).
 */
export function checkUpdate(includePrerelease: boolean): Promise<UpdateCheck> {
  return call<UpdateCheck>('check_update', { includePrerelease });
}

/** One download at a time: a second one rejects with kind `busy`. */
export function downloadUpdate(tag: string): Promise<UpdateDownload> {
  return call<UpdateDownload>('download_update', { tag });
}

/** Only a `path` that downloadUpdate returned in this run. */
export async function revealUpdate(path: string): Promise<void> {
  await call<null>('reveal_update', { path });
}

export function onUpdateProgress(handler: (p: UpdateProgress) => void): Promise<Unlisten> {
  return listen<UpdateProgress>(UPDATE_PROGRESS_EVENT, handler);
}

// ---- SemVer 2.0.0 (https://semver.org) ----

export interface Semver {
  major: number;
  minor: number;
  patch: number;
  prerelease: (string | number)[];
  build: string[];
}

// The regular expression suggested by semver.org, assembled from its parts.
const NUMERIC = '0|[1-9]\\d*';
const PRE_ID = `(?:${NUMERIC}|\\d*[a-zA-Z-][0-9a-zA-Z-]*)`;
const BUILD_ID = '[0-9a-zA-Z-]+';
const SEMVER = new RegExp(
  `^(${NUMERIC})\\.(${NUMERIC})\\.(${NUMERIC})` +
    `(?:-(${PRE_ID}(?:\\.${PRE_ID})*))?` +
    `(?:\\+(${BUILD_ID}(?:\\.${BUILD_ID})*))?$`,
);

/** `1.2.3-rc.1+b` (a leading `v` is accepted, as in the release tags); null if not SemVer. */
export function parseSemver(v: string): Semver | null {
  const m = SEMVER.exec(v.startsWith('v') ? v.slice(1) : v);
  if (!m) return null;
  const numbers = [m[1], m[2], m[3]].map(Number);
  if (numbers.some((n) => !Number.isSafeInteger(n))) return null;
  const [major = 0, minor = 0, patch = 0] = numbers;
  return {
    major,
    minor,
    patch,
    prerelease:
      m[4] === undefined ? [] : m[4].split('.').map((x) => (/^\d+$/.test(x) ? Number(x) : x)),
    build: m[5] === undefined ? [] : m[5].split('.'),
  };
}

function compareIdentifiers(a: string | number, b: string | number): number {
  if (typeof a === 'number' && typeof b === 'number') return Math.sign(a - b);
  // Numeric identifiers have lower precedence than alphanumeric ones.
  if (typeof a === 'number') return -1;
  if (typeof b === 'number') return 1;
  return a === b ? 0 : a < b ? -1 : 1;
}

/**
 * SemVer 2.0.0 §11 precedence: -1, 0 or 1; build metadata is ignored. A version that does not
 * parse sorts below every valid one (two invalid ones are equal).
 */
export function compareSemver(a: string, b: string): number {
  const x = parseSemver(a);
  const y = parseSemver(b);
  if (!x || !y) return x ? 1 : y ? -1 : 0;
  for (const key of ['major', 'minor', 'patch'] as const) {
    if (x[key] !== y[key]) return x[key] < y[key] ? -1 : 1;
  }
  const xp = x.prerelease;
  const yp = y.prerelease;
  if (xp.length === 0 || yp.length === 0) return xp.length === yp.length ? 0 : xp.length ? -1 : 1;
  for (let i = 0; i < Math.max(xp.length, yp.length); i += 1) {
    const xi = xp[i];
    const yi = yp[i];
    if (xi === undefined) return -1;
    if (yi === undefined) return 1;
    const c = compareIdentifiers(xi, yi);
    if (c !== 0) return c;
  }
  return 0;
}

/** Whether `v` is a valid SemVer version with a prerelease part. */
export function isPrerelease(v: string): boolean {
  return (parseSemver(v)?.prerelease.length ?? 0) > 0;
}
