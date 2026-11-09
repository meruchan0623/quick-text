// In-memory mock backend (FE-18): answers every command the frontend uses, 0.3 and 0.4, and can
// push the runtime events and cloud-state. Used by the development browser preview (`npm run dev`
// outside Tauri, or VITE_MOCK=1) and by Vitest, never by a production build (see ./transport). All
// sample data is synthetic; no real user phrases.
//
// Preview switches (query string): ?mock=empty|200|10000 (rule count, default 200),
// &mock-legacy=1 (behave like a 0.3 backend), &mock-health=<HealthState>,
// &mock-safe-mode=<SafeModeReason>, &mock-platform=windows|macos|linux, &mock-dialog=cancel,
// &mock-unreadable=1 (B-43 sample: rules entries the backend could not read, unknown fields),
// &mock-cloud=off|sample|error (team phrases, docs/cloud-phrases.md; the preview default is sample),
// &mock-update=available|none|error (in-app update check, src/lib/update.ts; default available).
// In the preview the backend is also reachable as `globalThis.quickerMock` for manual pokes
// (e.g. `quickerMock.recordExecution({ outcome: 'dropped_busy' })`).
import { AUTO_SYNC_MINUTES } from '../lib/cloud';
import { isReadableRule } from '../lib/rules';
import { UPDATE_PROGRESS_EVENT, compareSemver, isPrerelease, parseSemver } from '../lib/update';
import type { UpdateCheck, UpdateDownload, UpdateProgress, UpdateRelease } from '../lib/update';
import { DEFAULT_TERMINATORS } from '../lib/terminators';
import type { CommandError } from './errors';
import type { CommandArgs, Transport, Unlisten } from './transport';
import { CLOUD_STATE_EVENT } from './types';
import type {
  Capability,
  CapabilityStatus,
  CloudError,
  CloudRuleEntry,
  CloudState,
  CloudStatus,
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
  /** Format returned by the synthetic import picker. */
  importFormat?: 'json' | 'xlsx' | 'zip';
  /** Mix in the B-43 sample (unreadable entries, unknown fields). Default false. */
  unreadable?: boolean;
  /**
   * Team phrases: `off` (not subscribed, nothing cached), `sample` (subscribed, the sample table
   * synced) or `error` (as `sample`, but the key is refused). Default `off`.
   */
  cloud?: 'off' | 'sample' | 'error';
  /** How long a team phrase sync takes, in ms. Default 0 (settles with the command). */
  cloudDelayMs?: number;
  /**
   * In-app updates: `available` (a newer prerelease is published; the newest release is the
   * current version), `none` (only the current version) or `error` (GitHub cannot be reached).
   * Default `available`.
   */
  update?: 'available' | 'none' | 'error';
  /** Delay between two download progress events, in ms. Default 0. */
  updateDelayMs?: number;
}

/** The version the mock backend reports as running (`check_update.current`). */
export const MOCK_APP_VERSION = '0.3.1';

/** A synthetic release as the backend reads it from GitHub (src-tauri/src/update.rs GhRelease). */
interface MockRelease {
  tag: string;
  prerelease: boolean;
  published_at: string;
  notes: string;
}

/** Newest first, as the GitHub API lists them; one non-SemVer tag that is ignored. */
function mockReleases(update: 'available' | 'none' | 'error'): MockRelease[] {
  const day = 86_400_000;
  const current: MockRelease = {
    tag: `v${MOCK_APP_VERSION}`,
    prerelease: false,
    published_at: new Date(Date.now() - 30 * day).toISOString(),
    notes: '浏览器预览：示例发布说明（当前版本）。',
  };
  if (update === 'none') return [current];
  return [
    {
      tag: 'v0.4.0-test.12',
      prerelease: true,
      published_at: new Date(Date.now() - 2 * day).toISOString(),
      notes:
        '浏览器预览：示例测试版。\n\n- 新增：应用内检查更新\n- 构建自提交 `0000000`（示例）\n\nChecksums: SHA256SUMS-macos.txt, SHA256SUMS-windows.txt.',
    },
    {
      tag: 'test-4-eb4eebb',
      prerelease: true,
      published_at: new Date(Date.now() - 10 * day).toISOString(),
      notes: '',
    },
    current,
  ];
}

