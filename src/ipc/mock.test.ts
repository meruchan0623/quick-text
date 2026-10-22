import { emit } from '@tauri-apps/api/event';
import { clearMocks, mockIPC } from '@tauri-apps/api/mocks';
import { afterEach, describe, expect, it } from 'vitest';
import { isMissingCommand, parseCommandError } from './errors';
import { RuntimeSync } from './events';
import {
  createMockTransport,
  isMockRule,
  MockBackend,
  mockCapabilities,
  mockOptionsFromQuery,
  syntheticRules,
  validate,
  type MockConfig,
} from './mock';
import { call, createTauriTransport, invoke, setTransport } from './transport';
import { EVENT_NAMES, type HealthSnapshot, type HistoryEntry, type RuntimeSnapshot } from './types';

afterEach(() => {
  clearMocks();
  setTransport(null);
});

interface LegacyState {
  draft: MockConfig;
  revision: number;
  dirty: boolean;
  paused: boolean;
  engine_status: string;
  health?: unknown;
  safe_mode?: unknown;
}

/** Route Tauri's IPC to a mock backend, events included, and use the real Tauri transport. */
function throughTauri(backend: MockBackend) {
  mockIPC((command, args) => backend.handle(command, args as Record<string, unknown>), {
    shouldMockEvents: true,
  });
  for (const name of EVENT_NAMES) backend.on(name, (payload) => void emit(name, payload));
  const transport = createTauriTransport();
  setTransport(transport);
  return transport;
}

describe('mock backend through mockIPC and the Tauri transport', () => {
  it('round-trips draft, apply and rollback', async () => {
    throughTauri(new MockBackend({ rules: 3 }));
    const state = await invoke<LegacyState>('load_state');
    expect(state.draft.rules).toHaveLength(3);
    expect(state.health).toBeDefined();
    const config = { ...state.draft, rules: state.draft.rules.slice(1) };
    const saved = await invoke<LegacyState>('save_draft', { config });
    expect(saved.dirty).toBe(true);
    const applied = await invoke<LegacyState>('apply_settings');
    expect(applied.revision).toBe(2);
    expect(applied.dirty).toBe(false);
    const rolledBack = await invoke<LegacyState>('rollback');
    expect(rolledBack.draft.rules).toHaveLength(3);
  });

  it('rejects with a CommandError object that parses back', async () => {
    throughTauri(new MockBackend({ rules: 1 }));
    const state = await invoke<LegacyState>('load_state');
    const [rule] = state.draft.rules;
    if (!rule) throw new Error('missing rule');
    const config = { ...state.draft, rules: [{ ...rule, match: { kind: 'literal', value: '' } }] };
    const error: unknown = await invoke('save_draft', { config }).catch((e: unknown) => e);
    const parsed = parseCommandError(error);
    expect(parsed.code).toBe('validation');
    if (parsed.code === 'validation')
      expect(parsed.detail.diagnostics[0]?.path).toBe('rules[0].match.value');
  });

  it('delivers pushed events through Tauri listen with the payload unwrapped', async () => {
    const backend = new MockBackend({ rules: 0 });
    const transport = throughTauri(backend);
    const seen: unknown[] = [];
    const unlisten = await transport.listen('engine-state', (payload) => seen.push(payload));
    backend.setPaused('manual', true);
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(seen).toEqual([
      {
        seq: backend.seq,
        listening: false,
        paused_reasons: ['manual'],
        busy: false,
        current_process: 'notepad.exe',
      },
    ]);
    unlisten();
  });

  it('keeps a RuntimeSync aligned over the Tauri transport', async () => {
    const backend = new MockBackend({ rules: 0 });
    const transport = throughTauri(backend);
    const sync = new RuntimeSync(transport);
    await sync.start();
    backend.recordExecution({ outcome: 'failed', reason: 'focus_changed' });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(sync.getView().history[0]?.entry.reason).toBe('focus_changed');
    expect(sync.getView().seq).toBe(backend.seq);
    sync.dispose();
  });

  it('answers like a 0.3 backend in legacy mode', async () => {
    throughTauri(new MockBackend({ legacy: true, rules: 1 }));
    const error: unknown = await invoke('get_runtime_snapshot').catch((e: unknown) => e);
    expect(isMissingCommand(error)).toBe(true);
    const state = await call<LegacyState>('load_state');
    expect(state.health).toBeUndefined();
    expect(state.safe_mode).toBeUndefined();
  });
});

