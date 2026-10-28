// 团队话术 (docs/cloud-phrases.md §6): the read-only list of the phrase table synced from the TSim
// 客服系统, and its sync settings. Nothing here edits the table: a phrase is viewed, copied into
// 我的指令 (App's save does that, `onCopy`) or switched off on this computer only. Display rules
// live in ./lib/cloud.ts.
import { useEffect, useRef, useState } from 'react';
import { describeError, diagnosticText, parseCommandError } from './ipc/errors';
import type { CloudView } from './ipc/events';
import { label } from './ipc/labels';
import type { CloudSettings, CloudSettingsInput, CloudState } from './ipc/types';
import {
  actionBody,
  AUTO_SYNC_MINUTES,
  autoSyncLabel,
  cloudErrorText,
  cloudRows,
  cloudStatusClass,
  cloudStatusNames,
  phraseCopy,
  SETTINGS_ERRORS,
  syncTime,
  triggerText,
  type CloudRow,
} from './lib/cloud';
import {
  countByGroup,
  groupKey,
  ruleSearchText,
  summarizeAction,
  type ReadableRule,
} from './lib/rules';
import { actionLabel } from './model';
import { Modal } from './Modal';
import { TerminatorList } from './TerminatorList';

type Phrase = CloudRow & { rule: ReadableRule };

/** Switched off on this computer (§5 `disabled_local`; its status implies it too). */
const localOff = (entry: CloudRow['entry']): boolean =>
  entry.disabled_local || entry.status === 'disabled_local';

const isPhrase = (row: CloudRow): row is Phrase => row.rule !== null;

/** 结束键 of a phrase: its own keys, the defaults of 我的指令 it inherits, or none. */
function PhraseEndKeys({
  rule,
  terminators,
  platform,
}: {
  rule: ReadableRule;
  terminators: string[];
  platform: string | undefined;
}) {
  if (rule.trigger_mode === 'immediate') return <>—（输入完成后立即执行，不需要结束键）</>;
  if (!rule.terminator)
    return (
      <>
        <TerminatorList keys={terminators} platform={platform} />
        <small className="muted">使用「文本指令参数设置」里的默认触发键</small>
      </>
    );
  return (
    <>
      <TerminatorList keys={rule.terminator.keys} platform={platform} />
      <small className="muted">
        {rule.terminator.consume ? '执行时去掉按下的触发键' : '执行时保留按下的触发键'}
      </small>
    </>
  );
}

function StatusTag({ status }: { status: string }) {
  return (
    <span className={`cloud-status ${cloudStatusClass(status)}`}>
      {label(cloudStatusNames, status)}
    </span>
  );
}

/** Read-only details of one phrase (「查看」). */
function PhraseDetail({
  phrase,
  terminators,
  platform,
  ruleName,
  canCopy,
  onCopy,
  onClose,
}: {
  phrase: Phrase;
  terminators: string[];
  platform: string | undefined;
  ruleName: (id: string) => string;
  canCopy: boolean;
  onCopy: (row: CloudRow) => void;
  onClose: () => void;
}) {
  const { entry, rule } = phrase;
  const { reason } = phraseCopy(phrase);
  return (
    <Modal title="查看团队话术" className="overlay" onClose={onClose}>
      <div className="report cloud-detail">
        <div className="modal-title">
          <h2>团队话术 · {rule.match.value}</h2>
          <button aria-label="关闭查看" onClick={onClose}>
            ×
          </button>
        </div>
        <div className="cloud-detail-body">
          <dl>
            <dt>缩写词</dt>
            <dd>
              <strong className="abbr">{rule.match.value}</strong>
            </dd>
            <dt>状态</dt>
            <dd>
              <StatusTag status={entry.status} />
              <PhraseNote entry={entry} ruleName={ruleName} />
            </dd>
            <dt>触发方式</dt>
            <dd>{triggerText(rule)}</dd>
            <dt>结束键</dt>
            <dd>
              <PhraseEndKeys rule={rule} terminators={terminators} platform={platform} />
            </dd>
            <dt>说明</dt>
            <dd>{rule.description || '—'}</dd>
            <dt>分组</dt>
            <dd>{groupKey(rule.group)}</dd>
            <dt>操作类型</dt>
            <dd>{actionLabel(rule.action)}</dd>
          </dl>
          <h3>正文</h3>
          <pre>{actionBody(rule.action)}</pre>
          <details>
            <summary>完整内容（高级）</summary>
            <pre>{JSON.stringify(rule, null, 2)}</pre>
          </details>
        </div>
        <div className="modal-footer">
          <span>
            {reason === null ? '只读：团队话术由 TSim 客服系统维护' : `不能复制：${reason}`}
          </span>
          <button onClick={onClose}>关闭</button>
          <button
            className="primary"
            disabled={!canCopy || reason !== null}
            title={reason ?? undefined}
            onClick={() => {
              onCopy(phrase);
              onClose();
            }}
          >
            复制到我的指令
          </button>
        </div>
      </div>
    </Modal>
  );
}