/** This platform's package of a mock release (src-tauri/src/update.rs pick_asset). */
function mockAsset(
  platform: 'windows' | 'macos' | 'linux',
  version: string,
): UpdateRelease['asset'] {
  if (platform === 'macos') return { name: `Quicktext_${version}_aarch64.dmg`, size: 18_400_000 };
  if (platform === 'windows')
    return { name: `Quicktext-${version}-Windows-x64-Portable.zip`, size: 9_800_000 };
  return null;
}

const updateError = (kind: string, message: string): CommandError => ({
  code: 'update',
  detail: { kind, message },
});

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

// ---- team phrases (docs/cloud-phrases.md) ----

/** The mock phrase server's table (§1), as the download serves it. */
export interface MockCloudTable {
  revision: number;
  updated_at: string;
  rules: MockRule[];
  /** Entries the download skipped as unreadable (`unreadable_count`). */
  unreadable: number;
}

/** `prefs.json` `app.cloud` (§2): the key itself never leaves the backend. */
export interface MockCloudPrefs {
  enabled: boolean;
  base_url: string;
  key: string;
  auto_sync_minutes: number;
  disabled_ids: string[];
}

const cloudText = (value: string) => ({
  type: 'text',
  method: 'paste',
  source: { type: 'inline', value },
});

/**
 * Synthetic team phrases, one per status with the default prefs of `sample`: `mock-cloud-4` is
 * switched off in the table, `mock-cloud-5` on this computer, `mock-cloud-6` has the abbreviation of
 * synthetic rule 2 (and replaces it while both are enabled), `mock-cloud-7` a regex that does not
 * compile.
 */
export function sampleCloudRules(): MockRule[] {
  const phrase = (n: number, partial: Partial<MockRule>): MockRule => ({
    id: `mock-cloud-${n}`,
    enabled: true,
    group: '',
    description: '',
    ignore_case: false,
    match: { kind: 'literal', value: `tm${n}` },
    trigger_mode: 'terminator',
    terminator: null,
    ime_erasure: null,
    action: cloudText(`团队示例 ${n}`),
    parameter: { type: 'default_match' },
    notification: { enabled: false, message: '' },
    ...partial,
  });
  return [
    phrase(1, {
      group: '问候',
      description: '开场问候（示例）',
      match: { kind: 'literal', value: 'tnh' },
      action: cloudText('您好，这里是示例客服，请问有什么可以帮您？'),
    }),
    phrase(2, {
      group: '问候',
      description: '结束语（示例）',
      match: { kind: 'literal', value: 'tjs' },
      trigger_mode: 'immediate',
      action: cloudText('感谢您的耐心等待，祝您生活愉快！\n如有其他问题，欢迎随时联系。'),
    }),
    phrase(3, {
      group: '售后',
      description: '退款时效（示例）',
      ignore_case: true,
      match: { kind: 'literal', value: 'ttk' },
      terminator: { keys: ['semicolon'] },
      action: cloudText('示例：退款一般在 3 至 5 个工作日内原路退回。'),
    }),
    phrase(4, {
      enabled: false,
      group: '售后',
      description: '已下线的旧话术（示例）',
      match: { kind: 'literal', value: 'tjh' },
    }),
    phrase(5, {
      group: '物流',
      description: '发货时间（示例）',
      match: { kind: 'literal', value: 'tfh' },
      action: cloudText('示例：下单后 48 小时内发货。'),
    }),
    phrase(6, {
      group: '物流',
      description: '与「我的指令」同名的缩写（示例）',
      match: { kind: 'literal', value: ';s2' },
    }),
    phrase(7, {
      group: '物流',
      description: '订单号（正则写错的示例）',
      match: { kind: 'regex', value: '(DD[0-9' },
    }),
    phrase(8, {
      description: '帮助中心（示例）',
      match: { kind: 'literal', value: 'tbz' },
      action: {
        type: 'open',
        target: 'https://example.com/help',
        args: [],
        working_directory: null,
      },
    }),
  ];
}

const sampleCloudTable = (): MockCloudTable => ({
  revision: 12,
  updated_at: '2026-10-28T08:30:00+00:00',
  rules: sampleCloudRules(),
  unreadable: 1,
});

/**
 * Why a phrase cannot be used, in Chinese for the user as the backend words it (§5); null when it
 * can. JavaScript's RegExp stands in for the backend's lenient compile.
 */
