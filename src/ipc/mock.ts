// In-memory mock backend (FE-18): answers every command the frontend uses, 0.3 and 0.4, and can
// push the four runtime events. Used by the development browser preview (`npm run dev` outside
// Tauri, or VITE_MOCK=1) and by Vitest, never by a production build (see ./transport). All sample
// data is synthetic; no real user phrases.
//
// Preview switches (query string): ?mock=empty|200|10000 (rule count, default 200),
// &mock-legacy=1 (behave like a 0.3 backend), &mock-health=<HealthState>,
// &mock-safe-mode=<SafeModeReason>, &mock-platform=windows|macos|linux, &mock-dialog=cancel,
// &mock-unreadable=1 (B-43 sample: rules entries the backend could not read, unknown fields).
// In the preview the backend is also reachable as `globalThis.quickerMock` for manual pokes
// (e.g. `quickerMock.recordExecution({ outcome: 'dropped_busy' })`).
import { isReadableRule } from '../lib/rules';
import type { CommandError } from './errors';
import type { CommandArgs, Transport, Unlisten } from './transport';
import type {
  Capability,
  CapabilityStatus,
  Component,
  ComponentHealth,
  Diagnostic,
  EngineState,
  EventName,
  EventPayloads,
  HealthSnapshot,
  HealthState,
  HistoryEntry,
  PauseReason,
  ReasonCode,
  RuntimeSnapshot,
  SafeModeReason,
  StartupProblem,
} from './types';

// ---- legacy (0.3) data shapes, as far as the mock needs them ----

export interface MockRule {
  id: string;
  enabled: boolean;
  group: string;
  description: string;
  ignore_case: boolean;
  match: { kind: string; value: string };
  trigger_mode: string;
  terminator: null | { keys: string[] };
  ime_erasure: string | null;
  action: Record<string, unknown>;
  parameter: Record<string, unknown>;
  notification: { enabled: boolean; message: string };
}

/** A config as the tolerant reader (B-43) hands it out: unknown keys survive as extra keys. */
export interface MockConfig {
  version: number;
  revision: number;
  enabled: boolean;
  globals: {
    terminators: string[];
    word_separators: string;
    blacklist: string[];
    ime_erasure: string;
    [extra: string]: unknown;
  };
  /** Rules, and raw entries the backend could not read, kept as they are (any JSON value). */
  rules: unknown[];
  [extra: string]: unknown;
}

/**
 * Whether a `rules` entry deserialises as a rule (the mock's stand-in for serde): the same
 * structural check the UI uses, which mirrors the serde model.
 */
export const isMockRule = (entry: unknown): entry is MockRule => isReadableRule(entry);

/**
 * B-43 sample: an unreadable entry with an id between the rules, two without one at the end, and
 * fields from a newer version on the config, the globals and the first rule.
 */
function addTolerantSample(config: MockConfig): void {
  config.future_setting = { sample: true };
  config.globals.future_global = 'kept';
  const [first] = config.rules;
  if (isMockRule(first)) Object.assign(first, { future_rule_field: [1, 2] });
  config.rules.splice(Math.min(1, config.rules.length), 0, {
    id: 'mock-unreadable-1',
    match: 42,
    future_kind: 'from 0.6',
  });
  config.rules.push('not a rule', null);
}

export interface MockOptions {
  /** Number of synthetic rules (FE-18 sizes: 0, 200, 10,000). Default 200. */
  rules?: number;
  /** Behave like a 0.3 backend: no 0.4 commands, plain-string errors, no events. */
  legacy?: boolean;
  platform?: 'windows' | 'macos' | 'linux';
  health?: HealthState;
  safeMode?: SafeModeReason | null;
  problems?: StartupProblem[];
  /** How file dialogs answer. Default `accept`. */
  dialog?: 'accept' | 'cancel';
  /** Mix in the B-43 sample (unreadable entries, unknown fields). Default false. */
  unreadable?: boolean;
}

const COMPONENTS: ComponentHealth['component'][] = [
  'executor',
  'keyboard_hook',
  'mouse_hook',
  'raw_input',
  'supervisor',
  'focus_events',
  'clipboard',
  'protection',
];

