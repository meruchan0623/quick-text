// Transport selection: the mock backend only ever answers in development, a production build always
// talks to Tauri, and outside the desktop app it says so instead of falling back to sample data.
// `inTauri` is captured when the module loads, so every case imports a fresh copy after setting up
// the environment.
import { clearMocks, mockIPC } from '@tauri-apps/api/mocks';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { invalidNumber } from '../lib/numberFields';
import { errorText } from './errors';

type TransportModule = typeof import('./transport');

async function freshTransport(env: { dev: boolean; mock?: string }): Promise<TransportModule> {
  vi.stubEnv('DEV', env.dev);
  if (env.mock !== undefined) vi.stubEnv('VITE_MOCK', env.mock);
  vi.resetModules();
  return import('./transport');
}

/** Pretend to run inside the Tauri webview; `load_state` answers with a marker. */
function insideTauri(): void {
  mockIPC((command) => (command === 'load_state' ? { from: 'tauri' } : null));
}

const globals = globalThis as { quickerMock?: unknown };
const tauriWindow = window as { __TAURI_INTERNALS__?: unknown };

afterEach(() => {
  vi.unstubAllEnvs();
  clearMocks();
  delete tauriWindow.__TAURI_INTERNALS__;
  delete globals.quickerMock;
});

describe('production build', () => {
  it('outside Tauri rejects every call with 「未连接到桌面后端」', async () => {
    const transport = await freshTransport({ dev: false });
    expect(transport.usesMock).toBe(false);

    const error: unknown = await transport.invoke('load_state').catch((e: unknown) => e);
    expect(error).toBeInstanceOf(transport.BackendUnavailableError);
    expect(transport.isBackendUnavailable(error)).toBe(true);
    expect((error as Error).message).toBe(transport.NOT_CONNECTED_TEXT);
    expect(errorText(error)).toBe('未连接到桌面后端');

    await expect(transport.call('load_state')).rejects.toThrow('未连接到桌面后端');
    await expect(transport.listen('engine-state', () => undefined)).rejects.toThrow(
      '未连接到桌面后端',
    );
    expect(globals.quickerMock).toBeUndefined();
  });

  it('ignores VITE_MOCK=1: still no mock outside Tauri', async () => {
    const transport = await freshTransport({ dev: false, mock: '1' });
    expect(transport.usesMock).toBe(false);
    await expect(transport.invoke('load_state')).rejects.toThrow('未连接到桌面后端');
    expect(globals.quickerMock).toBeUndefined();
  });

  it('inside Tauri uses the real IPC, even with VITE_MOCK=1', async () => {
    insideTauri();
    const transport = await freshTransport({ dev: false, mock: '1' });
    expect(transport.usesMock).toBe(false);
    await expect(transport.invoke('load_state')).resolves.toEqual({ from: 'tauri' });
  });
});

describe('the last check before sending', () => {
  it('sends nothing whose arguments hold NaN, ±Infinity or an invalid-number marker', async () => {
    const sent: string[] = [];
    mockIPC((command) => {
      sent.push(command);
      return null;
    });
    const transport = await freshTransport({ dev: false });
    const marker = invalidNumber('3600001');
    const refused: [string, Record<string, unknown>, string][] = [
      ['execute_action', { action: { type: 'reusable', parameters: { n: marker } } }, 'action'],
      ['test_rule', { rule: { action: { plan: { steps: [{ ms: NaN }] } } } }, 'rule'],
      ['save_draft', { config: { rules: [{ speed: Infinity }] } }, 'config'],
    ];
    for (const [command, args, at] of refused) {
      const error: unknown = await transport.invoke(command, args).catch((e: unknown) => e);
      expect(error).toBeInstanceOf(Error);
      expect((error as Error).name).toBe('UnsendableArgsError');
      expect(errorText(error)).toContain(`（${at}.`);
      expect(errorText(error)).toContain('已停止发送；已填写的内容会保留。');
      await expect(transport.call(command, args)).rejects.toThrow('已停止发送');
    }
    expect(sent).toEqual([]);

    await transport.invoke('execute_action', {
      action: { type: 'reusable', parameters: { n: 1 } },
    });
    expect(sent).toEqual(['execute_action']);
  });

  it('sends a config whose kept fields hold arrays of any width (Codex round 6)', async () => {
    const received: unknown[] = [];
    mockIPC((command, payload) => {
      received.push([command, payload]);
      return null;
    });
    const transport = await freshTransport({ dev: false });
    const wide = (): number[] => new Array<number>(200_000).fill(0);
    const config = {
      version: 1,
      future_setting: wide(),
      rules: [
        { id: 'a', match: { kind: 'literal', value: ';a' }, future_rule_field: wide() },
        { id: 'raw', match: 42, future: wide() },
      ],
    };
    await expect(transport.invoke('save_draft', { config })).resolves.toBe(null);
    expect(received).toEqual([['save_draft', { config }]]);
  });
});

describe('development', () => {
  it('outside Tauri the preview mock answers', async () => {
    const transport = await freshTransport({ dev: true });
    expect(transport.usesMock).toBe(true);
    const state = await transport.invoke<{ draft: { rules: unknown[] } }>('load_state');
    expect(state.draft.rules.length).toBeGreaterThan(0);
    expect(globals.quickerMock).toBeDefined();
  });

  it('inside Tauri uses the real IPC unless VITE_MOCK=1', async () => {
    insideTauri();
    const real = await freshTransport({ dev: true });
    expect(real.usesMock).toBe(false);
    await expect(real.invoke('load_state')).resolves.toEqual({ from: 'tauri' });

    const mocked = await freshTransport({ dev: true, mock: '1' });
    expect(mocked.usesMock).toBe(true);
    await expect(mocked.invoke('load_state')).resolves.not.toEqual({ from: 'tauri' });
  });
});
