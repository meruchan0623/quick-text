import { Fragment, type ReactNode } from 'react';
import { capabilityNames, capabilityReasonText, label } from './ipc/labels';
import { Data, State } from './model';
const resultNames: Record<string, string> = {
  success: '执行成功',
  failed: '执行失败',
  failure: '执行失败',
  cancelled: '已停止',
  canceled: '已停止',
  unknown: '已发送 · 目标处理结果未知',
  pending: '等待执行',
  running: '执行中',
};
const compositionNames: Record<string, string> = {
  inactive: '当前没有正在选字的内容',
  active: '正在选字，仅完整候选与已输入缩写一致时可替换',
  unknown: '候选无法读取，使用按键兼容模式',
};
function targetSummary(state: State | null) {
  const detail: Data = state?.engine_detail ?? {};
  const espanso: Data | null = detail.espanso_input ?? null;
  if (espanso) {
    const heading = !state
      ? '尚未连接'
      : state.paused
        ? '监听已暂停'
        : espanso.ready
          ? '正在监听键盘缩写'
          : '输入监听尚未就绪';
    const rows = [
      ['输入后端', 'espanso Windows 输入组件'],
      [
        '输入来源',
        espanso.source === 'acceptance_sendinput_hook'
          ? '自动化验收输入（不代表实物键盘）'
          : 'Windows 原始键盘输入',
      ],
      ['目标程序', espanso.process || '以触发时的前台程序为准'],
      ['最近状态', espanso.blocked_reason || espanso.reason || '等待输入缩写'],
      ['正文读取', '无需读取输入框正文；发送后请查看目标结果'],
    ];
    return { heading, rows };
  }
  const physical: Data | null = detail.physical_input ?? null;
  const target: Data | null = physical ?? detail.target_observation ?? null;
  const heading = !state
    ? '尚未连接'
    : state.paused
      ? '监听已暂停'
      : target?.available
        ? physical
          ? target.composition === 'unknown'
            ? '候选无法读取，使用按键兼容模式'
            : '当前目标可进行按键缩写检查'
          : '当前输入框内容可读取'
        : '当前输入框暂不能确认是否可替换';
  const composition = target
    ? compositionNames[target.composition] || '尚未确认'
    : detail.ime_active
      ? '输入法处于活动状态'
      : detail.ime_committed_text_supported === false
        ? '当前输入框尚不能确认输入法文字'
        : '尚未确认';
  const rows = [
    ['目标程序', target?.process || '尚未确认'],
    [
      '检查方式',
      target?.provider || (physical ? '实际按键与输入目标检查' : '当前平台未提供目标观察详情'),
    ],
    ['可编辑状态', !target ? '尚未确认' : target.editable ? '可编辑' : '不可编辑或无法确认'],
    ['输入法选字状态', composition],
    [
      '阻止原因',
      target?.blocked_reason ||
        (physical ? null : detail.reason) ||
        (state?.paused
          ? '请先应用规则，再恢复监听。'
          : '未报告阻止原因；仍需符合触发边界与目标保护检查。'),
    ],
  ];
  if (physical)
    rows.push([
      '正文读取',
      detail.target_observation?.available
        ? '可读取，可追加正文核对'
        : '不可读取；发送结果可能无法确认',
    ]);
  if (target)
    rows.push([
      '上次检查距今',
      typeof target.age_ms === 'number' ? `${target.age_ms} 毫秒` : '未知',
    ]);
  return { heading, rows };
}
function TargetStatus({ state }: { state: State | null }) {
  const { heading, rows } = targetSummary(state);
  return (
    <>
      <h3>当前输入目标</h3>
      <p className="muted">
        状态由桌面引擎采样，切换应用后可能稍有延迟。打开本管理窗口也会改变焦点；这里不显示输入正文。
      </p>
      <div className="execution-result" role="status">
        <strong>{heading}</strong>
        <dl>
          {rows.map(([label, value]) => (
            <Fragment key={label}>
              <dt>{label}</dt>
              <dd>{value}</dd>
            </Fragment>
          ))}
        </dl>
      </div>
      {state?.platform === 'windows' && (
        <p className="muted">
          Windows 版使用 espanso
          输入组件识别缩写，匹配后退格并输出。支持立即执行、空格和分号触发；Tab、回车触发仍不可用。逐字键入与整段粘贴分别执行，输入框不提供正文也能发送。中文选字时，数字和空格可能用于选择候选词，建议切到英文输入后使用；发送结果仍需查看目标输入框。
        </p>
      )}
      <p className="muted">
        是否执行还取决于黑名单与目标焦点。已识别的密码框、只读输入框会被阻止。不能识别控件时，Windows
        仍按键盘输入尝试替换；不保证所有程序或输入法兼容。
      </p>
    </>
  );
}
function CapabilityTable({ capabilities }: { capabilities: Data[] }) {
  return (
    <>
      <h3>平台能力</h3>
      <p className="muted">
        支持状态来自当前桌面引擎。发送输入后，目标应用是否已完成处理可能无法确认。
      </p>
      <table className="capability-table">
        <thead>
          <tr>
            <th>功能</th>
            <th>当前支持状态</th>
            <th>原因与使用边界</th>
          </tr>
        </thead>
        <tbody>
          {capabilities.map((c: Data) => (
            <tr key={c.id}>
              <td>{label(capabilityNames, c.id)}</td>
              <td>
                <span className={c.available ? 'available' : 'unavailable'}>
                  {c.available ? '可用' : '不可用'}
                </span>
              </td>
              <td>{capabilityReasonText(c.reason) ?? '暂无额外说明'}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  );
}
function ExecutionStatus({ result }: { result: Data | null }) {
  return (
    <>
      <h3>最近执行结果</h3>
      {result ? (
        <div className="execution-result">
          <strong>{resultNames[result.status] || result.status}</strong>
          <dl>
            <dt>已完成步骤</dt>
            <dd>{result.completed_steps ?? 0}</dd>
            <dt>外部操作</dt>
            <dd>
              {result.side_effects
                ? '已经发生输入、点击或打开等操作；停止后不会撤销。'
                : '尚未报告已发生的外部操作。'}
            </dd>
            <dt>执行说明</dt>
            <dd>{result.message || '无额外说明'}</dd>
          </dl>
        </div>
      ) : (
        <p className="muted">暂无执行记录。完成一次文本指令或动作测试后会显示结果。</p>
      )}
    </>
  );
}
export default function RuntimeStatus({
  state,
  history,
}: {
  state: State | null;
  /** Live execution history (0.4 backends); without it the legacy `last_result` is shown. */
  history?: ReactNode;
}) {
  const result: Data | null = state?.last_result ?? null;
  return (
    <>
      <TargetStatus state={state} />
      <CapabilityTable capabilities={state?.capabilities ?? []} />
      {history ?? <ExecutionStatus result={result} />}
      <details className="expert-details">
        <summary>详细运行信息（高级）</summary>
        <pre>
          {JSON.stringify(
            {
              engine: state?.engine_detail,
              capabilities: state?.capabilities,
              last_result: result,
            },
            null,
            2,
          )}
        </pre>
      </details>
    </>
  );
}
