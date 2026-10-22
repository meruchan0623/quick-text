// IPC transport (FE-18). Commands and events go over the real Tauri IPC.
//
// Development only (`npm run dev`, Vitest): in a plain browser, or with VITE_MOCK=1, they go to the
// in-memory mock backend in ./mock instead. Every path to the mock sits behind
// `import.meta.env.DEV`, which `vite build` replaces with `false`, so a production bundle contains
// neither the mock nor its chunk, whatever VITE_MOCK says (`npm run check:bundle` verifies this).
// A production build opened outside the desktop app has no backend: every call rejects with
// BackendUnavailableError, shown as 「未连接到桌面后端」.
//
// Vitest swaps the transport with setTransport(), or drives the Tauri transport through mockIPC.
import { invoke as tauriInvoke } from '@tauri-apps/api/core';
import { listen as tauriListen } from '@tauri-apps/api/event';
import { UnsendableArgsError, unsendablePath } from './sendable';

export type Unlisten = () => void;
export type CommandArgs = Record<string, unknown>;

export interface Transport {
  readonly kind: 'tauri' | 'mock';
  /** Rejects with the backend's raw error value; see ./errors for parsing it. */
  invoke<T>(command: string, args?: CommandArgs): Promise<T>;
  // The payload type is the caller's claim about the backend, as with Tauri's own `listen<T>`.
  // eslint-disable-next-line @typescript-eslint/no-unnecessary-type-parameters
  listen<T>(event: string, handler: (payload: T) => void): Promise<Unlisten>;
}

export function createTauriTransport(): Transport {
  return {
    kind: 'tauri',
    invoke<T>(command: string, args?: CommandArgs) {
      return tauriInvoke<T>(command, args);
    },
    // eslint-disable-next-line @typescript-eslint/no-unnecessary-type-parameters -- see Transport
    listen<T>(event: string, handler: (payload: T) => void) {
      return tauriListen<T>(event, (e) => {
        handler(e.payload);
      });
    },
  };
}

/** Display text when no desktop backend is reachable. */
export const NOT_CONNECTED_TEXT = '未连接到桌面后端';

/** A production build running outside the Tauri webview: there is no backend to talk to. */
export class BackendUnavailableError extends Error {
  override readonly name = 'BackendUnavailableError';
  constructor() {
    super(NOT_CONNECTED_TEXT);
  }
}

/** By name rather than `instanceof`, so it also holds across module instances (tests). */
export const isBackendUnavailable = (value: unknown): boolean =>
  value instanceof Error && value.name === 'BackendUnavailableError';

// Captured once at load, before anything (e.g. @tauri-apps/api/mocks) can define the global.
const inTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
/** True in the development browser preview: data comes from the synthetic mock backend. */
export const usesMock = import.meta.env.DEV && (!inTauri || import.meta.env.VITE_MOCK === '1');

let current: Transport | null = null;
let pending: Promise<Transport> | null = null;

function transport(): Transport | Promise<Transport> {
  if (current) return current;
  // `import.meta.env.DEV` is repeated here, not only folded into `usesMock`, so the bundler sees a
  // literal `false` and drops this branch together with the dynamic import of the mock.
  if (import.meta.env.DEV && usesMock) {
    pending ??= import('./mock').then((m) => m.createPreviewTransport());
    return pending.then((t) => (current ??= t));
  }
  if (!inTauri) throw new BackendUnavailableError();
  return (current = createTauriTransport());
}

/** Replace the transport (tests). `null` restores environment detection. */
export function setTransport(next: Transport | null): void {
  current = next;
  pending = null;
}

/**
 * Every command goes through here. Arguments holding a number JSON cannot carry (NaN, ±Infinity,
 * or the editor's invalid-number marker) are refused before anything is sent; see ./sendable.
 */
export async function invoke<T>(command: string, args?: CommandArgs): Promise<T> {
  const path = args === undefined ? null : unsendablePath(args);
  if (path !== null) throw new UnsendableArgsError(command, path);
  return (await transport()).invoke<T>(command, args);
}

// eslint-disable-next-line @typescript-eslint/no-unnecessary-type-parameters -- see Transport
export async function listen<T>(event: string, handler: (payload: T) => void): Promise<Unlisten> {
  return (await transport()).listen<T>(event, handler);
}

/** Untyped `invoke` for the legacy 0.3 screens (replaces bridge.ts); new code uses `invoke<T>`. */
// eslint-disable-next-line @typescript-eslint/no-explicit-any -- legacy call sites rely on `any`
export function call<T = any>(command: string, args?: CommandArgs): Promise<T> {
  return invoke<T>(command, args);
}
