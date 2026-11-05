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
import {
  CLOUD_STATE_EVENT,
  EVENT_NAMES,
  type CloudState,
  type HealthSnapshot,
  type HistoryEntry,
  type RuntimeSnapshot,
} from './types';

afterEach(() => {
  clearMocks();
  setTransport(null);
});

interface LegacyState {
  active: MockConfig;
  revision: number;
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
  it('publishes once and round-trips rollback', async () => {
    throughTauri(new MockBackend({ rules: 3 }));
    const state = await invoke<LegacyState>('load_state');
    expect(state.active.rules).toHaveLength(3);
    expect(state.health).toBeDefined();
    const config = { ...state.active, rules: state.active.rules.slice(1) };
    const saved = await invoke<LegacyState>('save_config', { config });
    expect(saved.revision).toBe(2);
    expect(saved.active.rules).toHaveLength(2);
    expect(saved).not.toHaveProperty('draft');
    expect(saved).not.toHaveProperty('dirty');
    const rolledBack = await invoke<LegacyState>('rollback');
    expect(rolledBack.active.rules).toHaveLength(3);
  });

  it('rejects with a CommandError object that parses back', async () => {
    throughTauri(new MockBackend({ rules: 1 }));
    const state = await invoke<LegacyState>('load_state');
    const [rule] = state.active.rules;
    if (!rule) throw new Error('missing rule');
    const config = { ...state.active, rules: [{ ...rule, match: { kind: 'literal', value: '' } }] };
    const error: unknown = await invoke('save_config', { config }).catch((e: unknown) => e);
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
      'image',
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
    expect(windows.filter((c) => c.status === 'available')).toHaveLength(12);
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

  it('keeps auto_listen_on_start with set_auto_listen (S-09), allowed in safe mode, absent in 0.3', async () => {
    const backend = new MockBackend({ rules: 0 });
    use(backend);
    expect(
      (await invoke<{ auto_listen_on_start?: unknown }>('load_state')).auto_listen_on_start,
    ).toBe(false);
    expect(await invoke('set_auto_listen', { enabled: true })).toBe(true);
    expect(
      (await invoke<{ auto_listen_on_start?: unknown }>('load_state')).auto_listen_on_start,
    ).toBe(true);
    expect(await invoke('set_auto_listen', { enabled: false })).toBe(false);
    expect(backend.autoListen).toBe(false);

    // Safe mode allows it, unlike set_autostart (src-tauri commands/settings.rs, ipc-contract §5).
    const safe = new MockBackend({ rules: 0, safeMode: 'locked' });
    use(safe);
    expect(await invoke('set_auto_listen', { enabled: true })).toBe(true);
    expect(safe.autoListen).toBe(true);
    const refused: unknown = await invoke('set_autostart', { enabled: true }).catch(
      (e: unknown) => e,
    );
    expect(refused).toEqual({ code: 'safe_mode', detail: { reason: 'locked' } });
    expect(safe.autostart).toBe(false);

    use(new MockBackend({ rules: 0, legacy: true }));
    const missing: unknown = await invoke('set_auto_listen', { enabled: true }).catch(
      (e: unknown) => e,
    );
    expect(isMissingCommand(missing)).toBe(true);
    expect(
      (await invoke<{ auto_listen_on_start?: unknown }>('load_state')).auto_listen_on_start,
    ).toBeUndefined();
  });

  it('toggles only the manual pause reason', async () => {
    const backend = new MockBackend({ rules: 0 });
    use(backend);
    await invoke('start_recording');
    await invoke('set_paused', { paused: true });
    expect(backend.engine.paused_reasons).toEqual(['recording', 'manual']);
    // 0.4 (store set_manual_pause): recording no longer refuses a resume; its own reason keeps the
    // engine paused until it stops.
    const state = await invoke<LegacyState>('set_paused', { paused: false });
    expect(backend.engine.paused_reasons).toEqual(['recording']);
    expect(state.paused).toBe(true);
  });

  it('refuses a resume in safe mode or while the health does not accept it, never a pause', async () => {
    const safe = new MockBackend({ rules: 0, safeMode: 'crash_loop' });
    use(safe);
    await invoke('set_paused', { paused: true });
    expect(safe.engine.paused_reasons).toEqual(['safe_mode', 'manual']);
    const refused: unknown = await invoke('set_paused', { paused: false }).catch((e: unknown) => e);
    expect(refused).toEqual({ code: 'safe_mode', detail: { reason: 'crash_loop' } });
    expect(safe.engine.paused_reasons).toEqual(['safe_mode', 'manual']);

    const failed = new MockBackend({ rules: 0, health: 'failed' });
    use(failed);
    await invoke('set_paused', { paused: true });
    const unavailable: unknown = await invoke('set_paused', { paused: false }).catch(
      (e: unknown) => e,
    );
    expect(unavailable).toEqual({ code: 'engine_unavailable', detail: { health: 'failed' } });
    expect(failed.engine.paused_reasons).toContain('manual');
  });

  it('refuses a resume while recording in legacy mode, as 0.3 did', async () => {
    const backend = new MockBackend({ rules: 0, legacy: true });
    use(backend);
    await invoke('start_recording');
    const refused: unknown = await invoke('set_paused', { paused: false }).catch((e: unknown) => e);
    expect(refused).toBe('Stop recording before resuming automatic matching');
  });

  it('refuses writes in safe mode', async () => {
    const backend = new MockBackend({ rules: 0, safeMode: 'locked' });
    use(backend);
    const error: unknown = await invoke('save_config', { config: backend.active }).catch(
      (e: unknown) => e,
    );
    expect(error).toEqual({ code: 'safe_mode', detail: { reason: 'locked' } });
  });

  it('serves a placeholder in Locked and installs the store on disk when the retry succeeds', async () => {
    const backend = new MockBackend({ rules: 2, safeMode: 'locked' });
    use(backend);
    // src-tauri store/fallback.rs `read_only`: an empty revision-0 config, not the store.
    const locked = await invoke<LegacyState & { active: MockConfig }>('load_state');
    expect(locked.revision).toBe(0);
    expect(locked.active.rules).toEqual([]);
    expect(locked.active.rules).toEqual([]);
    // A store whose active revision is 0 too (rules saved, never applied): the retry's store-state
    // says the same revision, with safe mode null (store/load.rs `install`, review I6).
    const disk = backend.disk;
    if (disk === null) throw new Error('Locked keeps the store on disk');
    backend.disk = {
      active: { ...disk.active, revision: 0 },
    };
    const pushed: unknown[] = [];
    backend.on('store-state', (payload) => pushed.push(payload));
    backend.endSafeMode();
    expect(pushed).toEqual([{ seq: backend.seq, revision: 0, safe_mode: null }]);
    expect(backend.disk).toBeNull();
    const recovered = await invoke<LegacyState & { active: MockConfig }>('load_state');
    expect(recovered.revision).toBe(0);
    expect(recovered.active.rules).toHaveLength(2);
    expect(recovered.safe_mode).toBeNull();
  });

  it('pushes store-state when the revision or safe mode changes, and only then', async () => {
    const backend = new MockBackend({ rules: 0 });
    use(backend);
    const pushed: unknown[] = [];
    backend.on('store-state', (payload) => pushed.push(payload));
    const { active } = await invoke<LegacyState>('load_state');
    await invoke('save_config', { config: { ...active, enabled: false } });
    const applied = backend.seq;
    await invoke('rollback');
    backend.publishExternally();
    expect(pushed).toEqual([
      { seq: applied, revision: 2, safe_mode: null },
      { seq: applied + 1, revision: 3, safe_mode: null },
      { seq: applied + 2, revision: 4, safe_mode: null },
    ]);

    // In crash_loop a refused write pushes nothing; committing and applying an import ends it.
    const crashed = new MockBackend({ rules: 0, safeMode: 'crash_loop' });
    use(crashed);
    const states: unknown[] = [];
    crashed.on('store-state', (payload) => states.push(payload));
    expect(await invoke('rollback').catch((e: unknown) => e)).toMatchObject({ code: 'safe_mode' });
    const preview = await invoke<{ config: MockConfig; token: string }>('import_config', {
      path: 'mock://import.json',
      strategy: 'replace',
    });
    await invoke('save_config', { config: preview.config, importToken: preview.token });
    expect(states).toEqual([{ seq: crashed.seq, revision: 2, safe_mode: null }]);
    expect(crashed.problems).toEqual([]);
    expect(crashed.engine.paused_reasons).toEqual(['manual']);

    // The backend's Locked retry succeeding: pushed once, not again for the same values.
    const locked = new MockBackend({ rules: 0, safeMode: 'locked' });
    const retried: unknown[] = [];
    locked.on('store-state', (payload) => retried.push(payload));
    locked.endSafeMode(5);
    locked.endSafeMode();
    expect(retried).toEqual([{ seq: locked.seq, revision: 5, safe_mode: null }]);
  });

  it('returns null when a dialog is cancelled', async () => {
    use(new MockBackend({ rules: 0, dialog: 'cancel' }));
    expect(await invoke('export_config')).toBeNull();
    expect(await invoke('export_excel_template')).toBeNull();
    expect(await invoke('choose_file', { kind: 'import' })).toBeNull();
    expect(await invoke('export_diagnostics', { redaction: 'full' })).toBeNull();
  });

  it('exports Excel templates only where configuration exports are safe', async () => {
    for (const safeMode of [
      'locked',
      'newer_schema',
      'no_valid_config',
      'crash_loop',
      null,
    ] as const) {
      use(new MockBackend({ rules: 0, safeMode }));
      if (safeMode === null || safeMode === 'crash_loop')
        expect(await invoke('export_excel_template')).toBe('mock://quicktext-template.xlsx');
      else
        await expect(invoke('export_excel_template')).rejects.toMatchObject({ code: 'safe_mode' });
    }
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
    const { rules } = backend.active;
    expect(rules.map(isMockRule)).toEqual([true, false, true, false, false]);
    expect(rules[1]).toEqual({ id: 'mock-unreadable-1', match: 42, future_kind: 'from 0.6' });
    expect(backend.active.future_setting).toEqual({ sample: true });
    expect(backend.active.globals.future_global).toBe('kept');
    expect(rules[0]).toHaveProperty('future_rule_field');
    expect(mockOptionsFromQuery('?mock-unreadable=1')).toEqual({ unreadable: true });
  });

  it('reports unreadable entries as warnings that do not block saving', () => {
    const backend = new MockBackend({ rules: 2, unreadable: true });
    expect(validate(backend.active).map((d) => [d.code, d.path, d.severity])).toEqual([
      ['rule_unreadable', 'rules[1]', 'warning'],
      ['rule_unreadable', 'rules[3]', 'warning'],
      ['rule_unreadable', 'rules[4]', 'warning'],
    ]);
    return expect(backend.handle('save_config', { config: backend.active })).resolves.toBeDefined();
  });
});

describe('atomic active configuration contract', () => {
  it('refuses stale saves and leaves active and rollback history intact', async () => {
    const backend = new MockBackend({ rules: 2 });
    const stale = structuredClone(backend.active);
    backend.publishExternally((c) => {
      c.enabled = false;
    });
    const before = structuredClone(backend.active);
    const previous = structuredClone(backend.previous);
    await expect(backend.handle('save_config', { config: stale })).rejects.toEqual({
      code: 'revision_conflict',
      detail: { current: 2 },
    });
    expect(backend.active).toEqual(before);
    expect(backend.previous).toEqual(previous);
  });

  it('binds import confirmation to the latest preview token and exact configuration', async () => {
    const backend = new MockBackend({ rules: 2 });
    const a = (await backend.handle('import_config', { strategy: 'replace' })) as {
      config: MockConfig;
      token: string;
    };
    const b = (await backend.handle('import_config', { strategy: 'merge' })) as {
      config: MockConfig;
      token: string;
    };
    await expect(
      backend.handle('save_config', { config: a.config, importToken: a.token }),
    ).rejects.toEqual({ code: 'token_expired' });
    await expect(
      backend.handle('save_config', {
        config: { ...b.config, enabled: false },
        importToken: b.token,
      }),
    ).rejects.toEqual({ code: 'token_expired' });
    expect(backend.active.revision).toBe(1);
    await backend.handle('save_config', { config: b.config, importToken: b.token });
    expect(backend.active.revision).toBe(2);
    expect(backend.active.rules).toHaveLength(4);
    await expect(
      backend.handle('save_config', { config: b.config, importToken: b.token }),
    ).rejects.toEqual({ code: 'token_expired' });
  });

  it.each(['save_draft', 'apply_settings', 'discard_draft'])(
    'does not expose removed command %s',
    async (command) => {
      const backend = new MockBackend({ rules: 1 });
      await expect(backend.handle(command)).rejects.toMatch('not found');
      expect(backend.active.revision).toBe(1);
    },
  );
});

describe('team phrases (docs/cloud-phrases.md §5)', () => {
  const use = (backend: MockBackend) => setTransport(createMockTransport(backend));
  const statuses = (state: CloudState) => state.rules.map((r) => [r.id, r.status]);

  it('serves the sample table with one phrase per status, merged against 我的指令', async () => {
    use(new MockBackend({ rules: 3, cloud: 'sample' }));
    const state = await invoke<CloudState>('get_cloud_state');
    expect(state.settings).toEqual({
      enabled: true,
      base_url: 'https://cs.example.com',
      has_key: true,
      auto_sync_minutes: 30,
    });
    expect(state.settings).not.toHaveProperty('key');
    expect(state.status).toMatchObject({ revision: 12, unreadable_count: 1, last_error: null });
    expect(statuses(state)).toEqual([
      ['mock-cloud-1', 'active'],
      ['mock-cloud-2', 'active'],
      ['mock-cloud-3', 'active'],
      ['mock-cloud-4', 'disabled_remote'],
      ['mock-cloud-5', 'disabled_local'],
      ['mock-cloud-6', 'active'],
      ['mock-cloud-7', 'invalid'],
      ['mock-cloud-8', 'active'],
    ]);
    // The phrase with a personal rule's abbreviation wins and lists the rule it replaces.
    expect(state.rules[5]?.overrides).toEqual(['mock-rule-2']);
    expect(state.rules.filter((r) => r.overrides !== undefined).length).toBe(1);
    expect(state.rules[6]?.problem).toMatch(/正则表达式/);
    expect(state.rules.map((r) => r.disabled_local)).toEqual([
      false,
      false,
      false,
      false,
      true,
      false,
      false,
      false,
    ]);
    for (const entry of state.rules) expect(isMockRule(entry.rule)).toBe(true);
  });

  it('starts unsubscribed with nothing cached by default', async () => {
    use(new MockBackend({ rules: 0 }));
    const state = await invoke<CloudState>('get_cloud_state');
    expect(state.settings).toMatchObject({ enabled: false, has_key: false });
    expect(state.status).toMatchObject({ revision: null, last_success_at: null });
    expect(state.rules).toEqual([]);
    expect((await invoke<CloudState>('sync_cloud_now')).status.last_error?.code).toBe(
      'not_configured',
    );
  });

  it('keeps, replaces and clears the key as §5 says, and refuses invalid settings', async () => {
    const backend = new MockBackend({ rules: 0, cloud: 'sample' });
    use(backend);
    const settings = { enabled: false, base_url: ' https://a.example ', auto_sync_minutes: 60 };
    expect((await invoke<CloudState>('set_cloud_settings', { settings })).settings).toEqual({
      enabled: false,
      base_url: 'https://a.example',
      has_key: true,
      auto_sync_minutes: 60,
    });
    await invoke('set_cloud_settings', { settings: { ...settings, key: null } });
    expect(backend.cloudPrefs.key).toBe('mock-sync-key');
    await invoke('set_cloud_settings', { settings: { ...settings, key: 'new' } });
    expect(backend.cloudPrefs.key).toBe('new');
    const cleared = await invoke<CloudState>('set_cloud_settings', {
      settings: { ...settings, key: '' },
    });
    expect(cleared.settings.has_key).toBe(false);

    for (const bad of [
      { ...settings, base_url: 'http://cs.example.com' },
      { ...settings, auto_sync_minutes: 5 },
    ]) {
      const error = parseCommandError(
        await invoke('set_cloud_settings', { settings: bad }).catch((e: unknown) => e),
      );
      expect(error.code).toBe('validation');
    }
    await invoke('set_cloud_settings', {
      settings: { ...settings, base_url: 'http://127.0.0.1:8000' },
    });
    expect(backend.cloudPrefs.base_url).toBe('http://127.0.0.1:8000');
  });

  it('downloads a new revision, drops stale local disables and keeps the cache on failure', async () => {
    const backend = new MockBackend({ rules: 0, cloud: 'sample' });
    use(backend);
    await invoke('set_cloud_rules_disabled', { ids: ['mock-cloud-1', 'gone'], disabled: true });
    expect(backend.cloudPrefs.disabled_ids).toEqual(['mock-cloud-5', 'mock-cloud-1', 'gone']);
    backend.publishCloud((rules) => rules.splice(0, 1));
    const synced = await invoke<CloudState>('sync_cloud_now');
    expect(synced.status).toMatchObject({ revision: 13, syncing: false, last_error: null });
    expect(synced.rules.map((r) => r.id)).not.toContain('mock-cloud-1');
    expect(backend.cloudPrefs.disabled_ids).toEqual(['mock-cloud-5']);

    backend.setCloudFailure({ code: 'timeout', message: 'timed out' });
    backend.publishCloud();
    const failed = await invoke<CloudState>('sync_cloud_now');
    expect(failed.status).toMatchObject({ revision: 13, last_error: { code: 'timeout' } });
    expect(failed.status.last_success_at).toBe(synced.status.last_success_at);
    expect(failed.rules).toHaveLength(synced.rules.length);
  });

  it('switches phrases off on this computer only and back', async () => {
    use(new MockBackend({ rules: 0, cloud: 'sample' }));
    const off = await invoke<CloudState>('set_cloud_rules_disabled', {
      ids: ['mock-cloud-1', 'mock-cloud-4'],
      disabled: true,
    });
    expect(statuses(off).slice(0, 5)).toEqual([
      ['mock-cloud-1', 'disabled_local'],
      ['mock-cloud-2', 'active'],
      ['mock-cloud-3', 'active'],
      // The table's own switch comes first (§4); disabled_local still says this computer's choice.
      ['mock-cloud-4', 'disabled_remote'],
      ['mock-cloud-5', 'disabled_local'],
    ]);
    expect(off.rules.slice(0, 5).map((r) => r.disabled_local)).toEqual([
      true,
      false,
      false,
      true,
      true,
    ]);
    const on = await invoke<CloudState>('set_cloud_rules_disabled', {
      ids: ['mock-cloud-1', 'mock-cloud-5'],
      disabled: false,
    });
    expect(on.rules[0]).toMatchObject({ status: 'active', disabled_local: false });
    expect(on.rules[4]).toMatchObject({ status: 'active', disabled_local: false });
  });

  it('pushes cloud-state on sync, settings, local disables and saves, without a seq', async () => {
    const backend = new MockBackend({ rules: 3, cloud: 'sample' });
    use(backend);
    const pushed: CloudState[] = [];
    backend.on(CLOUD_STATE_EVENT, (payload) => pushed.push(payload as CloudState));
    const seq = backend.seq;
    await invoke('sync_cloud_now');
    expect(pushed.map((p) => p.status.syncing)).toEqual([true, false]);
    await invoke('set_cloud_rules_disabled', { ids: ['mock-cloud-1'], disabled: true });
    expect(pushed).toHaveLength(3);
    // 我的指令 saved without ;s2: the phrase no longer replaces anything.
    expect(pushed.at(-1)?.rules[5]?.overrides).toEqual(['mock-rule-2']);
    const { active } = await invoke<{ active: MockConfig }>('load_state');
    await invoke('save_config', {
      config: {
        ...active,
        rules: active.rules.filter((r) => isMockRule(r) && r.id !== 'mock-rule-2'),
      },
    });
    expect(pushed.at(-1)?.rules[5]).toMatchObject({ status: 'active' });
    expect(pushed.at(-1)?.rules[5]?.overrides).toBeUndefined();
    expect(pushed.every((p) => !('seq' in p))).toBe(true);
    // The runtime events' seq moved for store-state only.
    expect(backend.seq).toBe(seq + 1);
  });

  it('runs one sync at a time: a settings change syncs, sync_cloud_now joins it', async () => {
    const backend = new MockBackend({ rules: 0, cloud: 'off', cloudDelayMs: 20 });
    use(backend);
    const saved = await invoke<CloudState>('set_cloud_settings', {
      settings: { enabled: true, base_url: 'https://x.example', key: 'k', auto_sync_minutes: 0 },
    });
    expect(saved.status.syncing).toBe(true);
    const synced = await invoke<CloudState>('sync_cloud_now');
    expect(synced.status).toMatchObject({ syncing: false, revision: 12, last_error: null });
    expect(backend.cloudStatus.last_attempt_at).toBe(synced.status.last_success_at);
  });

  it('has no team phrase commands in legacy mode', async () => {
    use(new MockBackend({ rules: 0, legacy: true, cloud: 'sample' }));
    for (const command of [
      'get_cloud_state',
      'set_cloud_settings',
      'sync_cloud_now',
      'set_cloud_rules_disabled',
    ])
      expect(isMissingCommand(await invoke(command).catch((e: unknown) => e))).toBe(true);
  });

  it('reads the preview switch', () => {
    expect(mockOptionsFromQuery('?mock-cloud=error')).toEqual({ cloud: 'error' });
    expect(mockOptionsFromQuery('?mock-cloud=bogus')).toEqual({});
  });
});
