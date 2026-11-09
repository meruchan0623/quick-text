import { useEffect, useRef, useState } from 'react';
import { call, usesMock } from './ipc/transport';
import {
  errorText as describe,
  diagnosticText,
  isMissingCommand,
  parseCommandError,
} from './ipc/errors';
import {
  actionLabel,
  clone,
  completeConfig,
  Config,
  Data,
  emptyConfig,
  isRule,
  keyNames,
  keys,
  newRule,
  Rule,
  settingsHaveUnsafeNumber,
  State,
} from './model';
import ActionEditor from './ActionEditor';
import ExportMenu from './ExportMenu';
import CaptureTemplate from './CaptureTemplate';
import CloudPhrases, { CloudSettingsSection } from './CloudPhrases';
import { TerminatorList } from './TerminatorList';
import { useAsk } from './Ask';
import { Modal } from './Modal';
import { WindowFrame } from './WindowFrame';
import { cloudSync, runtimeSync, useCloud, useRuntime } from './ipc/events';
import type { SafeModeReason } from './ipc/types';
import { liveStatus } from './status';
import { BannerStack, HistoryList } from './StatusArea';
import UpdateSection, { AUTO_CHECK_DELAY_MS, updateAvailable, useUpdates } from './UpdateSection';
import { LinesInput } from './LinesInput';
import CandidateHintSection from './CandidateHintSection';
import { isCandidateHint } from './lib/hint';
import {
  countByGroup,
  diagnosticField,
  groupKey,
  pruneSelection,
  replaceReadable,
  ruleDiagnostics,
  ruleSearchText,
  summarizeAction,
  unknownRuleKeys,
  unreadableEntries,
} from './lib/rules';
import {
  ASSET_DOWNLOAD_ERROR,
  cloudRuleName,
  overridingPhrases,
  phraseCopy,
  type CloudRow,
} from './lib/cloud';
import { unsupportedTerminators } from './lib/terminators';
import { fixedImeErasure } from './lib/capabilities';
import { ruleEditDiagnostics } from './lib/numberFields';
import { describeMatch } from './lib/describe';
import { summaryText } from './lib/summary';
import { useTheme } from './theme';

// `null` from describe() means the user cancelled: show nothing.
const errorText = (e: unknown) => describe(e) ?? '';
// How many times one load_state is asked for while commands keep overtaking it (review C2).
const LOAD_ATTEMPTS = 3;
const OVERTAKEN = '读取期间状态一再变化，读到的内容已过时';
/** What the notice's 「重新读取」 says when it read, and when every load was overtaken. */
const REREAD = [
  '已重新读取配置',
  `${OVERTAKEN}，这次没有读取到。请再点一次「重新读取」。`,
] as const;
const NOT_READY = '还没有读取到当前配置，暂时不能修改指令和设置。';
const SAVE_PENDING = '上一次保存尚未完成，请稍候。';
const SAFE_MODE_CHANGED = '安全模式状态已变化，正在重新读取当前配置；读取完成前暂时不能修改。';
// A store revision this window has not read yet is read this many times in all, the retries
// RECONCILE_RETRY_MS apart and doubling, before a notice offers 「重新读取」 (review C5).
const RECONCILE_ATTEMPTS = 3;
const RECONCILE_RETRY_MS = 400;
const STALE_READ = '读到的仍是较早的版本';
/** A view shown of an older revision than the one caught up with (review C6). */
const stale = (shown: number | undefined, caught: number | null) =>
  shown !== undefined && caught !== null && shown < caught;
/** Commands that write the store and answer with the whole state. */
type StoreWrite = 'save_config';
/** A confirmed rule/list operation applied to the configuration. */
type Change = (c: Config) => Config;
/** The writes that publish a revision of their own, and how a notice about it names them. */
const MADE_BY = { save_config: '保存配置' } as const;
type Own = keyof typeof MADE_BY;
/** State fields a command answers alone, without the state around them. */
const SETTINGS = ['autostart', 'auto_listen_on_start'] as const;
type Setting = (typeof SETTINGS)[number];
function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="field">
      <span>{label}</span>
      {children}
    </label>
  );
}
const platformNames: Record<string, string> = {
  windows: 'Windows',
  macos: 'macOS',
  linux: 'Linux',
};
/**
 * Terminator checkboxes. Keys the platform cannot use are grayed out with a note (S-12); one that
 * an old config already has stays checked and can still be unchecked.
 */
function EndKeys({
  value,
  onChange,
  platform,
}: {
  value: string[];
  onChange: (v: string[]) => void;
  platform?: string | undefined;
}) {
  const keySet = new Set(value);
  const unsupported = unsupportedTerminators(platform);
  const stale = unsupported.some((k) => keySet.has(k));
  return (
    <div className="inline end-keys">
      {keys.map((k) => {
        const off = unsupported.includes(k);
        return (
          <label key={k} className={off ? 'unsupported' : undefined}>
            <input
              type="checkbox"
              checked={keySet.has(k)}
              disabled={off && !keySet.has(k)}
              onChange={(e) =>
                onChange(e.target.checked ? [...value, k] : value.filter((i) => i !== k))
              }
            />
            {keyNames[k]}
          </label>
        );
      })}
      {unsupported.length > 0 && (
        <small className="muted">
          {unsupported.map((k) => keyNames[k] ?? k).join('、')}：
          {platformNames[platform ?? ''] ?? platform} 上暂不支持
          {stale && '；已勾选的不会触发指令，可以取消勾选'}
        </small>
      )}
    </div>
  );
}

