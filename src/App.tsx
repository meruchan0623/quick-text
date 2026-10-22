import { useEffect, useRef, useState } from 'react';
import { call } from './bridge';
import {
  actionLabel,
  clone,
  Config,
  Data,
  emptyConfig,
  keyNames,
  keys,
  newRule,
  Rule,
  State,
} from './model';
import ActionEditor from './ActionEditor';
import CaptureTemplate from './CaptureTemplate';
import RuntimeStatus from './RuntimeStatus';
import { useAsk } from './Ask';
import { Modal } from './Modal';
import { every } from './lib/timers';

const errorText = (e: unknown) =>
  typeof e === 'string' ? e : e instanceof Error ? e.message : JSON.stringify(e, null, 2);
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
function EndKeys({ value, onChange }: { value: string[]; onChange: (v: string[]) => void }) {
  const keySet = new Set(value);
  return (
    <div className="inline">
      {keys.map((k) => (
        <label key={k}>
          <input
            type="checkbox"
            checked={keySet.has(k)}
            onChange={(e) =>
              onChange(e.target.checked ? [...value, k] : value.filter((i) => i !== k))
            }
          />
          {keyNames[k]}
        </label>
      ))}
    </div>
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
    setConfig(s.draft);
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
  useEffect(() => {
    if (!('__TAURI_INTERNALS__' in window)) return;
    return every(2000, () => {
      call<State>('load_state')
        .then(setState)
        .catch(() => {
          // Keep showing the last known state; the next poll retries.
        });
    });
  }, []);
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
  const groupCounts = new Map<string, number>();
  for (const r of config.rules) groupCounts.set(r.group, (groupCounts.get(r.group) || 0) + 1);
  const groups = ['全部', ...new Set(config.rules.map((r) => r.group || '默认'))];
  const rows = config.rules
    .filter(
      (r) =>
        (group === '全部' || r.group === group) &&
        `${r.match.value} ${r.description} ${r.group}`.toLowerCase().includes(search.toLowerCase()),
    )
    .sort((a, b) =>
      (sort === 'match' ? a.match.value : a.group).localeCompare(
        sort === 'match' ? b.match.value : b.group,
      ),
    );
  const mutate = async (fn: (rs: Rule[]) => Rule[]) =>
    run(() => save({ ...config, rules: fn(clone(config.rules)) }), '草稿已保存 · 请应用设置');
  const choose = async (kind = 'file') => call<string[] | null>('choose_file', { kind });
  const exportData = () =>
    run(async () => {
      if (localDirty) await save();
      const path = await call<string>('export_config');
      setMessage(`已导出：${path}`);
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
      setDialog({ title: '导入预览 · 确认后写入草稿', data: r.report, importConfig: r.config });
      setMessage('请核对导入报告后确认');
    });
  const commitEdit = () =>
    run(async () => {
      if (!editing) return;
      const c = {
        ...config,
        rules: config.rules.some((r) => r.id === editing.id)
          ? config.rules.map((r) => (r.id === editing.id ? editing : r))
          : [...config.rules, editing],
      };
      const v = await call('validate_config', { config: c });
      if (!v.valid) {
        setEditingErrors(v.errors);
        throw new Error('有设置需要修改，请查看提示；已填写的内容会保留。');
      }
      await save(c);
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
          <span className={`dot ${state?.paused ? 'paused' : ''}`} />
          {state ? (state.paused ? '引擎已暂停' : '引擎运行中') : '未连接'}
          <button
            onClick={() =>
              run(async () => receive(await call<State>('set_paused', { paused: !state?.paused })))
            }
            disabled={!state || busy}
          >
            {state?.paused ? '恢复监听' : '暂停监听'}
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
            <button
              className="primary"
              onClick={() => setEditing(newRule(state?.platform ?? state?.engine_detail?.platform))}
            >
              ＋ 添加指令
            </button>
            <button onClick={importData}>导入</button>
            <button onClick={exportData}>导出</button>
          </div>

          <div className="batch">
            <span>
              共 {config.rules.length} 条 · 已选 {selected.length} 条
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
                    onDoubleClick={() => setEditing(clone(r))}
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
                        {r.trigger_mode === 'immediate' && <i>直接</i>}
                        {r.match.kind === 'regex' && <i>正则</i>}
                      </span>
                    </td>
                    <td>
                      {r.trigger_mode === 'immediate'
                        ? '即时'
                        : (r.terminator?.keys ?? config.globals.terminators)
                            .map((k: string) => keyNames[k] || k)
                            .join(' / ')}
                      {!r.terminator && r.trigger_mode !== 'immediate' && (
                        <small className="muted">继承</small>
                      )}
                    </td>
                    <td>{r.description || '—'}</td>
                    <td>{r.group}</td>
                    <td>{actionLabel(r.action)}</td>
                    {!hide && (
                      <td className="summary">
                        {r.action.source?.value ||
                          r.action.source?.path ||
                          r.action.html ||
                          r.action.target ||
                          r.action.function ||
                          r.action.action_id ||
                          `${r.action.plan?.steps?.length ?? 0} 个步骤`}
                      </td>
                    )}
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
                      <button onClick={() => setEditing(clone(r))}>编辑</button>
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
              </tbody>
            </table>
            {!rows.length && (
              <div className="empty">
                <div>⌨</div>
                <strong>{search ? '没有匹配的指令' : '建立你的第一条文本指令'}</strong>
                <p>设置缩写、触发条件和操作，保存后应用设置。</p>
                <button
                  onClick={() =>
                    setEditing(newRule(state?.platform ?? state?.engine_detail?.platform))
                  }
                >
                  添加指令
                </button>
              </div>
            )}
          </div>
          <div className="group-tabs">
            {groups.map((g) => (
              <button key={g} className={group === g ? 'active' : ''} onClick={() => setGroup(g)}>
                {g}
                <small>{g === '全部' ? config.rules.length : groupCounts.get(g) || 0}</small>
              </button>
            ))}
          </div>
        </>
      )}
      {tab === 'settings' && (
        <section className="settings">
          <h2>文本指令参数设置</h2>
          <p className="muted">以下选项与指令草稿一并发布，单条指令可覆盖触发字符。</p>
          <Field label="默认触发字符">
            <EndKeys
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
            <textarea
              rows={5}
              value={config.globals.blacklist.join('\n')}
              onChange={(e) =>
                change({
                  ...config,
                  globals: {
                    ...config.globals,
                    blacklist: e.target.value.split('\n').filter(Boolean),
                  },
                })
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
        </section>
      )}
      {tab === 'status' && (
        <section className="settings">
          <h2>运行状态与恢复</h2>
          <dl>
            <dt>引擎状态</dt>
            <dd>
              {({ running: '正在执行动作', paused: '已暂停监听', listening: '正在监听' } as Data)[
                state?.engine_status || ''
              ] ||
                state?.engine_status ||
                '尚未连接'}
            </dd>
            <dt>当前生效版本</dt>
            <dd>{state?.revision ?? 0}</dd>
            <dt>草稿状态</dt>
            <dd>{state?.dirty || localDirty ? '存在未应用的设置' : '与生效版本一致'}</dd>
          </dl>
          <div className="inline">
            <button
              onClick={() =>
                run(async () => receive(await call<State>('load_state')), '状态已刷新')
              }
            >
              刷新状态
            </button>
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
            <button onClick={() => run(() => call('exit_app'))}>退出全部</button>
          </div>
          <RuntimeStatus state={state} />
        </section>
      )}
      <footer>
        <span className="status-text">{busy ? '正在处理…' : message}</span>
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
        <Modal title="编辑文本指令" className="overlay" onClose={() => setEditing(null)}>
          <div className="editor">
            <div className="modal-title">
              <h2>
                {config.rules.some((r) => r.id === editing.id) ? '编辑文本指令' : '添加文本指令'}
              </h2>
              <button aria-label="关闭编辑" onClick={() => setEditing(null)}>
                ×
              </button>
            </div>
            <div className="edit-grid">
              <div className="trigger-panel">
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
                    <option value="terminator">输入缩写后，再按空格、回车等指定键执行</option>
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
                  diagnostics={editingErrors.filter((d) =>
                    String(d.path || ' ').includes('action'),
                  )}
                  capabilities={state?.capabilities ?? []}
                  rules={config.rules.filter((r) => r.id !== editing.id)}
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
              <button onClick={() => setEditing(null)}>取消</button>
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
