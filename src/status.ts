// Runtime display rules as pure functions. The status line follows the §7.2 Health mapping table
// (docs/development-plan.md) row by row, in the same order as Rust `StatusText::resolve`
// (crates/platform/src/health.rs), so the tray and this window always agree (S-03).
import type { HistoryRow, RuntimeView } from './ipc/events';
import {
  componentNames,
  healthNames,
  label,
  logLevelNames,
  other,
  outcomeNames,
  pauseNames,
  reasonNames,
  safeModeNames,
  selfTestNames,
} from './ipc/labels';
import type { EngineState, HealthSnapshot, HistoryEntry, StartupProblem } from './ipc/types';
import { keyNames } from './model';

/** One row of the mapping table; `unknown` is the fallback for a Health value newer than us. */
export type StatusKind =
  | 'starting'
  | 'failed'
  | 'recovering'
  | 'paused'
  | 'executing'
  | 'degraded'
  | 'listening'
  | 'unknown';

/**
 * Pause reasons shown as 「已暂停」 (Rust `PauseReasons::USER_VISIBLE`). `engine_down` and
 * `self_test` are expressed by the Health rows (故障、恢复中、正在启动) instead.
 */
export const USER_VISIBLE_PAUSES: readonly string[] = [
  'manual',
  'timed',
  'fullscreen',
  'recording',
  'safe_mode',
];

/** First matching row of the mapping table, top to bottom. */
export function resolveStatus(
  health: string,
  paused: readonly string[],
  busy: boolean,
): StatusKind {
  if (health === 'starting') return 'starting';
  if (health === 'failed') return 'failed';
  if (health === 'recovering') return 'recovering';
  if (health !== 'healthy' && health !== 'degraded') return 'unknown';
  if (paused.some((reason) => USER_VISIBLE_PAUSES.includes(reason))) return 'paused';
  // 执行中 overlays Healthy/Degraded only when nothing user-visible pauses the engine.
  if (busy) return 'executing';
  return health === 'degraded' ? 'degraded' : 'listening';
}

/** 「受影响项」: every component that is not healthy, in the backend's order. */
export function affectedComponents(health: HealthSnapshot | null): string[] {
  return (health?.components ?? [])
    .filter((c) => c.state !== 'healthy')
    .map((c) => label(componentNames, c.component));
}

/** The 「状态栏 / 旧界面运行状态区」 column of the mapping table. */
export function statusText(
  kind: StatusKind,
  paused: readonly string[],
  health: HealthSnapshot | null,
): string {
  switch (kind) {
    case 'starting':
      return '正在启动…';
    case 'failed':
      return '输入监听已停止';
    case 'recovering':
      return '正在恢复…';
    case 'paused':
      // The contract carries no resume time, so a timed pause cannot show 「14:30 自动恢复」 yet.
      return paused.includes('safe_mode') ? '已暂停（安全模式）' : '已暂停';
    case 'executing':
      return '执行中';
    case 'degraded': {
      const items = affectedComponents(health);
      return items.length > 0 ? `部分功能降级：${items.join('、')}` : '部分功能降级';
    }
    case 'listening':
      return '监听中';
    case 'unknown':
      return `引擎状态：${health ? label(healthNames, health.state) : '未知'}`;
  }
}

export interface LiveStatus {
  kind: StatusKind;
  text: string;
  /** Whether the user's own pause is set: the header button toggles only this reason. */
  manualPause: boolean;
}

/** Status from pushed events; `null` until the first snapshot or with a pre-0.4 backend. */
export function liveStatus(view: RuntimeView): LiveStatus | null {
  if (view.status !== 'ready' || !view.health || !view.engine) return null;
  const paused = view.engine.paused_reasons;
  const kind = resolveStatus(view.health.state, paused, view.engine.busy);
  return {
    kind,
    text: statusText(kind, paused, view.health),
    manualPause: paused.includes('manual'),
  };
}

