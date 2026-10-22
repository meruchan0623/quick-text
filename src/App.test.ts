// App-level checks of the S-19 runtime status area against the mock backend: event-driven status
// line, repair, banners, history, diagnostics export and a 0.3 backend without the new commands.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { MockBackend, MockOptions } from './ipc/mock';

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

type Act = (callback: () => Promise<void> | void) => Promise<void>;

interface Mounted {
  backend: MockBackend;
  container: HTMLElement;
  act: Act;
}

const cleanups: (() => Promise<void>)[] = [];

beforeEach(() => {
  // jsdom has no modal dialogs; the Ask dialog only needs the open state.
  HTMLDialogElement.prototype.showModal = function showModal(this: HTMLDialogElement) {
    this.setAttribute('open', '');
  };
  HTMLDialogElement.prototype.close = function close(this: HTMLDialogElement) {
    this.removeAttribute('open');
  };
});

afterEach(async () => {
  for (const cleanup of cleanups.splice(0)) await cleanup();
});

/**
 * Render App against a fresh mock backend. Modules are reset so the app-wide RuntimeSync and the
 * transport start from scratch; React is imported from the same fresh module graph as App.
 */
async function mount(
  options: MockOptions = {},
  setup?: (backend: MockBackend) => void,
): Promise<Mounted> {
  vi.resetModules();
  const react = await import('react');
  const { createRoot } = await import('react-dom/client');
  const { MockBackend, createMockTransport } = await import('./ipc/mock');
  const { setTransport } = await import('./ipc/transport');
  const { default: App } = await import('./App');
  const backend = new MockBackend({ rules: 3, ...options });
  setup?.(backend);
  setTransport(createMockTransport(backend));
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  const act: Act = async (callback) => {
    await react.act(async () => {
      await callback();
    });
  };
  await act(() => {
    root.render(react.createElement(App));
  });
  cleanups.push(async () => {
    await act(() => {
      root.unmount();
    });
    container.remove();
    setTransport(null);
  });
  return { backend, container, act };
}

/** Poll until `check` passes, letting React and the mock's promises settle in between. */
async function waitFor({ act }: Mounted, check: () => void, timeout = 2000): Promise<void> {
  const started = Date.now();
  for (;;) {
    try {
      check();
      return;
    } catch (error) {
      if (Date.now() - started > timeout) throw error;
    }
    await act(() => new Promise((resolve) => setTimeout(resolve, 10)));
  }
}

const headerStatus = (container: HTMLElement) =>
  container.querySelector('.header-status [role="status"]')?.textContent ?? '';

function button(container: HTMLElement, text: string): HTMLButtonElement | undefined {
  return [...container.querySelectorAll('button')].find((b) => b.textContent === text);
}

async function click(mounted: Mounted, text: string): Promise<void> {
  const target = button(mounted.container, text);
  if (!target) throw new Error(`no button 「${text}」`);
  await mounted.act(() => {
    target.click();
  });
}

const callsOf = (backend: MockBackend, command: string) =>
  backend.calls.filter((c) => c.command === command);

