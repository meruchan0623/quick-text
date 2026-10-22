import { describe, expect, it } from 'vitest';
import { initialView, type RuntimeView } from './ipc/events';
import type {
  ComponentHealth,
  EngineState,
  HealthSnapshot,
  HealthState,
  StartupProblem,
} from './ipc/types';
import {
  affectedComponents,
  bannerItems,
  clock,
  formatDuration,
  liveStatus,
  outcomeText,
  pauseText,
  problemText,
  resolveStatus,
  statusText,
  suppressedText,
  USER_VISIBLE_PAUSES,
} from './status';

const component = (
  name: ComponentHealth['component'],
  state: HealthState = 'healthy',
): ComponentHealth => ({ component: name, state, reason: null, since_ms: 0 });

const health = (state: HealthState, components: ComponentHealth[] = []): HealthSnapshot => ({
  state,
  components,
});

describe('resolveStatus (§7.2 Health mapping table, same order as StatusText::resolve)', () => {
  // The cases of crates/platform/src/health.rs `mapping_table_order`, one for one.
  it.each([
    ['starting', ['manual'], true, 'starting'],
    ['failed', ['engine_down'], false, 'failed'],
    ['recovering', ['engine_down'], true, 'recovering'],
    ['healthy', ['manual'], true, 'paused'],
    ['degraded', ['manual'], false, 'paused'],
    ['healthy', ['engine_down'], true, 'executing'],
    ['degraded', [], false, 'degraded'],
    ['healthy', [], false, 'listening'],
  ] as const)('%s + %j + busy=%s → %s', (state, paused, busy, expected) => {
    expect(resolveStatus(state, paused, busy)).toBe(expected);
  });

  it('treats every user-visible reason as 已暂停 and the others as not paused', () => {
    for (const reason of USER_VISIBLE_PAUSES)
      expect(resolveStatus('healthy', [reason], false)).toBe('paused');
    expect(resolveStatus('healthy', ['engine_down', 'self_test'], false)).toBe('listening');
    expect(resolveStatus('degraded', ['self_test'], true)).toBe('executing');
  });

  it('puts the Health rows above any pause reason', () => {
    expect(resolveStatus('failed', ['manual', 'safe_mode'], true)).toBe('failed');
    expect(resolveStatus('starting', ['safe_mode'], false)).toBe('starting');
  });

  it('does not pretend to be listening for a Health value it does not know', () => {
    expect(resolveStatus('hibernating', [], false)).toBe('unknown');
  });
});

describe('statusText', () => {
  it('uses the status-area column of the table', () => {
    expect(statusText('starting', [], null)).toBe('正在启动…');
    expect(statusText('failed', ['engine_down'], null)).toBe('输入监听已停止');
    expect(statusText('recovering', [], null)).toBe('正在恢复…');
    expect(statusText('paused', ['manual'], null)).toBe('已暂停');
    expect(statusText('paused', ['timed', 'safe_mode'], null)).toBe('已暂停（安全模式）');
    expect(statusText('executing', [], null)).toBe('执行中');
    expect(statusText('listening', [], null)).toBe('监听中');
    expect(statusText('unknown', [], health('hibernating' as HealthState))).toBe(
      '引擎状态：其他（hibernating）',
    );
  });

  it('lists the affected components for Degraded', () => {
    const snapshot = health('degraded', [
      component('keyboard_hook'),
      component('focus_events', 'degraded'),
      component('clipboard', 'failed'),
      component('teleporter' as ComponentHealth['component'], 'degraded'),
    ]);
    expect(affectedComponents(snapshot)).toEqual(['前台窗口跟踪', '剪贴板', '其他（teleporter）']);
    expect(statusText('degraded', [], snapshot)).toBe(
      '部分功能降级：前台窗口跟踪、剪贴板、其他（teleporter）',
    );
    expect(statusText('degraded', [], health('degraded'))).toBe('部分功能降级');
  });
});

describe('liveStatus', () => {
  const ready = (overrides: Partial<RuntimeView>): RuntimeView => ({
    ...initialView,
    status: 'ready',
    health: health('healthy'),
    engine: { listening: true, paused_reasons: [], busy: false, current_process: null },
    ...overrides,
  });

  it('is null until a snapshot arrives and for a 0.3 backend', () => {
    expect(liveStatus(initialView)).toBeNull();
    expect(liveStatus({ ...initialView, status: 'unsupported' })).toBeNull();
  });

  it('reports the row, its text and whether the manual pause is set', () => {
    expect(liveStatus(ready({}))).toEqual({
      kind: 'listening',
      text: '监听中',
      manualPause: false,
    });
    const paused = ready({
      engine: {
        listening: false,
        paused_reasons: ['safe_mode', 'manual'],
        busy: false,
        current_process: null,
      },
    });
    expect(liveStatus(paused)).toEqual({
      kind: 'paused',
      text: '已暂停（安全模式）',
      manualPause: true,
    });
    expect(liveStatus(ready({ health: health('failed') }))?.text).toBe('输入监听已停止');
  });
});

