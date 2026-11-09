// Runtime status of the 0.4 shell (S-03, S-19, F-25): banners and the recent executions (日志).
// Everything here renders the RuntimeView kept in sync by pushed events; display rules live in
// ./status.ts.
import { useState } from 'react';
import type { HistoryRow, RuntimeView } from './ipc/events';
import { label, triggerNames } from './ipc/labels';
import { bannerItems, clock, formatDuration, outcomeText, suppressedText } from './status';

/** One banner at a time, highest priority first; 「知道了」 hides it for this session (§6.3 ⑪). */
export function BannerStack({
  view,
  onOpenDataDir,
}: {
  view: RuntimeView;
  onOpenDataDir?: (() => void) | undefined;
}) {
  const [dismissed, setDismissed] = useState<ReadonlySet<string>>(() => new Set());
  // Worded by the engine-state shown now (as the status line), keyed by the problem alone.
  const engine = view.status === 'ready' ? view.engine : null;
  const visible = bannerItems(view.safeMode, view.problems, engine).filter(
    (b) => !dismissed.has(b.key),
  );
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

/** 日志: recent executions (F-25 minimal), `list_history` plus `execution-result` pushes. */
export function HistoryList({
  rows,
  ruleName,
}: {
  rows: readonly HistoryRow[];
  ruleName: (id: string) => string;
}) {
  return (
    <>
      <h3>日志</h3>
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
        <p className="muted">暂无执行记录</p>
      )}
    </>
  );
}