describe('runtime status area (S-19)', () => {
  it('follows pushed events without polling', async () => {
    const setInterval = vi.spyOn(globalThis, 'setInterval');
    const mounted = await mount();
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('监听中');
    });

    await mounted.act(() => {
      backend.setPaused('manual', true);
    });
    expect(headerStatus(container)).toBe('已暂停');
    expect(button(container, '恢复监听')).toBeDefined();

    await mounted.act(() => {
      backend.setPaused('manual', false);
      backend.setHealth('degraded');
    });
    expect(headerStatus(container)).toBe('部分功能降级：前台窗口跟踪');

    // Let any would-be poller fire before counting round trips.
    await mounted.act(() => new Promise((resolve) => setTimeout(resolve, 50)));
    expect(setInterval).not.toHaveBeenCalled();
    expect(callsOf(backend, 'load_state')).toHaveLength(1);
    expect(callsOf(backend, 'get_runtime_snapshot')).toHaveLength(1);
  });

  it('offers 修复引擎 when the engine failed and shows the recovery', async () => {
    const mounted = await mount();
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('监听中');
    });
    await mounted.act(() => {
      backend.setHealth('failed');
    });
    expect(headerStatus(container)).toBe('输入监听已停止');
    await click(mounted, '修复引擎');
    expect(callsOf(backend, 'repair_engine')).toHaveLength(1);
    expect(['正在恢复…', '监听中']).toContain(headerStatus(container));
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('监听中');
    });
    expect(button(container, '修复引擎')).toBeUndefined();
  });

  it('shows the safe-mode banner once and hides it with 知道了', async () => {
    const mounted = await mount({ safeMode: 'crash_loop' });
    const { container } = mounted;
    await waitFor(mounted, () => {
      expect(container.querySelector('.banner')?.textContent).toMatch(/已进入安全模式/);
    });
    expect(headerStatus(container)).toBe('已暂停（安全模式）');
    expect(container.querySelectorAll('.banner')).toHaveLength(1);
    await click(mounted, '知道了');
    expect(container.querySelector('.banner')).toBeNull();
  });

  it('adds pushed startup problems and execution results', async () => {
    const mounted = await mount();
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('监听中');
    });
    await mounted.act(() => {
      backend.addProblem({ kind: 'autostart_repaired', severity: 'info', args: {} });
    });
    expect(container.querySelector('.banner.info')?.textContent).toMatch(/已自动修正/);

    await click(mounted, '运行状态与恢复');
    await waitFor(mounted, () => {
      expect(container.querySelector('.history-table')).toBeNull();
      expect(container.textContent).toMatch(/暂无执行记录/);
    });
    await mounted.act(() => {
      backend.recordExecution({ outcome: 'dropped_busy', reason: 'dropped_busy' }, 2);
    });
    const row = container.querySelector('.history-table tbody tr');
    expect(row?.textContent).toMatch(/未执行：上一条仍在执行/);
    expect(row?.textContent).toMatch(/另有 2 条/);
    expect(row?.textContent).toMatch(/12 毫秒/);
  });

  it('exports diagnostics with the chosen redaction, and stays quiet on cancel', async () => {
    const mounted = await mount();
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('监听中');
    });
    await click(mounted, '运行状态与恢复');
    await click(mounted, '导出诊断包');
    const select = container.querySelector('dialog select');
    if (!(select instanceof HTMLSelectElement)) throw new Error('no redaction choice');
    await mounted.act(() => {
      select.value = 'full';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await click(mounted, '导出');
    await waitFor(mounted, () => {
      expect(container.querySelector('.status-text')?.textContent).toBe(
        '诊断包已导出：mock://quicker-text-diagnostics.zip',
      );
    });
    expect(callsOf(backend, 'export_diagnostics')[0]?.args).toEqual({ redaction: 'full' });

    // Cancelling the redaction question sends nothing.
    await click(mounted, '导出诊断包');
    await click(mounted, '取消');
    expect(callsOf(backend, 'export_diagnostics')).toHaveLength(1);
  });

  it('says nothing when the save dialog of the diagnostics export is cancelled', async () => {
    const mounted = await mount({ dialog: 'cancel' });
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('监听中');
    });
    await click(mounted, '运行状态与恢复');
    const before = container.querySelector('.status-text')?.textContent;
    await click(mounted, '导出诊断包');
    await click(mounted, '导出');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'export_diagnostics')).toHaveLength(1);
      expect(container.querySelector('.status-text')?.textContent).toBe(before);
    });
    expect(container.querySelector('.alert.error')).toBeNull();
  });

  it('degrades to the 0.3 status line when the backend lacks the 0.4 commands', async () => {
    const mounted = await mount({ legacy: true });
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('引擎运行中');
    });
    expect(container.querySelector('.alert.error')).toBeNull();
    await click(mounted, '运行状态与恢复');
    expect(container.textContent).toMatch(/当前桌面引擎版本较旧/);
    expect(button(container, '导出诊断包')).toBeUndefined();

    await click(mounted, '暂停监听');
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('引擎已暂停');
    });
    await click(mounted, '刷新状态');
    await waitFor(mounted, () => {
      expect(container.querySelector('.status-text')?.textContent).toBe('状态已刷新');
    });
    // The snapshot is not retried on refresh once the backend is known to lack it.
    expect(callsOf(backend, 'get_runtime_snapshot')).toHaveLength(1);
  });
});

/** The input or textarea of the `<Field>` labelled `label`. */
function field(root: ParentNode, label: string): HTMLInputElement | HTMLTextAreaElement {
  const found = [...root.querySelectorAll('label.field')].find(
    (l) => l.querySelector(':scope > span')?.textContent === label,
  );
  const control = found?.querySelector<HTMLInputElement | HTMLTextAreaElement>('input, textarea');
  if (!control) throw new Error(`no field 「${label}」`);
  return control;
}