const GROUPS = ['默认', '问候', '日期', '代码片段', '邮件模板', '常用符号', ''];

/** Deterministic synthetic rules; group `''` is included on purpose (F-04 empty group). */
export function syntheticRules(count: number): MockRule[] {
  return Array.from({ length: count }, (_, i): MockRule => {
    const n = i + 1;
    const group = GROUPS[i % GROUPS.length] ?? '默认';
    const kind = i % 5;
    const action: Record<string, unknown> =
      kind === 3
        ? {
            type: 'hotkey',
            keys: [
              { type: 'named', name: 'Ctrl' },
              { type: 'named', name: 'S' },
            ],
          }
        : kind === 4
          ? { type: 'none' }
          : {
              type: 'text',
              method: kind === 1 ? 'paste' : 'keys',
              source: { type: 'inline', value: `示例文本 ${n}${kind === 2 ? '\n第二行' : ''}` },
            };
    return {
      id: `mock-rule-${n}`,
      enabled: i % 11 !== 10,
      group,
      description: i % 3 === 0 ? `合成示例 ${n}` : '',
      ignore_case: false,
      match: { kind: 'literal', value: `;s${n}` },
      trigger_mode: i % 4 === 0 ? 'terminator' : 'immediate',
      terminator: null,
      ime_erasure: null,
      action,
      parameter: { type: 'default_match' },
      notification: { enabled: false, message: '' },
    };
  });
}

// get_capabilities as crates/platform/src/capabilities.rs builds it: per row, whether an adapter
// provides it at all and the components it depends on at run time.
type CapabilityRow = [id: string, adapter: boolean, dependsOn: Component[]];
const SHARED_ROWS: CapabilityRow[] = [
  ['keys', true, ['executor']],
  ['paste', true, ['executor', 'clipboard']],
  ['html', true, ['executor', 'clipboard']],
  ['file_clipboard', true, ['executor', 'clipboard']],
  ['image_clipboard', true, ['executor', 'clipboard']],
  ['mouse', true, ['executor']],
  ['recording', true, ['keyboard_hook', 'mouse_hook']],
  ['foreground', true, ['focus_events']],
  ['open', true, ['executor']],
];
const WINDOWS_ROWS: CapabilityRow[] = [
  ['keyboard_hook', true, ['keyboard_hook', 'raw_input']],
  ...SHARED_ROWS,
  ['ime_committed_text', true, ['keyboard_hook', 'raw_input']],
  ['quicker_private_action', false, []],
];
const MACOS_ROWS: CapabilityRow[] = [
  ['keyboard_hook', true, ['keyboard_hook']],
  ...SHARED_ROWS,
  ['ime_committed_text', false, []],
  ['wheel_delta', false, []],
  ['media_system_actions', true, ['executor']],
  ['window_ime_actions', false, []],
  ['quicker_private_action', false, []],
];
const DEFAULT_REASON: Record<Component, ReasonCode> = {
  executor: 'engine_down',
  supervisor: 'engine_down',
  keyboard_hook: 'hook_missing',
  mouse_hook: 'hook_missing',
  focus_events: 'hook_missing',
  raw_input: 'raw_input_missing',
  clipboard: 'clipboard_busy',
  protection: 'internal',
};
const SEVERITY: Record<CapabilityStatus, number> = {
  available: 0,
  experimental: 1,
  degraded: 2,
  unavailable: 3,
};

/**
 * The capability table of `platform` lowered by component health, like `capabilities_for`: a
 * degraded dependency makes a capability degraded, a starting, recovering or failed one makes it
 * unavailable, the worst one wins. macOS is experimental throughout; other platforms than Windows
 * and macOS report the Windows ids, all `unsupported`.
 */