describe('mock backend commands', () => {
  const use = (backend: MockBackend) => setTransport(createMockTransport(backend));

  it('implements the 0.4 commands', async () => {
    const backend = new MockBackend({ rules: 0 });
    use(backend);
    backend.recordExecution({ outcome: 'succeeded' });
    backend.recordExecution({ outcome: 'dropped_busy', reason: 'dropped_busy' });
    const snapshot = await invoke<RuntimeSnapshot>('get_runtime_snapshot');
    expect(snapshot).toMatchObject({
      seq: backend.seq,
      history_seq: 2,
      revision: 1,
      safe_mode: null,
    });
    expect(await invoke<HistoryEntry[]>('list_history', { since_seq: 1 })).toHaveLength(1);
    expect(await invoke('explain_last_miss')).toMatchObject({ reason: 'dropped_busy' });
    expect(await invoke('export_diagnostics', { redaction: 'summary' })).toMatch(/\.zip$/);
    expect(await invoke('open_log_dir')).toBeNull();
    expect(await invoke('startup_problems')).toEqual([]);
    expect(await invoke('get_capabilities')).toContainEqual(
      expect.objectContaining({ id: 'keys', status: 'available', available: true }),
    );
    expect(await invoke('self_test')).toMatchObject({ passed: true });
    expect(await invoke('set_log_level', { level: 'debug', ttl_hours: 1 })).toEqual({
      level: 'debug',
      until_ms: expect.any(Number) as unknown,
    });
    expect(await invoke('set_log_level', { level: 'debug', ttl_hours: null })).toEqual({
      level: 'info',
      until_ms: null,
    });
    await invoke('clear_history');
    expect(await invoke('list_history', { since_seq: null })).toEqual([]);
  });

  it('answers repair_engine with the current snapshot and pushes recovering (contract §5)', async () => {
    const backend = new MockBackend({ rules: 0, health: 'failed' });
    use(backend);
    const pushed: string[] = [];
    backend.on('engine-health', (payload) => pushed.push((payload as HealthSnapshot).state));
    const answer = await invoke<HealthSnapshot>('repair_engine');
    expect(answer.state).toBe('failed');
    expect(answer).toEqual(backend.health);
    expect(pushed).toEqual([]);
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(pushed).toEqual(['recovering']);
    await new Promise((resolve) => setTimeout(resolve, 700));
    expect(pushed).toEqual(['recovering', 'healthy']);
  });

  it('builds get_capabilities like crates/platform/src/capabilities.rs', () => {
    const windows = mockCapabilities('windows');
    expect(windows.map((c) => c.id)).toEqual([
      'keyboard_hook',
      'keys',
      'paste',
      'html',
      'file_clipboard',
      'image_clipboard',
      'mouse',
      'recording',
      'foreground',
      'open',
      'ime_committed_text',
      'quicker_private_action',
    ]);
    expect(windows.find((c) => c.id === 'quicker_private_action')).toEqual({
      id: 'quicker_private_action',
      status: 'unavailable',
      reason: 'not_implemented',
      available: false,
    });
    expect(windows.filter((c) => c.status === 'available')).toHaveLength(11);
    for (const c of windows) expect(c.available).toBe(c.status !== 'unavailable');

    const macos = mockCapabilities('macos');
    expect(macos.filter((c) => c.reason === 'not_implemented').map((c) => c.id)).toEqual([
      'ime_committed_text',
      'wheel_delta',
      'window_ime_actions',
      'quicker_private_action',
    ]);
    for (const c of macos.filter((c) => c.reason !== 'not_implemented'))
      expect(c).toMatchObject({ status: 'experimental', reason: 'experimental_platform' });

    expect(mockCapabilities('linux').every((c) => c.reason === 'unsupported')).toBe(true);

    // Health lowers what depends on a component; the worst one wins.
    const lowered = mockCapabilities('windows', [
      { component: 'clipboard', state: 'degraded', reason: 'clipboard_changed', since_ms: 0 },
      { component: 'raw_input', state: 'failed', reason: null, since_ms: 0 },
    ]);
    const status = (id: string) => lowered.find((c) => c.id === id);
    expect(status('paste')).toMatchObject({ status: 'degraded', reason: 'clipboard_changed' });
    expect(status('keyboard_hook')).toMatchObject({
      status: 'unavailable',
      reason: 'raw_input_missing',
    });
    expect(status('keys')).toMatchObject({ status: 'available', reason: null });
  });

  it('reports the 0.4 capability table in load_state, the 0.3 one in legacy mode', async () => {
    use(new MockBackend({ rules: 0, platform: 'macos' }));
    const state = await invoke<{ capabilities: { id: string; status?: string }[] }>('load_state');
    expect(state.capabilities).toEqual(mockCapabilities('macos'));
    use(new MockBackend({ rules: 0, legacy: true }));
    const legacy = await invoke<{ capabilities: { status?: string }[] }>('load_state');
    expect(legacy.capabilities.every((c) => c.status === undefined)).toBe(true);
  });

  it('toggles only the manual pause reason', async () => {
    const backend = new MockBackend({ rules: 0, safeMode: 'crash_loop' });
    use(backend);
    await invoke('set_paused', { paused: true });
    expect(backend.engine.paused_reasons).toEqual(['safe_mode', 'manual']);
    const state = await invoke<LegacyState>('set_paused', { paused: false });
    expect(backend.engine.paused_reasons).toEqual(['safe_mode']);
    expect(state.paused).toBe(true);
  });

  it('refuses writes in safe mode', async () => {
    const backend = new MockBackend({ rules: 0, safeMode: 'locked' });
    use(backend);
    const error: unknown = await invoke('apply_settings').catch((e: unknown) => e);
    expect(error).toEqual({ code: 'safe_mode', detail: { reason: 'locked' } });
  });

  it('returns null when a dialog is cancelled', async () => {
    use(new MockBackend({ rules: 0, dialog: 'cancel' }));
    expect(await invoke('export_config')).toBeNull();
    expect(await invoke('choose_file', { kind: 'import' })).toBeNull();
    expect(await invoke('export_diagnostics', { redaction: 'full' })).toBeNull();
  });

  it('rejects unknown commands like Tauri does', async () => {
    use(new MockBackend({ rules: 0 }));
    const error: unknown = await invoke('no_such_command').catch((e: unknown) => e);
    expect(error).toBe('Command no_such_command not found');
  });
});