/** Why a phrase is not in effect, under its status: who shadows it, or what is wrong with it. */
function PhraseNote({
  entry,
  ruleName,
}: {
  entry: CloudRow['entry'];
  ruleName: (id: string) => string;
}) {
  if (entry.status === 'shadowed' && entry.shadowed_by)
    return <small className="muted">我的指令「{ruleName(entry.shadowed_by)}」生效</small>;
  // The table's switch shows first (§4); what this computer chose holds once it is on again.
  if (entry.status === 'disabled_remote' && entry.disabled_local)
    return <small className="muted">本机也已停用</small>;
  if (entry.status === 'invalid' && entry.problem)
    return (
      <small className="cloud-problem" title={entry.problem}>
        {entry.problem}
      </small>
    );
  return null;
}

/** 「来自 TSim 客服系统 · v12 · 上次同步 …」 and 「立即同步」, then the last error if any. */
function SourceBar({
  state,
  busy,
  onSync,
  onOpenSettings,
}: {
  state: CloudState;
  busy: boolean;
  onSync: () => void;
  onOpenSettings: () => void;
}) {
  const { status } = state;
  const synced = syncTime(status.last_success_at);
  const error = status.last_error;
  return (
    <>
      <div className="cloud-source">
        <span>
          {[
            '来自 TSim 客服系统',
            status.revision === null ? '尚未同步' : `v${status.revision}`,
            synced === null ? null : `上次同步 ${synced}`,
          ]
            .filter((part) => part !== null)
            .join(' · ')}
          {status.unreadable_count > 0 && (
            <small className="muted">
              （{status.unreadable_count} 条当前版本无法读取，已跳过）
            </small>
          )}
        </span>
        <button onClick={onSync} disabled={busy || status.syncing}>
          {status.syncing ? '正在同步…' : '立即同步'}
        </button>
      </div>
      {error && (
        <div className="alert error" role="alert">
          {`同步失败${status.last_attempt_at ? `（${syncTime(status.last_attempt_at) ?? ''}）` : ''}：`}
          {cloudErrorText(error)}
          {status.revision !== null && '目前仍使用上次同步的内容。'}
          {SETTINGS_ERRORS.includes(error.code) && (
            <button className="retry" onClick={onOpenSettings}>
              去参数设置
            </button>
          )}
        </div>
      )}
    </>
  );
}