export function mockCapabilities(
  platform: MockOptions['platform'],
  components: readonly ComponentHealth[] = [],
): Capability[] {
  const rows = platform === 'macos' ? MACOS_ROWS : WINDOWS_ROWS;
  return rows.map(([id, adapter, dependsOn]) => {
    let verdict: [CapabilityStatus, string | null] =
      platform !== 'windows' && platform !== 'macos'
        ? ['unavailable', 'unsupported']
        : !adapter
          ? ['unavailable', 'not_implemented']
          : platform === 'macos'
            ? ['experimental', 'experimental_platform']
            : ['available', null];
    for (const c of components) {
      if (!dependsOn.includes(c.component) || c.state === 'healthy') continue;
      const reason = c.reason && c.reason !== 'ok' ? c.reason : DEFAULT_REASON[c.component];
      const status: CapabilityStatus = c.state === 'degraded' ? 'degraded' : 'unavailable';
      if (SEVERITY[status] > SEVERITY[verdict[0]]) verdict = [status, reason];
    }
    const [status, reason] = verdict;
    return { id, status, reason, available: status !== 'unavailable' };
  });
}

const blankTimings = () => ({
  queue_us: null,
  resolve_us: null,
  erase_us: null,
  output_us: null,
  restore_us: null,
  total_us: null,
});

const notFound = (command: string) => `Command ${command} not found`;
const clone = <T>(value: T): T => structuredClone(value);

export class MockBackend {
  readonly options: Required<Omit<MockOptions, 'safeMode' | 'problems'>>;
  /** Every command received, in order (tests assert on it). */
  readonly calls: { command: string; args: CommandArgs }[] = [];
  /** Set to make the next command of that name reject with this value. */
  readonly failures = new Map<string, unknown>();

  draft: MockConfig;
  active: MockConfig;
  previous: MockConfig[] = [];
  health: HealthSnapshot;
  engine: EngineState;
  safeMode: SafeModeReason | null;
  problems: StartupProblem[];
  history: HistoryEntry[] = [];
  autostart = false;
  /** S-09 `auto_listen_on_start`; a 0.3 backend has no such setting. */
  autoListen = false;
  recording = false;
  #seq = 0;
  #historySeq = 0;
  readonly #handlers = new Map<string, Set<(payload: unknown) => void>>();

  constructor(options: MockOptions = {}) {
    this.options = {
      rules: options.rules ?? 200,
      legacy: options.legacy ?? false,
      platform: options.platform ?? 'windows',
      health: options.health ?? 'healthy',
      dialog: options.dialog ?? 'accept',
      unreadable: options.unreadable ?? false,
    };
    const config: MockConfig = {
      version: 1,
      revision: 1,
      enabled: true,
      globals: {
        terminators: ['space', 'semicolon'],
        word_separators: ' \t\n',
        blacklist: [],
        ime_erasure: 'escape',
      },
      rules: syntheticRules(this.options.rules),
    };
    if (this.options.unreadable) addTolerantSample(config);
    this.active = config;
    this.draft = clone(config);
    this.safeMode = options.safeMode ?? null;
    this.health = this.#healthOf(this.options.health);
    this.engine = {
      listening: false,
      paused_reasons: this.safeMode ? ['safe_mode'] : [],
      busy: false,
      current_process: 'notepad.exe',
    };
    this.#refreshListening();
    this.problems =
      options.problems ??
      (this.safeMode
        ? [{ kind: 'safe_mode', severity: 'error', args: { reason: this.safeMode } }]
        : []);
  }

  // ---- events ----

  on(event: string, handler: (payload: unknown) => void): Unlisten {
    let set = this.#handlers.get(event);
    if (!set) this.#handlers.set(event, (set = new Set()));
    set.add(handler);
    return () => set.delete(handler);
  }

  get seq(): number {
    return this.#seq;
  }

