// IPC transport (FE-18). Inside Tauri commands and events go over the real IPC; in a plain browser
// (`npm run dev` preview) or with VITE_MOCK=1 they go to the in-memory mock backend in ./mock.
// Vitest swaps the transport with setTransport(), or drives the Tauri transport through mockIPC.
import { invoke as tauriInvoke } from '@tauri-apps/api/core';
import { listen as tauriListen } from '@tauri-apps/api/event';

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

// Captured once at load, before anything (e.g. @tauri-apps/api/mocks) can define the global.
const inTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
/** True in the browser preview: data comes from the synthetic mock backend. */
export const usesMock = !inTauri || import.meta.env.VITE_MOCK === '1';

let current: Transport | null = null;
let pending: Promise<Transport> | null = null;

function transport(): Transport | Promise<Transport> {
  if (current) return current;
  // The mock is loaded lazily so it stays out of the code path of the desktop build.
  pending ??= usesMock
    ? import('./mock').then((m) => m.createPreviewTransport())
    : Promise.resolve(createTauriTransport());
  return pending.then((t) => (current ??= t));
}

/** Replace the transport (tests). `null` restores environment detection. */
export function setTransport(next: Transport | null): void {
  current = next;
  pending = null;
}

export async function invoke<T>(command: string, args?: CommandArgs): Promise<T> {
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