/** The 团队话术 page of 「文本指令管理」. */
export default function CloudPhrases({
  view,
  platform,
  terminators,
  ruleName,
  canCopy,
  onCopy,
  onSync,
  onSetDisabled,
  onRetry,
  onOpenSettings,
}: {
  view: CloudView;
  platform: string | undefined;
  /** The default 触发字符 of 我的指令, which a phrase without its own keys inherits (§4). */
  terminators: string[];
  /** Name of a 我的指令 rule by id (「被我的指令覆盖」). */
  ruleName: (id: string) => string;
  /** 我的指令 can be saved now (App's `writable`). */
  canCopy: boolean;
  /** Copy a phrase into 我的指令; only what `phraseCopy` allows, checked again there. */
  onCopy: (row: CloudRow) => void;
  onSync: () => void;
  onSetDisabled: (ids: string[], disabled: boolean) => void;
  onRetry: () => void;
  onOpenSettings: () => void;
}) {
  const [search, setSearch] = useState('');
  const [group, setGroup] = useState('全部');
  const [selected, setSelected] = useState<string[]>([]);
  const [viewing, setViewing] = useState<string | null>(null);
  const state = view.state;
  if (state === null)
    return (
      <div className="panel">
        <div className="empty">
          {view.loadError === null ? (
            <p>正在读取团队话术…</p>
          ) : (
            <>
              <strong>未能读取团队话术</strong>
              <p>{view.loadError}</p>
              <button onClick={onRetry}>重新读取</button>
            </>
          )}
        </div>
      </div>
    );
  if (!state.settings.enabled)
    return (
      <div className="panel">
        <div className="empty cloud-guide">
          <div className="empty-art" aria-hidden="true" />
          <strong>还没有订阅团队话术</strong>
          <p>
            团队话术由 TSim
            客服系统统一维护，订阅后自动同步到本机，与「我的指令」一起生效，但不会改动「我的指令」。
          </p>
          <button className="primary" onClick={onOpenSettings}>
            去参数设置开启
          </button>
        </div>
      </div>
    );
  const busy = view.pending !== null;
  const rows = cloudRows(state);
  const phrases = rows.filter(isPhrase);
  const unreadable = rows.filter((row) => !isPhrase(row));
  // An empty group is listed, counted and filtered as 「默认」, as in 我的指令.
  const groupCounts = countByGroup(phrases.map((p) => p.rule));
  const groups = ['全部', ...groupCounts.keys()];
  // A group gone with a sync shows them all again.
  const current = groups.includes(group) ? group : '全部';
  const needle = search.toLowerCase();
  const shown = phrases
    .filter(
      (p) =>
        (current === '全部' || groupKey(p.rule.group) === current) &&
        ruleSearchText(p.rule).toLowerCase().includes(needle),
    )
    .sort((a, b) => a.rule.match.value.localeCompare(b.rule.match.value));
  const shownUnreadable =
    current === '全部'
      ? unreadable.filter((row) =>
          `无法显示的团队话术 ${row.entry.id}`.toLowerCase().includes(needle),
        )
      : [];
  // Phrases gone with a sync are not selected any more.
  const ids = new Set(phrases.map((p) => p.entry.id));
  const chosen = selected.filter((id) => ids.has(id));
  const chosenSet = new Set(chosen);
  const detail = viewing === null ? null : (phrases.find((p) => p.entry.id === viewing) ?? null);
  return (
    <div className="panel">
      <SourceBar state={state} busy={busy} onSync={onSync} onOpenSettings={onOpenSettings} />
      <div className="toolbar">
        <input
          className="search"
          aria-label="搜索团队话术"
          placeholder="搜索缩写、说明或分组…"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
      </div>
      <div className="group-tabs">
        {groups.map((g) => (
          <button key={g} className={current === g ? 'active' : ''} onClick={() => setGroup(g)}>
            {g}
            <small>{g === '全部' ? rows.length : (groupCounts.get(g) ?? 0)}</small>
          </button>
        ))}
      </div>
      <div className="batch cloud-batch">
        <span>
          共 {rows.length} 条{unreadable.length > 0 && `（${unreadable.length} 条无法显示）`} · 已选{' '}
          {chosen.length} 条
        </span>
        <button
          disabled={busy || chosen.length === 0}
          onClick={() => {
            onSetDisabled(chosen, false);
          }}
        >
          本机启用
        </button>
        <button
          disabled={busy || chosen.length === 0}
          onClick={() => {
            onSetDisabled(chosen, true);
          }}
        >
          本机停用
        </button>
      </div>
      <div className="table-wrap">
        <table className="cloud-table">
          <thead>
            <tr>
              <th>
                <input
                  aria-label="全选团队话术"
                  type="checkbox"
                  checked={shown.length > 0 && shown.every((p) => chosenSet.has(p.entry.id))}
                  onChange={(e) =>
                    setSelected(e.target.checked ? shown.map((p) => p.entry.id) : [])
                  }
                />
              </th>
              <th>缩写词</th>
              <th>触发字符</th>
              <th>说明</th>
              <th>分组</th>
              <th>操作类型</th>
              <th>摘要</th>
              <th>本机停用</th>
              <th>状态</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            {shown.map((phrase) => {
              const { entry, rule } = phrase;
              const { reason } = phraseCopy(phrase);
              return (
                <tr
                  key={entry.id}
                  className={
                    entry.status === 'disabled_remote' || entry.status === 'disabled_local'
                      ? 'disabled-row'
                      : ''
                  }
                  onDoubleClick={() => setViewing(entry.id)}
                >
                  <td>
                    <input
                      aria-label={`选择 ${rule.match.value}`}
                      type="checkbox"
                      checked={chosenSet.has(entry.id)}
                      onChange={(e) =>
                        setSelected(
                          e.target.checked
                            ? [...chosen, entry.id]
                            : chosen.filter((i) => i !== entry.id),
                        )
                      }
                    />
                  </td>
                  <td>
                    <strong>{rule.match.value}</strong>
                    <span className="markers">
                      {rule.ignore_case && <i>Aa</i>}
                      {rule.trigger_mode === 'immediate' && <i>即时</i>}
                      {rule.match.kind === 'regex' && <i>正则</i>}
                    </span>
                  </td>
                  <td>
                    {rule.trigger_mode === 'immediate' ? (
                      '即时'
                    ) : (
                      <TerminatorList
                        keys={rule.terminator?.keys ?? terminators}
                        platform={platform}
                      />
                    )}
                    {!rule.terminator && rule.trigger_mode !== 'immediate' && (
                      <small className="muted">继承</small>
                    )}
                  </td>
                  <td>{rule.description || '—'}</td>
                  <td>{groupKey(rule.group)}</td>
                  <td>{actionLabel(rule.action)}</td>
                  <td className="summary">{summarizeAction(rule.action)}</td>
                  <td>
                    {/* This computer's choice, also for a phrase the table switched off (its
                      status says disabled_remote, §4): it holds once the table enables it again. */}
                    <input
                      aria-label={`本机停用 ${rule.match.value}`}
                      type="checkbox"
                      checked={localOff(entry)}
                      disabled={busy}
                      onChange={(e) => {
                        onSetDisabled([entry.id], e.target.checked);
                      }}
                    />
                  </td>
                  <td>
                    <StatusTag status={entry.status} />
                    <PhraseNote entry={entry} ruleName={ruleName} />
                  </td>
                  <td className="row-tools">
                    <button onClick={() => setViewing(entry.id)}>查看</button>
                    {/* The reason on the wrapper too: a disabled button may show no tooltip. */}
                    <span title={reason ?? undefined}>
                      <button
                        disabled={!canCopy || reason !== null}
                        title={reason ?? undefined}
                        onClick={() => {
                          onCopy(phrase);
                        }}
                      >
                        复制到我的指令
                      </button>
                    </span>
                  </td>
                </tr>
              );
            })}
            {shownUnreadable.map(({ entry }) => (
              <tr key={`unreadable-${entry.id}`} className="unreadable-row">
                <td />
                <td colSpan={9}>
                  <strong>无法显示的团队话术</strong>
                  <span className="unreadable-id">{entry.id}</span>
                  <StatusTag status={entry.status} />
                  <small className="muted">这条团队话术含有当前界面无法显示的内容。</small>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {shown.length === 0 && shownUnreadable.length === 0 && (
          <div className="empty">
            <strong>
              {search || current !== '全部'
                ? '没有匹配的团队话术'
                : state.status.revision === null
                  ? '还没有同步到团队话术'
                  : '团队话术表是空的'}
            </strong>
            <p>
              {state.status.revision === null
                ? '点「立即同步」从 TSim 客服系统下载团队话术。'
                : '团队话术由 TSim 客服系统维护，这里只能查看。'}
            </p>
          </div>
        )}
      </div>
      {detail && (
        <PhraseDetail
          phrase={detail}
          terminators={terminators}
          platform={platform}
          ruleName={ruleName}
          canCopy={canCopy}
          onCopy={onCopy}
          onClose={() => setViewing(null)}
        />
      )}
    </div>
  );
}

// ---- 「团队话术同步」 settings ----

interface Draft {
  enabled: boolean;
  base_url: string;
  auto_sync_minutes: number;
  /** The key typed; sent only when not empty. */
  key: string;
  /** What happens to a key already set: kept, replaced by `key`, or cleared. */
  keyMode: 'keep' | 'replace' | 'clear';
}

const draftOf = (settings: CloudSettings | null): Draft => ({
  enabled: settings?.enabled ?? false,
  base_url: settings?.base_url ?? '',
  auto_sync_minutes: settings?.auto_sync_minutes ?? 30,
  key: '',
  keyMode: 'keep',
});

const sameDraft = (a: Draft, b: Draft) => JSON.stringify(a) === JSON.stringify(b);

/** set_cloud_settings input: a key is sent only to replace or clear it. */
function inputOf(draft: Draft): CloudSettingsInput {
  const input: CloudSettingsInput = {
    enabled: draft.enabled,
    base_url: draft.base_url.trim(),
    auto_sync_minutes: draft.auto_sync_minutes,
  };
  if (draft.keyMode === 'clear') input.key = '';
  else if (draft.key !== '') input.key = draft.key;
  return input;
}

const settingNames: [string, string][] = [
  ['base_url', '服务器地址'],
  ['key', '同步密钥'],
  ['auto_sync_minutes', '自动同步间隔'],
];

/** 「保存失败」 text: field by field for set_cloud_settings' validation diagnostics. */
function saveErrorText(error: unknown): string {
  const parsed = parseCommandError(error);
  if (parsed.code === 'validation' && parsed.detail.diagnostics.length > 0)
    return parsed.detail.diagnostics
      .map((d) => {
        const name = settingNames.find(([key]) => d.path.endsWith(key))?.[1];
        return name ? `${name}：${diagnosticText(d)}` : diagnosticText(d);
      })
      .join('；');
  return describeError(parsed) ?? '已取消';
}

type Outcome = { tone: 'ok' | 'error'; text: string };

/** 「团队话术同步」 in 「文本指令参数设置」: saved on its own, apart from the settings above. */
export function CloudSettingsSection({
  view,
  focusOnMount = false,
  onSave,
  onSync,
}: {
  view: CloudView;
  /** Put the focus on the switch when shown (from the 团队话术 page's 「去参数设置」). */
  focusOnMount?: boolean;
  onSave: (input: CloudSettingsInput) => Promise<CloudState>;
  onSync: () => Promise<CloudState>;
}) {
  const settings = view.state?.settings ?? null;
  // The draft follows the settings as they change elsewhere until it is edited (the same
  // adjust-while-rendering pattern as App's editor errors).
  const [base, setBase] = useState(settings);
  const [draft, setDraft] = useState(() => draftOf(settings));
  if (JSON.stringify(settings) !== JSON.stringify(base)) {
    setBase(settings);
    if (sameDraft(draft, draftOf(base))) setDraft(draftOf(settings));
  }
  const [saving, setSaving] = useState(false);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const ready = view.status === 'ready';
  const switchRef = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (focusOnMount && ready) switchRef.current?.focus();
  }, [focusOnMount, ready]);
  const edit = (change: Partial<Draft>) => {
    setDraft({ ...draft, ...change });
    setOutcome(null);
  };
  const save = async () => {
    setSaving(true);
    setOutcome(null);
    try {
      let saved: CloudState;
      try {
        saved = await onSave(inputOf(draft));
      } catch (e) {
        setOutcome({ tone: 'error', text: `保存失败：${saveErrorText(e)}` });
        return;
      }
      // Saved: the key typed is not kept on screen.
      setBase(saved.settings);
      setDraft(draftOf(saved.settings));
      if (!saved.settings.enabled) {
        setOutcome({ tone: 'ok', text: '已保存，团队话术订阅已关闭。' });
        return;
      }
      let synced: CloudState;
      try {
        synced = await onSync();
      } catch (e) {
        setOutcome({ tone: 'error', text: `已保存，但没能开始同步：${saveErrorText(e)}` });
        return;
      }
      const { last_error: error, revision } = synced.status;
      setOutcome(
        error
          ? { tone: 'error', text: `已保存，但同步失败：${cloudErrorText(error)}` }
          : {
              tone: 'ok',
              text: `已保存并同步${revision === null ? '' : `，团队话术 v${revision}`}。`,
            },
      );
    } finally {
      setSaving(false);
    }
  };
  const hasKey = settings?.has_key ?? false;
  const intervals = AUTO_SYNC_MINUTES.includes(draft.auto_sync_minutes)
    ? AUTO_SYNC_MINUTES
    : [...AUTO_SYNC_MINUTES, draft.auto_sync_minutes];
  return (
    <>
      <h3>团队话术同步</h3>
      <p className="muted">
        订阅 TSim
        客服系统维护的团队话术表。同步只下载、不上传；团队话术与「我的指令」分开保存，不进导出和版本历史。「启用文本指令」总开关同时控制两者。
      </p>
      {!ready ? (
        <p className="muted">
          {view.loadError === null
            ? '正在读取团队话术设置…'
            : `未能读取团队话术设置：${view.loadError}`}
        </p>
      ) : (
        <>
          <fieldset className="settings-fields cloud-settings" disabled={saving}>
            <div className="field">
              <label className="switch">
                <input
                  type="checkbox"
                  ref={switchRef}
                  checked={draft.enabled}
                  onChange={(e) => edit({ enabled: e.target.checked })}
                />
                <span />
                订阅团队话术
              </label>
            </div>
            <label className="field">
              <span>服务器地址</span>
              <input
                value={draft.base_url}
                placeholder="https://"
                spellCheck={false}
                onChange={(e) => edit({ base_url: e.target.value })}
              />
              <small>TSim 客服系统的地址，只支持 https://（本机调试可用 http://localhost）。</small>
            </label>
            <div className="field">
              <span>同步密钥</span>
              <div className="key-state">
                {hasKey && draft.keyMode === 'keep' ? (
                  <>
                    <span className="cloud-status active">已设置</span>
                    <button onClick={() => edit({ keyMode: 'replace', key: '' })}>替换</button>
                    <button className="danger" onClick={() => edit({ keyMode: 'clear', key: '' })}>
                      清除
                    </button>
                  </>
                ) : draft.keyMode === 'clear' ? (
                  <>
                    <span className="cloud-status other">保存后清除</span>
                    <button onClick={() => edit({ keyMode: 'keep' })}>撤销</button>
                  </>
                ) : (
                  <>
                    <input
                      type="password"
                      aria-label="同步密钥"
                      autoComplete="new-password"
                      spellCheck={false}
                      placeholder={hasKey ? '输入新的同步密钥' : '粘贴管理员提供的同步密钥'}
                      value={draft.key}
                      onChange={(e) =>
                        edit({ key: e.target.value, keyMode: hasKey ? 'replace' : 'keep' })
                      }
                    />
                    {hasKey && (
                      <button onClick={() => edit({ keyMode: 'keep', key: '' })}>保留原密钥</button>
                    )}
                  </>
                )}
              </div>
              <small>密钥只保存在本机，保存后不再显示。</small>
            </div>
            <label className="field">
              <span>自动同步间隔</span>
              <select
                value={draft.auto_sync_minutes}
                onChange={(e) => edit({ auto_sync_minutes: Number(e.target.value) })}
              >
                {intervals.map((m) => (
                  <option key={m} value={m}>
                    {autoSyncLabel(m)}
                  </option>
                ))}
              </select>
            </label>
          </fieldset>
          <div className="inline">
            <button
              className="primary"
              disabled={saving || view.pending !== null}
              onClick={() => void save()}
            >
              {saving ? '正在保存…' : '保存并同步'}
            </button>
            {outcome && (
              <span
                className={outcome.tone === 'error' ? 'cloud-outcome error' : 'cloud-outcome'}
                role={outcome.tone === 'error' ? 'alert' : 'status'}
              >
                {outcome.text}
              </span>
            )}
          </div>
        </>
      )}
    </>
  );
}