// ---- startup problems and banners ----

const str = (v: unknown): string => (typeof v === 'string' ? v : '');
const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const strs = (v: unknown): string[] =>
  Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];

const pad = (n: number) => String(n).padStart(2, '0');

/** Local wall-clock time, `HH:MM` or `HH:MM:SS`. */
export function clock(ms: number, seconds = false): string {
  const d = new Date(ms);
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  return seconds ? `${hm}:${pad(d.getSeconds())}` : hm;
}

function listIds(ids: string[], total: number): string {
  const shown = ids.slice(0, 3).map((id) => `「${id}」`);
  return shown.join('、') + (Math.max(total, ids.length) > shown.length ? ' 等' : '');
}

/** What the engine is doing now, as far as a problem's wording depends on it. */
export type EngineNow = Pick<EngineState, 'listening' | 'paused_reasons'>;

/**
 * One sentence per startup-problem kind; an unknown kind is 「其他」 with its raw name (contract §1).
 * `engine` is the engine-state shown now (null while it is not known): a banner stands all session,
 * so a sentence about listening follows the state as it changes, not the one the problem came with.
 */
export function problemText(problem: StartupProblem, engine: EngineNow | null = null): string {
  const a = problem.args;
  switch (problem.kind) {
    case 'safe_mode': {
      const reason = str(a.reason);
      return `已进入安全模式：${reason ? label(safeModeNames, reason) : '原因未知'}。输入监听已暂停，设置暂时不能修改；可以导入配置或打开数据目录检查。`;
    }
    case 'recovered': {
      const revision = num(a.revision);
      const from =
        a.from === 'last_good'
          ? '上次正常运行时的配置'
          : a.from === 'revision'
            ? revision === null
              ? '历史版本'
              : `历史版本 v${revision} ` // keep a space between the number and 「恢复」
            : a.from === 'previous'
              ? '上一个版本'
              : '备份';
      return `配置文件无法读取，已自动从${from}恢复。请检查最近的改动是否还在。`;
    }
    case 'config_quarantined': {
      const file = str(a.file);
      const moved = str(a.quarantined_as);
      const what = file ? `${file} 已损坏` : '配置文件已损坏';
      return moved
        ? `${what}，已改名为 ${moved} 保留，可在数据目录中查看。`
        : `${what}，已改名保留，可在数据目录中查看。`;
    }
    case 'rules_skipped': {
      const ids = strs(a.ids);
      const count = num(a.count) ?? ids.length;
      const which = ids.length > 0 ? `：${listIds(ids, count)}` : '';
      return `有 ${count} 条规则无法读取或无法使用，已跳过${which}。其他规则不受影响。`;
    }
    case 'terminator_unsupported': {
      const keys = strs(a.keys).map((k) => label(keyNames, k));
      const which = keys.length > 0 ? keys.join('、') : '部分按键';
      return `配置中的触发键 ${which} 在当前平台上暂不支持，按下它们不会执行指令；可在「文本指令参数设置」中改用空格或分号。`;
    }
    case 'autostart_repaired':
      return '登录自动启动项指向的程序位置已过期，已自动修正。';
    case 'self_test_failed': {
      const checks = strs(a.checks).map((c) => label(selfTestNames, c));
      const which = checks.length > 0 ? `：${checks.join('、')}` : '';
      // S-09 / B-10: a failed self-test keeps the engine paused instead of starting to listen, but
      // keeps a resume the user made while startup waited (contract §5 「自检」), and the user may
      // resume or pause later. `listening` is the backend's own verdict (Health accepts a resume and
      // no pause reason at all), so a pause for any other reason, or a failed engine, never reads
      // as listening; 「恢复监听」 is pointed to only while the user's own pause is set, the one
      // reason it clears (the header button reads 「暂停监听」 otherwise).
      if (engine?.listening)
        return `启动自检未通过${which}。输入监听当前已开启，但可能无法正常工作；可在「运行状态与恢复」中查看详情或导出诊断包。`;
      if (engine && !engine.paused_reasons.includes('manual'))
        return `启动自检未通过${which}，输入监听可能无法正常工作。可在「运行状态与恢复」中查看详情或导出诊断包。`;
      return `启动自检未通过${which}，没有自动开始监听。可在「运行状态与恢复」中查看详情或导出诊断包；排除问题后可点「恢复监听」。`;
    }
    case 'log_level_elevated': {
      const until = num(a.until_ms);
      const level = str(a.level) ? label(logLevelNames, str(a.level)) : '详细';
      return until === null
        ? `日志已临时调为「${level}」级别。`
        : `日志已临时调为「${level}」级别，将于 ${clock(until)} 自动恢复。`;
    }
    case 'store_meta_reset':
      return '启动记录文件无法读取，已重新建立；连续启动失败的计数已清零。';
    default:
      return `启动时发现一个问题：${other(problem.kind)}。可在「运行状态与恢复」中导出诊断包了解详情。`;
  }
}