export default function App({
  updateCheckDelayMs = usesMock ? null : AUTO_CHECK_DELAY_MS,
}: {
  /**
   * When 「自动检查更新」 checks after start; null never (the mock backend of the browser preview
   * and the tests, which pass a delay to see it).
   */
  updateCheckDelayMs?: number | null;
}) {
  const [state, setState] = useState<State | null>(null),
    [config, setConfig] = useState<Config>(emptyConfig),
    [tab, setTab] = useState<'manage' | 'settings'>('manage'),
    [theme, setTheme] = useTheme(),
    [search, setSearch] = useState(''),
    [group, setGroup] = useState('全部'),
    [selected, setSelected] = useState<string[]>([]),
    [editing, setEditingState] = useState<Rule | null>(null),
    [busy, setBusy] = useState(false),
    [message, setMessage] = useState('正在连接桌面引擎…'),
    [error, setError] = useState(''),
    [dialog, setDialog] = useState<{
      title: string;
      data: any;
      importConfig?: Config;
      importToken?: string;
    } | null>(null),
    [localDirty, setLocalDirty] = useState(false),
    [autoListenMissing, setAutoListenMissing] = useState(false);
  const [editingErrors, setEditingErrors] = useState<Data[]>([]);
  // The import's save in flight, from 「确认导入并生效」 until it is done or failed: its dialog
  // cannot be closed meanwhile (review C10, see there).
  const [importing, setImporting] = useState(false);
  // Clear the editor's validation errors when another rule is opened.
  const [errorsFor, setErrorsFor] = useState(editing?.id);
  if (errorsFor !== editing?.id) {
    setErrorsFor(editing?.id);
    setEditingErrors([]);
  }
  const { askDialog, confirmAction, askValue } = useAsk();
  // Health, engine state and history arrive as events (S-19); no polling.
  const sync = runtimeSync();
  const updates = useUpdates(updateCheckDelayMs);
  const runtime = useRuntime(sync);
  const live = liveStatus(runtime);
  // Team phrases (docs/cloud-phrases.md): a table of their own, pushed with cloud-state. They are
  // not part of the config: nothing of theirs goes through `write`, only a copy into 我的指令 does.
  const cloudSyncer = cloudSync();
  const cloud = useCloud(cloudSyncer);
  const [manageView, setManageView] = useState<'mine' | 'team'>('mine');
  // 「去设置」 from the 团队话术 page focuses 「团队话术同步」 there, until another page is shown.
  const [cloudFocus, setCloudFocus] = useState(false);
  if (cloudFocus && tab !== 'settings') setCloudFocus(false);
  // Without a subscription the engine runs no team phrase, so none is counted or takes a rule's place.
  const subscribed = cloud.state?.settings.enabled ? cloud.state : null;
  const editingCurrent = useRef<Rule | null>(null);
  const setEditing = (value: Rule | null) => {
    editingCurrent.current = value;
    setEditingState(value);
  };
  const edited = useRef(false);
  // Counts every local edit (`change`), so a write can tell whether any came after it was sent.
  const edits = useRef(0);
  const modeCache = useRef(
    new Map<
      string,
      { match: Rule['match']; terminator: Rule['terminator']; parameter: Rule['parameter'] }
    >(),
  );
  const changeMode = (value: string) => {
    if (!editing) return;
    if (value === 'immediate') {
      modeCache.current.set(
        editing.id,
        clone({
          match: editing.match,
          terminator: editing.terminator,
          parameter: editing.parameter,
        }),
      );
      setEditing({
        ...editing,
        trigger_mode: value,
        match: { kind: 'literal', value: editing.match.value },
        terminator: null,
        parameter: { type: 'default_match' },
      });
    } else {
      const cache = modeCache.current.get(editing.id);
      setEditing({ ...editing, trigger_mode: value, ...(cache ?? {}) });
    }
  };
  // The revision this window's unconfirmed settings are based on.
  const editorRevision = useRef<number | null>(null);
  const [newer, setNewer] = useState<{ revision: number; by: Own | null } | null>(null);
  // Revisions published by this window identify delayed answers and their own runtime events.
  const made = useRef(new Map<number, Own>());
  const madeBy = (revision: number) => made.current.get(revision) ?? null;
  // The state shown, as soon as it is set: a write whose answer was not installed puts what is
  // shown in the editor (review C7), maybe before the render that shows it.
  const shown = useRef<State | null>(null);
  const show = (s: State) => {
    shown.current = s;
    setState(s);
  };
  // So is the editor's config: a save's change is reconciled after a publication, and
  // an edit made after an await changes the config as it is then (`change`).
  const editorConfig = useRef(config);
  const putConfig = (c: Config) => {
    editorConfig.current = c;
    setConfig(c);
  };
  const receive = (s: State) => {
    show(s);
    // A rule field with a serde default may be absent; the editor reads it filled in, as Rust does.
    const active = completeConfig(s.active);
    putConfig(active);
    // Rules removed by a delete or import must not stay selected (F-04).
    setSelected((current) => pruneSelection(current, active.rules));
    setLocalDirty(false);
    edited.current = false;
    editorRevision.current = s.revision;
  };
  /** Say a store revision newer than the one the editor's unsaved edits are based on. */
  const sayNewer = (revision: number) => {
    // Unsaved local edits are never replaced by an answer. When the store moved on under them, say
    // so; the user loads the new config or keeps editing with the original expected revision.
    const base = editorRevision.current;
    if (base !== null && revision > base) {
      const by = madeBy(revision);
      setNewer((n) => (n !== null && n.revision >= revision ? n : { revision, by }));
    }
  };
  // Install by monotonic revision first and request order second. A delayed response may
  // complete its caller, but cannot replace a later authoritative state.
  const requests = useRef(0);
  const installed = useRef({ revision: -1, request: 0 });
  // set_autostart and set_auto_listen answer with the value alone, without a revision; it is
  // ordered by request id against the view that set it (review C2). A state answer sent before
  // such a command keeps the value it answered (a load_state is asked again instead, see `load`);
  // one sent after it takes over.
  const settings = useRef<Record<Setting, { request: number; value: boolean } | null>>({
    autostart: null,
    auto_listen_on_start: null,
  });
  /** The view to show for state answer `s` to request `request`, or null when it is not installed. */
  const install = (s: State, request: number): State | null => {
    const last = installed.current;
    if (s.revision < last.revision || (s.revision === last.revision && request <= last.request))
      return null;
    installed.current = { revision: s.revision, request };
    let view = s;
    for (const field of SETTINGS) {
      const setting = settings.current[field];
      if (setting === null) continue;
      if (setting.request > request) view = { ...view, [field]: setting.value };
      else settings.current[field] = null;
    }
    return view;
  };
  /** Show what set_autostart or set_auto_listen (request `request`) answered, unless overtaken. */
  const installSetting = (field: Setting, value: boolean, request: number) => {
    if (request <= (settings.current[field]?.request ?? installed.current.request)) return;
    settings.current[field] = { request, value };
    if (shown.current) show({ ...shown.current, [field]: value });
  };
  // Last revision and safe mode read by this window; runtime events reconcile through load_state.
  const [reconciled, setReconciled] = useState<number | null>(null);
  const reconciledRef = useRef<number | null>(null);
  const [reconciledSafe, setReconciledSafe] = useState<SafeModeReason | null>(null);
  const reconciledSafeRef = useRef<SafeModeReason | null>(null);
  const reconcileTo = (revision: number, safeMode: SafeModeReason | null) => {
    const caught = reconciledRef.current;
    if (caught !== null && caught > revision) return;
    reconciledSafeRef.current = safeMode;
    setReconciledSafe(safeMode);
    if (caught === revision) return;
    reconciledRef.current = revision;
    setReconciled(revision);
  };
  /**
   * The safe mode now, read as `safeMode` below is but from the sync view and the ref, which are
   * never behind: the handlers check it against `reconciledSafeRef` (review I6).
   */
  const currentSafeMode = (): SafeModeReason | null => {
    const view = sync.getView();
    return view.status === 'ready' ? view.safeMode : (shown.current?.safe_mode ?? null);
  };
  // This window's writes of the store in flight. The store-state a write causes may arrive before
  // its answer; the answer carries the new revision, so no reload is needed for it (see below).
  const [writing, setWriting] = useState(0);
  // All configuration publications are serialized for one IPC round trip.
  const changePending = useRef(false);
  const [changing, setChanging] = useState(false);
  /** Publish once; delayed answers never replace a newer installed state or later input. */
  const write = async (command: StoreWrite, args?: Record<string, unknown>, op?: Change) => {
    if (changePending.current) throw new Error(SAVE_PENDING);
    if (reconciledSafeRef.current !== currentSafeMode()) throw new Error(SAFE_MODE_CHANGED);
    changePending.current = true;
    setChanging(true);
    setWriting((n) => n + 1);
    const request = ++requests.current;
    const sent = edits.current;
    // Rule/list saves must preserve, but do not publish, unconfirmed settings inputs.
    const keepSettings = op !== undefined && edited.current;
    try {
      const s = await call<State>(command, args);
      made.current.set(s.revision, command);
      const view = install(s, request);
      if (view !== null) {
        show(view);
        reconcileTo(s.revision, s.safe_mode ?? null);
      }
      const now = shown.current;
      if (now === null) return s;
      if (edits.current === sent && !keepSettings) {
        receive(now);
        setNewer(null);
      } else if ((editorRevision.current ?? -1) <= s.revision) {
        // Keep settings typed before an unrelated write or after this request. Their next
        // confirmation still compares against the revision this publication actually made.
        const globals = editorConfig.current.globals;
        const next = { ...completeConfig(s.active), globals };
        putConfig(next);
        editorRevision.current = s.revision;
        setSelected((current) => pruneSelection(current, next.rules));
        setLocalDirty(true);
        edited.current = true;
        setNewer(null);
        if (now.revision > s.revision) sayNewer(now.revision);
      }
      return s;
    } catch (error) {
      const parsed = parseCommandError(error);
      if (parsed.code === 'revision_conflict') {
        sayNewer(parsed.detail.current);
        // Read the latest authoritative state, retaining any unconfirmed settings input.
        void load().catch(() => undefined);
      }
      throw error;
    } finally {
      setWriting((n) => n - 1);
      changePending.current = false;
      setChanging(false);
    }
  };
  // Until the first load_state snapshot is applied, `config` is emptyConfig, not the user's: a write
  // would replace their active configuration with it. Every write is disabled until then (`ready`) and
  // refused again by its handler (`requireReady`, read from the ref, which is never behind).
  const loaded = useRef(false);
  const [ready, setReady] = useState(false);
  // Why the first load_state did not apply; shown with 「重试」 until one does.
  const [loadFailure, setLoadFailure] = useState('');
  const requireReady = () => {
    if (!loaded.current) throw new Error(NOT_READY);
  };
  // Safe mode as soon as it is known: the runtime view follows store-state; until its first
  // snapshot, or from a backend without one, load_state's field stands in.
  const safeMode: SafeModeReason | null =
    runtime.status === 'ready' ? runtime.safeMode : (state?.safe_mode ?? null);
  // The writes safe mode refuses (ipc-contract §4: all but importing) are disabled with it, and
  // enabled again when it ends. Import, confirming the import and 保存配置 stay, as committing and
  // publishing an import is the way out of CrashLoop and NoValidConfig; so does 导出 (allowed in
  // CrashLoop). The backend decides: the handlers do not check safe mode, and a write made before
  // the event arrives is refused there with the reason.
  // Review I6: a change of safe mode makes the editor's view stale (see `reconciledSafe`). Until
  // the read it causes installs a view of the safe mode now, every config write stays disabled,
  // those safe mode allows too (`safeModeRead`), and `write` refuses; an edit needs a view read
  // outside safe mode as well.
  const safeModeRead = reconciledSafe === safeMode;
  const editable = ready && safeMode === null && reconciledSafe === null;
  // The entries that send a store write: not while a save's change is in flight (`changePending`).
  // 导入 and the rule editor only open; what they save is sent by an entry disabled meanwhile.
  const writable = editable && !changing;
  /** Apply a load_state snapshot; the first one initializes the editor and unlocks the writes. */
  const applySnapshot = (s: State) => {
    reconcileTo(s.revision, s.safe_mode ?? null);
    if (loaded.current) {
      if (!edited.current) {
        receive(s);
        // A later revision is in the editor now: a notice about an earlier one is moot.
        setNewer((n) => (n !== null && s.revision > n.revision ? null : n));
      } else {
        show(s);
        sayNewer(s.revision);
      }
      return;
    }
    receive(s);
    loaded.current = true;
    setReady(true);
    setLoadFailure('');
    setMessage('已连接');
  };
  /**
   * load_state, applied; resolves whether it was. Its answer is installed by the rule above. One
   * overtaken by a command's answer (installed over it, or a setting answered since it was sent)
   * is not applied but asked for again, at most LOAD_ATTEMPTS times in all: the fields only
   * load_state carries would otherwise stay stale until the next focus. A load sent after a command
   * answered reads that command's change, so the next answer applies. The check and the apply run
   * in one step after the answer arrives, with nothing in between.
   *
   * Loads overlap (start, focus, 「重新读取」, a revision read). One not installed after a later one
   * was is dropped, and resolves as applied, the later one standing for it (review C5).
   */
  const lastLoad = useRef(0);
  const load = async (): Promise<boolean> => {
    for (let attempt = 0; attempt < LOAD_ATTEMPTS; attempt++) {
      const request = ++requests.current;
      const s = await call<State>('load_state');
      const overtaken = SETTINGS.some((field) => (settings.current[field]?.request ?? 0) > request);
      const view = overtaken ? null : install(s, request);
      if (view !== null) {
        lastLoad.current = Math.max(lastLoad.current, request);
        applySnapshot(view);
        return true;
      }
      if (request < lastLoad.current) return true;
    }
    // Overtaken every time: keep what the commands answered; the caller says so.
    return false;
  };
  /**
   * The first load, at start and on 「重试」: ends initialized or with a retryable failure. A
   * failure shown stays until replaced: by nothing on success, or by the new failure.
   */
  const loadFirst = async () => {
    let failure = OVERTAKEN;
    try {
      if (await load()) return;
    } catch (e) {
      failure = errorText(e) || '读取被中断';
    }
    // A reload (window focus) may have initialized meanwhile.
    if (loaded.current) return;
    setLoadFailure(failure);
    setMessage('未能读取当前配置');
  };
  const run = async (fn: () => Promise<any>, success?: string) => {
    setBusy(true);
    setError('');
    try {
      const r = await fn();
      if (success) setMessage(success);
      return r;
    } catch (e) {
      setError(errorText(e));
      return null;
    } finally {
      setBusy(false);
    }
  };
  // Once, at start; 「重试」 calls loadFirst itself.
  const loadFirstRef = useRef(loadFirst);
  useEffect(() => {
    void loadFirstRef.current();
  }, []);
  // The legacy load_state fields (target observation, capabilities, configuration) have no event:
  // reload them when the window regains focus and on 「重新读取」. Unsaved local edits are kept.
  const reload = load;
  const reloadRef = useRef(reload);
  useEffect(() => {
    reloadRef.current = reload;
  });
  useEffect(() => {
    const onFocus = () => {
      reloadRef.current().catch(() => {
        // Keep showing the last known state; 「重新读取」 reports errors and an overtaken load.
      });
    };
    window.addEventListener('focus', onFocus);
    return () => window.removeEventListener('focus', onFocus);
  }, []);
  // cloud-state has no seq to tell a missed push by: read the team phrases again on focus too.
  useEffect(() => {
    const onFocus = () => void cloudSyncer.refresh();
    window.addEventListener('focus', onFocus);
    return () => window.removeEventListener('focus', onFocus);
  }, [cloudSyncer]);
  // store-state moved the revision (runtime.revision, the target): a change this window did not
  // make (another window, a background Locked retry) is read with load_state until `reconciled`
  // reaches it (review C5). Read once the first load has applied and while no write of this window
  // is in flight (its answer carries the revision it made, so its own store-state is caught up with
  // and nothing is read), one read at a time: a target moving meanwhile is read after it. A read
  // that fails, is overtaken every time or reaches less than the target it was sent for is retried
  // after a pause; after RECONCILE_ATTEMPTS a notice says so until a load catches up (「重新读取」,
  // focus) or a write of this window makes a revision as new. Never a loop: each read is sent for
  // a target and either reaches it or counts as failed. A backend without store-state moves
  // runtime.revision only with a snapshot. A view older than `reconciled` (the install rule never
  // shows one) counts as behind too: read again, and say so if that fails, never silently (C6).
  //
  // Review I6: the target is the runtime's safe mode as well. A view caught up with at the target
  // revision but read in another safe mode (Locked's placeholder, then the recovered config at the
  // same revision) is behind too, and a read reaches the target only with the safe mode. Compared
  // at the target revision alone: while the runtime has not yet seen a revision this window's own
  // write made, its safe mode is of an earlier one, and is compared when the runtime catches up.
  const target = runtime.revision;
  const targetSafe = runtime.status === 'ready' ? runtime.safeMode : null;
  const behind =
    ready &&
    target !== null &&
    (reconciled === null ||
      reconciled < target ||
      (reconciled === target && reconciledSafe !== targetSafe) ||
      stale(state?.revision, reconciled));
  // Why the last read for the target failed, once the retries ran out; cleared when caught up.
  const [readFailure, setReadFailure] = useState<string | null>(null);
  if (readFailure !== null && !behind) setReadFailure(null);
  const reconciling = useRef<{
    reading: boolean;
    failures: number;
    retry: ReturnType<typeof setTimeout> | undefined;
  }>({ reading: false, failures: 0, retry: undefined });
  const [reconcileTick, setReconcileTick] = useState(0);
  useEffect(() => {
    const r = reconciling.current;
    if (!behind) {
      r.failures = 0;
      return;
    }
    if (writing > 0 || r.reading || r.retry !== undefined || r.failures >= RECONCILE_ATTEMPTS)
      return;
    r.reading = true;
    void reloadRef
      .current()
      .then(
        (applied) => (applied ? null : OVERTAKEN),
        (e: unknown) => errorText(e) || '读取被中断',
      )
      .then((failure) => {
        r.reading = false;
        const caught = reconciledRef.current;
        const reached =
          caught !== null &&
          caught >= target &&
          !(caught === target && reconciledSafeRef.current !== targetSafe) &&
          !stale(installed.current.revision, caught);
        if (failure === null && reached) r.failures = 0;
        else {
          r.failures += 1;
          if (r.failures >= RECONCILE_ATTEMPTS) setReadFailure(failure ?? STALE_READ);
          else
            r.retry = setTimeout(
              () => {
                r.retry = undefined;
                setReconcileTick((t) => t + 1);
              },
              RECONCILE_RETRY_MS * 2 ** (r.failures - 1),
            );
        }
        // Look again: the target may have moved on, or not been reached.
        setReconcileTick((t) => t + 1);
      });
  }, [behind, target, targetSafe, writing, reconcileTick]);
  useEffect(() => {
    const r = reconciling.current;
    return () => clearTimeout(r.retry);
  }, []);
  // The revision the notice says could not be read.
  const unread = readFailure !== null && behind ? target : null;
  /** The notice's 「重新读取」. */
  const reread = () => {
    const [done, overtaken] = REREAD;
    return run(async () => {
      const applied = await reload();
      if (runtime.status !== 'unsupported') await sync.resync();
      if (!applied) throw new Error(overtaken);
    }, done);
  };
  const repair = () =>
    run(async () => {
      await call('repair_engine');
      await sync.resync();
    }, '已开始修复引擎，结果会自动更新');
  // macOS: the system prompt shows once at most; the button asks again and opens the privacy pane.
  const requestPermissions = () =>
    run(() => call('request_permissions'), '已打开系统设置，授权后会自动开始监听');
  const togglePause = () =>
    run(async () => {
      const manual = live ? live.manualPause : !!state?.paused;
      // set_paused only toggles the manual reason; the unconfirmed settings input in the editor stays untouched.
      const request = ++requests.current;
      const view = install(await call<State>('set_paused', { paused: !manual }), request);
      if (view !== null) show(view);
    });
  const openDataDir = () => run(() => call('open_data_dir'));
  const platform: string | undefined = state?.platform ?? state?.engine_detail?.platform;
  // S-09: a 0.3 backend has no such setting and load_state leaves the field out; the toggle is hidden.
  const autoListen: boolean | null =
    typeof state?.auto_listen_on_start === 'boolean' ? state.auto_listen_on_start : null;
  const setAutoListen = (enabled: boolean) =>
    run(async () => {
      requireReady();
      try {
        const request = ++requests.current;
        installSetting(
          'auto_listen_on_start',
          await call<boolean>('set_auto_listen', { enabled }),
          request,
        );
      } catch (e) {
        // A backend that reports the setting but lacks the command: say so once, then disable.
        if (isMissingCommand(e)) setAutoListenMissing(true);
        throw e;
      }
    });
  const setAutostart = (enabled: boolean) =>
    run(async () => {
      requireReady();
      // Show what the backend set (Rust returns the resulting value), not the request.
      const request = ++requests.current;
      const value = await call<unknown>('set_autostart', { enabled });
      installSetting('autostart', typeof value === 'boolean' ? value : enabled, request);
    });
  // The rule as opened, to tell whether closing the editor would lose changes (F-04).
  const editingBase = useRef('');
  const editingRevision = useRef<number | null>(null);
  // Settings not offered for new rules, shown when the rule had them as opened: they stay on screen
  // for the whole edit, so clearing or switching one off can be taken back.
  const [editingShown, setEditingShown] = useState({ regex: false, ime: false, parameter: false });
  const openEditor = (rule: Rule) => {
    editingBase.current = JSON.stringify(rule);
    editingRevision.current = editorRevision.current;
    setEditingShown({
      regex: rule.match.kind === 'regex',
      ime: rule.ime_erasure !== null,
      parameter: rule.parameter.type !== 'default_match',
    });
    setEditing(rule);
  };
  const closeEditor = async () => {
    if (
      editing &&
      JSON.stringify(editing) !== editingBase.current &&
      !(await confirmAction({
        title: '放弃未保存的改动？',
        detail: '这条指令的改动还没有保存，关闭后会丢失。',
        confirm: '放弃改动',
      }))
    )
      return;
    setEditing(null);
  };
  // A local edit: the globals, given whole. Local edits change nothing else; the rules and 总开关
  // change by saves alone (a rule committed, a list changed, 总开关, an import), whose change is made
  // to the authoritative active revision while settings edits remain local. That is exact only because
  // the two change different fields: should an edit come to change the rules or 总开关, think the
  // replay through again.
  const change = (globals: Config['globals']) => {
    // The settings fields are disabled through their fieldset, which React does not see: an input
    // event still reaches onChange. Before the first snapshot there is nothing to edit yet; nor in
    // safe mode, or on a view read in it (review I6): no edit is made then, so the view read when
    // it ends is taken in whole (`applySnapshot`, the editor not `edited`).
    if (!loaded.current || reconciledSafeRef.current !== null || currentSafeMode() !== null) return;
    putConfig({ ...editorConfig.current, globals });
    setLocalDirty(true);
    edited.current = true;
    edits.current += 1;
  };
  // A list/rule change publishes against the editor's base revision while settings inputs
  // remain local until their own save. Import confirmation uses the preview's exact config.
  const save = async (op?: Change, expectedRevision = editorRevision.current) => {
    requireReady();
    const now = editorConfig.current;
    const base = op && shown.current ? { ...now, globals: shown.current.active.globals } : now;
    const next = op ? op(base) : base;
    return write(
      'save_config',
      { config: { ...next, revision: expectedRevision ?? next.revision } },
      op,
    );
  };
  const selectedSet = new Set(selected);
  // Rules the UI can show and edit. Any other entry (one the backend could not read, B-43, or one
  // this UI cannot edit safely even if the backend runs it) is listed read-only and written back in
  // place as the value received, which within the IPC number contract (docs/ipc-contract-0.4.md
  // §6) is the same value; so are keys this version does not know.
  const readable = config.rules.filter(isRule);
  const unreadable = unreadableEntries(config.rules);
  const ruleNames = new Map<string, string>(readable.map((r) => [r.id, r.match.value || r.id]));
  // A team phrase runs as `cloud:<id>` (§4); its name comes from the phrase table.
  const ruleName = (id: string) => ruleNames.get(id) ?? cloudRuleName(id, cloud.state) ?? id;
  // 我的指令 rules that an active team phrase takes the place of (「被团队话术覆盖」).
  const overridden = overridingPhrases(subscribed);
  // An empty group is listed, counted and filtered as 「默认」 (F-04).
  const groupCounts = countByGroup(config.rules);
  const groups = ['全部', ...groupCounts.keys()];
  const needle = search.toLowerCase();
  const rows = readable
    .filter(
      (r) =>
        (group === '全部' || groupKey(r.group) === group) &&
        ruleSearchText(r).toLowerCase().includes(needle),
    )
    .sort((a, b) => a.match.value.localeCompare(b.match.value));
  const unreadableRows =
    group === '全部'
      ? unreadable.filter((u) => `无法读取的规则 ${u.id ?? ''}`.toLowerCase().includes(needle))
      : [];
  // Diagnostics refer to the whole config; split out the ones about the rule being edited.
  const indexOfRule = (id: string) => config.rules.findIndex((r) => isRule(r) && r.id === id);
  const editingFound = editing ? indexOfRule(editing.id) : -1;
  const editingIndex = editingFound >= 0 ? editingFound : config.rules.length;
  const editingDiagnostics = ruleDiagnostics(editingErrors, editingIndex);
  // Warnings about other rules (an unreadable entry, say) are not this edit's business.
  const editingFieldErrors = [
    ...editingDiagnostics.rule,
    ...editingDiagnostics.other.filter((d) => d.severity !== 'warning'),
  ];
  // Kept in place on save. Their numbers are within the IPC number contract (a rule holding one
  // past it is unreadable and never reaches the editor), so their values stay the same.
  const editingExtraKeys = editing ? unknownRuleKeys(editing) : [];
  // `fn` may run again, on the rules then (see `save`): it sets what the click chose, from values
  // taken at the click.
  const mutate = async (fn: (rs: Rule[]) => Rule[]) =>
    run(
      () =>
        save((c) => ({
          ...c,
          rules: replaceReadable(c.rules, fn(clone(c.rules.filter(isRule)))),
        })),
      '指令已保存并立即生效',
    );
  /**
   * 「复制到我的指令」: a new rule (its defaults, a new id, enabled) with only what `phraseCopy`
   * lets through of a team phrase: fixed text or a picture, never the phrase as it came from the server. Checked
   * here, where it is written, whatever the button offered. Saved into 我的指令 like any rule change
   * (`save`: the same readiness, one write at a time, the revision checked). While the phrase is
   * active the team phrase wins over its copy (§4 overrides): the copy runs once the phrase is
   * switched off on this computer, and the message says so.
   */
  const copyPhrase = (row: CloudRow) => {
    const { copy, reason } = phraseCopy(row);
    // Taken now: the change may be made again later (see `save`), with the same copy.
    const rule: Rule | null = copy && { ...newRule(platform), ...clone(copy) };
    return run(
      async () => {
        if (rule === null) throw new Error(reason ?? '这条团队话术不能复制。');
        await save((c) => ({ ...c, rules: [...c.rules, rule] }));
      },
      row.entry.status === 'active'
        ? `已复制「${copy?.match.value ?? ''}」到我的指令；团队话术那条启用期间以团队话术为准`
        : `已复制「${copy?.match.value ?? ''}」到我的指令`,
    );
  };
  // Team phrase commands write prefs, not the config: none of `write` applies. Nor do they hold
  // `busy`: a sync may run to its 15-second timeout, and pausing must stay possible meanwhile.
  const cloudRun = async (fn: () => Promise<void>) => {
    setError('');
    try {
      await fn();
    } catch (e) {
      setError(errorText(e));
    }
  };
  const syncPhrases = () =>
    cloudRun(async () => {
      const s = await cloudSyncer.syncNow();
      setMessage(
        s.status.last_error
          ? s.status.last_error.code === ASSET_DOWNLOAD_ERROR
            ? '团队话术已同步，但有图片没能下载，将在下次同步时重试'
            : '团队话术同步失败，仍使用上次同步的内容'
          : `团队话术已同步${s.status.revision === null ? '' : ` · v${s.status.revision}`}`,
      );
    });
  const disablePhrases = (ids: string[], disabled: boolean) =>
    cloudRun(async () => {
      await cloudSyncer.setDisabled(ids, disabled);
      setMessage(`已在本机${disabled ? '停用' : '启用'} ${ids.length} 条团队话术`);
    });
  const openCloudSettings = () => {
    setCloudFocus(true);
    setTab('settings');
  };
  // 「我的指令 | 团队话术」 above both lists; not offered by a backend without team phrases.
  const teamView = manageView === 'team' && cloud.status !== 'unsupported';
  const viewSwitch = cloud.status !== 'unsupported' && (
    <div className="view-switch" role="group" aria-label="指令来源">
      <button
        className={teamView ? '' : 'active'}
        aria-pressed={!teamView}
        onClick={() => setManageView('mine')}
      >
        我的指令<small>{config.rules.length}</small>
      </button>
      <button
        className={teamView ? 'active' : ''}
        aria-pressed={teamView}
        onClick={() => setManageView('team')}
      >
        团队话术{subscribed && <small>{subscribed.rules.length}</small>}
      </button>
    </div>
  );
  const choose = async (kind = 'file') => call<string[] | null>('choose_file', { kind });
  const exportData = (format: 'zip' | 'xlsx' | 'template' = 'zip') =>
    run(async () => {
      requireReady();
      const path = await call<string | null>(
        format === 'template'
          ? 'export_excel_template'
          : format === 'xlsx'
            ? 'export_excel_config'
            : 'export_config',
      );
      // null: the save dialog was cancelled; say nothing (ipc-contract §4).
      if (path) setMessage(`已导出：${path}`);
    });
  const importData = () =>
    run(async () => {
      requireReady();
      const p = await choose('import');
      if (!p?.length) return;
      const strategy = await askValue({
        title: '遇到重复的指令时怎么处理？',
        detail:
          '文件可以是 ZIP 配置包、JSON 或 Excel（.xlsx）。用文件更新会替换编号相同的指令并保留其他指令；替换全部会移除文件中没有的指令。',
        confirm: '导入',
        input: {
          label: '重复的指令',
          value: 'merge',
          options: [
            { value: 'merge', label: '保留现有指令，只添加新的' },
            { value: 'overwrite', label: '用文件中的内容更新' },
            { value: 'replace', label: '替换全部指令' },
          ],
        },
      });
      if (!strategy) return;
      const r = await call('import_config', { path: p[0], strategy });
      if (!r) return; // cancelled
      setDialog({
        title: '导入预览 · 确认后立即生效',
        data: r.report,
        importConfig: r.config,
        importToken: r.token,
      });
      setMessage('请核对导入报告后确认');
    });
  // 关闭, ✕ and Esc; ignored while the import's save is in flight (`importing`).
  const closeReport = () => {
    if (!importing) setDialog(null);
  };
  const reject = (diagnostics: Data[]) => {
    setEditingErrors(diagnostics);
    return new Error('有设置需要修改，请查看编辑框中的提示；已填写的内容会保留。');
  };
  // The editor's own checks (src/lib/numberFields.ts) come before anything leaves it: a number
  // field holding no number its Rust type and validation accept (NaN marks invalid text), or a
  // shape the backend would not read as meant. Only the edited rule is checked; unreadable
  // entries are written back as received, untouched by it.
  const editProblems = (rule: Rule) => ruleEditDiagnostics(rule, editingIndex);
  const commitEdit = () =>
    run(async () => {
      if (!editing) return;
      const problems = editProblems(editing);
      if (problems.length > 0) throw reject(problems);
      const rule = editing;
      // Put in place by id, on the config given (see `save`).
      const commit: Change = (c) => {
        const found = c.rules.findIndex((r) => isRule(r) && r.id === rule.id);
        return {
          ...c,
          rules: found >= 0 ? c.rules.map((r, i) => (i === found ? rule : r)) : [...c.rules, rule],
        };
      };
      const v = await call('validate_config', { config: commit(editorConfig.current) });
      if (!v.valid) throw reject(v.errors);
      try {
        const saved = await save(commit, editingRevision.current);
        editingRevision.current = saved.revision;
      } catch (e) {
        // save_config validates too (B-42); its diagnostics go to the same place.
        const error = parseCommandError(e);
        if (error.code === 'validation' && error.detail.diagnostics.length > 0)
          throw reject(error.detail.diagnostics);
        throw e;
      }
      if (JSON.stringify(editingCurrent.current) === JSON.stringify(rule)) {
        setEditing(null);
        setMessage('指令已保存并立即生效');
      } else {
        editingBase.current = JSON.stringify(rule);
        setMessage('已保存的内容立即生效；保存期间新增的修改尚未保存');
      }
    });
  // What is in effect, from the state the backend answered: not the editor's unsaved copy.
  const summary = summaryText({
    config: state?.active ?? null,
    cloud: cloud.state,
    loaded: ready,
    safeMode,
  });
  const settingsOpen = tab === 'settings';
  const hasUpdate = updateAvailable(updates);
  return (
    <div className="app">
      <header>
        <div className="brand" aria-hidden="true">
          <span className="brand-logo" />
          Quicktext
        </div>
        <div className="header-status">
          <span role="status">
            <span
              aria-hidden="true"
              className={`dot ${live ? live.kind : state?.paused ? 'paused' : ''}`}
            />
            {live ? live.text : state ? (state.paused ? '引擎已暂停' : '引擎运行中') : '未连接'}
          </span>
          {live?.kind === 'failed' && (
            <button className="primary" onClick={repair} disabled={busy}>
              修复引擎
            </button>
          )}
          {live?.awaitingPermission && (
            <button className="primary" onClick={requestPermissions} disabled={busy}>
              去授权
            </button>
          )}
          {/* Not a write of the config: usable before it is read, as soon as the current pause is
              known, so the engine can be stopped even when load_state fails. */}
          <button onClick={togglePause} disabled={busy || (!live && !state)}>
            {(live ? live.manualPause : state?.paused) ? '恢复监听' : '暂停监听'}
          </button>
        </div>
        {/* Unsaved parameters stay in view from the list too; 设置 shows them in its head. */}
        {localDirty && !settingsOpen && <span className="revision">参数有未保存修改</span>}
        <button
          type="button"
          className={`settings-toggle${settingsOpen ? ' active' : ''}`}
          aria-pressed={settingsOpen}
          aria-label={hasUpdate ? '设置（有新版本）' : undefined}
          onClick={() => setTab(settingsOpen ? 'manage' : 'settings')}
        >
          <span aria-hidden="true" className="settings-icon" />
          设置
          {hasUpdate && <span aria-hidden="true" className="update-dot" />}
        </button>
        <span className="header-summary">{summary}</span>
        <label className="switch startup-toggle">
          <input
            type="checkbox"
            checked={!!state?.autostart}
            disabled={!editable}
            onChange={(e) => setAutostart(e.target.checked)}
          />
          <span />
          开机时启动
        </label>
        <button
          type="button"
          className={`theme-toggle ${theme}`}
          onClick={() => setTheme(theme === 'dark' ? 'light' : 'dark')}
          title={theme === 'dark' ? '切换到白天模式' : '切换到暗黑模式'}
          aria-label={theme === 'dark' ? '切换到白天模式' : '切换到暗黑模式'}
        >
          <span aria-hidden="true" className="theme-icon" />
          {theme === 'dark' ? '暗黑' : '白天'}
        </button>
      </header>
      <BannerStack view={runtime} onOpenDataDir={() => void openDataDir()} />
      {!ready && loadFailure && (
        <div className="alert error" role="alert">
          未能读取当前配置：{loadFailure}。读取成功前不能修改指令和设置。
          <button className="retry" disabled={busy} onClick={() => run(loadFirst)}>
            重试
          </button>
        </div>
      )}
      {unread !== null && (
        // Not blocking either: were saving refused until a read succeeds, failing reads would lock
        // the editor. Saving is not silent: the notice stands until a load catches up.
        <div className="alert notice" role="status">
          {`后台配置已更新为 v${unread}，但未能读取最新配置（${readFailure ?? ''}）。保存前请重新读取；版本冲突时不会覆盖后台配置。`}
          <button disabled={busy} onClick={() => void reread()}>
            重新读取
          </button>
        </div>
      )}
      {newer !== null && (
        // Not blocking: the editor stays usable, and nothing changes until the user picks one.
        <div className="alert notice" role="status">
          {newer.by === null
            ? `配置已在后台更新为 v${newer.revision}`
            : `配置已更新为 v${newer.revision}（${MADE_BY[newer.by]}）`}
          {'。未保存的修改已保留；请载入新配置后重新编辑，避免覆盖其他窗口的修改。'}
          <button
            onClick={() => {
              setNewer(null);
            }}
          >
            保留我的修改
          </button>
          <button
            disabled={busy || !state}
            onClick={() => {
              if (!state) return;
              receive(state);
              setNewer(null);
            }}
          >
            放弃修改，载入新配置
          </button>
        </div>
      )}
      {error && (
        <div className="alert error" role="alert">
          {error}
          <button aria-label="关闭错误提示" onClick={() => setError('')}>
            ×
          </button>
        </div>
      )}
      {tab === 'manage' && teamView && (
        <div className="workspace">
          {viewSwitch}
          <CloudPhrases
            view={cloud}
            platform={platform}
            terminators={config.globals.terminators}
            ruleName={(id) => ruleNames.get(id) ?? id}
            canCopy={writable}
            onCopy={(row) => void copyPhrase(row)}
            onSync={() => void syncPhrases()}
            onSetDisabled={(ids, disabled) => void disablePhrases(ids, disabled)}
            onRetry={() => void cloudSyncer.refresh()}
            onOpenSettings={openCloudSettings}
          />
        </div>
      )}
      {tab === 'manage' && !teamView && (
        <div className="workspace">
          {viewSwitch}
          <div className="panel">
            <div className="toolbar">
              <label className="switch">
                <input
                  type="checkbox"
                  checked={config.enabled}
                  disabled={!writable}
                  onChange={(e) => {
                    // Taken now: the change may be made again later (see `save`), when the box shows
                    // config.enabled again. Set to it, not turned over.
                    const on = e.target.checked;
                    void run(() => save((c) => ({ ...c, enabled: on })), '总开关已立即生效');
                  }}
                />
                <span />
                启用文本指令
              </label>
              <input
                className="search"
                aria-label="搜索指令"
                placeholder="搜索缩写、说明或分组…"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
              />
              <button
                className="primary"
                disabled={!editable}
                onClick={() => openEditor(newRule(platform))}
              >
                ＋ 添加指令
              </button>
              <button
                disabled={!ready}
                onClick={importData}
                title="导入 ZIP 配置包、JSON 或 Excel（.xlsx）配置文件"
              >
                导入
              </button>
              <ExportMenu
                disabled={!ready || changing}
                excelDisabled={safeMode !== null && safeMode !== 'crash_loop'}
                onExport={exportData}
              />
            </div>
            <div className="group-tabs">
              {groups.map((g) => (
                <button key={g} className={group === g ? 'active' : ''} onClick={() => setGroup(g)}>
                  {g}
                  <small>{g === '全部' ? config.rules.length : (groupCounts.get(g) ?? 0)}</small>
                </button>
              ))}
            </div>
            <div className="batch">
              <span>
                共 {config.rules.length} 条
                {unreadable.length > 0 && `（${unreadable.length} 条无法读取）`} · 已选{' '}
                {selected.length} 条
              </span>
              <button
                disabled={!writable || !selected.length}
                onClick={() =>
                  mutate((rs) =>
                    rs.map((r) => (selectedSet.has(r.id) ? { ...r, enabled: true } : r)),
                  )
                }
              >
                启用
              </button>
              <button
                disabled={!writable || !selected.length}
                onClick={() =>
                  mutate((rs) =>
                    rs.map((r) => (selectedSet.has(r.id) ? { ...r, enabled: false } : r)),
                  )
                }
              >
                停用
              </button>
              <button
                disabled={!writable || !selected.length}
                onClick={async () => {
                  const g = await askValue({
                    title: `把所选 ${selected.length} 条指令移到哪个分组？`,
                    confirm: '移动',
                    input: { label: '分组名称', value: '默认' },
                  });
                  if (g)
                    void mutate((rs) =>
                      rs.map((r) => (selectedSet.has(r.id) ? { ...r, group: g } : r)),
                    );
                }}
              >
                移至分组
              </button>
              <button
                className="danger"
                disabled={!writable || !selected.length}
                onClick={async () => {
                  if (
                    await confirmAction({
                      title: `删除所选 ${selected.length} 条指令？`,
                      detail: '确认后立即生效。',
                      confirm: '删除',
                    })
                  )
                    void mutate((rs) => rs.filter((r) => !selectedSet.has(r.id)));
                }}
              >
                删除
              </button>
            </div>
            <div className="table-wrap">
              <table>
                <thead>
                  <tr>
                    <th>
                      <input
                        aria-label="全选"
                        type="checkbox"
                        checked={!!rows.length && rows.every((r) => selectedSet.has(r.id))}
                        onChange={(e) => setSelected(e.target.checked ? rows.map((r) => r.id) : [])}
                      />
                    </th>
                    <th>缩写词</th>
                    <th>触发字符</th>
                    <th>说明</th>
                    <th>分组</th>
                    <th>操作类型</th>
                    <th>摘要</th>
                    <th>停用</th>
                    <th>操作</th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((r) => (
                    <tr
                      key={r.id}
                      className={!r.enabled ? 'disabled-row' : ''}
                      onDoubleClick={() => openEditor(clone(r))}
                    >
                      <td>
                        <input
                          aria-label={`选择 ${r.match.value}`}
                          type="checkbox"
                          checked={selectedSet.has(r.id)}
                          onChange={(e) =>
                            setSelected(
                              e.target.checked
                                ? [...selected, r.id]
                                : selected.filter((i) => i !== r.id),
                            )
                          }
                        />
                      </td>
                      <td>
                        <strong>{r.match.value}</strong>
                        <span className="markers">
                          {r.ignore_case && <i>Aa</i>}
                          {r.trigger_mode === 'immediate' && <i>即时</i>}
                          {r.match.kind === 'regex' && <i>正则</i>}
                          {overridden.has(r.id) && (
                            <i
                              className="overridden"
                              title={`团队话术「${(overridden.get(r.id) ?? []).join('」「')}」优先`}
                            >
                              被团队话术覆盖
                            </i>
                          )}
                        </span>
                      </td>
                      <td>
                        {r.trigger_mode === 'immediate' ? (
                          '即时'
                        ) : (
                          <TerminatorList
                            keys={r.terminator?.keys ?? config.globals.terminators}
                            platform={platform}
                          />
                        )}
                        {!r.terminator && r.trigger_mode !== 'immediate' && (
                          <small className="muted">继承</small>
                        )}
                      </td>
                      <td>{r.description || '—'}</td>
                      <td>{groupKey(r.group)}</td>
                      <td>{actionLabel(r.action)}</td>
                      <td className="summary">{summarizeAction(r.action)}</td>
                      <td>
                        <input
                          aria-label={`停用 ${r.match.value}`}
                          type="checkbox"
                          checked={!r.enabled}
                          disabled={!writable}
                          onChange={(e) => {
                            const off = e.target.checked;
                            void mutate((rs) =>
                              rs.map((x) => (x.id === r.id ? { ...x, enabled: !off } : x)),
                            );
                          }}
                        />
                      </td>
                      <td className="row-tools">
                        <button onClick={() => openEditor(clone(r))}>编辑</button>
                        <button
                          className="danger"
                          disabled={!writable}
                          onClick={async () => {
                            if (
                              await confirmAction({
                                title: `删除指令“${r.match.value}”？`,
                                confirm: '删除',
                              })
                            )
                              void mutate((rs) => rs.filter((x) => x.id !== r.id));
                          }}
                        >
                          删除
                        </button>
                      </td>
                    </tr>
                  ))}
                  {unreadableRows.map((u) => (
                    <tr key={`unreadable-${u.index}`} className="unreadable-row">
                      <td />
                      <td colSpan={8}>
                        <strong>无法读取的规则</strong>
                        {u.id && <span className="unreadable-id">{u.id}</span>}
                        {/* Read-only here means only that this UI cannot edit the entry safely; the
                          backend may still read and run it, so the row makes no claim either way.
                          Only a backend rule_unreadable diagnostic may say it is not in effect, and
                          the list has none to map to a rule. */}
                        <small className="muted">只读</small>
                        {u.unsafeNumber && (
                          <small className="unreadable-reason">
                            含超出可安全保存范围的数值，保存其他内容时该数值可能被改写。
                          </small>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {/* No empty state before the first load_state snapshot (F-04); set_paused's answer
                may fill `state` earlier. */}
              {ready && !rows.length && !unreadableRows.length && (
                <div className="empty">
                  <div className="empty-art" aria-hidden="true" />
                  <strong>{search ? '没有匹配的指令' : '还没有文本指令'}</strong>
                  <button
                    className="primary"
                    disabled={!editable}
                    onClick={() => openEditor(newRule(platform))}
                  >
                    添加指令
                  </button>
                </div>
              )}
            </div>
          </div>
        </div>
      )}
      {settingsOpen && (
        <section className="settings">
          <div className="settings-head">
            <button type="button" className="back" onClick={() => setTab('manage')}>
              返回
            </button>
            <h2>设置</h2>
            {localDirty && <span className="revision">参数有未保存修改</span>}
          </div>
          {platform === 'macos' && (
            <p className="muted">
              <span className="capability-tag experimental">实验性</span> macOS 版功能均为实验性。
            </p>
          )}
          {/* IPC number contract (docs/ipc-contract-0.4.md §6); saving is not blocked. */}
          {settingsHaveUnsafeNumber(config) && (
            <p className="unsafe-number-note">
              配置中含超出可安全保存范围的数值，保存时可能被改写。
            </p>
          )}
          <h3>参数</h3>
          {/* Edits here only become a write on 「保存参数设置」, but made before the config is read
              they would be edits of emptyConfig, dropped by the first snapshot. */}
          <fieldset className="settings-fields" disabled={!editable}>
            {/* Principle 5 (S-16): the Windows input backend only erases by Backspace, so there is
                nothing to choose there; a stored `escape` is kept for other platforms. */}
            {!fixedImeErasure(platform) && (
              <Field label="默认输入法删除方式">
                <select
                  value={config.globals.ime_erasure}
                  onChange={(e) => change({ ...config.globals, ime_erasure: e.target.value })}
                >
                  <option value="escape">按 Esc 关闭选字框</option>
                  <option value="backspace">Backspace 删除已上屏字符</option>
                </select>
              </Field>
            )}
            <Field label="黑名单程序">
              <LinesInput
                rows={5}
                placeholder="每行一个进程名或应用标识"
                value={config.globals.blacklist}
                onChange={(blacklist) => change({ ...config.globals, blacklist })}
              />
            </Field>
            <details className="advanced-settings">
              <summary>高级设置</summary>
              <Field label="词分隔符">
                <input
                  value={config.globals.word_separators}
                  onChange={(e) => change({ ...config.globals, word_separators: e.target.value })}
                />
              </Field>
            </details>
          </fieldset>
          <button
            disabled={!editable}
            onClick={() =>
              run(async () => {
                requireReady();
                const w = await call('get_foreground');
                const platform = w.platform ?? state?.platform ?? state?.engine_detail?.platform;
                const identity =
                  platform === 'macos' ? w.bundle_id : platform === 'windows' ? w.process : '';
                if (!identity) throw new Error('目标平台或应用标识未确认，黑名单未修改');
                // The globals as they are now, not as they were before the await.
                const { globals } = editorConfig.current;
                change({ ...globals, blacklist: [...new Set([...globals.blacklist, identity])] });
                setDialog({ title: '窗口信息', data: w });
              })
            }
          >
            选择当前窗口加入黑名单
          </button>
          <div className="section-line">
            {autoListen !== null && (
              <label title={autoListenMissing ? '当前版本的桌面引擎不支持此设置' : undefined}>
                {/* Not `editable`: safe mode allows set_auto_listen, unlike set_autostart
                    (ipc-contract §5; prefs live apart from what safe mode protects), and turning
                    it off can be the way out of a crash loop. */}
                <input
                  type="checkbox"
                  checked={autoListen}
                  disabled={!ready || autoListenMissing}
                  onChange={(e) => setAutoListen(e.target.checked)}
                />
                启动时自动开始监听
              </label>
            )}
          </div>
          <button
            className="primary"
            disabled={!writable}
            onClick={() => run(() => save(), '参数已保存并立即生效')}
          >
            保存参数设置
          </button>
          <button
            disabled={!writable || !localDirty}
            onClick={() => {
              if (shown.current) receive(shown.current);
              setNewer(null);
              setMessage('已取消未保存的参数修改');
            }}
          >
            取消修改
          </button>
          <CandidateHintSection
            key={
              isCandidateHint(state?.candidate_hint)
                ? `${state.candidate_hint.enabled}-${state.candidate_hint.duration_ms}`
                : 'none'
            }
            stored={isCandidateHint(state?.candidate_hint) ? state.candidate_hint : undefined}
            disabled={!ready}
          />
          {cloud.status !== 'unsupported' && (
            <CloudSettingsSection
              view={cloud}
              focusOnMount={cloudFocus}
              onSave={(input) => cloudSyncer.saveSettings(input)}
              onSync={() => cloudSyncer.syncNow()}
            />
          )}
          <UpdateSection updates={updates} />
          <HistoryList
            rows={runtime.status === 'ready' ? runtime.history : []}
            ruleName={ruleName}
          />
          <p className="muted license">
            Quicktext · GPL-3.0-or-later · 第三方组件见 THIRD_PARTY_NOTICES.md
          </p>
        </section>
      )}
      <footer>
        <span className="status-text" aria-live="polite">
          {busy ? '正在处理…' : message}
        </span>
      </footer>
      {editing && (
        <Modal title="编辑文本指令" className="overlay windowed" onClose={() => void closeEditor()}>
          <WindowFrame
            className="editor"
            storageKey="quicker-text.editor-window"
            title={editingFound >= 0 ? '编辑文本指令' : '添加文本指令'}
            closeLabel="关闭编辑"
            onClose={() => void closeEditor()}
          >
            <div className="edit-grid">
              <div className="trigger-panel">
                {editingFieldErrors.length > 0 && (
                  <div className="field-errors" role="alert">
                    <strong>需要修改的设置</strong>
                    <ul>
                      {editingFieldErrors.map((d, i) => (
                        <li key={i}>
                          {diagnosticField(d.path, editingIndex)}：{diagnosticText(d)}
                        </li>
                      ))}
                    </ul>
                  </div>
                )}
                {editingExtraKeys.length > 0 && (
                  <p className="muted extra-keys">
                    含有本版本不认识的字段（{editingExtraKeys.join('、')}），保存时会保留。
                  </p>
                )}
                <h3>什么时候执行</h3>
                <Field label="缩写词">
                  <input
                    autoFocus
                    placeholder="例如 /hello"
                    value={editing.match.value}
                    onChange={(e) =>
                      setEditing({ ...editing, match: { ...editing.match, value: e.target.value } })
                    }
                  />
                </Field>
                <div className="inline">
                  <label>
                    <input
                      type="checkbox"
                      checked={editing.enabled}
                      onChange={(e) => setEditing({ ...editing, enabled: e.target.checked })}
                    />
                    启用
                  </label>
                  <label>
                    <input
                      type="checkbox"
                      checked={editing.ignore_case}
                      onChange={(e) => setEditing({ ...editing, ignore_case: e.target.checked })}
                    />
                    不区分大小写
                  </label>
                  {/* Regular expressions are not offered for new rules; one that has it can turn it off. */}
                  {editingShown.regex && (
                    <label>
                      <input
                        type="checkbox"
                        disabled={editing.trigger_mode === 'immediate'}
                        checked={editing.match.kind === 'regex'}
                        onChange={(e) =>
                          setEditing({
                            ...editing,
                            match: {
                              ...editing.match,
                              kind: e.target.checked ? 'regex' : 'literal',
                            },
                          })
                        }
                      />
                      正则表达式
                    </label>
                  )}
                </div>
                <Field label="说明">
                  <input
                    value={editing.description}
                    onChange={(e) => setEditing({ ...editing, description: e.target.value })}
                  />
                </Field>
                <Field label="分组">
                  <input
                    list="groups"
                    value={editing.group}
                    onChange={(e) => setEditing({ ...editing, group: e.target.value })}
                  />
                  <datalist id="groups">
                    {groups
                      .filter((g) => g !== '全部')
                      .map((g) => (
                        <option key={g}>{g}</option>
                      ))}
                  </datalist>
                </Field>
                <Field label="什么时候执行">
                  <select value={editing.trigger_mode} onChange={(e) => changeMode(e.target.value)}>
                    <option value="terminator">输入缩写后，再按空格、分号等指定键执行</option>
                    <option value="immediate">缩写输入完成后立即执行</option>
                  </select>
                </Field>
                {editing.trigger_mode === 'terminator' && (
                  <>
                    <label>
                      <input
                        type="checkbox"
                        checked={!editing.terminator}
                        onChange={(e) =>
                          setEditing({
                            ...editing,
                            terminator: e.target.checked
                              ? null
                              : { keys: config.globals.terminators, consume: true },
                          })
                        }
                      />
                      默认触发键
                    </label>
                    <EndKeys
                      platform={platform}
                      value={editing.terminator?.keys ?? config.globals.terminators}
                      onChange={(v) =>
                        setEditing({
                          ...editing,
                          terminator: {
                            ...editing.terminator,
                            keys: v,
                            consume: editing.terminator?.consume ?? true,
                          },
                        })
                      }
                    />
                    {editing.terminator && (
                      <label>
                        <input
                          type="checkbox"
                          checked={editing.terminator.consume}
                          onChange={(e) =>
                            setEditing({
                              ...editing,
                              terminator: { ...editing.terminator, consume: e.target.checked },
                            })
                          }
                        />
                        执行时去掉刚按下的触发键（如空格）
                      </label>
                    )}
                  </>
                )}
                {/* Each rule follows the default; an override from an older version stays editable. */}
                {!fixedImeErasure(platform) && editingShown.ime && (
                  <Field label="如何清除输入法中的缩写">
                    <select
                      value={editing.ime_erasure ?? 'inherit'}
                      onChange={(e) =>
                        setEditing({
                          ...editing,
                          ime_erasure: e.target.value === 'inherit' ? null : e.target.value,
                        })
                      }
                    >
                      <option value="inherit">
                        使用默认设置（
                        {config.globals.ime_erasure === 'backspace'
                          ? '按退格键删除已输入的字'
                          : '按 Esc 关闭选字框'}
                        ）
                      </option>
                      <option value="escape">按 Esc 关闭选字框</option>
                      <option value="backspace">按退格键删除已输入的字</option>
                    </select>
                  </Field>
                )}
                {/* Not offered for new rules; a rule that already sets it can change it back. */}
                {editingShown.parameter && (
                  <Field label="传给操作的内容来自哪里">
                    <select
                      value={editing.parameter.type}
                      onChange={(e) =>
                        setEditing({
                          ...editing,
                          parameter:
                            e.target.value === 'fixed'
                              ? { type: 'fixed', value: '' }
                              : e.target.value === 'capture_template'
                                ? { type: 'capture_template', template: '' }
                                : { type: e.target.value },
                        })
                      }
                    >
                      <option value="default_match">这次匹配到的文字（默认）</option>
                      <option value="fixed">每次使用相同内容</option>
                      <option value="first_capture">匹配规则中第一对括号里的文字（高级）</option>
                      <option value="capture_template">组合匹配规则中括号里的文字（高级）</option>
                      <option value="null">不传入内容</option>
                    </select>
                  </Field>
                )}
                {editing.parameter.type === 'fixed' && (
                  <input
                    aria-label="每次传入的固定内容"
                    value={editing.parameter.value ?? ''}
                    onChange={(e) =>
                      setEditing({
                        ...editing,
                        parameter: { ...editing.parameter, value: e.target.value },
                      })
                    }
                  />
                )}{' '}
                {editing.parameter.type === 'capture_template' && (
                  <CaptureTemplate
                    value={editing.parameter.template ?? ''}
                    onChange={(template) =>
                      setEditing({ ...editing, parameter: { ...editing.parameter, template } })
                    }
                  />
                )}
                <button
                  onClick={async () => {
                    const problems = editProblems(editing);
                    if (problems.length > 0) {
                      void run(() => Promise.reject(reject(problems)));
                      return;
                    }
                    const input = await askValue({
                      title: '用哪段文字检查？',
                      confirm: '检查',
                      input: {
                        label: '输入一段文字，检查是否会触发指令',
                        value: editing.match.value,
                      },
                    });
                    if (input !== null)
                      void run(async () =>
                        setDialog({
                          title: '查看是否匹配及附带内容',
                          data: describeMatch(
                            await call('test_rule', { rule: editing, input }),
                            input,
                          ),
                        }),
                      );
                  }}
                >
                  检查这段文字能否触发（不会执行操作）
                </button>
              </div>
              <div className="action-panel">
                <h3>操作内容</h3>
                <ActionEditor
                  // Remembers what the action had when opened (fields shown for the whole edit).
                  key={editing.id}
                  platform={state?.platform ?? state?.engine_detail?.platform}
                  diagnostics={editingDiagnostics.action}
                  capabilities={state?.capabilities ?? []}
                  rules={readable.filter((r) => r.id !== editing.id)}
                  action={editing.action}
                  onChange={(a) => setEditing({ ...editing, action: a })}
                  run={run}
                  show={setDialog}
                />
                <div className="section-line">
                  <label>
                    <input
                      type="checkbox"
                      checked={editing.notification.enabled}
                      onChange={(e) =>
                        setEditing({
                          ...editing,
                          notification: { ...editing.notification, enabled: e.target.checked },
                        })
                      }
                    />
                    触发后显示提示
                  </label>
                  {editing.notification.enabled && (
                    <input
                      aria-label="提示内容"
                      placeholder="提示内容"
                      value={editing.notification.message}
                      onChange={(e) =>
                        setEditing({
                          ...editing,
                          notification: { ...editing.notification, message: e.target.value },
                        })
                      }
                    />
                  )}
                </div>
              </div>
            </div>
            <div className="modal-footer">
              <button onClick={() => void closeEditor()}>取消</button>
              <button disabled={busy || !writable} className="primary" onClick={commitEdit}>
                保存
              </button>
            </div>
          </WindowFrame>
        </Modal>
      )}
      {dialog && (
        <Modal title={dialog.title} className="overlay top" onClose={closeReport}>
          <div className="report">
            <div className="modal-title">
              <h2>{dialog.title}</h2>
              <button aria-label="关闭报告" disabled={importing} onClick={closeReport}>
                ×
              </button>
            </div>
            <pre>
              {typeof dialog.data === 'string' ? dialog.data : JSON.stringify(dialog.data, null, 2)}
            </pre>
            <div className="modal-footer">
              {dialog.importConfig && (
                <button
                  className="primary"
                  disabled={changing || !safeModeRead}
                  onClick={() => {
                    const imported = dialog.importConfig;
                    if (!imported) return;
                    setImporting(true);
                    void run(async () => {
                      // Keep the preview modal until its atomic publication completes.
                      try {
                        await write('save_config', {
                          config: imported,
                          importToken: dialog.importToken,
                        });
                        setDialog(null);
                      } finally {
                        setImporting(false);
                      }
                    }, '导入配置已确认并立即生效');
                  }}
                >
                  确认导入并生效
                </button>
              )}
              <button disabled={importing} onClick={closeReport}>
                关闭
              </button>
            </div>
          </div>
        </Modal>
      )}
      {askDialog}
    </div>
  );
}