describe('problemText', () => {
  it('has Chinese text for every 0.4 kind', () => {
    const cases: [string, Record<string, unknown>, string][] = [
      [
        'safe_mode',
        { reason: 'crash_loop' },
        '已进入安全模式：连续多次启动失败。输入监听已暂停，设置暂时不能修改；可以导入配置或打开数据目录检查。',
      ],
      [
        'recovered',
        { from: 'revision', revision: 41 },
        '配置文件无法读取，已自动从历史版本 v41 恢复。请检查最近的改动是否还在。',
      ],
      [
        'recovered',
        { from: 'last_good', revision: null },
        '配置文件无法读取，已自动从上次正常运行时的配置恢复。请检查最近的改动是否还在。',
      ],
      [
        'config_quarantined',
        { file: 'active.json', quarantined_as: 'active.corrupt-20261022.json' },
        'active.json 已损坏，已改名为 active.corrupt-20261022.json 保留，可在数据目录中查看。',
      ],
      [
        'rules_skipped',
        { count: 5, ids: ['a', 'b', 'c', 'd', 'e'] },
        '有 5 条规则无法读取或无法使用，已跳过：「a」、「b」、「c」 等。其他规则不受影响。',
      ],
      [
        'terminator_unsupported',
        { keys: ['tab', 'enter'] },
        '配置中的触发键 Tab、回车 在当前平台上暂不支持，按下它们不会执行指令；可在「文本指令参数设置」中改用空格或分号。',
      ],
      ['autostart_repaired', {}, '登录自动启动项指向的程序位置已过期，已自动修正。'],
      [
        'self_test_failed',
        { checks: ['raw_input', 'hook_heartbeat'] },
        '启动自检未通过：原始键盘输入、键盘监听心跳，没有自动开始监听。可在「运行状态与恢复」中查看详情或导出诊断包；排除问题后可点「恢复监听」。',
      ],
      ['log_level_elevated', { level: 'debug', until_ms: null }, '日志已临时调为「调试」级别。'],
      ['store_meta_reset', {}, '启动记录文件无法读取，已重新建立；连续启动失败的计数已清零。'],
    ];
    for (const [kind, args, text] of cases)
      expect(problemText({ kind, severity: 'warning', args })).toBe(text);
  });

  it('formats the log level deadline as local time', () => {
    const until = new Date(2026, 9, 22, 14, 5).getTime();
    expect(
      problemText({
        kind: 'log_level_elevated',
        severity: 'info',
        args: { level: 'debug', until_ms: until },
      }),
    ).toBe('日志已临时调为「调试」级别，将于 14:05 自动恢复。');
  });

  it('survives missing or malformed args', () => {
    expect(problemText({ kind: 'rules_skipped', severity: 'warning', args: {} })).toBe(
      '有 0 条规则无法读取或无法使用，已跳过。其他规则不受影响。',
    );
    expect(
      problemText({ kind: 'config_quarantined', severity: 'warning', args: { file: 3 } }),
    ).toBe('配置文件已损坏，已改名保留，可在数据目录中查看。');
    expect(
      problemText({ kind: 'safe_mode', severity: 'error', args: { reason: 'solar_flare' } }),
    ).toMatch(/^已进入安全模式：其他（solar_flare）。/);
  });

  it('shows an unknown kind verbatim with a generic text', () => {
    expect(problemText({ kind: 'disk_almost_full', severity: 'warning', args: { free: 1 } })).toBe(
      '启动时发现一个问题：其他（disk_almost_full）。可在「运行状态与恢复」中导出诊断包了解详情。',
    );
  });
});