  /** Push an event with the next global seq. 0.3 backends had no events, so legacy mode is silent. */
  emit<K extends EventName>(name: K, payload: Omit<EventPayloads[K], 'seq'>): number {
    this.#seq += 1;
    if (this.options.legacy) return this.#seq;
    const full = { ...payload, seq: this.#seq };
    for (const handler of this.#handlers.get(name) ?? []) handler(clone(full));
    return this.#seq;
  }

  /** Burn seq numbers without delivering anything, as if events were missed. */
  skipSeq(count = 1): void {
    this.#seq += count;
  }

  /** Deliver a payload with an explicit seq (replays, reordering); does not touch the counter. */
  deliver<K extends EventName>(name: K, payload: EventPayloads[K]): void {
    for (const handler of this.#handlers.get(name) ?? []) handler(clone(payload));
  }

  setHealth(state: HealthState, components?: ComponentHealth[]): void {
    this.health = components ? { state, components } : this.#healthOf(state);
    this.#togglePause('engine_down', state === 'failed' || state === 'recovering');
    this.emit('engine-health', clone(this.health));
    this.#emitState();
  }

  setPaused(reason: PauseReason, on: boolean): void {
    this.#togglePause(reason, on);
    this.#emitState();
  }

  setBusy(busy: boolean): void {
    this.engine = { ...this.engine, busy };
    this.#emitState();
  }

  addProblem(problem: StartupProblem): void {
    this.problems.push(problem);
    this.emit('startup-problem', clone(problem));
  }

  /** Append a history entry and push it; `suppressed` simulates rate-limited pushes. */
  recordExecution(partial: Partial<HistoryEntry> = {}, suppressed = 0): HistoryEntry {
    for (let i = 0; i < suppressed; i += 1) this.#appendHistory(partial);
    const entry = this.#appendHistory(partial);
    this.emit('execution-result', { entry: clone(entry), suppressed });
    return entry;
  }

  // ---- commands ----

  async handle(command: string, args: CommandArgs = {}): Promise<unknown> {
    this.calls.push({ command, args: clone(args) });
    // Settle asynchronously, like real IPC.
    await Promise.resolve();
    if (this.failures.has(command)) {
      const failure = this.failures.get(command);
      this.failures.delete(command);
      throw failure;
    }
    const legacy = this.options.legacy;
    const handler = (legacy ? this.#legacyCommands : this.#commands)[command];
    // eslint-disable-next-line @typescript-eslint/only-throw-error -- Tauri rejects with a plain string
    if (!handler) throw notFound(command);
    return handler(args);
  }

  /** Reject like the backend: CommandError in 0.4, plain string in legacy mode. */
  #fail(error: CommandError, legacyText: string): never {
    // eslint-disable-next-line @typescript-eslint/only-throw-error -- IPC rejects with plain values
    throw this.options.legacy ? legacyText : error;
  }

  #guardSafeMode(): void {
    if (this.safeMode && !this.options.legacy)
      this.#fail({ code: 'safe_mode', detail: { reason: this.safeMode } }, '');
  }

  readonly #shared: Record<string, (args: CommandArgs) => unknown> = {
    load_state: () => this.#state(),
    save_draft: (args) => {
      this.#guardSafeMode();
      const config = clone(args.config as MockConfig);
      if (!this.options.legacy) {
        const errors = validate(config).filter((d) => d.severity === 'error');
        if (errors.length > 0)
          this.#fail({ code: 'validation', detail: { diagnostics: errors } }, '');
      }
      this.draft = config;
      return this.#state();
    },
    apply_settings: () => {
      this.#guardSafeMode();
      const errors = validate(this.draft).filter((d) => d.severity === 'error');
      if (errors.length > 0)
        this.#fail({ code: 'validation', detail: { diagnostics: errors } }, JSON.stringify(errors));
      this.previous.push(this.active);
      this.active = { ...clone(this.draft), revision: this.active.revision + 1 };
      this.draft = clone(this.active);
      return this.#state();
    },
    discard_draft: () => {
      this.draft = clone(this.active);
      return this.#state();
    },
    rollback: () => {
      this.#guardSafeMode();
      const previous = this.previous.pop();
      if (!previous)
        this.#fail(
          { code: 'not_found', detail: { kind: 'revision', id: '' } },
          'No previous revision',
        );
      this.active = { ...previous, revision: this.active.revision + 1 };
      this.draft = clone(this.active);
      return this.#state();
    },
    set_paused: (args) => {
      if (args.paused === false && this.recording)
        this.#fail({ code: 'busy' }, 'Stop recording before resuming automatic matching');
      this.setPaused('manual', args.paused === true);
      return this.#state();
    },
    validate_config: (args) => {
      const errors = validate(args.config as MockConfig);
      return { valid: !errors.some((d) => d.severity === 'error'), errors };
    },
    test_rule: (args) => {
      const rule = args.rule as MockRule;
      const input = typeof args.input === 'string' ? args.input : '';
      if (!rule.match.value || !input.endsWith(rule.match.value)) return null;
      return {
        rule_id: rule.id,
        matched: rule.match.value,
        captures: [],
        erase_chars: Array.from(rule.match.value).length,
        parameter: null,
        consume_terminator: rule.trigger_mode !== 'immediate',
        revision: this.active.revision,
      };
    },
    parse_source: (args) => {
      const source = typeof args.source === 'string' ? args.source : '';
      const lines = source.split('\n').filter((line) => line.trim() !== '');
      return {
        plan: {
          version: 1,
          dialect: args.dialect,
          source,
          steps: lines.map((text, i) => ({
            operation: { type: 'text', text, method: 'keys' },
            span: { line: i + 1, column: 1, length: text.length },
            target: { type: 'locked' },
          })),
        },
        diagnostics: [],
      };
    },
    cancel_execution: () => true,
    execute_action: () => {
      if (this.engine.busy) this.#fail({ code: 'busy' }, 'An action is already running');
      const entry = this.recordExecution({ rule_id: 'manual-test', trigger: 'manual' });
      return {
        id: `mock-exec-${entry.seq}`,
        status: 'success',
        message: '浏览器预览：模拟执行，未发送任何输入',
        completed_steps: 1,
        side_effects: false,
      };
    },
    get_foreground: () => ({
      platform: this.options.platform,
      bundle_id: '',
      id: 1,
      process: 'notepad.exe',
      title: '示例窗口',
    }),
    choose_file: (args) => {
      if (this.options.dialog === 'cancel') return null;
      return args.kind === 'image' ? ['mock://示例图片.png'] : ['mock://示例文件.json'];
    },
    export_config: () => {
      if (this.options.dialog === 'cancel') {
        if (this.options.legacy) this.#fail({ code: 'cancelled' }, 'Export cancelled');
        return null;
      }
      return 'mock://quicker-text-package.json';
    },
    import_config: (args) => {
      const incoming = syntheticRules(2).map((rule, i) => ({
        ...rule,
        id: `mock-import-${i + 1}`,
        group: '导入示例',
      }));
      const config = clone(this.draft);
      if (args.strategy === 'replace') config.rules = incoming;
      else for (const rule of incoming) config.rules.push(rule);
      return {
        config,
        report: [`2 条合成示例规则；策略：${String(args.strategy)}`, '0 conflicts'],
      };
    },
    preview_file: () => '浏览器预览：文件内容示例',
    preview_image: () =>
      'data:image/svg+xml,' +
      encodeURIComponent(
        '<svg xmlns="http://www.w3.org/2000/svg" width="160" height="90"><rect width="160" height="90" fill="#ddd"/></svg>',
      ),
    get_cursor: () => ({ x: 640, y: 360, monitors: [{ x: 0, y: 0, width: 1920, height: 1080 }] }),
    capture_selection: () => ({
      text: '示例选中文字',
      status: 'success',
      message: '浏览器预览：模拟读取',
    }),
    exit_app: () => null,
    start_recording: () => {
      this.recording = true;
      this.setPaused('recording', true);
      return null;
    },
    stop_recording: () => {
      this.recording = false;
      this.setPaused('recording', false);
      return { version: 1, dialect: 'structured', source: null, steps: [] };
    },
    set_autostart: (args) => {
      this.autostart = args.enabled === true;
      return this.autostart;
    },
  };

