import { useEffect, useRef, useState } from 'react';
import { call } from './ipc/transport';
import { errorText as describe, diagnosticText, parseCommandError } from './ipc/errors';
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
import CaptureTemplate from './CaptureTemplate';
import RuntimeStatus from './RuntimeStatus';
import { useAsk } from './Ask';
import { Modal } from './Modal';
import { runtimeSync, useRuntime } from './ipc/events';
import { liveStatus } from './status';
import { BannerStack, HealthCard, HistoryList } from './StatusArea';
import { LinesInput } from './LinesInput';
import {
  countByGroup,
  diagnosticField,
  groupKey,
  pruneSelection,
  replaceReadable,
  ruleDiagnostics,
  summarizeAction,
  unknownRuleKeys,
  unreadableEntries,
} from './lib/rules';
import { effectiveTerminators, unsupportedTerminators } from './lib/terminators';

// `null` from describe() means the user cancelled: show nothing.
const errorText = (e: unknown) => describe(e) ?? '';
function Field({
  label,
  children,
  hint,
}: {
  label: string;
  children: React.ReactNode;
  hint?: string | undefined;
}) {
  return (
    <label className="field">
      <span>{label}</span>
      {children}
      {hint && <small>{hint}</small>}
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
/** 触发字符 column: supported keys, then the unsupported ones marked as such. */
function TerminatorList({
  keys: list,
  platform,
}: {
  keys: string[];
  platform?: string | undefined;
}) {
  const usable = effectiveTerminators(list, platform);
  const unusable = list.filter((k) => !usable.includes(k));
  return (
    <>
      {usable.map((k) => keyNames[k] || k).join(' / ') || '—'}
      {unusable.length > 0 && (
        <small className="muted">
          {' '}
          {unusable.map((k) => keyNames[k] || k).join('、')} 暂不支持
        </small>
      )}
    </>
  );
}

export default function App() {
  const [state, setState] = useState<State | null>(null),
    [config, setConfig] = useState<Config>(emptyConfig),
    [tab, setTab] = useState('manage'),
    [search, setSearch] = useState(''),
    [group, setGroup] = useState('全部'),
    [hide, setHide] = useState(false),
    [selected, setSelected] = useState<string[]>([]),
    [editing, setEditing] = useState<Rule | null>(null),
    [busy, setBusy] = useState(false),
    [message, setMessage] = useState('正在连接桌面引擎…'),
    [error, setError] = useState(''),
    [dialog, setDialog] = useState<{ title: string; data: any; importConfig?: Config } | null>(
      null,
    ),
    [sort, setSort] = useState<'match' | 'group'>('match'),
    [localDirty, setLocalDirty] = useState(false);
  const [editingErrors, setEditingErrors] = useState<Data[]>([]);
  // Clear the editor's validation errors when another rule is opened.
  const [errorsFor, setErrorsFor] = useState(editing?.id);
  if (errorsFor !== editing?.id) {
    setErrorsFor(editing?.id);
    setEditingErrors([]);
  }
  const { askDialog, confirmAction, askValue } = useAsk();
  // Health, engine state and history arrive as events (S-19); no polling.
  const sync = runtimeSync();
  const runtime = useRuntime(sync);
  const live = liveStatus(runtime);
  const edited = useRef(false);
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
  const receive = (s: State) => {
    setState(s);
    // A rule field with a serde default may be absent; the editor reads it filled in, as Rust does.
    const draft = completeConfig(s.draft);
    setConfig(draft);
    // Rules removed by a delete, rollback or import must not stay selected (F-04).
    setSelected((current) => pruneSelection(current, draft.rules));
    setLocalDirty(false);
    edited.current = false;
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
  useEffect(() => {
    call<State>('load_state')
      .then((s) => {
        receive(s);
        setMessage('已连接 · 保存写入草稿，应用设置后生效');
      })
      .catch((e: unknown) => setError(errorText(e)));
  }, []);
  // The legacy load_state fields (target observation, capabilities, draft flags) have no event:
  // reload them when the window regains focus and on 「刷新状态」. Unsaved local edits are kept.
  const reload = async () => {
    const s = await call<State>('load_state');
    if (edited.current) setState(s);
    else receive(s);
  };
  const reloadRef = useRef(reload);
  useEffect(() => {
    reloadRef.current = reload;
  });
  useEffect(() => {
    const onFocus = () => {
      reloadRef.current().catch(() => {
        // Keep showing the last known state; 「刷新状态」 reports errors.
      });
    };
    window.addEventListener('focus', onFocus);
    return () => window.removeEventListener('focus', onFocus);
  }, []);
  const refresh = () =>
    run(async () => {
      await reload();
      if (runtime.status !== 'unsupported') await sync.resync();
    }, '状态已刷新');
  const repair = () =>
    run(async () => {
      await call('repair_engine');
      await sync.resync();
    }, '已开始修复引擎，结果会自动更新');
  const togglePause = () =>
    run(async () => {
      const manual = live ? live.manualPause : !!state?.paused;
      // set_paused only toggles the manual reason; the unsaved draft in the editor stays untouched.
      setState(await call<State>('set_paused', { paused: !manual }));
    });
  const exportDiagnostics = () =>
    run(async () => {
      const redaction = await askValue({
        title: '导出诊断包？',
        detail:
          '诊断包包含最近 7 天的日志、崩溃记录、健康与能力快照和配置摘要，不含剪贴板与输入内容。只有选择「包含短语内容」时才附带完整配置。',
        confirm: '导出',
        input: {
          label: '短语内容',
          value: 'summary',
          options: [
            { value: 'summary', label: '不包含短语内容（推荐）' },
            { value: 'full', label: '包含短语内容' },
          ],
        },
      });
      if (!redaction) return;
      const path = await call<string | null>('export_diagnostics', { redaction });
      // null: the save dialog was cancelled; nothing to report.
      if (path) setMessage(`诊断包已导出：${path}`);
    });
  const openDir = (command: 'open_log_dir' | 'open_data_dir') => run(() => call(command));
  const platform: string | undefined = state?.platform ?? state?.engine_detail?.platform;
  // The rule as opened, to tell whether closing the editor would lose changes (F-04).
  const editingBase = useRef('');
  const openEditor = (rule: Rule) => {
    editingBase.current = JSON.stringify(rule);
    setEditing(rule);
  };
  const closeEditor = async () => {
    if (
      editing &&
      JSON.stringify(editing) !== editingBase.current &&
      !(await confirmAction({
        title: '放弃未保存的改动？',
        detail: '这条指令的改动还没有保存到草稿，关闭后会丢失。',
        confirm: '放弃改动',
      }))
    )
      return;
    setEditing(null);
  };
  const change = (c: Config) => {
    setConfig(c);
    setLocalDirty(true);
    edited.current = true;
  };
  const save = async (c = config) => {
    const s = await call<State>('save_draft', { config: c });
    receive(s);
    return s;
  };
  const selectedSet = new Set(selected);
  // Rules the UI can show and edit. An entry the backend could not read (B-43) is listed read-only
  // and written back in place as the value received, which within the IPC number contract
  // (docs/ipc-contract-0.4.md §6) is the same value; so are keys this version does not know.
  const readable = config.rules.filter(isRule);
  const unreadable = unreadableEntries(config.rules);
  const ruleNames = new Map<string, string>(readable.map((r) => [r.id, r.match.value || r.id]));
  const ruleName = (id: string) => ruleNames.get(id) ?? id;
  // An empty group is listed, counted and filtered as 「默认」 (F-04).
  const groupCounts = countByGroup(config.rules);
  const groups = ['全部', ...groupCounts.keys()];
  const needle = search.toLowerCase();
  const rows = readable
    .filter(
      (r) =>
        (group === '全部' || groupKey(r.group) === group) &&
        `${r.match.value} ${r.description} ${groupKey(r.group)} ${summarizeAction(r.action)}`
          .toLowerCase()
          .includes(needle),
    )
    .sort((a, b) =>
      (sort === 'match' ? a.match.value : groupKey(a.group)).localeCompare(
        sort === 'match' ? b.match.value : groupKey(b.group),
      ),
    );
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
  const mutate = async (fn: (rs: Rule[]) => Rule[]) =>
    run(
      () => save({ ...config, rules: replaceReadable(config.rules, fn(clone(readable))) }),
      '草稿已保存 · 请应用设置',
    );
  const choose = async (kind = 'file') => call<string[] | null>('choose_file', { kind });
  const exportData = () =>
    run(async () => {
      if (localDirty) await save();
      const path = await call<string | null>('export_config');
      // null: the save dialog was cancelled; say nothing (ipc-contract §4).
      if (path) setMessage(`已导出：${path}`);
    });
  const importData = () =>
    run(async () => {
      const p = await choose('import');
      if (!p?.length) return;
      const strategy = await askValue({
        title: '遇到重复的指令时怎么处理？',
        confirm: '导入',
        input: {
          label: '冲突策略',
          value: 'merge',
          options: [
            { value: 'merge', label: '保留现有（merge）' },
            { value: 'keep_both', label: '保留两份（keep_both）' },
            { value: 'replace', label: '替换全部（replace）' },
          ],
        },
      });
      if (!strategy) return;
      const r = await call('import_config', { path: p[0], strategy });
      if (!r) return; // cancelled
      setDialog({ title: '导入预览 · 确认后写入草稿', data: r.report, importConfig: r.config });
      setMessage('请核对导入报告后确认');
    });
  const commitEdit = () =>
    run(async () => {
      if (!editing) return;
      const found = indexOfRule(editing.id);
      const c = {
        ...config,
        rules:
          found >= 0
            ? config.rules.map((r, i) => (i === found ? editing : r))
            : [...config.rules, editing],
      };
      const reject = (diagnostics: Data[]) => {
        setEditingErrors(diagnostics);
        return new Error('有设置需要修改，请查看编辑框中的提示；已填写的内容会保留。');
      };
      const v = await call('validate_config', { config: c });
      if (!v.valid) throw reject(v.errors);
      try {
        await save(c);
      } catch (e) {
        // save_draft validates too (B-42); its diagnostics go to the same place.
        const error = parseCommandError(e);
        if (error.code === 'validation' && error.detail.diagnostics.length > 0)
          throw reject(error.detail.diagnostics);
        throw e;
      }
      setEditing(null);
    }, '指令已保存到草稿');
  return (
    <div className="app">
      <header>
        <div className="brand">
          <b>Q</b>
          <div>
            <strong>文本指令</strong>
            <small>Quicker Text · 跨平台工作台</small>
          </div>
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
          <button onClick={togglePause} disabled={!state || busy}>
            {(live ? live.manualPause : state?.paused) ? '恢复监听' : '暂停监听'}
          </button>
        </div>
      </header>
      <nav className="main-tabs">
        <button className={tab === 'manage' ? 'active' : ''} onClick={() => setTab('manage')}>
          文本指令管理
        </button>
        <button className={tab === 'settings' ? 'active' : ''} onClick={() => setTab('settings')}>
          文本指令参数设置
        </button>
        <button className={tab === 'status' ? 'active' : ''} onClick={() => setTab('status')}>
          运行状态与恢复
        </button>
        <span className="revision">
          当前生效 v{state?.revision ?? 0}
          <em>{state?.dirty || localDirty ? '草稿待应用' : '配置已同步'}</em>
        </span>
      </nav>
      <BannerStack view={runtime} onOpenDataDir={() => openDir('open_data_dir')} />
      {error && (
        <div className="alert error" role="alert">
          {error}
          <button aria-label="关闭错误提示" onClick={() => setError('')}>
            ×
          </button>
        </div>
      )}
      {tab === 'manage' && (
        <>
          <div className="toolbar">
            <label className="switch">
              <input
                type="checkbox"
                checked={config.enabled}
                onChange={(e) =>
                  run(() => save({ ...config, enabled: e.target.checked }), '总开关已保存到草稿')
                }
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
            <label>
              <input type="checkbox" checked={hide} onChange={(e) => setHide(e.target.checked)} />
              隐藏摘要
            </label>
            <button className="primary" onClick={() => openEditor(newRule(platform))}>
              ＋ 添加指令
            </button>
            <button onClick={importData}>导入</button>
            <button onClick={exportData}>导出</button>
          </div>

          <div className="batch">
            <span>
              共 {config.rules.length} 条
              {unreadable.length > 0 && `（${unreadable.length} 条无法读取）`} · 已选{' '}
              {selected.length} 条
            </span>
            <button
              disabled={!selected.length}
              onClick={() =>
                mutate((rs) => rs.map((r) => (selectedSet.has(r.id) ? { ...r, enabled: true } : r)))
              }
            >
              启用
            </button>
            <button
              disabled={!selected.length}
              onClick={() =>
                mutate((rs) =>
                  rs.map((r) => (selectedSet.has(r.id) ? { ...r, enabled: false } : r)),
                )
              }
            >
              停用
            </button>
            <button
              disabled={!selected.length}
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
              disabled={!selected.length}
              onClick={async () => {
                if (
                  await confirmAction({
                    title: `删除所选 ${selected.length} 条指令？`,
                    detail: '此操作保存至草稿。',
                    confirm: '删除',
                  })
                )
                  void mutate((rs) => rs.filter((r) => !selectedSet.has(r.id)));
              }}
            >
              删除
            </button>
            <button onClick={() => setSort(sort === 'match' ? 'group' : 'match')}>
              排序：{sort === 'match' ? '缩写' : '分组'}
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
                  {!hide && <th>摘要</th>}
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
                    {!hide && <td className="summary">{summarizeAction(r.action)}</td>}
                    <td>
                      <input
                        aria-label={`停用 ${r.match.value}`}
                        type="checkbox"
                        checked={!r.enabled}
                        onChange={(e) =>
                          mutate((rs) =>
                            rs.map((x) =>
                              x.id === r.id ? { ...x, enabled: !e.target.checked } : x,
                            ),
                          )
                        }
                      />
                    </td>
                    <td className="row-tools">
                      <button onClick={() => openEditor(clone(r))}>编辑</button>
                      <button
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
                    <td colSpan={hide ? 7 : 8}>
                      <strong>无法读取的规则</strong>
                      {u.id && <span className="unreadable-id">{u.id}</span>}
                      {u.unsafeNumber ? (
                        <>
                          <small className="muted">
                            这条规则的内容当前版本无法读取，暂不生效；已保留在配置中。
                          </small>
                          <small className="unreadable-reason">
                            含超出可安全保存范围的数值，保存其他内容时该数值可能被改写。
                          </small>
                        </>
                      ) : (
                        <small className="muted">
                          这条规则的内容当前版本无法读取，暂不生效；已原样保留在配置中，保存时不会改动它。
                        </small>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {/* No empty state before the first load_state answer (F-04). */}
            {state && !rows.length && !unreadableRows.length && (
              <div className="empty">
                <div aria-hidden="true">⌨</div>
                <strong>{search ? '没有匹配的指令' : '建立你的第一条文本指令'}</strong>
                <p>设置缩写、触发条件和操作，保存后应用设置。</p>
                <button onClick={() => openEditor(newRule(platform))}>添加指令</button>
              </div>
            )}
          </div>
          <div className="group-tabs">
            {groups.map((g) => (
              <button key={g} className={group === g ? 'active' : ''} onClick={() => setGroup(g)}>
                {g}
                <small>{g === '全部' ? config.rules.length : (groupCounts.get(g) ?? 0)}</small>
              </button>
            ))}
          </div>
        </>
      )}
      {tab === 'settings' && (
        <section className="settings">
          <h2>文本指令参数设置</h2>
          <p className="muted">以下选项与指令草稿一并发布，单条指令可覆盖触发字符。</p>
          {/* IPC number contract (docs/ipc-contract-0.4.md §6); saving is not blocked. */}
          {settingsHaveUnsafeNumber(config) && (
            <p className="unsafe-number-note">
              配置中含超出可安全保存范围的数值，保存时可能被改写。
            </p>
          )}
          <Field label="默认触发字符">
            <EndKeys
              platform={platform}
              value={config.globals.terminators}
              onChange={(v) =>
                change({ ...config, globals: { ...config.globals, terminators: v } })
              }
            />
          </Field>
          <Field label="词分隔符" hint="空格、Tab、换行用于确定触发边界。">
            <input
              value={config.globals.word_separators}
              onChange={(e) =>
                change({
                  ...config,
                  globals: { ...config.globals, word_separators: e.target.value },
                })
              }
            />
          </Field>
          <Field
            label="默认输入法删除方式"
            hint="Windows 新输入后端统一按缩写长度退格；Esc 选项仅保留原配置，当前不会执行 Esc。中文选字中的数字或空格可能选择候选词，建议切到英文输入后使用。"
          >
            <select
              value={config.globals.ime_erasure}
              onChange={(e) =>
                change({ ...config, globals: { ...config.globals, ime_erasure: e.target.value } })
              }
            >
              <option value="escape">按 Esc 关闭选字框</option>
              <option value="backspace">Backspace 删除已上屏字符</option>
            </select>
          </Field>
          <Field
            label="黑名单程序"
            hint="每行一个 Windows 进程名或 Mac 应用标识；黑名单内不执行文本指令。"
          >
            <LinesInput
              rows={5}
              value={config.globals.blacklist}
              onChange={(blacklist) =>
                change({ ...config, globals: { ...config.globals, blacklist } })
              }
            />
          </Field>
          <button
            onClick={() =>
              run(async () => {
                const w = await call('get_foreground');
                const platform = w.platform ?? state?.platform ?? state?.engine_detail?.platform;
                const identity =
                  platform === 'macos' ? w.bundle_id : platform === 'windows' ? w.process : '';
                if (!identity) throw new Error('目标平台或应用标识未确认，黑名单未修改');
                change({
                  ...config,
                  globals: {
                    ...config.globals,
                    blacklist: [...new Set([...config.globals.blacklist, identity])],
                  },
                });
                setDialog({ title: '窗口信息', data: w });
              })
            }
          >
            选择当前窗口加入黑名单
          </button>
          <div className="section-line">
            <label>
              <input
                type="checkbox"
                checked={!!state?.autostart}
                onChange={(e) =>
                  run(async () => {
                    const enabled = e.target.checked;
                    await call('set_autostart', { enabled });
                    setState((s) => (s ? { ...s, autostart: enabled } : s));
                  })
                }
              />
              登录后自动启动
            </label>
          </div>
          <button className="primary" onClick={() => run(() => save(), '参数已保存到草稿')}>
            保存参数草稿
          </button>
          <h3>许可证与第三方声明</h3>
          <p className="muted license">
            Quicker Text 是自由软件，按 GNU 通用公共许可证第 3 版或（由你选择）任何更新的版本授权
            （GPL-3.0-or-later），不提供任何担保。许可证全文见源码中的
            LICENSE，所用第三方组件及其许可证见 THIRD_PARTY_NOTICES.md。
          </p>
        </section>
      )}
      {tab === 'status' && (
        <section className="settings">
          <h2>运行状态与恢复</h2>
          <HealthCard view={runtime} busy={busy} onRepair={repair} />
          <dl>
            <dt>引擎状态</dt>
            <dd>
              {live?.text ??
                (({ running: '正在执行动作', paused: '已暂停监听', listening: '正在监听' } as Data)[
                  state?.engine_status || ''
                ] ||
                  state?.engine_status ||
                  '尚未连接')}
            </dd>
            <dt>当前生效版本</dt>
            <dd>{state?.revision ?? 0}</dd>
            <dt>草稿状态</dt>
            <dd>{state?.dirty || localDirty ? '存在未应用的设置' : '与生效版本一致'}</dd>
          </dl>
          <div className="inline">
            <button onClick={refresh}>刷新状态</button>
            <button
              onClick={async () => {
                if (await confirmAction({ title: '放弃全部未应用草稿？', confirm: '放弃草稿' }))
                  void run(async () => receive(await call<State>('discard_draft')));
              }}
            >
              放弃未应用草稿
            </button>
            <button
              onClick={async () => {
                if (await confirmAction({ title: '回滚到上一个生效版本？', confirm: '回滚' }))
                  void run(async () => receive(await call<State>('rollback')));
              }}
            >
              恢复上一个版本
            </button>
            <button onClick={() => run(() => call('cancel_execution'), '已请求停止')}>
              停止当前动作
            </button>
            <button
              onClick={async () => {
                if (
                  await confirmAction({
                    title: '退出 Quicker Text？',
                    detail: '退出后文本指令不再生效，直到再次启动。',
                    confirm: '退出',
                  })
                )
                  void run(() => call('exit_app'));
              }}
            >
              退出全部
            </button>
          </div>
          {runtime.status === 'ready' && (
            <div className="inline diagnostics">
              <button onClick={exportDiagnostics}>导出诊断包</button>
              <button onClick={() => openDir('open_log_dir')}>打开日志目录</button>
            </div>
          )}
          <RuntimeStatus
            state={state}
            history={
              runtime.status === 'ready' ? (
                <HistoryList
                  rows={runtime.history}
                  available={runtime.historyAvailable}
                  ruleName={ruleName}
                />
              ) : undefined
            }
          />
        </section>
      )}
      <footer>
        <span className="status-text" aria-live="polite">
          {busy ? '正在处理…' : message}
        </span>
        <button
          disabled={busy || !state}
          className="primary"
          onClick={() =>
            run(async () => {
              if (localDirty) await save();
              receive(await call<State>('apply_settings'));
            }, '设置已应用，生效版本已更新')
          }
        >
          应用设置
        </button>
      </footer>
      {editing && (
        <Modal title="编辑文本指令" className="overlay" onClose={() => void closeEditor()}>
          <div className="editor">
            <div className="modal-title">
              <h2>{editingFound >= 0 ? '编辑文本指令' : '添加文本指令'}</h2>
              <button aria-label="关闭编辑" onClick={() => void closeEditor()}>
                ×
              </button>
            </div>
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
                <Field
                  label="缩写词"
                  hint="输入一个容易记住的短词，例如 /hello。以后输入它，就能执行右侧设置的操作。"
                >
                  <input
                    autoFocus
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
                    英文不区分大小写（如 abc 和 ABC）
                  </label>
                  <label>
                    <input
                      type="checkbox"
                      disabled={editing.trigger_mode === 'immediate'}
                      checked={editing.match.kind === 'regex'}
                      onChange={(e) =>
                        setEditing({
                          ...editing,
                          match: { ...editing.match, kind: e.target.checked ? 'regex' : 'literal' },
                        })
                      }
                    />
                    用文字匹配规则（正则表达式，高级）
                  </label>
                </div>
                <Field label="说明" hint="写下这条指令的用途，方便以后查找。">
                  <input
                    value={editing.description}
                    onChange={(e) => setEditing({ ...editing, description: e.target.value })}
                  />
                </Field>
                <Field label="分组" hint="把用途相似的指令放在同一组，例如“工作回复”。">
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
                <Field
                  label="什么时候执行"
                  hint={
                    state?.platform === 'windows'
                      ? 'Windows 支持缩写输入完成后立即执行，也支持再按空格或分号执行。Tab、回车触发暂不可用；这两个按键仍由目标程序正常处理。'
                      : undefined
                  }
                >
                  <select value={editing.trigger_mode} onChange={(e) => changeMode(e.target.value)}>
                    <option value="terminator">输入缩写后，再按空格、分号等指定键执行</option>
                    <option value="immediate">缩写输入完成后立即执行（不支持匹配规则）</option>
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
                      使用“文本指令参数设置”里的默认触发键
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
                <Field
                  label="如何清除输入法中的缩写"
                  hint="此项保留原配置，便于导入和跨平台使用。Windows 新输入后端当前统一按缩写长度退格，不执行 Esc；输出后请查看目标结果。"
                >
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
                <Field
                  label="传给操作的内容来自哪里"
                  hint="通常保持默认。调用另一条指令或打开链接时，可把匹配到的文字作为附带内容传过去。"
                >
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
                          data: await call('test_rule', { rule: editing, input }),
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
                      placeholder="输入执行时要显示的提示，避免填写隐私内容"
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
              <span>保存后还需点击主窗口的“应用设置”，指令才会生效</span>
              <button onClick={() => void closeEditor()}>取消</button>
              <button disabled={busy} className="primary" onClick={commitEdit}>
                保存
              </button>
            </div>
          </div>
        </Modal>
      )}
      {dialog && (
        <Modal title={dialog.title} className="overlay top" onClose={() => setDialog(null)}>
          <div className="report">
            <div className="modal-title">
              <h2>{dialog.title}</h2>
              <button aria-label="关闭报告" onClick={() => setDialog(null)}>
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
                  onClick={() =>
                    run(async () => {
                      await save(dialog.importConfig);
                      setDialog(null);
                    }, '导入草稿已保存，请应用设置')
                  }
                >
                  确认导入草稿
                </button>
              )}
              <button onClick={() => setDialog(null)}>关闭</button>
            </div>
          </div>
        </Modal>
      )}
      {askDialog}
    </div>
  );
}