describe('self_test_failed, worded by the engine state now (review A6-2)', () => {
  const failed: StartupProblem = {
    kind: 'self_test_failed',
    severity: 'warning',
    args: { checks: ['clipboard'] },
  };
  const PAUSED =
    '启动自检未通过：剪贴板，没有自动开始监听。可在「运行状态与恢复」中查看详情或导出诊断包；排除问题后可点「恢复监听」。';

  it('says listening is on, and points to no resume, while the engine listens', () => {
    const text = problemText(failed, { listening: true, paused_reasons: [] });
    expect(text).toBe(
      '启动自检未通过：剪贴板。输入监听当前已开启，但可能无法正常工作；可在「运行状态与恢复」中查看详情或导出诊断包。',
    );
    expect(text).not.toMatch(/恢复监听|没有自动开始监听/);
  });

  it('keeps 「没有自动开始监听」 and 「恢复监听」 while the user pause is set', () => {
    expect(problemText(failed, { listening: false, paused_reasons: ['manual'] })).toBe(PAUSED);
    // With another reason too: resuming clears the user pause, the rest stands.
    expect(problemText(failed, { listening: false, paused_reasons: ['safe_mode', 'manual'] })).toBe(
      PAUSED,
    );
    // Not known yet: the backend's default, paused.
    expect(problemText(failed)).toBe(PAUSED);
    expect(problemText(failed, null)).toBe(PAUSED);
  });

  it('claims neither listening nor a resume to click while paused otherwise', () => {
    for (const paused_reasons of [['recording'], ['fullscreen'], ['safe_mode'], ['engine_down']]) {
      const text = problemText(failed, {
        listening: false,
        paused_reasons: paused_reasons as EngineState['paused_reasons'],
      });
      expect(text).toBe(
        '启动自检未通过：剪贴板，输入监听可能无法正常工作。可在「运行状态与恢复」中查看详情或导出诊断包。',
      );
    }
  });

  it('keeps one banner key whatever the text', () => {
    const [listening] = bannerItems(null, [failed], { listening: true, paused_reasons: [] });
    const [paused] = bannerItems(null, [failed], { listening: false, paused_reasons: ['manual'] });
    expect(listening?.key).toBe(paused?.key);
    expect(listening?.text).not.toBe(paused?.text);
    expect(paused?.text).toBe(PAUSED);
  });
});

describe('bannerItems', () => {
  it('orders safe mode first, then by severity, and shows safe mode once', () => {
    const items = bannerItems('locked', [
      { kind: 'autostart_repaired', severity: 'info', args: {} },
      { kind: 'safe_mode', severity: 'error', args: { reason: 'locked' } },
      { kind: 'rules_skipped', severity: 'warning', args: { count: 1, ids: ['x'] } },
      { kind: 'mystery', severity: 'fatal' as 'error', args: {} },
    ]);
    expect(items.map((i) => [i.kind, i.tone])).toEqual([
      ['safe_mode', 'error'],
      ['rules_skipped', 'warning'],
      ['mystery', 'warning'],
      ['autostart_repaired', 'info'],
    ]);
    expect(new Set(items.map((i) => i.key)).size).toBe(items.length);
  });

  it('keeps a safe_mode problem when the snapshot has no safe mode yet', () => {
    const items = bannerItems(null, [
      { kind: 'safe_mode', severity: 'error', args: { reason: 'crash_loop' } },
    ]);
    expect(items).toHaveLength(1);
    expect(items[0]?.text).toMatch(/连续多次启动失败/);
  });
});

describe('history formatting', () => {
  it('formats durations', () => {
    expect(formatDuration(null)).toBeNull();
    expect(formatDuration(-1)).toBeNull();
    expect(formatDuration(400)).toBe('<1 毫秒');
    expect(formatDuration(182_400)).toBe('182 毫秒');
    expect(formatDuration(1_250_000)).toBe('1.3 秒');
  });

  it('joins outcome and reason in Chinese', () => {
    expect(outcomeText({ outcome: 'succeeded', reason: 'ok' })).toBe('已完成');
    expect(outcomeText({ outcome: 'dropped_busy', reason: 'dropped_busy' })).toBe(
      '未执行：上一条仍在执行',
    );
    expect(outcomeText({ outcome: 'skipped', reason: 'protected_control' })).toBe(
      '已跳过：目标是密码框或只读控件',
    );
    expect(outcomeText({ outcome: 'exploded' as 'failed', reason: 'gremlins' as 'internal' })).toBe(
      '其他（exploded）：其他（gremlins）',
    );
  });

  it('notes rate-limited rows as 另有 N 条', () => {
    const entry = {
      seq: 1,
      at_ms: 0,
      rule_id: 'r',
      trigger: 'immediate',
      outcome: 'succeeded',
      reason: 'ok',
      process: null,
      completed_steps: 1,
      side_effects: true,
      timings: {
        queue_us: null,
        resolve_us: null,
        erase_us: null,
        output_us: null,
        restore_us: null,
        total_us: null,
      },
    } as const;
    expect(suppressedText({ entry, suppressed: 0 })).toBeNull();
    expect(suppressedText({ entry, suppressed: 4 })).toBe('另有 4 条');
  });

  it('formats clock times and pause reasons', () => {
    const at = new Date(2026, 0, 2, 3, 4, 5).getTime();
    expect(clock(at)).toBe('03:04');
    expect(clock(at, true)).toBe('03:04:05');
    expect(pauseText(['safe_mode', 'manual', 'later'])).toBe('安全模式、手动暂停、其他（later）');
  });
});