  readonly #legacyCommands: Record<string, (args: CommandArgs) => unknown> = this.#shared;

  readonly #commands: Record<string, (args: CommandArgs) => unknown> = {
    ...this.#shared,
    get_runtime_snapshot: (): RuntimeSnapshot => ({
      seq: this.#seq,
      health: clone(this.health),
      state: clone(this.engine),
      revision: this.active.revision,
      history_seq: this.#historySeq,
      safe_mode: this.safeMode,
    }),
    get_health: () => clone(this.health),
    // Contract §5: the snapshot as it is now; `recovering` and the outcome follow as engine-health
    // events (a latched failure answers `failed` here first).
    repair_engine: (): HealthSnapshot => {
      const current = clone(this.health);
      setTimeout(() => {
        this.setHealth('recovering');
        setTimeout(() => this.setHealth('healthy'), 600);
      }, 0);
      return current;
    },
    startup_problems: () => clone(this.problems),
    get_capabilities: (): Capability[] => this.#capabilities(),
    export_diagnostics: () =>
      this.options.dialog === 'cancel' ? null : 'mock://quicker-text-diagnostics.zip',
    // `ttl_hours: null` restores the default level, `info` (diagnostics.rs).
    set_log_level: (args) =>
      typeof args.ttl_hours === 'number'
        ? { level: args.level, until_ms: Date.now() + args.ttl_hours * 3_600_000 }
        : { level: 'info', until_ms: null },
    open_log_dir: () => null,
    open_data_dir: () => null,
    self_test: () => ({
      passed: true,
      checks: ['raw_input', 'hook_heartbeat', 'foreground', 'clipboard'].map((name) => ({
        name,
        passed: true,
        reason: null,
        elapsed_ms: 3,
      })),
    }),
    log_frontend_error: () => null,
    report_metric: () => null,
    list_history: (args) => {
      const since = typeof args.since_seq === 'number' ? args.since_seq : null;
      return clone(this.history.filter((e) => since === null || e.seq > since));
    },
    clear_history: () => {
      this.history = [];
      return null;
    },
    explain_last_miss: () => {
      const miss = [...this.history].reverse().find((e) => e.outcome !== 'succeeded');
      return miss
        ? {
            seq: miss.seq,
            at_ms: miss.at_ms,
            rule_id: miss.rule_id,
            outcome: miss.outcome,
            reason: miss.reason,
          }
        : null;
    },
    // S-09: a setting, so safe mode refuses it like set_autostart in the backend.
    set_auto_listen: (args) => {
      this.#guardSafeMode();
      this.autoListen = args.enabled === true;
      return this.autoListen;
    },
  };

  // ---- internals ----

  #state() {
    const paused = this.engine.paused_reasons.length > 0;
    const legacy = {
      draft: clone(this.draft),
      active: clone(this.active),
      revision: this.active.revision,
      dirty: JSON.stringify(this.draft) !== JSON.stringify(this.active),
      paused,
      engine_status: this.engine.busy ? 'running' : paused ? 'paused' : 'listening',
      last_result: null,
      capabilities: [
        { id: 'keyboard_hook', available: true, reason: '浏览器预览：模拟数据' },
        { id: 'keys', available: true, reason: '' },
        { id: 'paste', available: true, reason: '' },
      ],
      autostart: this.autostart,
      engine_detail: { platform: this.options.platform, reason: '浏览器预览：未连接真实输入引擎' },
      platform: this.options.platform,
    };
    if (this.options.legacy) return legacy;
    return {
      ...legacy,
      capabilities: this.#capabilities(),
      auto_listen_on_start: this.autoListen,
      health: clone(this.health),
      engine_state: clone(this.engine),
      startup_problems: clone(this.problems),
      safe_mode: this.safeMode,
    };
  }

  #capabilities(): Capability[] {
    return mockCapabilities(this.options.platform, this.health.components);
  }

  #healthOf(state: HealthState): HealthSnapshot {
    const now = Date.now();
    return {
      state,
      components: COMPONENTS.map((component) => {
        const broken =
          (state === 'failed' || state === 'recovering') && component === 'raw_input'
            ? state
            : state === 'degraded' && component === 'focus_events'
              ? 'degraded'
              : null;
        return {
          component,
          state: broken ?? 'healthy',
          reason:
            broken === null ? null : broken === 'degraded' ? 'unsupported' : 'raw_input_missing',
          since_ms: now,
        };
      }),
    };
  }

  #togglePause(reason: PauseReason, on: boolean): void {
    const reasons = this.engine.paused_reasons.filter((r) => r !== reason);
    if (on) reasons.push(reason);
    this.engine = { ...this.engine, paused_reasons: reasons };
    this.#refreshListening();
  }

  #refreshListening(): void {
    const healthy = this.health.state === 'healthy' || this.health.state === 'degraded';
    this.engine = { ...this.engine, listening: healthy && this.engine.paused_reasons.length === 0 };
  }

  #emitState(): void {
    this.emit('engine-state', clone(this.engine));
  }

  #appendHistory(partial: Partial<HistoryEntry>): HistoryEntry {
    this.#historySeq += 1;
    const entry: HistoryEntry = {
      seq: this.#historySeq,
      at_ms: Date.now(),
      rule_id: 'mock-rule-1',
      trigger: 'immediate',
      outcome: 'succeeded',
      reason: 'ok',
      process: 'notepad.exe',
      completed_steps: 1,
      side_effects: true,
      timings: { ...blankTimings(), total_us: 12_000 },
      ...partial,
    };
    entry.seq = this.#historySeq;
    this.history.push(entry);
    if (this.history.length > 500) this.history.shift();
    return entry;
  }
}