function cloudProblem(rule: MockRule): string | null {
  if (rule.match.kind !== 'regex') return null;
  try {
    new RegExp(rule.match.value);
    return null;
  } catch {
    return '缩写的正则表达式写法有误，无法使用。';
  }
}

// get_capabilities as crates/platform/src/capabilities.rs builds it: per row, whether an adapter
// provides it at all and the components it depends on at run time.
type CapabilityRow = [id: string, adapter: boolean, dependsOn: Component[]];
const SHARED_ROWS: CapabilityRow[] = [
  ['keys', true, ['executor']],
  ['paste', true, ['executor', 'clipboard']],
  ['html', true, ['executor', 'clipboard']],
  ['image', true, ['executor', 'clipboard']],
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

/** What the backend serves in Locked and NewerSchema: `Config::default()`, revision 0, no rules. */
const placeholderConfig = (): MockConfig => ({
  version: 1,
  revision: 0,
  enabled: true,
  globals: {
    terminators: [...DEFAULT_TERMINATORS],
    word_separators: ' \t\n',
    blacklist: [],
    ime_erasure: 'escape',
  },
  rules: [],
});

export class MockBackend {
  readonly options: Required<Omit<MockOptions, 'safeMode' | 'problems'>>;
  /** Every command received, in order (tests assert on it). */
  readonly calls: { command: string; args: CommandArgs }[] = [];
  /** Set to make the next command of that name reject with this value. */
  readonly failures = new Map<string, unknown>();

  active: MockConfig;
  /**
   * In Locked and NewerSchema, the store on disk, which the backend did not load: it serves an
   * empty revision-0 placeholder as active instead (src-tauri store/fallback.rs
   * `read_only`). The Locked retry that succeeds installs this (`endSafeMode`). Tests may replace
   * it before that.
   */
  disk: { active: MockConfig } | null = null;
  previous: MockConfig[] = [];
  health: HealthSnapshot;
  engine: EngineState;
  safeMode: SafeModeReason | null;
  problems: StartupProblem[];
  history: HistoryEntry[] = [];
  autostart = false;
  /** S-09 `auto_listen_on_start`; a 0.3 backend has no such setting. */
  autoListen = false;
  /** Beginner typing hint (`app.candidate_hint`). */
  candidateHint = { enabled: true, duration_ms: 2000 };
  recording = false;
  /** Team phrase sync settings (`app.cloud`). */
  cloudPrefs: MockCloudPrefs;
  /** `cloud-phrases.json`: the table last downloaded, or null. */
  cloudCache: MockCloudTable | null;
  /** CloudState.status without the cached table's revision, updated_at and unreadable_count. */
  cloudStatus: Pick<CloudStatus, 'syncing' | 'last_attempt_at' | 'last_success_at' | 'last_error'>;
  /** What the phrase server answers a download with: the table, unless `error` is set. */
  cloudServer: { table: MockCloudTable; error: CloudError | null };
  /** The sync running; another sync request joins it (§3: one at a time). */
  #cloudSync: Promise<CloudState> | null = null;
  /** An update download is running (one at a time). */
  #updateDownloading = false;
  /** Paths download_update returned; the only ones reveal_update accepts. */
  readonly updateDownloads = new Set<string>();
  #seq = 0;
  #historySeq = 0;
  readonly #handlers = new Map<string, Set<(payload: unknown) => void>>();
  /** What the last store-state said (or the start); store-state is sent only on a change. */
  #store: { revision: number; safeMode: SafeModeReason | null };
  /** The config import_config last previewed (as JSON), until the next save_config. */
  #pendingImport: { json: string; token: string } | null = null;
  #importSequence = 0;

  constructor(options: MockOptions = {}) {
    this.options = {
      rules: options.rules ?? 200,
      legacy: options.legacy ?? false,
      platform: options.platform ?? 'windows',
      health: options.health ?? 'healthy',
      dialog: options.dialog ?? 'accept',
      importFormat: options.importFormat ?? 'json',
      unreadable: options.unreadable ?? false,
      cloud: options.cloud ?? 'off',
      cloudDelayMs: options.cloudDelayMs ?? 0,
      update: options.update ?? 'available',
      updateDelayMs: options.updateDelayMs ?? 0,
    };
    const subscribed = this.options.cloud !== 'off';
    const synced = new Date(Date.now() - 5 * 60_000).toISOString();
    this.cloudPrefs = {
      enabled: subscribed,
      base_url: subscribed ? 'https://cs.example.com' : '',
      key: subscribed ? 'mock-sync-key' : '',
      auto_sync_minutes: 30,
      disabled_ids: subscribed ? ['mock-cloud-5'] : [],
    };
    this.cloudCache = subscribed ? sampleCloudTable() : null;
    const refused: CloudError = {
      code: 'unauthorized',
      message: '服务器拒绝了同步密钥（HTTP 401）',
    };
    this.cloudStatus = {
      syncing: false,
      last_attempt_at: subscribed ? new Date().toISOString() : null,
      last_success_at: subscribed ? synced : null,
      last_error: this.options.cloud === 'error' ? refused : null,
    };
    this.cloudServer = {
      table: sampleCloudTable(),
      error: this.options.cloud === 'error' ? refused : null,
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
    this.safeMode = options.safeMode ?? null;
    if (this.safeMode === 'locked' || this.safeMode === 'newer_schema') {
      this.disk = { active: config };
      this.active = placeholderConfig();
    } else this.active = config;
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
    this.#store = { revision: this.active.revision, safeMode: this.safeMode };
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

  /**
   * A revision published from outside this window (another instance, say): active config is published. `edit` changes the published config.
   */
  publishExternally(edit?: (config: MockConfig) => void): void {
    const next = { ...clone(this.active), revision: this.active.revision + 1 };
    edit?.(next);
    this.previous.push(this.active);
    this.active = next;
    this.#emitStore();
  }

  /** The phrase server publishes a new revision of its table; the next sync downloads it. */
  publishCloud(edit?: (rules: MockRule[]) => void): void {
    const table = clone(this.cloudServer.table);
    edit?.(table.rules);
    table.revision += 1;
    table.updated_at = new Date().toISOString();
    this.cloudServer = { ...this.cloudServer, table };
  }

  /** Make every following download fail with `error` (null: succeed again). */
  setCloudFailure(error: CloudError | null): void {
    this.cloudServer = { ...this.cloudServer, error };
  }

  /**
   * Safe mode ends without a command, as when the backend's SafeMode(Locked) retry succeeds: the
   * store is read again (at `revision`, when given), the safe_mode pause and problem go, and the
   * changes are pushed. A problem going is never pushed (contract §3).
   *
   * From Locked, the store read replaces the placeholder active with the ones on disk
   * (src-tauri store/load.rs `install`), whose revision may be the placeholder's: store-state then
   * says the same revision, with safe mode null (review I6).
   */
  endSafeMode(revision?: number): void {
    if (this.disk !== null) {
      this.active = this.disk.active;
      this.disk = null;
    }
    if (revision !== undefined && revision !== this.active.revision) {
      this.active = { ...clone(this.active), revision };
    }
    this.#leaveSafeMode();
    this.#emitStore();
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
    const answer = handler(args);
    // A command that changed the revision or safe mode pushes store-state before it answers, so the
    // event may reach the window first (save_config, rollback).
    this.#emitStore();
    return answer;
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
    save_config: (args) => {
      const config = clone(args.config as MockConfig);
      // In safe mode only the previewed import may be committed, and only where importing is the
      // way out (src-tauri store/mod.rs).
      const importing =
        args.importToken !== undefined &&
        this.#pendingImport?.token === args.importToken &&
        this.#pendingImport.json === JSON.stringify(config);
      if (args.importToken !== undefined && !importing)
        this.#fail({ code: 'token_expired' }, 'Import preview expired');
      if (!(importing && (this.safeMode === 'crash_loop' || this.safeMode === 'no_valid_config')))
        this.#guardSafeMode();
      if (!this.options.legacy) {
        const errors = validate(config).filter((d) => d.severity === 'error');
        if (errors.length > 0)
          this.#fail({ code: 'validation', detail: { diagnostics: errors } }, '');
      }
      if (config.revision !== this.active.revision)
        this.#fail(
          { code: 'revision_conflict', detail: { current: this.active.revision } },
          'Configuration changed',
        );
      this.previous.push(clone(this.active));
      this.active = { ...config, revision: this.active.revision + 1 };
      this.#pendingImport = null;
      if (this.safeMode) this.#leaveSafeMode();
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
      return this.#state();
    },
    set_paused: (args) => {
      const resume = args.paused === false;
      if (resume && this.options.legacy && this.recording)
        this.#fail({ code: 'busy' }, 'Stop recording before resuming automatic matching');
      // 0.4 (store set_manual_pause, ipc-contract §4): a resume is refused in safe mode and while
      // the health does not accept it. Recording no longer refuses it: its own pause reason keeps
      // the engine paused until it stops.
      if (resume && !this.options.legacy) {
        this.#guardSafeMode();
        const health = this.health.state;
        if (health !== 'healthy' && health !== 'degraded')
          this.#fail({ code: 'engine_unavailable', detail: { health } }, '');
      }
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
      // Refused in safe mode before anything else (src-tauri commands/engine.rs ensure_writable).
      this.#guardSafeMode();
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
      return args.kind === 'image'
        ? ['mock://示例图片.png']
        : [`mock://示例文件.${args.kind === 'import' ? this.options.importFormat : 'json'}`];
    },
    export_config: () => {
      if (this.options.dialog === 'cancel') {
        if (this.options.legacy) this.#fail({ code: 'cancelled' }, 'Export cancelled');
        return null;
      }
      return 'mock://quicktext-config.zip';
    },
    export_excel_template: () => {
      if (this.options.dialog === 'cancel') return null;
      if (this.safeMode && this.safeMode !== 'crash_loop')
        this.#fail({ code: 'safe_mode', detail: { reason: this.safeMode } }, '');
      return 'mock://quicktext-template.xlsx';
    },
    export_excel_config: () => {
      if (this.options.dialog === 'cancel') return null;
      if (this.safeMode && this.safeMode !== 'crash_loop')
        this.#fail({ code: 'safe_mode', detail: { reason: this.safeMode } }, '');
      return 'mock://quicktext-config.xlsx';
    },
    import_config: (args) => {
      const incoming = syntheticRules(2).map((rule, i) => ({
        ...rule,
        id: `mock-import-${i + 1}`,
        group: '导入示例',
      }));
      const config = clone(this.active);
      if (args.strategy === 'replace') config.rules = incoming;
      else if (args.strategy === 'overwrite') {
        for (const rule of incoming) {
          const index = config.rules.findIndex(
            (existing) => isReadableRule(existing) && existing.id === rule.id,
          );
          if (index < 0) config.rules.push(rule);
          else config.rules[index] = rule;
        }
      } else for (const rule of incoming) config.rules.push(rule);
      const token = `mock-import-${++this.#importSequence}`;
      this.#pendingImport = { json: JSON.stringify(config), token };
      return {
        config,
        token,
        report: [`2 条合成示例规则；策略：${String(args.strategy)}`, '0 conflicts'],
      };
    },
    preview_file: () => '浏览器预览：文件内容示例',
    preview_image: () =>
      'data:image/svg+xml,' +
      encodeURIComponent(
        '<svg xmlns="http://www.w3.org/2000/svg" width="160" height="90"><rect width="160" height="90" fill="#ddd"/></svg>',
      ),
    import_image_asset: (args) => ({
      asset: `${'0'.repeat(64)}.png`,
      name: String(args.path).split('/').pop() ?? '',
    }),
    preview_image_asset: () =>
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
    request_permissions: () => ({ granted: false, supported: true }),
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
      // Refused in safe mode (src-tauri commands/settings.rs ensure_writable), unlike set_auto_listen.
      this.#guardSafeMode();
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
      this.options.dialog === 'cancel' ? null : 'mock://quicktext-diagnostics.zip',
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
    // S-09: allowed in safe mode, unlike set_autostart (src-tauri commands/settings.rs,
    // ipc-contract §5): prefs live apart from what safe mode protects.
    set_auto_listen: (args) => {
      this.autoListen = args.enabled === true;
      return this.autoListen;
    },
    // Beginner typing hint: prefs, allowed in safe mode (src-tauri commands/settings.rs).
    set_candidate_hint: (args) => {
      const ms = args.duration_ms;
      if (typeof ms !== 'number' || ms < 1000 || ms > 10000)
        this.#fail(
          {
            code: 'validation',
            detail: {
              diagnostics: [
                {
                  code: 'candidate_hint_duration',
                  message: '显示时长须在 1 到 10 秒之间',
                  path: 'candidate_hint.duration_ms',
                  severity: 'error',
                },
              ],
            },
          },
          '',
        );
      this.candidateHint = { enabled: args.enabled === true, duration_ms: ms };
      return clone(this.candidateHint);
    },
    // There is no hint window in the browser preview.
    preview_candidate_hint: () => null,
    candidate_hint_measured: () => null,
    // Team phrases (docs/cloud-phrases.md §5); prefs too, so not refused in safe mode here.
    get_cloud_state: () => this.#cloudState(),
    set_cloud_settings: (args) => {
      const input = (args.settings ?? {}) as Partial<Omit<MockCloudPrefs, 'key'>> & {
        key?: string | null;
      };
      const baseUrl = typeof input.base_url === 'string' ? input.base_url.trim() : '';
      const minutes = input.auto_sync_minutes;
      const errors: Diagnostic[] = [];
      // §3: https only; http for the local development hosts.
      if (
        baseUrl &&
        !/^(https:\/\/|http:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?(\/|$))/.test(baseUrl)
      )
        errors.push(
          diagnostic(
            'invalid_url',
            '服务器地址必须以 https:// 开头（本机调试可用 http://localhost）',
            'settings.base_url',
          ),
        );
      // The backend's codes and messages (src-tauri/src/cloud/sync.rs `validate`).
      if (typeof input.key === 'string' && !/^[\x21-\x7e]*$/.test(input.key))
        errors.push(
          diagnostic(
            'invalid_key',
            '同步密钥只能包含英文字母、数字和符号，不能有空格或中文',
            'settings.key',
          ),
        );
      if (typeof minutes !== 'number' || !AUTO_SYNC_MINUTES.includes(minutes))
        errors.push(
          diagnostic(
            'invalid_interval',
            '自动同步间隔只能是 0（仅手动）、15、30、60、120 或 240 分钟',
            'settings.auto_sync_minutes',
          ),
        );
      if (errors.length > 0 || typeof minutes !== 'number')
        this.#fail({ code: 'validation', detail: { diagnostics: errors } }, '');
      this.cloudPrefs = {
        ...this.cloudPrefs,
        enabled: input.enabled === true,
        base_url: baseUrl,
        auto_sync_minutes: minutes,
        // Absent or null keeps the key; '' clears it.
        key: typeof input.key === 'string' ? input.key : this.cloudPrefs.key,
      };
      this.#pushCloud();
      // §3: a change of the settings syncs once right away; sync_cloud_now meanwhile joins it.
      if (this.cloudPrefs.enabled) void this.#syncCloud();
      return this.#cloudState();
    },
    sync_cloud_now: () => this.#syncCloud(),
    set_cloud_rules_disabled: (args) => {
      const ids = Array.isArray(args.ids)
        ? args.ids.filter((id): id is string => typeof id === 'string')
        : [];
      const disabled = new Set(this.cloudPrefs.disabled_ids);
      for (const id of ids) {
        if (args.disabled === true) disabled.add(id);
        else disabled.delete(id);
      }
      this.cloudPrefs = { ...this.cloudPrefs, disabled_ids: [...disabled] };
      this.#pushCloud();
      return this.#cloudState();
    },
    // In-app updates (src-tauri/src/commands/update.rs); camelCase arguments.
    app_version: () => MOCK_APP_VERSION,
    check_update: (args) => this.#checkUpdate(args.includePrerelease === true),
    download_update: (args) => this.#downloadUpdate(typeof args.tag === 'string' ? args.tag : ''),
    reveal_update: (args) => {
      if (typeof args.path !== 'string' || !this.updateDownloads.has(args.path))
        this.#fail(
          updateError('not_downloaded', '只能打开本次运行中下载的更新文件'),
          'reveal_update',
        );
      return null;
    },
  };

  // ---- internals ----

  #state() {
    const paused = this.engine.paused_reasons.length > 0;
    const legacy = {
      active: clone(this.active),
      revision: this.active.revision,
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
      candidate_hint: clone(this.candidateHint),
      health: clone(this.health),
      engine_state: clone(this.engine),
      startup_problems: clone(this.problems),
      safe_mode: this.safeMode,
    };
  }

  /** CloudState now, the statuses merged against the active config as §4 orders them. */
  #cloudState(): CloudState {
    const prefs = this.cloudPrefs;
    const table = this.cloudCache;
    const disabled = new Set(prefs.disabled_ids);
    const personal = this.active.rules.filter(isMockRule).filter((r) => r.enabled);
    const same = (a: MockRule, b: MockRule) =>
      a.match.kind === b.match.kind &&
      (a.ignore_case || b.ignore_case
        ? a.match.value.toLowerCase() === b.match.value.toLowerCase()
        : a.match.value === b.match.value);
    const rules = (table?.rules ?? []).map((rule): CloudRuleEntry => {
      const local = disabled.has(rule.id);
      const entry = { id: rule.id, rule: clone(rule), disabled_local: local };
      if (!rule.enabled) return { ...entry, status: 'disabled_remote' };
      if (local) return { ...entry, status: 'disabled_local' };
      const problem = cloudProblem(rule);
      if (problem !== null) return { ...entry, status: 'invalid', problem };
      // Only a phrase that runs takes the place of the personal rules with its abbreviation.
      const overrides = personal.filter((p) => same(p, rule)).map((p) => p.id);
      return overrides.length > 0
        ? { ...entry, status: 'active', overrides }
        : { ...entry, status: 'active' };
    });
    return {
      settings: {
        enabled: prefs.enabled,
        base_url: prefs.base_url,
        has_key: prefs.key !== '',
        auto_sync_minutes: prefs.auto_sync_minutes,
      },
      status: {
        ...clone(this.cloudStatus),
        revision: table?.revision ?? null,
        updated_at: table?.updated_at ?? null,
        unreadable_count: table?.unreadable ?? 0,
      },
      rules,
    };
  }

  /** Push cloud-state. It carries no seq: the runtime events' counter is not touched. */
  #pushCloud(): void {
    if (this.options.legacy) return;
    const handlers = this.#handlers.get(CLOUD_STATE_EVENT);
    if (!handlers?.size) return;
    const state = this.#cloudState();
    for (const handler of handlers) handler(clone(state));
  }

  #syncCloud(): Promise<CloudState> {
    this.#cloudSync ??= this.#runCloudSync().finally(() => {
      this.#cloudSync = null;
    });
    return this.#cloudSync;
  }

  /** One download (§3): on success the cache is replaced; on failure it stays, with the error. */
  async #runCloudSync(): Promise<CloudState> {
    const at = new Date().toISOString();
    this.cloudStatus = { ...this.cloudStatus, syncing: true, last_attempt_at: at };
    this.#pushCloud();
    const delay = this.options.cloudDelayMs;
    await (delay > 0 ? new Promise((resolve) => setTimeout(resolve, delay)) : Promise.resolve());
    const prefs = this.cloudPrefs;
    const error: CloudError | null =
      !prefs.enabled || !prefs.base_url || !prefs.key
        ? { code: 'not_configured', message: '未开启订阅，或缺少服务器地址、同步密钥' }
        : this.cloudServer.error;
    if (error) {
      this.cloudStatus = { ...this.cloudStatus, syncing: false, last_error: clone(error) };
    } else {
      const table = clone(this.cloudServer.table);
      this.cloudCache = table;
      // §4: ids no phrase has any more are dropped after a successful sync.
      const ids = new Set(table.rules.map((r) => r.id));
      this.cloudPrefs = {
        ...prefs,
        disabled_ids: prefs.disabled_ids.filter((id) => ids.has(id)),
      };
      this.cloudStatus = {
        syncing: false,
        last_attempt_at: at,
        last_success_at: at,
        last_error: null,
      };
    }
    this.#pushCloud();
    return this.#cloudState();
  }

  /** src-tauri/src/update.rs `evaluate` over the synthetic releases. */
  #checkUpdate(includePrerelease: boolean): UpdateCheck {
    if (this.options.update === 'error')
      this.#fail(
        updateError('network', '无法连接 GitHub，请检查网络连接（公司网络可能需要代理）后重试'),
        'check_update',
      );
    const include = includePrerelease;
    let latest: UpdateRelease | null = null;
    for (const release of mockReleases(this.options.update)) {
      const version = release.tag.slice(1);
      if (!release.tag.startsWith('v') || parseSemver(version) === null) continue;
      const prerelease = release.prerelease || isPrerelease(version);
      if (prerelease && !include) continue;
      if (latest && compareSemver(version, latest.version) <= 0) continue;
      latest = {
        version,
        tag: release.tag,
        prerelease,
        published_at: release.published_at,
        notes: release.notes,
        html_url: `https://github.com/meruchan0623/quick-text/releases/tag/${release.tag}`,
        asset: mockAsset(this.options.platform, version),
      };
    }
    return {
      current: MOCK_APP_VERSION,
      latest,
      update_available: latest !== null && compareSemver(latest.version, MOCK_APP_VERSION) > 0,
    };
  }

  /** Simulated progress in five steps, then a verified file in a synthetic Downloads folder. */
  async #downloadUpdate(tag: string): Promise<UpdateDownload> {
    if (this.#updateDownloading)
      this.#fail(updateError('busy', '正在下载更新，请等这次下载完成'), 'download_update');
    const release = mockReleases(this.options.update).find(
      (r) => r.tag === tag && r.tag.startsWith('v'),
    );
    if (!release)
      this.#fail(
        updateError('release_not_found', 'GitHub 上没有找到这个版本，它可能已被撤下'),
        'download_update',
      );
    const asset = mockAsset(this.options.platform, release.tag.slice(1));
    if (!asset)
      this.#fail(
        updateError('no_asset', '这个版本没有适用于本机系统的安装包，请到发布页查看'),
        'download_update',
      );
    this.#updateDownloading = true;
    try {
      const delay = this.options.updateDelayMs;
      for (let step = 0; step <= 5; step += 1) {
        if (step > 0)
          await (delay > 0
            ? new Promise((resolve) => setTimeout(resolve, delay))
            : Promise.resolve());
        this.#push(UPDATE_PROGRESS_EVENT, {
          tag,
          downloaded: Math.round((asset.size * step) / 5),
          total: asset.size,
        } satisfies UpdateProgress);
      }
    } finally {
      this.#updateDownloading = false;
    }
    const path =
      this.options.platform === 'windows'
        ? `C:\\Users\\mock\\Downloads\\${asset.name}`
        : `/Users/mock/Downloads/${asset.name}`;
    this.updateDownloads.add(path);
    return { path, sha256: '0'.repeat(64), verified: true };
  }

  /** Push an event outside the runtime stream (no seq), like cloud-state. */
  #push(event: string, payload: unknown): void {
    if (this.options.legacy) return;
    for (const handler of this.#handlers.get(event) ?? []) handler(clone(payload));
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

  /**
   * Safe mode ends (store set_safe_mode(None)): its pause reason and problem go. The engine is not
   * resumed by it; it stays paused (manually) until the user resumes listening.
   */
  #leaveSafeMode(): void {
    if (!this.safeMode) return;
    this.safeMode = null;
    this.problems = this.problems.filter((p) => p.kind !== 'safe_mode');
    this.#togglePause('safe_mode', false);
    if (!this.engine.paused_reasons.includes('manual')) this.#togglePause('manual', true);
    this.#emitState();
  }

  /** Push store-state when the revision or safe mode differs from the last push. */
  #emitStore(): void {
    const store = { revision: this.active.revision, safeMode: this.safeMode };
    if (store.revision === this.#store.revision && store.safeMode === this.#store.safeMode) return;
    this.#store = store;
    this.emit('store-state', { revision: store.revision, safe_mode: store.safeMode });
    // §5: 我的指令 changed, so did which of its rules a team phrase replaces.
    this.#pushCloud();
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
  const cloud = q.get('mock-cloud');
  if (cloud === 'off' || cloud === 'sample' || cloud === 'error') options.cloud = cloud;
  const update = q.get('mock-update');
  if (update === 'available' || update === 'none' || update === 'error') options.update = update;
  return options;
}

/** Transport for the browser preview, configured from the URL. */
export function createPreviewTransport(): Transport {
  // The preview shows the team phrases, synced, with a sync slow enough to be seen.
  const backend = new MockBackend({
    cloud: 'sample',
    cloudDelayMs: 800,
    updateDelayMs: 400,
    ...mockOptionsFromQuery(window.location.search),
  });
  (globalThis as { quickerMock?: MockBackend }).quickerMock = backend;
  return createMockTransport(backend);
}