describe('synthetic data', () => {
  it('generates the FE-18 sizes deterministically', () => {
    expect(syntheticRules(0)).toEqual([]);
    expect(syntheticRules(200)).toHaveLength(200);
    const large = syntheticRules(10_000);
    expect(new Set(large.map((r) => r.id)).size).toBe(10_000);
    expect(syntheticRules(7)).toEqual(syntheticRules(7));
    expect(syntheticRules(7).some((r) => r.group === '')).toBe(true);
  });

  it('reads preview switches from the query string', () => {
    expect(mockOptionsFromQuery('')).toEqual({});
    expect(mockOptionsFromQuery('?mock=empty')).toEqual({ rules: 0 });
    expect(mockOptionsFromQuery('?mock=10000&mock-legacy=1&mock-health=failed')).toEqual({
      rules: 10_000,
      legacy: true,
      health: 'failed',
    });
    expect(
      mockOptionsFromQuery('?mock-safe-mode=crash_loop&mock-platform=macos&mock-dialog=cancel'),
    ).toEqual({ safeMode: 'crash_loop', platform: 'macos', dialog: 'cancel' });
    expect(mockOptionsFromQuery('?mock=99999999&mock-health=bogus')).toEqual({ rules: 10_000 });
  });
});

describe('B-43 sample', () => {
  it('mixes unreadable entries and unknown keys into the config', () => {
    const backend = new MockBackend({ rules: 2, unreadable: true });
    const { rules } = backend.draft;
    expect(rules.map(isMockRule)).toEqual([true, false, true, false, false]);
    expect(rules[1]).toEqual({ id: 'mock-unreadable-1', match: 42, future_kind: 'from 0.6' });
    expect(backend.draft.future_setting).toEqual({ sample: true });
    expect(backend.draft.globals.future_global).toBe('kept');
    expect(rules[0]).toHaveProperty('future_rule_field');
    expect(mockOptionsFromQuery('?mock-unreadable=1')).toEqual({ unreadable: true });
  });

  it('reports unreadable entries as warnings that do not block saving', () => {
    const backend = new MockBackend({ rules: 2, unreadable: true });
    expect(validate(backend.draft).map((d) => [d.code, d.path, d.severity])).toEqual([
      ['rule_unreadable', 'rules[1]', 'warning'],
      ['rule_unreadable', 'rules[3]', 'warning'],
      ['rule_unreadable', 'rules[4]', 'warning'],
    ]);
    return expect(backend.handle('save_draft', { config: backend.draft })).resolves.toBeDefined();
  });
});