/** Subset of the backend's checks (crates/domain matching.rs) that the UI flows exercise. */
export function validate(config: MockConfig): Diagnostic[] {
  const errors: Diagnostic[] = [];
  const ids = new Set<string>();
  config.rules.forEach((rule, i) => {
    const base = `rules[${i}]`;
    if (!isMockRule(rule)) {
      errors.push({
        ...diagnostic('rule_unreadable', 'Rule could not be read and is kept as is', base),
        severity: 'warning',
      });
      return;
    }
    if (!rule.id.trim() || ids.has(rule.id))
      errors.push(diagnostic('rule_id', 'Rule ID must be nonempty and unique', `${base}.id`));
    ids.add(rule.id);
    if (rule.enabled && !rule.match.value)
      errors.push(diagnostic('empty_match', 'Match value is required', `${base}.match.value`));
  });
  return errors;
}

const diagnostic = (code: string, message: string, path: string): Diagnostic => ({
  code,
  message,
  path,
  span: { line: 0, column: 0, length: 0 },
  severity: 'error',
});

export function createMockTransport(backend: MockBackend): Transport {
  return {
    kind: 'mock',
    // The caller picks the result type, as with the real transport.
    invoke: (command, args) => backend.handle(command, args) as Promise<never>,
    listen: (event, handler) =>
      Promise.resolve(backend.on(event, handler as (payload: unknown) => void)),
  };
}

