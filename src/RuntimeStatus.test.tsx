import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { describe, expect, it } from 'vitest';
import RuntimeStatus from './RuntimeStatus';
import type { State } from './model';

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const entry = (id: string, status: string, reason: string | null = null) => ({
  id,
  status,
  reason,
  available: status !== 'unavailable',
});

function render(platform: string, capabilities: unknown) {
  const host = document.createElement('div');
  const root = createRoot(host);
  const state = { platform, capabilities, paused: false } as unknown as State;
  act(() => {
    root.render(<RuntimeStatus state={state} />);
  });
  const rows = [...host.querySelectorAll('.capability-table tbody tr')].map((tr) => ({
    className: tr.className,
    cells: [...tr.querySelectorAll('td')].map((td) => td.textContent),
  }));
  const text = host.textContent;
  act(() => {
    root.unmount();
  });
  return { text, rows };
}

describe('CapabilityTable', () => {
  it('tags, grays out and hides capabilities by status (principle 5)', () => {
    const { text, rows } = render('windows', [
      entry('keys', 'available'),
      entry('html', 'experimental', 'experimental_platform'),
      entry('keyboard_hook', 'degraded', 'hook_missing'),
      entry('recording', 'unavailable', 'engine_down'),
      entry('quicker_private_action', 'unavailable', 'not_implemented'),
    ]);
    expect(rows.map((r) => r.className)).toEqual([
      'capability-row available',
      'capability-row experimental',
      'capability-row degraded',
      'capability-row unavailable',
    ]);
    expect(rows.map((r) => r.cells.slice(0, 2))).toEqual([
      ['逐字键入', '可用'],
      ['粘贴 HTML', '实验性'],
      ['键盘监听', '降级'],
      ['键鼠录制', '不可用'],
    ]);
    expect(rows[0]?.cells[2]).toBe('暂无额外说明');
    expect(rows[2]?.cells[2]).toMatch(/键盘/);
    for (const row of rows) expect(row.cells.join('')).not.toMatch(/[a-z]+_[a-z_]+/);
    expect(text).toMatch(/本平台不提供：Quicker 专有动作。/);
    expect(text).not.toMatch(/macOS 版尚未经过实机验收/);
  });

  it('marks everything experimental on macOS, also for a backend without statuses', () => {
    const { text, rows } = render('macos', [
      { id: 'keys', available: true, reason: null },
      { id: 'mouse', available: false, reason: 'permission_missing' },
    ]);
    expect(text).toMatch(/macOS 版尚未经过实机验收/);
    expect(rows.map((r) => r.cells.slice(0, 2))).toEqual([
      ['逐字键入', '实验性'],
      ['鼠标操作', '不可用'],
    ]);
    expect(rows[1]?.cells[2]).toMatch(/缺少系统权限/);
  });

  it('survives capabilities of an unexpected shape', () => {
    for (const odd of [
      undefined,
      null,
      'keys',
      { keys: true },
      [null, 7, { id: 'x', status: 9 }],
    ]) {
      const { rows } = render('windows', odd);
      expect(rows.length).toBeLessThanOrEqual(1);
    }
  });
});
