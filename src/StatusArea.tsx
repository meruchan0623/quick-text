// Runtime status area of the 0.4 shell (S-03, S-19, F-25): banners, health card and the recent
// executions list. Everything here renders the RuntimeView kept in sync by pushed events; display
// rules live in ./status.ts.
import { useState } from 'react';
import type { HistoryRow, RuntimeView } from './ipc/events';
import { componentNames, healthNames, label, reasonNames, triggerNames } from './ipc/labels';
import {
  bannerItems,
  clock,
  formatDuration,
  liveStatus,
  outcomeText,
  pauseText,
  suppressedText,
} from './status';

/** One banner at a time, highest priority first; 「知道了」 hides it for this session (§6.3 ⑪). */
export function BannerStack({
  view,
  onOpenDataDir,
}: {
  view: RuntimeView;
  onOpenDataDir?: (() => void) | undefined;
}) {
  const [dismissed, setDismissed] = useState<ReadonlySet<string>>(() => new Set());
  const visible = bannerItems(view.safeMode, view.problems).filter((b) => !dismissed.has(b.key));
  const [first] = visible;
  if (!first) return null;
  return (
    <div className={`banner ${first.tone}`} role={first.tone === 'error' ? 'alert' : 'status'}>
      <span>{first.text}</span>
      {visible.length > 1 && <small>还有 {visible.length - 1} 条提示</small>}
      {first.kind === 'safe_mode' && onOpenDataDir && (
        <button onClick={onOpenDataDir}>打开数据目录</button>
      )}
      <button onClick={() => setDismissed(new Set([...dismissed, first.key]))}>知道了</button>
    </div>
  );
}

/** Overall health, the mapping-table status line and the per-component states. */
export function HealthCard({
  view,
  busy,
  onRepair,
}: {
  view: RuntimeView;
  busy: boolean;
  onRepair: () => void;
}) {
  const live = liveStatus(view);
  if (!live || !view.health || !view.engine)
    return (
      <p className="muted">
        {view.status === 'unsupported'
          ? '当前桌面引擎版本较旧，不提供实时健康状态；以下信息在切回本窗口或点击「刷新状态」时更新。'
          : '正在读取引擎状态…'}
      </p>
    );
  const { health, engine } = view;
  return (
    <div className="health-card">
      <div className="health-head">
        <span className={`health-tag ${health.state}`}>{label(healthNames, health.state)}</span>
        <strong role="status">{live.text}</strong>
        {live.kind === 'failed' && (
          <button className="primary" disabled={busy} onClick={onRepair}>
            修复引擎
          </button>
        )}
      </div>
      <dl>
        {engine.paused_reasons.length > 0 && (
          <>
            <dt>暂停原因</dt>
            <dd>{pauseText(engine.paused_reasons)}</dd>
          </>
        )}
        <dt>当前前台程序</dt>
        <dd>{engine.current_process ?? '尚未确认'}</dd>
      </dl>
      {health.components.length > 0 && (
        <table className="component-table">
          <thead>
            <tr>
              <th>组件</th>
              <th>状态</th>
              <th>原因</th>
            </tr>
          </thead>
          <tbody>
            {health.components.map((c) => (
              <tr key={c.component}>
                <td>{label(componentNames, c.component)}</td>
                <td>
                  <span className={`health-tag ${c.state}`}>{label(healthNames, c.state)}</span>
                </td>
                <td>{c.reason && c.reason !== 'ok' ? label(reasonNames, c.reason) : '—'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

/** Recent executions (F-25 minimal): `list_history` plus `execution-result` pushes. */
export function HistoryList({
  rows,
  available,
  ruleName,
}: {
  rows: readonly HistoryRow[];
  available: boolean;
  ruleName: (id: string) => string;
}) {
  return (
    <>
      <h3>最近执行</h3>
      <p className="muted">
        只保存在内存中，不含正文；最多 200 条，退出程序后清空。
        {!available && ' 桌面引擎暂未提供完整记录，下面只列出本窗口打开后收到的结果。'}
      </p>
      {rows.length > 0 ? (
        <table className="history-table">
          <thead>
            <tr>
              <th>时间</th>
              <th>指令</th>
              <th>方式</th>
              <th>程序</th>
              <th>结果</th>
              <th>耗时</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => {
              const { entry } = row;
              const more = suppressedText(row);
              return (
                <tr key={entry.seq} className={`outcome-${entry.outcome}`}>
                  <td>{clock(entry.at_ms, true)}</td>
                  <td>{ruleName(entry.rule_id)}</td>
                  <td>{label(triggerNames, entry.trigger)}</td>
                  <td>{entry.process ?? '—'}</td>
                  <td>
                    {outcomeText(entry)}
                    {more && <small> · {more}</small>}
                  </td>
                  <td>{formatDuration(entry.timings.total_us) ?? '—'}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      ) : (
        <p className="muted">暂无执行记录。完成一次文本指令或动作测试后会显示在这里。</p>
      )}
    </>
  );
}