const HEALTH_STATES: readonly HealthState[] = [
  'starting',
  'healthy',
  'degraded',
  'recovering',
  'failed',
];
const SAFE_MODES: readonly SafeModeReason[] = [
  'crash_loop',
  'locked',
  'newer_schema',
  'no_valid_config',
];

/** Parse the preview query string (see the header comment). */
export function mockOptionsFromQuery(search: string): MockOptions {
  const q = new URLSearchParams(search);
  const options: MockOptions = {};
  const size = q.get('mock');
  if (size === 'empty') options.rules = 0;
  else if (size && /^\d+$/.test(size)) options.rules = Math.min(Number(size), 10_000);
  if (q.get('mock-legacy') === '1') options.legacy = true;
  const health = q.get('mock-health');
  const found = HEALTH_STATES.find((s) => s === health);
  if (found) options.health = found;
  const safeMode = SAFE_MODES.find((s) => s === q.get('mock-safe-mode'));
  if (safeMode) options.safeMode = safeMode;
  const platform = q.get('mock-platform');
  if (platform === 'windows' || platform === 'macos' || platform === 'linux')
    options.platform = platform;
  if (q.get('mock-dialog') === 'cancel') options.dialog = 'cancel';
  if (q.get('mock-unreadable') === '1') options.unreadable = true;
  return options;
}

/** Transport for the browser preview, configured from the URL. */
export function createPreviewTransport(): Transport {
  const backend = new MockBackend(mockOptionsFromQuery(window.location.search));
  (globalThis as { quickerMock?: MockBackend }).quickerMock = backend;
  return createMockTransport(backend);
}