/** Type into a controlled input the way React sees it (native setter + input event). */
async function type(
  mounted: Mounted,
  control: HTMLInputElement | HTMLTextAreaElement,
  value: string,
): Promise<void> {
  const proto =
    control instanceof HTMLTextAreaElement
      ? HTMLTextAreaElement.prototype
      : HTMLInputElement.prototype;
  await mounted.act(() => {
    // The prototype's setter, not React's tracker on the element.
    Reflect.set(proto, 'value', value, control);
    control.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

const dialog = (container: HTMLElement, title: string) =>
  container.querySelector<HTMLDialogElement>(`dialog[aria-label="${title}"]`);

async function clickIn(mounted: Mounted, root: ParentNode | null, text: string): Promise<void> {
  const target = [...(root?.querySelectorAll('button') ?? [])].find((b) => b.textContent === text);
  if (!target) throw new Error(`no button 「${text}」`);
  await mounted.act(() => {
    target.click();
  });
}

const statusText = (container: HTMLElement) =>
  container.querySelector('.status-text')?.textContent ?? '';

describe('rule list, editor and settings (F-04)', () => {
  it('lists rules without a group under 「默认」', async () => {
    // Rule 1 is in 「默认」, rule 7 has group "".
    const mounted = await mount({ rules: 7 });
    const { container } = mounted;
    await waitFor(mounted, () => {
      expect(container.querySelectorAll('tbody tr')).toHaveLength(7);
    });
    const tabs = [...container.querySelectorAll('.group-tabs button')].map((b) => b.textContent);
    expect(tabs.filter((t) => t.startsWith('默认'))).toEqual(['默认2']);
    await click(mounted, '默认2');
    const rows = [...container.querySelectorAll('tbody tr')];
    expect(rows.map((r) => r.querySelector('strong')?.textContent)).toEqual([';s1', ';s7']);
    expect(rows.every((r) => r.textContent.includes('默认'))).toBe(true);
  });

  it('keeps a trailing newline in the blacklist while typing', async () => {
    const mounted = await mount();
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'load_state')).toHaveLength(1);
    });
    await click(mounted, '文本指令参数设置');
    const blacklist = field(container, '黑名单程序');
    await type(mounted, blacklist, 'a.exe\n');
    expect(blacklist.value).toBe('a.exe\n');
    await type(mounted, blacklist, 'a.exe\n b.exe \n');
    expect(blacklist.value).toBe('a.exe\n b.exe \n');
    await click(mounted, '保存参数草稿');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    const saved = callsOf(backend, 'save_draft')[0]?.args.config as {
      globals: { blacklist: string[]; terminators: string[] };
    };
    expect(saved.globals.blacklist).toEqual(['a.exe', 'b.exe']);
  });

  it('asks before Esc or 取消 throws away unsaved changes', async () => {
    const mounted = await mount();
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(container.querySelectorAll('tbody tr')).toHaveLength(3);
    });
    // Unchanged: closes right away.
    await click(mounted, '编辑');
    expect(dialog(container, '编辑文本指令')).not.toBeNull();
    await clickIn(mounted, dialog(container, '编辑文本指令'), '取消');
    expect(dialog(container, '编辑文本指令')).toBeNull();

    await click(mounted, '编辑');
    const editor = dialog(container, '编辑文本指令');
    await type(mounted, field(container, '说明'), '改过的说明');
    await mounted.act(() => {
      editor?.dispatchEvent(new Event('cancel', { cancelable: true }));
    });
    // 取消 in the question keeps the editor and the change.
    await waitFor(mounted, () => {
      expect(dialog(container, '放弃未保存的改动？')).not.toBeNull();
    });
    await clickIn(mounted, dialog(container, '放弃未保存的改动？'), '取消');
    await waitFor(mounted, () => {
      expect(dialog(container, '放弃未保存的改动？')).toBeNull();
    });
    expect(field(container, '说明').value).toBe('改过的说明');

    await clickIn(mounted, editor, '取消');
    await waitFor(mounted, () => {
      expect(dialog(container, '放弃未保存的改动？')).not.toBeNull();
    });
    await clickIn(mounted, dialog(container, '放弃未保存的改动？'), '放弃改动');
    await waitFor(mounted, () => {
      expect(dialog(container, '编辑文本指令')).toBeNull();
    });
    expect(callsOf(backend, 'save_draft')).toHaveLength(0);
  });

  it('grays out Tab and 回车 on Windows and lets an old config uncheck them', async () => {
    const mounted = await mount({}, (backend) => {
      backend.draft.globals.terminators = ['space', 'tab'];
    });
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'load_state')).toHaveLength(1);
    });
    await click(mounted, '文本指令参数设置');
    const keys = container.querySelector('.end-keys');
    const labels = [...(keys?.querySelectorAll('label.unsupported') ?? [])];
    expect(labels.map((l) => l.textContent)).toEqual(['Tab', '回车']);
    const [tab, enter] = labels.map((l) => l.querySelector('input'));
    expect(tab?.checked).toBe(true);
    expect(tab?.disabled).toBe(false);
    expect(enter?.checked).toBe(false);
    expect(enter?.disabled).toBe(true);
    expect(keys?.textContent).toMatch(/Tab、回车：Windows 上暂不支持；已勾选的不会触发指令/);
    // The 触发字符 column of a terminator rule says so too.
    await click(mounted, '文本指令管理');
    expect(container.querySelector('tbody')?.textContent).toMatch(/空格.*Tab 暂不支持/);
  });

  it('shows no unsupported terminators on macOS', async () => {
    const mounted = await mount({ platform: 'macos' });
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'load_state')).toHaveLength(1);
    });
    await click(mounted, '文本指令参数设置');
    expect(container.querySelectorAll('.end-keys label.unsupported')).toHaveLength(0);
    expect(container.querySelector('.end-keys small')).toBeNull();
  });

  it('shows save_draft validation in the editor and keeps it open', async () => {
    const mounted = await mount();
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(container.querySelectorAll('tbody tr')).toHaveLength(3);
    });
    await click(mounted, '编辑');
    backend.failures.set('save_draft', {
      code: 'validation',
      detail: {
        diagnostics: [
          {
            code: 'description_too_long',
            message: '说明太长',
            path: 'rules[0].description',
            span: null,
            severity: 'error',
          },
          {
            code: 'duplicate',
            message: '缩写重复',
            path: 'rules[2].match',
            span: null,
            severity: 'error',
          },
        ],
      },
    });
    await clickIn(mounted, dialog(container, '编辑文本指令'), '保存');
    await waitFor(mounted, () => {
      expect(container.querySelector('.field-errors')).not.toBeNull();
    });
    const items = [...container.querySelectorAll('.field-errors li')].map((li) => li.textContent);
    expect(items).toEqual(['说明：说明太长', '第 3 条指令：缩写重复']);
    expect(dialog(container, '编辑文本指令')).not.toBeNull();
    expect(container.querySelector('.alert.error')?.textContent).toMatch(/有设置需要修改/);
  });

  it.each([false, true])(
    'says nothing when the export dialog is cancelled (legacy %s)',
    async (legacy) => {
      const mounted = await mount({ dialog: 'cancel', legacy });
      const { backend, container } = mounted;
      await waitFor(mounted, () => {
        expect(callsOf(backend, 'load_state')).toHaveLength(1);
      });
      const before = statusText(container);
      await click(mounted, '导出');
      await waitFor(mounted, () => {
        expect(callsOf(backend, 'export_config')).toHaveLength(1);
      });
      await mounted.act(() => new Promise((resolve) => setTimeout(resolve, 20)));
      expect(statusText(container)).toBe(before);
      expect(container.querySelector('.alert.error')).toBeNull();
    },
  );

  it('reports a finished export', async () => {
    const mounted = await mount();
    const { container } = mounted;
    await click(mounted, '导出');
    await waitFor(mounted, () => {
      expect(statusText(container)).toMatch(/mock:\/\/quicker-text-package\.json/);
    });
  });

  it('shows the license notice and asks before 退出全部', async () => {
    const mounted = await mount();
    const { backend, container } = mounted;
    await click(mounted, '文本指令参数设置');
    expect(container.textContent).toMatch(/GPL-3\.0-or-later/);
    await click(mounted, '运行状态与恢复');
    await click(mounted, '退出全部');
    await waitFor(mounted, () => {
      expect(dialog(container, '退出 Quicker Text？')).not.toBeNull();
    });
    await clickIn(mounted, dialog(container, '退出 Quicker Text？'), '取消');
    await mounted.act(() => new Promise((resolve) => setTimeout(resolve, 20)));
    expect(callsOf(backend, 'exit_app')).toHaveLength(0);
  });
});
