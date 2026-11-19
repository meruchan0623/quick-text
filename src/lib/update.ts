// In-app updates (src-tauri/src/commands/update.rs): the same check as the background auto-update,
// against both TSim update servers (production and test; the newest version wins, production on a
// tie). A found update is downloaded and verified against the key built into the app, then
// installed by restarting. Version numbers are SemVer 2.0.0 (scripts/version.mjs).
import { call, listen, type Unlisten } from '../ipc/transport';

export type { Unlisten };

/** The server that announced an update. */
export type UpdateSource = 'production' | 'test';

export interface UpdateInfo {
  version: string;
  /** Plain text, as published. */
  notes: string;
  /** RFC 3339. */
  pub_date: string | null;
  source: UpdateSource;
}

export interface UpdateCheck {
  current: string;
  /** The newer version found; null when the running one is the newest. */
  latest: UpdateInfo | null;
  update_available: boolean;
  /** `latest` is downloaded and verified: installUpdate restarts into it. */
  ready: boolean;
}

export interface UpdateProgress {
  version: string;
  downloaded: number;
  total: number | null;
}

export interface UpdateDownload {
  /** Downloaded and verified. */
  version: string;
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
export function checkUpdate(): Promise<UpdateCheck> {
  return call<UpdateCheck>('check_update');
}

/** Only the version the last check found; one download at a time (kind `busy`). */
export function downloadUpdate(version: string): Promise<UpdateDownload> {
  return call<UpdateDownload>('download_update', { version });
}

/** Installs the downloaded update and restarts the app; resolves only if nothing happened. */
export async function installUpdate(): Promise<void> {
  await call<null>('install_update');
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