export interface BannerItem {
  /** Stable identity: a dismissed banner stays hidden for the rest of the session. */
  key: string;
  kind: string;
  tone: 'error' | 'warning' | 'info';
  text: string;
}

const toneOrder = { error: 0, warning: 1, info: 2 } as const;
const toneOf = (severity: string): BannerItem['tone'] =>
  severity === 'error' || severity === 'info' ? severity : 'warning';

/**
 * Banners in priority order (§6.3 ⑪): safe mode first, then startup problems by severity.
 * A `safe_mode` problem is the same fact as the snapshot's `safe_mode`, so it is shown once.
 * The text follows `engine` (see problemText); the key is the problem's alone, so a dismissed
 * banner stays dismissed whatever its text becomes.
 */
export function bannerItems(
  safeMode: string | null,
  problems: StartupProblem[],
  engine: EngineNow | null = null,
): BannerItem[] {
  const items: BannerItem[] = [];
  if (safeMode)
    items.push({
      key: `safe_mode:${safeMode}`,
      kind: 'safe_mode',
      tone: 'error',
      text: problemText({ kind: 'safe_mode', severity: 'error', args: { reason: safeMode } }),
    });
  const rest = problems
    .filter((p) => !(safeMode && p.kind === 'safe_mode'))
    .map((p): BannerItem => ({
      key: `${p.kind}:${JSON.stringify(p.args)}`,
      kind: p.kind,
      tone: toneOf(p.severity),
      text: problemText(p, engine),
    }));
  // Array.prototype.sort is stable, so equal severities keep the backend's order.
  rest.sort((a, b) => toneOrder[a.tone] - toneOrder[b.tone]);
  return [...items, ...rest];
}

// ---- execution history ----

/** Total duration for the history list; `null` when the backend did not measure it. */
export function formatDuration(us: number | null): string | null {
  if (us === null || !Number.isFinite(us) || us < 0) return null;
  if (us < 1000) return '<1 毫秒';
  if (us < 1_000_000) return `${Math.round(us / 1000)} 毫秒`;
  return `${(us / 1_000_000).toFixed(1)} 秒`;
}

/** 「结果：原因」, without the reason when it is just `ok`. */
export function outcomeText(entry: Pick<HistoryEntry, 'outcome' | 'reason'>): string {
  const outcome = label(outcomeNames, entry.outcome);
  return entry.reason === 'ok' ? outcome : `${outcome}：${label(reasonNames, entry.reason)}`;
}

/** 「另有 N 条」 for rows that stand in for rate-limited pushes. */
export const suppressedText = (row: HistoryRow): string | null =>
  row.suppressed > 0 ? `另有 ${row.suppressed} 条` : null;

export const pauseText = (reasons: readonly string[]): string =>
  reasons.map((r) => label(pauseNames, r)).join('、');
