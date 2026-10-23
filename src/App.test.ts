// App-level checks of the S-19 runtime status area against the mock backend: event-driven status
// line, repair, banners, history, diagnostics export and a 0.3 backend without the new commands.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { MockBackend, MockOptions } from './ipc/mock';
import type { Component, HealthState, ReasonCode } from './ipc/types';
import { reasonNames } from './ipc/labels';

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
  const { runtimeSync } = await import('./ipc/events');
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
    // The app-wide RuntimeSync of this module graph lives as long as the app; end it with this one,
    // or a later window focus event would make it call a reset transport (the preview mock).
    runtimeSync().dispose();
    container.remove();
    setTransport(null);
  });
  return { backend, container, act };
}

/** Poll until `check` passes, letting React and the mock's promises settle in between. */
async function waitFor(
  { act }: Pick<Mounted, 'act'>,
  check: () => void,
  timeout = 2000,
): Promise<void> {
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
    // repair_engine answers the current snapshot; `recovering` and the outcome arrive as events.
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('正在恢复…');
    });
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

  it('rewords the self-test banner as the user resumes and pauses, as one banner (review A6-2)', async () => {
    const mounted = await mount();
    const { backend, container } = mounted;
    const banners = () => [...container.querySelectorAll('.banner')];
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('监听中');
    });
    // A check failed and nobody resumed while startup waited: still paused by the user reason.
    await mounted.act(() => {
      backend.setPaused('manual', true);
      backend.addProblem({
        kind: 'self_test_failed',
        severity: 'warning',
        args: { checks: ['clipboard'] },
      });
    });
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('已暂停');
    });
    expect(banners()).toHaveLength(1);
    expect(banners()[0]?.textContent).toContain(
      '启动自检未通过：剪贴板，没有自动开始监听。可在「运行状态与恢复」中查看详情或导出诊断包；排除问题后可点「恢复监听」。',
    );
    await click(mounted, '恢复监听');
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('监听中');
    });
    // The same banner, worded by the state now.
    expect(banners()).toHaveLength(1);
    expect(banners()[0]?.querySelector('small')).toBeNull();
    expect(banners()[0]?.textContent).toContain(
      '启动自检未通过：剪贴板。输入监听当前已开启，但可能无法正常工作；可在「运行状态与恢复」中查看详情或导出诊断包。',
    );
    expect(banners()[0]?.textContent).not.toMatch(/恢复监听|没有自动开始监听/);
    await click(mounted, '暂停监听');
    await waitFor(mounted, () => {
      expect(banners()[0]?.textContent).toContain('没有自动开始监听');
    });
    expect(banners()).toHaveLength(1);
  });

  it('keeps a dismissed self-test banner hidden whatever its wording becomes (review A6-2)', async () => {
    const mounted = await mount();
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('监听中');
    });
    // The user resumed while startup waited: listening, the problem reported all the same.
    await mounted.act(() => {
      backend.addProblem({
        kind: 'self_test_failed',
        severity: 'warning',
        args: { checks: ['raw_input'] },
      });
    });
    expect(container.querySelector('.banner')?.textContent).toContain('输入监听当前已开启');
    await click(mounted, '知道了');
    expect(container.querySelector('.banner')).toBeNull();
    await click(mounted, '暂停监听');
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('已暂停');
    });
    expect(container.querySelector('.banner')).toBeNull();
    await click(mounted, '恢复监听');
    await waitFor(mounted, () => {
      expect(headerStatus(container)).toBe('监听中');
    });
    expect(container.querySelector('.banner')).toBeNull();
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
        '诊断包已导出：mock://quicktext-diagnostics.zip',
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

/**
 * Call `element`'s React handler `name` itself, as an event the UI did not stop would. A disabled
 * control gets no events; this reaches the handler's own refusal. Not awaited: it may wait on a
 * dialog.
 */
async function callHandler(
  mounted: Mounted,
  element: Element | null | undefined,
  name: 'onClick' | 'onChange',
  event: object = {},
): Promise<void> {
  const props = Object.entries(element ?? {}).find(([key]) => key.startsWith('__reactProps$'));
  const handler = (props?.[1] as Record<string, unknown> | undefined)?.[name];
  if (typeof handler !== 'function') throw new Error(`no ${name} on the element`);
  await mounted.act(() => {
    void (handler as (event: object) => unknown)(event);
  });
}

const statusText = (container: HTMLElement) =>
  container.querySelector('.status-text')?.textContent ?? '';

/** The checkbox of the `<label>` reading `text`, or null when there is none. */
const checkbox = (container: HTMLElement, text: string): HTMLInputElement | null =>
  [...container.querySelectorAll('label')]
    .find((l) => l.textContent.trim() === text)
    ?.querySelector<HTMLInputElement>('input[type="checkbox"]') ?? null;

/** Let React and the mock's pending promises settle. */
const settle = (mounted: Pick<Mounted, 'act'>) =>
  mounted.act(() => new Promise((resolve) => setTimeout(resolve, 20)));

interface HeldLoad {
  /** The backend has run the call: read (or changed) the state it answers with. */
  read: boolean;
  /** The answer has been sent back. */
  answered: boolean;
  release: () => void;
}

interface LoadHold {
  held: HeldLoad[];
  attach: (backend: MockBackend) => void;
  /** Hold the next `count` calls. */
  arm: (count?: number) => void;
}

/**
 * Once attached, each armed call of command `name` runs at once, as the backend would, but answers
 * only on its `release()`: an answer overtaken by others (reviews C2, C6).
 */
function holdAnswers(name: string): LoadHold {
  let armed = 0;
  const held: HeldLoad[] = [];
  return {
    held,
    attach: (backend) => {
      const handle = backend.handle.bind(backend);
      backend.handle = async (...[command, args]: Parameters<MockBackend['handle']>) => {
        if (command !== name || armed === 0) return handle(command, args);
        armed -= 1;
        let release: () => void = () => undefined;
        const released = new Promise<void>((resolve) => {
          release = () => {
            resolve();
          };
        });
        const entry: HeldLoad = {
          read: false,
          answered: false,
          release: () => {
            release();
          },
        };
        held.push(entry);
        const answer = await handle(command, args);
        entry.read = true;
        await released;
        entry.answered = true;
        return answer;
      };
    },
    arm: (count = 1) => {
      armed += count;
    },
  };
}

/** Held load_state calls: a load overtaken by a command's answer (review C2). */
const holdLoads = () => holdAnswers('load_state');

/** Release held call `index` and let its answer reach the app. */
async function answerHeld(mounted: Mounted, hold: LoadHold, index: number): Promise<void> {
  hold.held[index]?.release();
  await waitFor(mounted, () => {
    expect(hold.held[index]?.answered).toBe(true);
  });
  await settle(mounted);
}

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

  it.each(['windows', 'macos'] as const)(
    'offers the Esc IME erasure only where it runs, and says macOS is experimental (S-16): %s',
    async (platform) => {
      const mounted = await mount({ platform });
      const { container } = mounted;
      await waitFor(mounted, () => {
        expect(container.querySelectorAll('tbody tr')).toHaveLength(3);
      });
      await click(mounted, '文本指令参数设置');
      const settings = container.querySelector('.settings')?.textContent ?? '';
      expect(settings.includes('按 Esc')).toBe(platform !== 'windows');
      expect(settings.includes('按缩写长度退格删除')).toBe(platform === 'windows');
      expect(settings.includes('macOS 版尚未经过实机验收')).toBe(platform === 'macos');
      await click(mounted, '文本指令管理');
      await click(mounted, '编辑');
      const editor = dialog(container, '编辑文本指令')?.textContent ?? '';
      expect(editor).toMatch(/编辑文本指令|触发/);
      expect(editor.includes('如何清除输入法中的缩写')).toBe(platform !== 'windows');
      expect(editor.includes('按 Esc')).toBe(platform !== 'windows');
    },
  );

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
      expect(statusText(container)).toMatch(/mock:\/\/quicktext-package\.json/);
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
      expect(dialog(container, '退出 Quicktext？')).not.toBeNull();
    });
    await clickIn(mounted, dialog(container, '退出 Quicktext？'), '取消');
    await mounted.act(() => new Promise((resolve) => setTimeout(resolve, 20)));
    expect(callsOf(backend, 'exit_app')).toHaveLength(0);
  });
});

describe('启动时自动开始监听 (S-09)', () => {
  const AUTO_LISTEN = '启动时自动开始监听';

  /** Mount, wait for load_state and open the settings page. */
  async function openSettings(
    options: MockOptions = {},
    setup?: (backend: MockBackend) => void,
  ): Promise<Mounted> {
    const mounted = await mount(options, setup);
    await waitFor(mounted, () => {
      expect(callsOf(mounted.backend, 'load_state')).toHaveLength(1);
    });
    await click(mounted, '文本指令参数设置');
    return mounted;
  }

  async function toggle(mounted: Mounted): Promise<void> {
    const box = checkbox(mounted.container, AUTO_LISTEN);
    if (!box) throw new Error(`no checkbox 「${AUTO_LISTEN}」`);
    await mounted.act(() => {
      box.click();
    });
  }

  it('starts from load_state.auto_listen_on_start and saves with set_auto_listen', async () => {
    const mounted = await openSettings({}, (backend) => {
      backend.autoListen = true;
    });
    const { backend, container } = mounted;
    expect(checkbox(container, AUTO_LISTEN)?.checked).toBe(true);
    expect(checkbox(container, AUTO_LISTEN)?.disabled).toBe(false);
    await toggle(mounted);
    await waitFor(mounted, () => {
      expect(checkbox(container, AUTO_LISTEN)?.checked).toBe(false);
    });
    await toggle(mounted);
    await waitFor(mounted, () => {
      expect(checkbox(container, AUTO_LISTEN)?.checked).toBe(true);
    });
    expect(callsOf(backend, 'set_auto_listen').map((c) => c.args)).toEqual([
      { enabled: false },
      { enabled: true },
    ]);
    expect(backend.autoListen).toBe(true);
    expect(container.querySelector('.alert.error')).toBeNull();
  });

  it('stays usable in safe mode, which allows it, unlike 开机时启动 (ipc-contract §5)', async () => {
    const mounted = await openSettings({ safeMode: 'crash_loop' });
    const { backend, container } = mounted;
    expect(container.querySelector('.banner')?.textContent).toMatch(/已进入安全模式/);
    expect(checkbox(container, '开机时启动')?.disabled).toBe(true);
    expect(checkbox(container, AUTO_LISTEN)?.disabled).toBe(false);
    await toggle(mounted);
    await waitFor(mounted, () => {
      expect(checkbox(container, AUTO_LISTEN)?.checked).toBe(true);
    });
    expect(callsOf(backend, 'set_auto_listen').map((c) => c.args)).toEqual([{ enabled: true }]);
    expect(backend.autoListen).toBe(true);
    expect(container.querySelector('.alert.error')).toBeNull();
  });

  it('keeps the old value and says why when the backend refuses', async () => {
    // prefs.json cannot be read: the backend never overwrites it and refuses, usually with io.
    const mounted = await openSettings();
    const { backend, container } = mounted;
    backend.failures.set('set_auto_listen', {
      code: 'io',
      detail: { op: 'read', path: null, os_code: 32 },
    });
    expect(checkbox(container, AUTO_LISTEN)?.checked).toBe(false);
    await toggle(mounted);
    await waitFor(mounted, () => {
      expect(container.querySelector('.alert.error')?.textContent).toMatch(
        /^读取文件失败（系统错误 32）。/,
      );
    });
    expect(checkbox(container, AUTO_LISTEN)?.checked).toBe(false);
    // A refusal is not a missing command: the toggle stays usable for later.
    expect(checkbox(container, AUTO_LISTEN)?.disabled).toBe(false);
    expect(backend.autoListen).toBe(false);
  });

  it('is not shown by a 0.3 backend, which has no such setting', async () => {
    const mounted = await openSettings({ legacy: true });
    const { backend, container } = mounted;
    expect(checkbox(container, '开机时启动')).not.toBeNull();
    expect(checkbox(container, AUTO_LISTEN)).toBeNull();
    expect(container.textContent.includes(AUTO_LISTEN)).toBe(false);
    expect(callsOf(backend, 'set_auto_listen')).toHaveLength(0);
  });

  it('turns disabled, without crashing, when the backend lacks set_auto_listen', async () => {
    const mounted = await openSettings();
    const { backend, container } = mounted;
    backend.failures.set('set_auto_listen', 'Command set_auto_listen not found');
    await toggle(mounted);
    await waitFor(mounted, () => {
      expect(container.querySelector('.alert.error')?.textContent).toMatch(/不支持此功能/);
    });
    const box = checkbox(container, AUTO_LISTEN);
    expect(box?.checked).toBe(false);
    expect(box?.disabled).toBe(true);
    expect(box?.closest('label')?.title).toBe('当前版本的桌面引擎不支持此设置');
    expect(backend.autoListen).toBe(false);
    // The rest of the page still works.
    await click(mounted, '文本指令管理');
    expect(container.querySelectorAll('tbody tr')).toHaveLength(3);
  });

  /** Reload on focus with load_state held, toggle the setting meanwhile, then let the load answer. */
  async function overtakeReload(mounted: Mounted, hold: LoadHold) {
    const { container } = mounted;
    hold.arm();
    await mounted.act(() => {
      window.dispatchEvent(new Event('focus'));
    });
    await waitFor(mounted, () => {
      expect(hold.held[0]?.read).toBe(true);
    });
    await toggle(mounted);
    await waitFor(mounted, () => {
      expect(checkbox(container, AUTO_LISTEN)?.checked).toBe(false);
    });
    await answerHeld(mounted, hold, 0);
  }

  it('keeps set_auto_listen’s answer over a load_state read before it (review C2)', async () => {
    const hold = holdLoads();
    const mounted = await openSettings({}, (backend) => {
      backend.autoListen = true;
      hold.attach(backend);
    });
    await overtakeReload(mounted, hold);
    const { backend, container } = mounted;
    expect(checkbox(container, AUTO_LISTEN)?.checked).toBe(false);
    expect(callsOf(backend, 'set_auto_listen').map((c) => c.args)).toEqual([{ enabled: false }]);
    expect(backend.autoListen).toBe(false);
    // The overtaken answer was dropped and load_state asked again; that answer read the new value.
    expect(callsOf(backend, 'load_state')).toHaveLength(3);
    expect(container.querySelector('.alert.error')).toBeNull();
  });

  it('keeps unsaved settings edits when the reload asked again answers (review C2)', async () => {
    const hold = holdLoads();
    const mounted = await openSettings({}, (backend) => {
      backend.autoListen = true;
      hold.attach(backend);
    });
    const { container } = mounted;
    await type(mounted, field(container, '词分隔符'), ' ;');
    await overtakeReload(mounted, hold);
    expect(checkbox(container, AUTO_LISTEN)?.checked).toBe(false);
    expect(callsOf(mounted.backend, 'load_state')).toHaveLength(3);
    // An answer to load_state never replaces the editor's unsaved values.
    expect(field(container, '词分隔符').value).toBe(' ;');
  });

  it('shows what the backend answered, not what was asked (review C2)', async () => {
    const mounted = await openSettings({}, (backend) => {
      const handle = backend.handle.bind(backend);
      // A backend that could not turn either setting on answers with the value it kept.
      backend.handle = async (...[command, args]: Parameters<MockBackend['handle']>) => {
        const answer = await handle(command, args);
        if (command === 'set_auto_listen') backend.autoListen = false;
        if (command === 'set_autostart') backend.autostart = false;
        return command === 'set_auto_listen' || command === 'set_autostart' ? false : answer;
      };
    });
    const { backend, container } = mounted;
    expect(checkbox(container, AUTO_LISTEN)?.checked).toBe(false);
    await toggle(mounted);
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'set_auto_listen')).toHaveLength(1);
    });
    await settle(mounted);
    expect(callsOf(backend, 'set_auto_listen')[0]?.args).toEqual({ enabled: true });
    expect(checkbox(container, AUTO_LISTEN)?.checked).toBe(false);

    const autostart = checkbox(container, '开机时启动');
    expect(autostart?.checked).toBe(false);
    await mounted.act(() => {
      autostart?.click();
    });
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'set_autostart')).toHaveLength(1);
    });
    await settle(mounted);
    expect(callsOf(backend, 'set_autostart')[0]?.args).toEqual({ enabled: true });
    expect(checkbox(container, '开机时启动')?.checked).toBe(false);
    expect(container.querySelector('.alert.error')).toBeNull();
  });
});

describe('writes wait for the first load_state snapshot (review C2 follow-up)', () => {
  /** Commands that write the backend or submit the config, and the ones only their entries send. */
  const WRITES = [
    'save_draft',
    'apply_settings',
    'discard_draft',
    'rollback',
    'set_autostart',
    'set_auto_listen',
    'import_config',
    'export_config',
    'choose_file',
    'get_foreground',
  ];

  const alerts = (container: HTMLElement) =>
    [...container.querySelectorAll('[role="alert"]')].map((a) => a.textContent).join('\n');

  const pauseButton = (container: HTMLElement) =>
    button(container, '暂停监听') ?? button(container, '恢复监听');

  /** Every write entry of the three pages, by page; `null` when it is not rendered. */
  function writeEntries(container: HTMLElement, page: 'manage' | 'settings' | 'status') {
    const entries = {
      manage: () => [
        checkbox(container, '启用文本指令'),
        button(container, '＋ 添加指令'),
        button(container, '导入'),
        button(container, '导出'),
        button(container, '应用设置'),
      ],
      settings: () => [
        button(container, '保存参数草稿'),
        button(container, '选择当前窗口加入黑名单'),
        checkbox(container, '开机时启动'),
        button(container, '应用设置'),
      ],
      status: () => [
        button(container, '放弃未应用草稿'),
        button(container, '恢复上一个版本'),
        button(container, '应用设置'),
      ],
    };
    return entries[page]().map((entry) => entry ?? null);
  }

  async function failFirstLoad(): Promise<Mounted> {
    const mounted = await mount({}, (backend) => {
      backend.failures.set('load_state', '桌面引擎没有响应');
    });
    await waitFor(mounted, () => {
      expect(alerts(mounted.container)).toContain('未能读取当前配置');
    });
    return mounted;
  }

  it('keeps every write disabled, and unsent, while the first load_state failed', async () => {
    const mounted = await failFirstLoad();
    const { backend, container } = mounted;
    expect(alerts(container)).toContain('桌面引擎没有响应');
    expect(button(container, '重试')?.disabled).toBe(false);
    expect(statusText(container)).toBe('未能读取当前配置');
    expect(container.textContent).not.toContain('已连接');
    expect(container.querySelectorAll('tbody tr')).toHaveLength(0);
    // Not a write of the config: the engine can still be paused.
    expect(pauseButton(container)?.disabled).toBe(false);

    const tryAll = async (entries: (HTMLInputElement | HTMLButtonElement | null)[]) => {
      for (const entry of entries) {
        expect(entry).not.toBeNull();
        expect(entry?.disabled).toBe(true);
        await mounted.act(() => {
          entry?.click();
        });
      }
    };
    await tryAll(writeEntries(container, 'manage'));

    await click(mounted, '文本指令参数设置');
    await tryAll(writeEntries(container, 'settings'));
    const separators = field(container, '词分隔符');
    expect(separators.matches(':disabled')).toBe(true);
    // The fieldset disables the field for the user only; an input event still reaches React.
    const before = separators.value;
    await type(mounted, separators, ' ;');
    expect(field(container, '词分隔符').value).toBe(before);

    await click(mounted, '运行状态与恢复');
    await tryAll(writeEntries(container, 'status'));
    expect(button(container, '刷新状态')?.disabled).toBe(false);
    expect(button(container, '停止当前动作')?.disabled).toBe(false);

    await settle(mounted);
    expect(dialog(container, '放弃全部未应用草稿？')?.open ?? false).toBe(false);
    expect(dialog(container, '回滚到上一个生效版本？')?.open ?? false).toBe(false);
    for (const command of WRITES) expect(callsOf(backend, command)).toEqual([]);
    expect(callsOf(backend, 'load_state')).toHaveLength(1);
  });

  it('says so, never 「已连接」, when commands overtake all three first loads', async () => {
    const hold = holdLoads();
    hold.arm(3);
    const mounted = await mount({}, (backend) => {
      hold.attach(backend);
    });
    const { backend, container } = mounted;
    for (let i = 0; i < 3; i++) {
      await waitFor(mounted, () => {
        expect(hold.held[i]?.read).toBe(true);
        expect(pauseButton(container)?.disabled).toBe(false);
      });
      // set_paused answers with the whole state while load_state i, read before it, is on its way.
      await mounted.act(() => {
        pauseButton(container)?.click();
      });
      await waitFor(mounted, () => {
        expect(callsOf(backend, 'set_paused')).toHaveLength(i + 1);
      });
      await settle(mounted);
      expect(container.textContent).not.toContain('已连接');
      await answerHeld(mounted, hold, i);
    }
    await waitFor(mounted, () => {
      expect(alerts(container)).toContain('未能读取当前配置：读取期间状态一再变化');
    });
    expect(callsOf(backend, 'load_state')).toHaveLength(3);
    expect(container.textContent).not.toContain('已连接');
    expect(statusText(container)).toBe('未能读取当前配置');
    expect(container.querySelectorAll('tbody tr')).toHaveLength(0);
    expect(writeEntries(container, 'manage').map((entry) => entry?.disabled)).toEqual(
      Array<boolean>(5).fill(true),
    );

    await click(mounted, '重试');
    await waitFor(mounted, () => {
      expect(statusText(container)).toContain('已连接');
    });
    expect(callsOf(backend, 'load_state')).toHaveLength(4);
    expect(alerts(container)).not.toContain('未能读取当前配置');
    expect(container.querySelectorAll('tbody tr')).toHaveLength(3);
    expect(writeEntries(container, 'manage').map((entry) => entry?.disabled)).toEqual(
      Array<boolean>(5).fill(false),
    );
  });

  it('unlocks the writes once the config is read', async () => {
    const mounted = await failFirstLoad();
    const { backend, container } = mounted;
    await click(mounted, '重试');
    await waitFor(mounted, () => {
      expect(statusText(container)).toContain('已连接');
    });
    expect(alerts(container)).not.toContain('未能读取当前配置');
    expect(container.querySelectorAll('tbody tr')).toHaveLength(3);
    for (const entry of writeEntries(container, 'manage')) expect(entry?.disabled).toBe(false);

    await click(mounted, '文本指令参数设置');
    for (const entry of writeEntries(container, 'settings')) expect(entry?.disabled).toBe(false);
    expect(field(container, '词分隔符').matches(':disabled')).toBe(false);
    await type(mounted, field(container, '词分隔符'), ' ;');
    expect(field(container, '词分隔符').value).toBe(' ;');
    await click(mounted, '保存参数草稿');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    const saved = callsOf(backend, 'save_draft')[0]?.args.config as {
      rules: unknown[];
      globals: { word_separators: string };
    };
    // The draft submitted is the config read, not the empty one shown before.
    expect(saved.rules).toHaveLength(3);
    expect(saved.globals.word_separators).toBe(' ;');

    await click(mounted, '运行状态与恢复');
    for (const entry of writeEntries(container, 'status')) expect(entry?.disabled).toBe(false);
  });

  /** Mount with load_state holdable and wait until the config is read. */
  async function mountReady(hold: LoadHold): Promise<Mounted> {
    const mounted = await mount({}, (backend) => {
      hold.attach(backend);
    });
    await waitFor(mounted, () => {
      expect(statusText(mounted.container)).toContain('已连接');
    });
    return mounted;
  }

  /** Let set_autostart overtake each of the three held loads of a reload already started. */
  async function overtakeAll(mounted: Mounted, hold: LoadHold): Promise<void> {
    const { backend, container } = mounted;
    await click(mounted, '文本指令参数设置');
    for (let i = 0; i < 3; i++) {
      await waitFor(mounted, () => {
        expect(hold.held[i]?.read).toBe(true);
      });
      await mounted.act(() => {
        checkbox(container, '开机时启动')?.click();
      });
      await waitFor(mounted, () => {
        expect(callsOf(backend, 'set_autostart')).toHaveLength(i + 1);
      });
      await settle(mounted);
      await answerHeld(mounted, hold, i);
    }
    await settle(mounted);
    expect(callsOf(backend, 'load_state')).toHaveLength(4);
  }

  it('does not report 「状态已刷新」 when commands overtake every load of a refresh', async () => {
    const hold = holdLoads();
    const mounted = await mountReady(hold);
    const { container } = mounted;
    await click(mounted, '运行状态与恢复');
    hold.arm(3);
    await click(mounted, '刷新状态');
    await overtakeAll(mounted, hold);
    await waitFor(mounted, () => {
      expect(alerts(container)).toContain('读取期间状态一再变化，读到的内容已过时，这次没有刷新');
    });
    expect(statusText(container)).not.toContain('状态已刷新');
    // The config read first stays; the writes stay usable.
    expect(button(container, '保存参数草稿')?.disabled).toBe(false);
  });

  it('stays silent when commands overtake every load of a focus reload', async () => {
    const hold = holdLoads();
    const mounted = await mountReady(hold);
    const { container } = mounted;
    hold.arm(3);
    await mounted.act(() => {
      window.dispatchEvent(new Event('focus'));
    });
    await overtakeAll(mounted, hold);
    expect(alerts(container)).toBe('');
    expect(statusText(container)).toContain('已连接');
  });
});

describe('store revision and safe mode pushed with store-state', () => {
  const revisionText = (container: HTMLElement) =>
    container.querySelector('.revision')?.textContent ?? '';
  const notice = (container: HTMLElement) =>
    container.querySelector('.alert.notice')?.textContent ?? '';

  /** Mount and wait until the config is read and the runtime view follows the events. */
  async function mountReady(
    options: MockOptions = {},
    setup?: (backend: MockBackend) => void,
  ): Promise<Mounted> {
    const mounted = await mount(options, setup);
    await waitFor(mounted, () => {
      expect(statusText(mounted.container)).toContain('已连接');
      expect(callsOf(mounted.backend, 'startup_problems')).toHaveLength(1);
    });
    await settle(mounted);
    return mounted;
  }

  it('reads a revision published elsewhere with load_state', async () => {
    const mounted = await mountReady();
    const { backend, container } = mounted;
    expect(revisionText(container)).toContain('当前生效 v1');
    expect(container.querySelectorAll('tbody tr')).toHaveLength(3);
    await mounted.act(() => {
      backend.publishExternally((config) => {
        config.rules = config.rules.slice(1);
      });
    });
    await waitFor(mounted, () => {
      expect(revisionText(container)).toContain('当前生效 v2');
    });
    expect(container.querySelectorAll('tbody tr')).toHaveLength(2);
    expect(callsOf(backend, 'load_state')).toHaveLength(2);
    expect(notice(container)).toBe('');
    // Pushed, not polled, and continued without a snapshot.
    expect(callsOf(backend, 'get_runtime_snapshot')).toHaveLength(1);
  });

  it('reads nothing again for the store-state of its own apply_settings and rollback', async () => {
    const mounted = await mountReady();
    const { backend, container } = mounted;
    await click(mounted, '应用设置');
    await waitFor(mounted, () => {
      expect(revisionText(container)).toContain('当前生效 v2');
    });
    await click(mounted, '运行状态与恢复');
    await click(mounted, '恢复上一个版本');
    await clickIn(mounted, dialog(container, '回滚到上一个生效版本？'), '回滚');
    await waitFor(mounted, () => {
      expect(revisionText(container)).toContain('当前生效 v3');
    });
    await settle(mounted);
    // The mock pushes each store-state before the answer, the earliest it can arrive.
    expect(callsOf(backend, 'apply_settings')).toHaveLength(1);
    expect(callsOf(backend, 'rollback')).toHaveLength(1);
    expect(callsOf(backend, 'load_state')).toHaveLength(1);
    expect(notice(container)).toBe('');
  });

  it('keeps unsaved settings edits over a revision published elsewhere and says so', async () => {
    const mounted = await mountReady();
    const { backend, container } = mounted;
    await click(mounted, '文本指令参数设置');
    await type(mounted, field(container, '词分隔符'), ' ;');
    await mounted.act(() => {
      backend.publishExternally((config) => {
        config.globals.word_separators = ',';
      });
    });
    await waitFor(mounted, () => {
      expect(notice(container)).toContain('配置已在后台更新为 v2');
    });
    expect(callsOf(backend, 'load_state')).toHaveLength(2);
    expect(field(container, '词分隔符').value).toBe(' ;');
    expect(revisionText(container)).toContain('当前生效 v2');
    // Keep: the notice goes and the edit stays; saving it overwrites the newer draft.
    await click(mounted, '保留我的修改');
    expect(notice(container)).toBe('');
    expect(field(container, '词分隔符').value).toBe(' ;');
    await click(mounted, '保存参数草稿');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    const saved = callsOf(backend, 'save_draft')[0]?.args.config as {
      globals: { word_separators: string };
    };
    expect(saved.globals.word_separators).toBe(' ;');
    // With nothing unsaved, the next revision is loaded straight away.
    await mounted.act(() => {
      backend.publishExternally((config) => {
        config.globals.word_separators = '|';
      });
    });
    await waitFor(mounted, () => {
      expect(field(container, '词分隔符').value).toBe('|');
    });
    expect(notice(container)).toBe('');
  });

  it('loads the newer revision over unsaved edits only when asked to', async () => {
    const mounted = await mountReady();
    const { backend, container } = mounted;
    await click(mounted, '文本指令参数设置');
    await type(mounted, field(container, '词分隔符'), ' ;');
    await mounted.act(() => {
      backend.publishExternally((config) => {
        config.globals.word_separators = ',';
      });
    });
    await waitFor(mounted, () => {
      expect(notice(container)).toContain('配置已在后台更新为 v2');
    });
    expect(field(container, '词分隔符').value).toBe(' ;');
    await click(mounted, '放弃修改，载入新配置');
    expect(notice(container)).toBe('');
    expect(field(container, '词分隔符').value).toBe(',');
    expect(callsOf(backend, 'save_draft')).toHaveLength(0);
  });

  it('keeps the rule being edited over a revision published elsewhere', async () => {
    const mounted = await mountReady();
    const { backend, container } = mounted;
    await click(mounted, '编辑');
    await type(mounted, field(container, '说明'), '改过的说明');
    await mounted.act(() => {
      backend.publishExternally((config) => {
        config.enabled = false;
      });
    });
    await waitFor(mounted, () => {
      expect(revisionText(container)).toContain('当前生效 v2');
    });
    expect(callsOf(backend, 'load_state')).toHaveLength(2);
    expect(dialog(container, '编辑文本指令')).not.toBeNull();
    expect(field(container, '说明').value).toBe('改过的说明');
    await clickIn(mounted, dialog(container, '编辑文本指令'), '保存');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    // The edited rule goes into the config read just now.
    const saved = callsOf(backend, 'save_draft')[0]?.args.config as {
      enabled: boolean;
      rules: { description: string }[];
    };
    expect(saved.enabled).toBe(false);
    expect(saved.rules.map((r) => r.description)).toContain('改过的说明');
  });

  it('disables the writes safe mode refuses and enables them when it ends, the store read again', async () => {
    const mounted = await mountReady({ safeMode: 'locked' });
    const { backend, container } = mounted;
    const manage = () =>
      [checkbox(container, '启用文本指令'), button(container, '＋ 添加指令')].map(
        (entry) => entry?.disabled,
      );
    expect(container.querySelector('.banner')?.textContent).toMatch(/已进入安全模式/);
    expect(manage()).toEqual([true, true]);
    // Importing, and publishing what was imported, is the way out of crash_loop: never disabled.
    expect(button(container, '导入')?.disabled).toBe(false);
    expect(button(container, '应用设置')?.disabled).toBe(false);
    await click(mounted, '文本指令参数设置');
    expect(button(container, '保存参数草稿')?.disabled).toBe(true);
    expect(checkbox(container, '开机时启动')?.disabled).toBe(true);
    // Not a write safe mode refuses (ipc-contract §5 set_auto_listen).
    expect(checkbox(container, '启动时自动开始监听')?.disabled).toBe(false);
    expect(field(container, '词分隔符').matches(':disabled')).toBe(true);
    // Locked serves a placeholder, not the store (src-tauri store/fallback.rs `read_only`).
    expect(revisionText(container)).toContain('当前生效 v0');
    await click(mounted, '文本指令管理');
    expect(container.querySelectorAll('tbody tr')).toHaveLength(0);
    // The backend's Locked retry succeeds: store-state says so, and the banner goes at once; the
    // writes come back with the store, read again (review I6).
    await mounted.act(() => {
      backend.endSafeMode();
    });
    expect(container.querySelector('.banner')).toBeNull();
    await waitFor(mounted, () => {
      expect(manage()).toEqual([false, false]);
    });
    expect(container.querySelectorAll('tbody tr')).toHaveLength(3);
    expect(revisionText(container)).toContain('当前生效 v1');
    await click(mounted, '文本指令参数设置');
    expect(button(container, '保存参数草稿')?.disabled).toBe(false);
    expect(checkbox(container, '开机时启动')?.disabled).toBe(false);
    expect(checkbox(container, '启动时自动开始监听')?.disabled).toBe(false);
    expect(field(container, '词分隔符').matches(':disabled')).toBe(false);
    await settle(mounted);
    // The store read once more; the problems, which go unannounced, are read again.
    expect(callsOf(backend, 'load_state')).toHaveLength(2);
    expect(callsOf(backend, 'startup_problems')).toHaveLength(2);
  });

  it('reads the store again when the Locked retry recovers it at the same revision (review I6)', async () => {
    const hold = holdLoads();
    const mounted = await mountReady({ safeMode: 'locked' }, (backend) => {
      // A new profile: rules saved to the draft, never applied (active revision 0, no rules).
      const disk = backend.disk;
      if (disk === null) throw new Error('Locked keeps the store on disk');
      backend.disk = {
        active: { ...disk.active, revision: 0, rules: [] },
        draft: { ...disk.draft, revision: 0 },
      };
      hold.attach(backend);
    });
    const { backend, container } = mounted;
    // Locked: the placeholder, revision 0 and no rules.
    expect(revisionText(container)).toContain('当前生效 v0');
    expect(container.querySelectorAll('tbody tr')).toHaveLength(0);
    // The retry recovers the store: store-state says revision 0 again, with safe mode null. The
    // window keeps the focus: no focus event reads the state again.
    const pushed: unknown[] = [];
    backend.on('store-state', (payload) => pushed.push(payload));
    hold.arm();
    await mounted.act(() => {
      backend.endSafeMode();
    });
    await settle(mounted);
    expect(pushed).toEqual([{ seq: backend.seq, revision: 0, safe_mode: null }]);
    expect(container.querySelector('.banner')).toBeNull();
    // The read it causes is on its way: until it is installed, the editor holds the placeholder,
    // and no entry sends a config write.
    expect(hold.held).toHaveLength(1);
    expect(button(container, '＋ 添加指令')?.disabled).toBe(true);
    expect(checkbox(container, '启用文本指令')?.disabled).toBe(true);
    expect(button(container, '应用设置')?.disabled).toBe(true);
    await click(mounted, '文本指令参数设置');
    expect(button(container, '保存参数草稿')?.disabled).toBe(true);
    expect(field(container, '词分隔符').matches(':disabled')).toBe(true);
    // Nor do the handlers themselves.
    await callHandler(mounted, button(container, '保存参数草稿'), 'onClick');
    await settle(mounted);
    expect(callsOf(backend, 'save_draft')).toHaveLength(0);
    expect(container.querySelector('.alert.error')?.textContent).toContain('安全模式状态已变化');
    await callHandler(mounted, button(container, '应用设置'), 'onClick');
    await settle(mounted);
    expect(callsOf(backend, 'apply_settings')).toHaveLength(0);
    // The read installs the recovered store: its rules are in the editor, and the writes are back.
    await answerHeld(mounted, hold, 0);
    expect(button(container, '保存参数草稿')?.disabled).toBe(false);
    expect(button(container, '应用设置')?.disabled).toBe(false);
    await click(mounted, '文本指令管理');
    expect(container.querySelectorAll('tbody tr')).toHaveLength(3);
    // Saving the parameters keeps the rules.
    await click(mounted, '文本指令参数设置');
    await type(mounted, field(container, '词分隔符'), ' ;');
    await click(mounted, '保存参数草稿');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    const saved = callsOf(backend, 'save_draft')[0]?.args.config as {
      rules: unknown[];
      globals: { word_separators: string };
    };
    expect(saved.globals.word_separators).toBe(' ;');
    expect(saved.rules).toHaveLength(3);
    expect(backend.draft.rules).toHaveLength(3);
    expect(callsOf(backend, 'load_state')).toHaveLength(2);
  });

  it('makes no local edit in safe mode, so the store read when it ends is taken in whole', async () => {
    const mounted = await mountReady({ safeMode: 'locked' }, (backend) => {
      const disk = backend.disk;
      if (disk === null) throw new Error('Locked keeps the store on disk');
      disk.active.globals.word_separators = ',';
      disk.draft.globals.word_separators = ',';
    });
    const { backend, container } = mounted;
    await click(mounted, '文本指令参数设置');
    const separators = () => field(container, '词分隔符');
    expect(separators().matches(':disabled')).toBe(true);
    expect(button(container, '选择当前窗口加入黑名单')?.disabled).toBe(true);
    const placeholder = separators().value;
    // The disabled fieldset stops the user, not an input event, which React still hands to
    // onChange; nor does the blacklist button's handler, called itself, edit anything.
    await type(mounted, separators(), ' ;');
    await callHandler(mounted, button(container, '选择当前窗口加入黑名单'), 'onClick');
    await settle(mounted);
    expect(separators().value).toBe(placeholder);
    expect(field(container, '黑名单程序').value).toBe('');
    expect(revisionText(container)).toContain('配置已同步');
    // The store read when the Locked retry recovers it is taken in whole, nothing kept over it.
    await mounted.act(() => {
      backend.endSafeMode();
    });
    await waitFor(mounted, () => {
      expect(separators().value).toBe(',');
    });
    expect(separators().matches(':disabled')).toBe(false);
    expect(field(container, '黑名单程序').value).toBe('');
    expect(revisionText(container)).toContain('配置已同步');
    expect(notice(container)).toBe('');
    expect(callsOf(backend, 'save_draft')).toHaveLength(0);
  });

  it('leaves crash_loop by importing and applying, without reading the state again', async () => {
    const mounted = await mountReady({ safeMode: 'crash_loop' });
    const { backend, container } = mounted;
    await click(mounted, '导入');
    await waitFor(mounted, () => {
      expect(dialog(container, '遇到重复的指令时怎么处理？')).not.toBeNull();
    });
    await clickIn(mounted, dialog(container, '遇到重复的指令时怎么处理？'), '导入');
    await waitFor(mounted, () => {
      expect(button(container, '确认导入草稿')).toBeDefined();
    });
    await click(mounted, '确认导入草稿');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    await settle(mounted);
    expect(container.querySelector('.alert.error')).toBeNull();
    await click(mounted, '应用设置');
    await waitFor(mounted, () => {
      expect(revisionText(container)).toContain('当前生效 v2');
    });
    await settle(mounted);
    expect(container.querySelector('.alert.error')).toBeNull();
    expect(container.querySelector('.banner')).toBeNull();
    expect(container.querySelectorAll('tbody tr')).toHaveLength(5);
    expect(checkbox(container, '启用文本指令')?.disabled).toBe(false);
    expect(callsOf(backend, 'load_state')).toHaveLength(1);
  });
});

describe('catching up with a revision published elsewhere (review C5)', () => {
  const UNREAD = '未能读取最新配置';
  const LOAD_FAILURE = '桌面引擎没有响应';
  const OVERWRITTEN = '配置已在后台更新为 v2，保存已用编辑器中的配置覆盖了它的草稿';
  const revisionText = (container: HTMLElement) =>
    container.querySelector('.revision')?.textContent ?? '';
  const notices = (container: HTMLElement) =>
    [...container.querySelectorAll('.alert.notice')].map((n) => n.textContent).join('\n');
  const separators = (container: HTMLElement) => field(container, '词分隔符').value;
  const focus = (mounted: Mounted) =>
    mounted.act(() => {
      window.dispatchEvent(new Event('focus'));
    });
  /** Long enough for every retry a failed read could still have pending. */
  const outwait = (mounted: Mounted) =>
    mounted.act(() => new Promise((resolve) => setTimeout(resolve, 1000)));

  /** Mount, load_state holdable when `hold` is given, and wait until the config is read. */
  async function mountReady(hold?: LoadHold): Promise<Mounted> {
    const mounted = await mount({}, (backend) => {
      hold?.attach(backend);
    });
    await waitFor(mounted, () => {
      expect(statusText(mounted.container)).toContain('已连接');
      expect(callsOf(mounted.backend, 'startup_problems')).toHaveLength(1);
    });
    await settle(mounted);
    return mounted;
  }

  /** Make the next `count` load_state calls reject. */
  function failLoads(backend: MockBackend, count: number): void {
    const handle = backend.handle.bind(backend);
    let left = count;
    backend.handle = (...[command, args]: Parameters<MockBackend['handle']>) => {
      if (command === 'load_state' && left > 0) {
        left -= 1;
        backend.failures.set('load_state', LOAD_FAILURE);
      }
      return handle(command, args);
    };
  }

  /** Another window publishes a revision with these word separators. */
  const publish = (mounted: Mounted, value: string) =>
    mounted.act(() => {
      mounted.backend.publishExternally((config) => {
        config.globals.word_separators = value;
      });
    });

  it('retries a failed read of the new revision and then shows it', async () => {
    const mounted = await mountReady();
    const { backend, container } = mounted;
    await click(mounted, '文本指令参数设置');
    failLoads(backend, 1);
    await publish(mounted, ',');
    await waitFor(mounted, () => {
      expect(separators(container)).toBe(',');
    });
    expect(revisionText(container)).toContain('当前生效 v2');
    // The first load, the read that failed, the retry.
    expect(callsOf(backend, 'load_state')).toHaveLength(3);
    expect(notices(container)).toBe('');
  });

  it('says the new revision could not be read once the retries run out, until it is', async () => {
    const mounted = await mountReady();
    const { backend, container } = mounted;
    await click(mounted, '文本指令参数设置');
    const before = separators(container);
    failLoads(backend, 3);
    await publish(mounted, ',');
    await waitFor(
      mounted,
      () => {
        expect(notices(container)).toContain(`后台配置已更新为 v2，但${UNREAD}（${LOAD_FAILURE}）`);
      },
      3000,
    );
    expect(notices(container)).toContain('此时保存会以编辑器中的配置覆盖后台较新的配置');
    expect(callsOf(backend, 'load_state')).toHaveLength(4);
    expect(separators(container)).toBe(before);
    expect(revisionText(container)).toContain('当前生效 v1');
    // Not blocking: saving stays possible. And no more reads by themselves.
    expect(button(container, '保存参数草稿')?.disabled).toBe(false);
    await outwait(mounted);
    expect(callsOf(backend, 'load_state')).toHaveLength(4);
    await click(mounted, '重新读取');
    await waitFor(mounted, () => {
      expect(separators(container)).toBe(',');
    });
    expect(notices(container)).toBe('');
    expect(revisionText(container)).toContain('当前生效 v2');
    expect(callsOf(backend, 'load_state')).toHaveLength(5);
    // Said of the button pressed (review C6).
    await waitFor(mounted, () => {
      expect(statusText(container)).toBe('已重新读取配置');
    });
  });

  it('reads a bounded number of times while load_state answers an earlier revision', async () => {
    const mounted = await mountReady();
    const { backend, container } = mounted;
    // store-state says v2; load_state keeps answering v1, as a backend lagging behind its event.
    const handle = backend.handle.bind(backend);
    backend.handle = async (...[command, args]: Parameters<MockBackend['handle']>) => {
      const answer = await handle(command, args);
      return command === 'load_state' ? { ...(answer as object), revision: 1 } : answer;
    };
    await publish(mounted, ',');
    await waitFor(
      mounted,
      () => {
        expect(notices(container)).toContain(`${UNREAD}（读到的仍是较早的版本）`);
      },
      3000,
    );
    await outwait(mounted);
    expect(callsOf(backend, 'load_state')).toHaveLength(4);
  });

  it('reads one revision at a time and then the one that arrived meanwhile', async () => {
    const hold = holdLoads();
    const mounted = await mountReady(hold);
    const { backend, container } = mounted;
    await click(mounted, '文本指令参数设置');
    hold.arm();
    await publish(mounted, ',');
    await waitFor(mounted, () => {
      expect(hold.held[0]?.read).toBe(true);
    });
    await publish(mounted, '|');
    await settle(mounted);
    // v3 only moves the target while the read of v2 is on its way.
    expect(callsOf(backend, 'load_state')).toHaveLength(2);
    await answerHeld(mounted, hold, 0);
    await waitFor(mounted, () => {
      expect(separators(container)).toBe('|');
    });
    expect(revisionText(container)).toContain('当前生效 v3');
    expect(callsOf(backend, 'load_state')).toHaveLength(3);
  });

  it('drops the read of v2 answering after a later read of v3 was shown', async () => {
    const hold = holdLoads();
    const mounted = await mountReady(hold);
    const { backend, container } = mounted;
    await click(mounted, '文本指令参数设置');
    hold.arm();
    await publish(mounted, ',');
    await waitFor(mounted, () => {
      expect(hold.held[0]?.read).toBe(true);
    });
    await publish(mounted, '|');
    // A focus reload reads v3, and answers first.
    await focus(mounted);
    await waitFor(mounted, () => {
      expect(separators(container)).toBe('|');
    });
    expect(revisionText(container)).toContain('当前生效 v3');
    await answerHeld(mounted, hold, 0);
    await settle(mounted);
    expect(separators(container)).toBe('|');
    expect(revisionText(container)).toContain('当前生效 v3');
    expect(callsOf(backend, 'load_state')).toHaveLength(3);
    expect(notices(container)).toBe('');
  });

  it('drops a focus reload that read the old config and answers after the revision read', async () => {
    const hold = holdLoads();
    const mounted = await mountReady(hold);
    const { backend, container } = mounted;
    await click(mounted, '文本指令参数设置');
    expect(separators(container)).not.toBe(',');
    hold.arm();
    await focus(mounted);
    await waitFor(mounted, () => {
      expect(hold.held[0]?.read).toBe(true);
    });
    await publish(mounted, ',');
    await waitFor(mounted, () => {
      expect(separators(container)).toBe(',');
    });
    await answerHeld(mounted, hold, 0);
    await settle(mounted);
    expect(separators(container)).toBe(',');
    expect(revisionText(container)).toContain('当前生效 v2');
    expect(callsOf(backend, 'load_state')).toHaveLength(3);
  });

  it('keeps the unread notice through an unrelated save, and says the save overwrote', async () => {
    const mounted = await mountReady();
    const { backend, container } = mounted;
    failLoads(backend, 3);
    await publish(mounted, ',');
    await waitFor(
      mounted,
      () => {
        expect(notices(container)).toContain(UNREAD);
      },
      3000,
    );
    // 总开关 saves the editor's whole config, over the draft of v2.
    await mounted.act(() => {
      checkbox(container, '启用文本指令')?.click();
    });
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    await settle(mounted);
    expect(notices(container)).toContain(`后台配置已更新为 v2，但${UNREAD}`);
    expect(notices(container)).toContain(OVERWRITTEN);
    expect(callsOf(backend, 'load_state')).toHaveLength(4);
    // Caught up: the unread notice goes; the one about the save stays until acknowledged.
    await click(mounted, '重新读取');
    await waitFor(mounted, () => {
      expect(notices(container)).not.toContain(UNREAD);
    });
    expect(notices(container)).toContain(OVERWRITTEN);
    await click(mounted, '知道了');
    expect(notices(container)).toBe('');
  });

  it('keeps the notice about unsaved edits through an unrelated save, and says it overwrote', async () => {
    const mounted = await mountReady();
    const { backend, container } = mounted;
    await click(mounted, '文本指令参数设置');
    await type(mounted, field(container, '词分隔符'), ' ;');
    await publish(mounted, ',');
    await waitFor(mounted, () => {
      expect(notices(container)).toContain('配置已在后台更新为 v2。编辑器中未保存的修改仍然保留');
    });
    await click(mounted, '文本指令管理');
    await mounted.act(() => {
      checkbox(container, '启用文本指令')?.click();
    });
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    await settle(mounted);
    const saved = callsOf(backend, 'save_draft')[0]?.args.config as {
      globals: { word_separators: string };
    };
    expect(saved.globals.word_separators).toBe(' ;');
    expect(notices(container)).toContain(OVERWRITTEN);
    // Nor does a reload of the same revision take it away.
    await focus(mounted);
    await settle(mounted);
    expect(notices(container)).toContain(OVERWRITTEN);
    expect(callsOf(backend, 'load_state')).toHaveLength(3);
    await click(mounted, '知道了');
    expect(notices(container)).toBe('');
  });

  describe('one install rule for load and write answers (review C6)', () => {
    /**
     * 总开关 saves while v1 is shown, and its answer (v1) is held while another window publishes
     * v2 and a focus load shows it. Then the save answers.
     */
    async function answerSaveLate(): Promise<Mounted> {
      const hold = holdAnswers('save_draft');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      hold.arm();
      await mounted.act(() => {
        checkbox(container, '启用文本指令')?.click();
      });
      await waitFor(mounted, () => {
        expect(hold.held[0]?.read).toBe(true);
      });
      await publish(mounted, ',');
      await focus(mounted);
      await waitFor(mounted, () => {
        expect(revisionText(container)).toContain('当前生效 v2');
      });
      expect(callsOf(backend, 'load_state')).toHaveLength(2);
      expect(statusText(container)).toBe('正在处理…');
      await answerHeld(mounted, hold, 0);
      await click(mounted, '文本指令参数设置');
      return mounted;
    }

    it('keeps showing v2 when a save sent at v1 answers after a load showed v2', async () => {
      const mounted = await answerSaveLate();
      const { backend, container } = mounted;
      // Long enough for a read the late answer would have caused.
      await outwait(mounted);
      expect(separators(container)).toBe(',');
      expect(revisionText(container)).toContain('当前生效 v2');
      expect(notices(container)).toBe('');
      // Nothing to read again: v2 was never replaced by v1.
      expect(callsOf(backend, 'load_state')).toHaveLength(2);
    });

    it('ends the save as usual when its late answer is dropped', async () => {
      const mounted = await answerSaveLate();
      const { container } = mounted;
      // The caller had its result: busy ended with the success message, and no error.
      expect(statusText(container)).toBe('总开关已保存到草稿');
      expect(container.querySelector('.alert.error')).toBeNull();
      // The write is no longer in flight: the next revision published elsewhere is read.
      await publish(mounted, '|');
      await waitFor(mounted, () => {
        expect(separators(container)).toBe('|');
      });
      expect(revisionText(container)).toContain('当前生效 v3');
    });

    it('says a save sent at v1 overwrote v2, though a load showed v2 before it answered', async () => {
      const hold = holdAnswers('save_draft');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      // Another window publishes v2 between the save being sent and its running.
      const handle = backend.handle.bind(backend);
      let published = false;
      backend.handle = (...[command, args]: Parameters<MockBackend['handle']>) => {
        if (command === 'save_draft' && !published) {
          published = true;
          backend.publishExternally((config) => {
            config.globals.word_separators = ',';
          });
        }
        return handle(command, args);
      };
      hold.arm();
      await mounted.act(() => {
        checkbox(container, '启用文本指令')?.click();
      });
      await waitFor(mounted, () => {
        expect(hold.held[0]?.read).toBe(true);
      });
      await focus(mounted);
      await waitFor(mounted, () => {
        expect(revisionText(container)).toContain('当前生效 v2');
      });
      await answerHeld(mounted, hold, 0);
      // The editor's config was of v1 when the save was sent: it replaced the draft of v2.
      expect(notices(container)).toContain(OVERWRITTEN);
      expect(revisionText(container)).toContain('当前生效 v2');
    });

    it('shows the later of two saves of one revision when the earlier one answers last', async () => {
      const hold = holdAnswers('save_draft');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      await click(mounted, '文本指令参数设置');
      hold.arm(2);
      await type(mounted, field(container, '词分隔符'), ' ;');
      await click(mounted, '保存参数草稿');
      await waitFor(mounted, () => {
        expect(hold.held[0]?.read).toBe(true);
      });
      await type(mounted, field(container, '词分隔符'), ' |');
      await click(mounted, '保存参数草稿');
      await waitFor(mounted, () => {
        expect(hold.held[1]?.read).toBe(true);
      });
      await answerHeld(mounted, hold, 1);
      expect(separators(container)).toBe(' |');
      await answerHeld(mounted, hold, 0);
      expect(separators(container)).toBe(' |');
      expect(backend.draft.globals.word_separators).toBe(' |');
      expect(revisionText(container)).toContain('当前生效 v1');
      expect(statusText(container)).toBe('参数已保存到草稿');
      expect(notices(container)).toBe('');
    });

    it('keeps a setting changed after a save was sent when the save answers', async () => {
      const hold = holdAnswers('save_draft');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      await click(mounted, '文本指令参数设置');
      const before = checkbox(container, '开机时启动')?.checked;
      hold.arm();
      await click(mounted, '保存参数草稿');
      await waitFor(mounted, () => {
        expect(hold.held[0]?.read).toBe(true);
      });
      await mounted.act(() => {
        checkbox(container, '开机时启动')?.click();
      });
      await waitFor(mounted, () => {
        expect(callsOf(backend, 'set_autostart')).toHaveLength(1);
      });
      await settle(mounted);
      expect(checkbox(container, '开机时启动')?.checked).toBe(!before);
      // Answered with the autostart of before: the save ran first.
      await answerHeld(mounted, hold, 0);
      expect(checkbox(container, '开机时启动')?.checked).toBe(!before);
      expect(backend.autostart).toBe(!before);
    });

    it('says 「重新读取」, not 「刷新状态」, when commands overtake every load of 重新读取', async () => {
      const hold = holdLoads();
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      await click(mounted, '文本指令参数设置');
      failLoads(backend, 3);
      await publish(mounted, ',');
      await waitFor(
        mounted,
        () => {
          expect(notices(container)).toContain(UNREAD);
        },
        3000,
      );
      hold.arm(3);
      await click(mounted, '重新读取');
      for (let i = 0; i < 3; i++) {
        await waitFor(mounted, () => {
          expect(hold.held[i]?.read).toBe(true);
        });
        await mounted.act(() => {
          checkbox(container, '开机时启动')?.click();
        });
        await waitFor(mounted, () => {
          expect(callsOf(backend, 'set_autostart')).toHaveLength(i + 1);
        });
        await settle(mounted);
        await answerHeld(mounted, hold, i);
      }
      const alert = () => container.querySelector('.alert.error')?.textContent ?? '';
      await waitFor(mounted, () => {
        expect(alert()).toContain('这次没有读取到。请再点一次「重新读取」');
      });
      expect(alert()).not.toContain('刷新');
      expect(statusText(container)).not.toContain('已重新读取配置');
    });
  });

  describe('the editor ends each write, its answer installed or not (review C7)', () => {
    type Confirmed = readonly [button: string, title: string, confirm: string];
    const DISCARD: Confirmed = ['放弃未应用草稿', '放弃全部未应用草稿？', '放弃草稿'];
    const ROLLBACK: Confirmed = ['恢复上一个版本', '回滚到上一个生效版本？', '回滚'];

    /** Press `button` on 「运行状态与恢复」, confirm, and go back to the settings. */
    async function confirmOnStatus(mounted: Mounted, [text, title, confirm]: Confirmed) {
      await click(mounted, '运行状态与恢复');
      await click(mounted, text);
      await clickIn(mounted, dialog(mounted.container, title), confirm);
      await click(mounted, '文本指令参数设置');
    }

    /** The word separators the last save_draft sent. */
    const sentSeparators = (backend: MockBackend) =>
      (
        callsOf(backend, 'save_draft').at(-1)?.args.config as
          { globals: { word_separators: string } } | undefined
      )?.globals.word_separators;

    /** Save from the settings and wait for it. */
    async function saveSettings(mounted: Mounted) {
      const count = callsOf(mounted.backend, 'save_draft').length;
      await click(mounted, '保存参数草稿');
      await waitFor(mounted, () => {
        expect(callsOf(mounted.backend, 'save_draft')).toHaveLength(count + 1);
      });
      await settle(mounted);
    }

    /** A focus load, applied. */
    async function focusLoad(mounted: Mounted) {
      const count = callsOf(mounted.backend, 'load_state').length;
      await focus(mounted);
      await waitFor(mounted, () => {
        expect(callsOf(mounted.backend, 'load_state')).toHaveLength(count + 1);
      });
      await settle(mounted);
    }

    /**
     * `confirmed` sent, its answer held (after `meanwhile`, if any) while a focus load shows the
     * store as the command left it; then the answer arrives, and is dropped: the load's revision is
     * as new, and its request later.
     */
    async function dropAnswer(
      mounted: Mounted,
      hold: LoadHold,
      confirmed: Confirmed,
      meanwhile?: () => Promise<void>,
    ) {
      hold.arm();
      await confirmOnStatus(mounted, confirmed);
      await waitFor(mounted, () => {
        expect(hold.held[0]?.read).toBe(true);
      });
      if (meanwhile) await meanwhile();
      await focusLoad(mounted);
      await answerHeld(mounted, hold, 0);
    }

    it('puts the discarded draft in the editor when the answer of 放弃未应用草稿 is dropped', async () => {
      const hold = holdAnswers('discard_draft');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      await click(mounted, '文本指令参数设置');
      const before = separators(container);
      // The field shows it without the line break.
      const stored = backend.draft.globals.word_separators;
      await type(mounted, field(container, '词分隔符'), ' ;');
      await dropAnswer(mounted, hold, DISCARD);
      expect(separators(container)).toBe(before);
      expect(revisionText(container)).toContain('配置已同步');
      // Saving does not bring the discarded edit back.
      await saveSettings(mounted);
      expect(sentSeparators(backend)).toBe(stored);
      expect(backend.draft.globals.word_separators).toBe(stored);
    });

    it('puts the restored config in the editor when the answer of 恢复上一个版本 is dropped', async () => {
      const hold = holdAnswers('rollback');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      await click(mounted, '文本指令参数设置');
      const before = separators(container);
      const stored = backend.draft.globals.word_separators;
      // v2 published elsewhere, so there is a v1 to go back to.
      await publish(mounted, ',');
      await waitFor(mounted, () => {
        expect(separators(container)).toBe(',');
      });
      await type(mounted, field(container, '词分隔符'), ' ;');
      await dropAnswer(mounted, hold, ROLLBACK);
      expect(separators(container)).toBe(before);
      expect(revisionText(container)).toContain('当前生效 v3');
      expect(revisionText(container)).toContain('配置已同步');
      // The load said v3 was newer than the edit; the editor has v3 now.
      expect(notices(container)).toBe('');
      // Taken from the view shown, not an earlier one: nothing to read again.
      expect(callsOf(backend, 'load_state')).toHaveLength(3);
      await saveSettings(mounted);
      expect(sentSeparators(backend)).toBe(stored);
    });

    it('marks the editor saved when the answer of its save is dropped', async () => {
      const hold = holdAnswers('save_draft');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      await click(mounted, '文本指令参数设置');
      await type(mounted, field(container, '词分隔符'), ' ;');
      hold.arm();
      await click(mounted, '保存参数草稿');
      await waitFor(mounted, () => {
        expect(hold.held[0]?.read).toBe(true);
      });
      await focusLoad(mounted);
      await answerHeld(mounted, hold, 0);
      expect(separators(container)).toBe(' ;');
      expect(statusText(container)).toBe('参数已保存到草稿');
      expect(notices(container)).toBe('');
      // Nothing left to save: 应用设置 applies at once.
      await click(mounted, '应用设置');
      await waitFor(mounted, () => {
        expect(revisionText(container)).toContain('当前生效 v2');
      });
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
      expect(backend.active.globals.word_separators).toBe(' ;');
      // Nor an unsaved edit to keep: a revision published elsewhere is taken straight in.
      await publish(mounted, '|');
      await waitFor(mounted, () => {
        expect(separators(container)).toBe('|');
      });
      expect(notices(container)).toBe('');
    });

    it('keeps edits made after 放弃未应用草稿 was sent when its answer is dropped', async () => {
      const hold = holdAnswers('discard_draft');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      await click(mounted, '文本指令参数设置');
      await type(mounted, field(container, '词分隔符'), ' ;');
      await dropAnswer(mounted, hold, DISCARD, () =>
        type(mounted, field(container, '词分隔符'), ' |'),
      );
      expect(separators(container)).toBe(' |');
      expect(revisionText(container)).toContain('草稿待应用');
      await saveSettings(mounted);
      expect(sentSeparators(backend)).toBe(' |');
    });

    it('keeps edits made while 恢复上一个版本 ran, and says the revision it made', async () => {
      const hold = holdAnswers('rollback');
      const mounted = await mountReady(hold);
      const { container } = mounted;
      await click(mounted, '文本指令参数设置');
      const before = separators(container);
      await publish(mounted, ',');
      await waitFor(mounted, () => {
        expect(separators(container)).toBe(',');
      });
      hold.arm();
      await confirmOnStatus(mounted, ROLLBACK);
      await waitFor(mounted, () => {
        expect(hold.held[0]?.read).toBe(true);
      });
      await type(mounted, field(container, '词分隔符'), ' |');
      // Installed: v3 is newer than anything shown.
      await answerHeld(mounted, hold, 0);
      expect(separators(container)).toBe(' |');
      expect(revisionText(container)).toContain('当前生效 v3');
      // This window made v3: not said to come from 「后台」.
      expect(notices(container)).toContain(
        '配置已更新为 v3（恢复上一个版本）。编辑器中未保存的修改仍然保留',
      );
      expect(notices(container)).not.toContain('后台');
      await click(mounted, '放弃修改，载入新配置');
      expect(separators(container)).toBe(before);
      expect(notices(container)).toBe('');
    });

    it('keeps edits made while 应用设置 saved, unsaved, over the revision it applied', async () => {
      const hold = holdAnswers('save_draft');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      await click(mounted, '文本指令参数设置');
      await type(mounted, field(container, '词分隔符'), ' ;');
      hold.arm();
      await click(mounted, '应用设置');
      await waitFor(mounted, () => {
        expect(hold.held[0]?.read).toBe(true);
      });
      await type(mounted, field(container, '词分隔符'), ' |');
      await answerHeld(mounted, hold, 0);
      await waitFor(mounted, () => {
        expect(revisionText(container)).toContain('当前生效 v2');
      });
      await settle(mounted);
      // What was saved is applied; the later edit stays in the editor, still to be saved.
      expect(backend.active.globals.word_separators).toBe(' ;');
      expect(separators(container)).toBe(' |');
      expect(revisionText(container)).toContain('草稿待应用');
      // v2 is this window's own: loading it says nothing newer.
      await focusLoad(mounted);
      expect(notices(container)).toBe('');
      expect(separators(container)).toBe(' |');
    });

    it('names 恢复上一个版本 in the notice a load raised before its dropped answer came', async () => {
      const hold = holdAnswers('rollback');
      const mounted = await mountReady(hold);
      const { container } = mounted;
      await click(mounted, '文本指令参数设置');
      await publish(mounted, ',');
      await waitFor(mounted, () => {
        expect(separators(container)).toBe(',');
      });
      // The load shows v3 over the later edit, before the answer says this window made it.
      await dropAnswer(mounted, hold, ROLLBACK, () =>
        type(mounted, field(container, '词分隔符'), ' |'),
      );
      expect(separators(container)).toBe(' |');
      expect(notices(container)).toContain(
        '配置已更新为 v3（恢复上一个版本）。编辑器中未保存的修改仍然保留',
      );
      expect(notices(container)).not.toContain('后台');
      // Saving over it names it the same way.
      await saveSettings(mounted);
      expect(notices(container)).toContain(
        '配置已更新为 v3（恢复上一个版本），保存已用编辑器中的配置覆盖了它的草稿',
      );
      expect(notices(container)).not.toContain('后台');
    });

    it('says nothing newer of the revision 应用设置 made when its answer is dropped', async () => {
      const hold = holdAnswers('apply_settings');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      await click(mounted, '文本指令参数设置');
      await type(mounted, field(container, '词分隔符'), ' ;');
      hold.arm();
      await click(mounted, '应用设置');
      await waitFor(mounted, () => {
        expect(hold.held[0]?.read).toBe(true);
      });
      await type(mounted, field(container, '词分隔符'), ' |');
      await focusLoad(mounted);
      await answerHeld(mounted, hold, 0);
      expect(backend.active.globals.word_separators).toBe(' ;');
      expect(revisionText(container)).toContain('当前生效 v2');
      // As had the answer come first: the later edit kept, over v2, which is this window's own.
      expect(separators(container)).toBe(' |');
      expect(revisionText(container)).toContain('草稿待应用');
      expect(notices(container)).toBe('');
      await focusLoad(mounted);
      expect(notices(container)).toBe('');
      expect(separators(container)).toBe(' |');
    });

    /** 总开关 as its checkbox shows it. */
    const enabled = (container: HTMLElement) => checkbox(container, '启用文本指令')?.checked;
    /** What the last save_draft sent for 总开关. */
    const sentEnabled = (backend: MockBackend) =>
      (callsOf(backend, 'save_draft').at(-1)?.args.config as { enabled: boolean } | undefined)
        ?.enabled;

    /**
     * Over an unsaved edit of the word separators, 总开关 saves a config the editor does not show:
     * the edit, and the switch turned. Its answer is held; returns the switch as it was.
     */
    async function toggleHeld(mounted: Mounted, hold: LoadHold) {
      const { container } = mounted;
      await click(mounted, '文本指令参数设置');
      await type(mounted, field(container, '词分隔符'), ' ;');
      await click(mounted, '文本指令管理');
      const before = enabled(container);
      hold.arm();
      await mounted.act(() => {
        checkbox(container, '启用文本指令')?.click();
      });
      await waitFor(mounted, () => {
        expect(hold.held[0]?.read).toBe(true);
      });
      return before;
    }

    it('puts what 总开关 saved in the editor when its answer is dropped for a later load', async () => {
      const hold = holdAnswers('save_draft');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      const before = await toggleHeld(mounted, hold);
      // The load shows the store as the save left it; the editor, with its unsaved edit, keeps its
      // config.
      await focusLoad(mounted);
      expect(enabled(container)).toBe(before);
      await answerHeld(mounted, hold, 0);
      expect(enabled(container)).toBe(!before);
      expect(notices(container)).toBe('');
      // Nothing unsaved: 导出 does not save first.
      await click(mounted, '导出');
      await waitFor(mounted, () => {
        expect(callsOf(backend, 'export_config')).toHaveLength(1);
      });
      await settle(mounted);
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
      await click(mounted, '文本指令参数设置');
      expect(separators(container)).toBe(' ;');
      // A save from the editor keeps what 总开关 saved.
      await saveSettings(mounted);
      expect(sentEnabled(backend)).toBe(!before);
      expect(sentSeparators(backend)).toBe(' ;');
    });

    it('keeps edits made after 总开关 saved when its answer is dropped for a later load', async () => {
      const hold = holdAnswers('save_draft');
      const mounted = await mountReady(hold);
      const { container } = mounted;
      const before = await toggleHeld(mounted, hold);
      await click(mounted, '文本指令参数设置');
      await type(mounted, field(container, '词分隔符'), ' |');
      await focusLoad(mounted);
      await answerHeld(mounted, hold, 0);
      expect(separators(container)).toBe(' |');
      expect(revisionText(container)).toContain('草稿待应用');
      // With what 总开关 saved, under the edit (review C7 §4).
      await click(mounted, '文本指令管理');
      expect(enabled(container)).toBe(!before);
      await click(mounted, '文本指令参数设置');
      // Still unsaved: a revision published elsewhere does not replace it.
      await publish(mounted, ',');
      await waitFor(mounted, () => {
        expect(notices(container)).toContain('配置已在后台更新为 v2。编辑器中未保存的修改仍然保留');
      });
      expect(separators(container)).toBe(' |');
    });

    it('takes in what was published after a save ran when its answer is dropped', async () => {
      const hold = holdAnswers('save_draft');
      const mounted = await mountReady(hold);
      const { backend, container } = mounted;
      // Another window publishes v2 just before the save runs, and v3 after.
      const handle = backend.handle.bind(backend);
      let published = false;
      backend.handle = (...[command, args]: Parameters<MockBackend['handle']>) => {
        if (command === 'save_draft' && !published) {
          published = true;
          backend.publishExternally((config) => {
            config.globals.word_separators = ',';
          });
        }
        return handle(command, args);
      };
      await click(mounted, '文本指令参数设置');
      await type(mounted, field(container, '词分隔符'), ' ;');
      hold.arm();
      await click(mounted, '保存参数草稿');
      await waitFor(mounted, () => {
        expect(hold.held[0]?.read).toBe(true);
      });
      await publish(mounted, '|');
      // Shown over the edit being saved, so said newer.
      await focusLoad(mounted);
      expect(notices(container)).toContain('配置已在后台更新为 v3');
      await answerHeld(mounted, hold, 0);
      // As had the answer come first: the save over v2 said, then v3 taken in, which makes both moot.
      expect(separators(container)).toBe('|');
      expect(revisionText(container)).toContain('当前生效 v3');
      expect(notices(container)).toBe('');
    });

    describe("a save's change survives edits made while it ran (review C7 §4)", () => {
      const FAILURE = '草稿文件无法写入';

      /** 导出, which saves the editor's config first when it has unsaved edits: whether it did. */
      async function exportSaves(mounted: Mounted): Promise<boolean> {
        const { backend } = mounted;
        const saves = callsOf(backend, 'save_draft').length;
        const exports = callsOf(backend, 'export_config').length;
        await click(mounted, '导出');
        await waitFor(mounted, () => {
          expect(callsOf(backend, 'export_config')).toHaveLength(exports + 1);
        });
        await settle(mounted);
        return callsOf(backend, 'save_draft').length > saves;
      }

      /** The config the last save_draft sent. */
      const sent = (backend: MockBackend) =>
        callsOf(backend, 'save_draft').at(-1)?.args.config as
          { rules: { match: { value: string } }[]; globals: { blacklist: string[] } } | undefined;
      /** The abbreviations of the rules listed. */
      const listed = (container: HTMLElement) =>
        [...container.querySelectorAll('input[aria-label^="选择 "]')].map((box) =>
          box.getAttribute('aria-label')?.slice('选择 '.length),
        );
      const batch = (container: HTMLElement) =>
        container.querySelector('.batch span')?.textContent ?? '';
      const toggle = (mounted: Mounted) =>
        mounted.act(() => {
          checkbox(mounted.container, '启用文本指令')?.click();
        });

      /** Each armed call of `name` runs nothing and fails with `failure`, on its `release()`. */
      function holdFailures(name: string, failure: unknown): LoadHold {
        let armed = 0;
        const held: HeldLoad[] = [];
        return {
          held,
          attach: (backend) => {
            const handle = backend.handle.bind(backend);
            backend.handle = async (...[command, args]: Parameters<MockBackend['handle']>) => {
              if (command !== name || armed === 0) return handle(command, args);
              armed -= 1;
              const entry: HeldLoad = { read: true, answered: false, release: () => undefined };
              held.push(entry);
              await new Promise<void>((resolve) => {
                entry.release = () => {
                  resolve();
                };
              });
              entry.answered = true;
              throw failure;
            };
          },
          arm: (count = 1) => {
            armed += count;
          },
        };
      }

      it('puts what 总开关 saved under edits made while it ran, its answer installed', async () => {
        const hold = holdAnswers('save_draft');
        const mounted = await mountReady(hold);
        const { backend, container } = mounted;
        const before = await toggleHeld(mounted, hold);
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' |');
        await answerHeld(mounted, hold, 0);
        expect(separators(container)).toBe(' |');
        await click(mounted, '文本指令管理');
        expect(enabled(container)).toBe(!before);
        // The edit is still unsaved, and the editor's next save has both.
        expect(await exportSaves(mounted)).toBe(true);
        expect(sentEnabled(backend)).toBe(!before);
        expect(sentSeparators(backend)).toBe(' |');
      });

      it('puts what 总开关 saved under edits made while it ran, its answer dropped', async () => {
        const hold = holdAnswers('save_draft');
        const mounted = await mountReady(hold);
        const { backend, container } = mounted;
        const before = await toggleHeld(mounted, hold);
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' |');
        await focusLoad(mounted);
        await answerHeld(mounted, hold, 0);
        expect(separators(container)).toBe(' |');
        expect(notices(container)).toBe('');
        await click(mounted, '文本指令管理');
        expect(enabled(container)).toBe(!before);
        expect(await exportSaves(mounted)).toBe(true);
        expect(sentEnabled(backend)).toBe(!before);
        expect(sentSeparators(backend)).toBe(' |');
      });

      it('sets 总开关 as the click chose, over a view taken in that turned it elsewhere', async () => {
        const hold = holdAnswers('save_draft');
        const mounted = await mountReady(hold);
        const { backend, container } = mounted;
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' ;');
        // Another window turns 总开关, read over the edit: the editor keeps its own.
        await mounted.act(() => {
          backend.publishExternally((config) => {
            config.enabled = !config.enabled;
          });
        });
        await waitFor(mounted, () => {
          expect(notices(container)).toContain(
            '配置已在后台更新为 v2。编辑器中未保存的修改仍然保留',
          );
        });
        await click(mounted, '文本指令管理');
        const before = enabled(container);
        hold.arm();
        await toggle(mounted);
        await waitFor(mounted, () => {
          expect(hold.held[0]?.read).toBe(true);
        });
        expect(sentEnabled(backend)).toBe(!before);
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' |');
        // Another command done meanwhile leaves the notice's buttons usable.
        await mounted.act(() => {
          checkbox(container, '开机时启动')?.click();
        });
        await waitFor(mounted, () => {
          expect(button(container, '放弃修改，载入新配置')?.disabled).toBe(false);
        });
        // v2, read before the save ran, has 总开关 as the click set it: turned over, it is undone.
        await click(mounted, '放弃修改，载入新配置');
        await answerHeld(mounted, hold, 0);
        await click(mounted, '文本指令管理');
        expect(enabled(container)).toBe(!before);
        expect(await exportSaves(mounted)).toBe(true);
        expect(sentEnabled(backend)).toBe(!before);
      });

      it('deletes the selected rules under edits made while the delete ran, and unselects them', async () => {
        const hold = holdAnswers('save_draft');
        const mounted = await mountReady(hold);
        const { backend, container } = mounted;
        for (const label of ['选择 ;s1', '选择 ;s3']) {
          const box = container.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`);
          await mounted.act(() => {
            box?.click();
          });
        }
        expect(batch(container)).toBe('共 3 条 · 已选 2 条');
        hold.arm();
        await clickIn(mounted, container.querySelector('.batch'), '删除');
        const question = '删除所选 2 条指令？';
        await waitFor(mounted, () => {
          expect(dialog(container, question)).not.toBeNull();
        });
        await clickIn(mounted, dialog(container, question), '删除');
        await waitFor(mounted, () => {
          expect(hold.held[0]?.read).toBe(true);
        });
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' |');
        await answerHeld(mounted, hold, 0);
        expect(separators(container)).toBe(' |');
        await click(mounted, '文本指令管理');
        expect(listed(container)).toEqual([';s2']);
        expect(batch(container)).toBe('共 1 条 · 已选 0 条');
        expect(await exportSaves(mounted)).toBe(true);
        expect(sent(backend)?.rules.map((r) => r.match.value)).toEqual([';s2']);
        expect(sentSeparators(backend)).toBe(' |');
      });

      // Not a path of the app, where the rule editor is a modal dialog: no edit can be made while
      // it saves. jsdom's dialog is not modal; kept as cover of `conclude` for a rule committed.
      it('puts a rule committed under edits made while it was saved (unit cover: modal in the app)', async () => {
        const hold = holdAnswers('save_draft');
        const mounted = await mountReady(hold);
        const { backend, container } = mounted;
        await click(mounted, '＋ 添加指令');
        const editor = dialog(container, '编辑文本指令');
        await type(mounted, field(editor ?? container, '缩写词'), ';new');
        hold.arm();
        await clickIn(mounted, editor, '保存');
        await waitFor(mounted, () => {
          expect(hold.held[0]?.read).toBe(true);
        });
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' |');
        await answerHeld(mounted, hold, 0);
        expect(dialog(container, '编辑文本指令')).toBeNull();
        expect(separators(container)).toBe(' |');
        await click(mounted, '文本指令管理');
        expect(listed(container)).toContain(';new');
        expect(await exportSaves(mounted)).toBe(true);
        expect(sent(backend)?.rules.map((r) => r.match.value)).toEqual([
          ';s1',
          ';s2',
          ';s3',
          ';new',
        ]);
        expect(sentSeparators(backend)).toBe(' |');
      });

      it('sends the config as it is when the save goes out, with edits made while it was awaited', async () => {
        // The rule's check is awaited before its save; in the app the editor is modal meanwhile.
        const check = holdAnswers('validate_config');
        const mounted = await mountReady(check);
        const { backend, container } = mounted;
        await click(mounted, '＋ 添加指令');
        const editor = dialog(container, '编辑文本指令');
        await type(mounted, field(editor ?? container, '缩写词'), ';new');
        check.arm();
        await clickIn(mounted, editor, '保存');
        await waitFor(mounted, () => {
          expect(check.held[0]?.read).toBe(true);
        });
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' |');
        await answerHeld(mounted, check, 0);
        await waitFor(mounted, () => {
          expect(callsOf(backend, 'save_draft')).toHaveLength(1);
        });
        await settle(mounted);
        expect(sentSeparators(backend)).toBe(' |');
        expect(sent(backend)?.rules.map((r) => r.match.value)).toEqual([
          ';s1',
          ';s2',
          ';s3',
          ';new',
        ]);
        // Saved, not made since: the editor has the store's config, with nothing left to save.
        expect(separators(container)).toBe(' |');
        expect(backend.draft.globals.word_separators).toBe(' |');
        await click(mounted, '文本指令管理');
        expect(await exportSaves(mounted)).toBe(false);
      });

      it('makes nothing again of a save that failed, keeping edits made while it ran', async () => {
        const hold = holdFailures('save_draft', FAILURE);
        const mounted = await mountReady(hold);
        const { backend, container } = mounted;
        const before = await toggleHeld(mounted, hold);
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' |');
        await answerHeld(mounted, hold, 0);
        expect(container.querySelector('.alert.error')?.textContent).toContain(FAILURE);
        expect(separators(container)).toBe(' |');
        await click(mounted, '文本指令管理');
        expect(enabled(container)).toBe(before);
        expect(await exportSaves(mounted)).toBe(true);
        expect(sentEnabled(backend)).toBe(before);
        expect(sentSeparators(backend)).toBe(' |');
      });

      // No store write is sent while the save runs (review C10): another window undoes it, and a
      // load reads that after the save ran, which the editor takes in.
      it('makes nothing again over a view read after the save ran, undoing it, taken in', async () => {
        const hold = holdAnswers('save_draft');
        const mounted = await mountReady(hold);
        const { backend, container } = mounted;
        const before = await toggleHeld(mounted, hold);
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' |');
        await mounted.act(() => {
          backend.publishExternally((config) => {
            config.enabled = !config.enabled;
          });
        });
        await focusLoad(mounted);
        expect(notices(container)).toContain('配置已在后台更新为 v2。编辑器中未保存的修改仍然保留');
        // Another command done meanwhile leaves the notice's buttons usable.
        await mounted.act(() => {
          checkbox(container, '开机时启动')?.click();
        });
        await waitFor(mounted, () => {
          expect(button(container, '放弃修改，载入新配置')?.disabled).toBe(false);
        });
        await click(mounted, '放弃修改，载入新配置');
        // v2: the draft the save left (with the edit it sent), 总开关 turned back.
        expect(separators(container)).toBe(' ;');
        // Dropped: v2 is newer. The view taken in stays as it is.
        await answerHeld(mounted, hold, 0);
        expect(separators(container)).toBe(' ;');
        await click(mounted, '文本指令管理');
        expect(enabled(container)).toBe(before);
        expect(await exportSaves(mounted)).toBe(false);
      });

      // As above, the view read after the save ran comes after its answer: the change is made again
      // under the edits, and the view, said newer, takes the editor when asked to.
      it('lets a view read after the save ran take the editor, asked to, after the change is made again', async () => {
        const hold = holdAnswers('save_draft');
        const mounted = await mountReady(hold);
        const { backend, container } = mounted;
        const before = await toggleHeld(mounted, hold);
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' |');
        // Read once the save is done: a revision's read waits for the writes in flight.
        await mounted.act(() => {
          backend.publishExternally((config) => {
            config.enabled = !config.enabled;
          });
        });
        await answerHeld(mounted, hold, 0);
        await waitFor(mounted, () => {
          expect(notices(container)).toContain(
            '配置已在后台更新为 v2。编辑器中未保存的修改仍然保留',
          );
        });
        expect(separators(container)).toBe(' |');
        await click(mounted, '文本指令管理');
        expect(enabled(container)).toBe(!before);
        await click(mounted, '放弃修改，载入新配置');
        expect(enabled(container)).toBe(before);
        expect(await exportSaves(mounted)).toBe(false);
        await click(mounted, '文本指令参数设置');
        expect(separators(container)).toBe(' ;');
      });

      it('keeps the editor unsaved when a save is made again over a view read before it ran', async () => {
        const hold = holdAnswers('save_draft');
        const mounted = await mountReady(hold);
        const { backend, container } = mounted;
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' ;');
        // v2, read over the edit before 总开关 saves.
        await publish(mounted, ',');
        await waitFor(mounted, () => {
          expect(notices(container)).toContain(
            '配置已在后台更新为 v2。编辑器中未保存的修改仍然保留',
          );
        });
        await click(mounted, '文本指令管理');
        const before = enabled(container);
        hold.arm();
        await toggle(mounted);
        await waitFor(mounted, () => {
          expect(hold.held[0]?.read).toBe(true);
        });
        await click(mounted, '文本指令参数设置');
        await type(mounted, field(container, '词分隔符'), ' |');
        // Another command done meanwhile leaves the notice's buttons usable.
        await mounted.act(() => {
          checkbox(container, '开机时启动')?.click();
        });
        await waitFor(mounted, () => {
          expect(button(container, '放弃修改，载入新配置')?.disabled).toBe(false);
        });
        await click(mounted, '放弃修改，载入新配置');
        expect(separators(container)).toBe(',');
        await answerHeld(mounted, hold, 0);
        // v2 with what 总开关 saved: not what the store has (the save replaced v2's draft).
        expect(notices(container)).toContain('保存已用编辑器中的配置覆盖了它的草稿');
        expect(separators(container)).toBe(',');
        // Unsaved: a load does not replace it, and 导出 saves it first.
        await focusLoad(mounted);
        expect(separators(container)).toBe(',');
        await click(mounted, '文本指令管理');
        expect(enabled(container)).toBe(!before);
        expect(await exportSaves(mounted)).toBe(true);
        expect(sentEnabled(backend)).toBe(!before);
        expect(sentSeparators(backend)).toBe(',');
      });

      it('adds the window to 黑名单 over what 总开关 saved while it was asked for', async () => {
        // Edits change the globals alone, as they are when made (see `change` in App).
        const foreground = holdAnswers('get_foreground');
        const mounted = await mountReady(foreground);
        const { backend, container } = mounted;
        await click(mounted, '文本指令参数设置');
        foreground.arm();
        await click(mounted, '选择当前窗口加入黑名单');
        await waitFor(mounted, () => {
          expect(foreground.held[0]?.read).toBe(true);
        });
        await click(mounted, '文本指令管理');
        const before = enabled(container);
        await toggle(mounted);
        // No edit yet: the editor takes the answer.
        await waitFor(mounted, () => {
          expect(enabled(container)).toBe(!before);
        });
        await answerHeld(mounted, foreground, 0);
        await clickIn(mounted, dialog(container, '窗口信息'), '关闭');
        expect(enabled(container)).toBe(!before);
        expect(await exportSaves(mounted)).toBe(true);
        expect(sentEnabled(backend)).toBe(!before);
        expect(sent(backend)?.globals.blacklist).toContain('notepad.exe');
      });

      describe("no store write while a save's change is in flight (review C10)", () => {
        const PENDING = '上一次保存尚未完成，请稍候';
        const errorShown = (container: HTMLElement) =>
          container.querySelector('.alert.error')?.textContent ?? '';
        const batchDelete = (container: HTMLElement) =>
          [...container.querySelectorAll<HTMLButtonElement>('.batch button')].find(
            (b) => b.textContent === '删除',
          );

        it('sends no other save while 总开关 saves, and saves again once it answers', async () => {
          const hold = holdAnswers('save_draft');
          const mounted = await mountReady(hold);
          const { backend, container } = mounted;
          const before = await toggleHeld(mounted, hold);
          await click(mounted, '文本指令参数设置');
          await type(mounted, field(container, '词分隔符'), ' |');
          const saveButton = () => button(container, '保存参数草稿');
          expect(saveButton()?.disabled).toBe(true);
          // Sent, it would have the edits without 总开关 turned, and undo it.
          await callHandler(mounted, saveButton(), 'onClick');
          await settle(mounted);
          expect(errorShown(container)).toContain(PENDING);
          expect(callsOf(backend, 'save_draft')).toHaveLength(1);
          await answerHeld(mounted, hold, 0);
          expect(saveButton()?.disabled).toBe(false);
          await saveSettings(mounted);
          expect(sentEnabled(backend)).toBe(!before);
          expect(sentSeparators(backend)).toBe(' |');
        });

        it('deletes nothing while 总开关 saves its change', async () => {
          const hold = holdAnswers('save_draft');
          const mounted = await mountReady(hold);
          const { backend, container } = mounted;
          for (const label of ['选择 ;s1', '选择 ;s3']) {
            const box = container.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`);
            await mounted.act(() => {
              box?.click();
            });
          }
          const before = enabled(container);
          hold.arm();
          await toggle(mounted);
          await waitFor(mounted, () => {
            expect(hold.held[0]?.read).toBe(true);
          });
          expect(batchDelete(container)?.disabled).toBe(true);
          await callHandler(mounted, batchDelete(container), 'onClick');
          const question = '删除所选 2 条指令？';
          await waitFor(mounted, () => {
            expect(dialog(container, question)).not.toBeNull();
          });
          await clickIn(mounted, dialog(container, question), '删除');
          await settle(mounted);
          expect(errorShown(container)).toContain(PENDING);
          expect(callsOf(backend, 'save_draft')).toHaveLength(1);
          await answerHeld(mounted, hold, 0);
          expect(listed(container)).toEqual([';s1', ';s2', ';s3']);
          expect(enabled(container)).toBe(!before);
          expect(batchDelete(container)?.disabled).toBe(false);
        });

        it("leaves 总开关 usable while a save of the editor's config is in flight", async () => {
          const hold = holdAnswers('save_draft');
          const mounted = await mountReady(hold);
          const { backend, container } = mounted;
          await click(mounted, '文本指令参数设置');
          await type(mounted, field(container, '词分隔符'), ' ;');
          hold.arm();
          await click(mounted, '保存参数草稿');
          await waitFor(mounted, () => {
            expect(hold.held[0]?.read).toBe(true);
          });
          await click(mounted, '文本指令管理');
          const before = enabled(container);
          expect(checkbox(container, '启用文本指令')?.disabled).toBe(false);
          await toggle(mounted);
          await waitFor(mounted, () => {
            expect(callsOf(backend, 'save_draft')).toHaveLength(2);
          });
          await settle(mounted);
          // Sent after the save of the edit, and with it.
          expect(sentEnabled(backend)).toBe(!before);
          expect(sentSeparators(backend)).toBe(' ;');
          await answerHeld(mounted, hold, 0);
          expect(errorShown(container)).toBe('');
          expect(enabled(container)).toBe(!before);
          expect(backend.draft.enabled).toBe(!before);
        });

        it('allows the writes again once a save of a change fails', async () => {
          const hold = holdFailures('save_draft', FAILURE);
          const mounted = await mountReady(hold);
          const { backend, container } = mounted;
          const before = await toggleHeld(mounted, hold);
          expect(checkbox(container, '启用文本指令')?.disabled).toBe(true);
          expect(button(container, '导出')?.disabled).toBe(true);
          await answerHeld(mounted, hold, 0);
          expect(errorShown(container)).toContain(FAILURE);
          expect(checkbox(container, '启用文本指令')?.disabled).toBe(false);
          expect(button(container, '导出')?.disabled).toBe(false);
          const saves = callsOf(backend, 'save_draft').length;
          await toggle(mounted);
          await waitFor(mounted, () => {
            expect(callsOf(backend, 'save_draft')).toHaveLength(saves + 1);
          });
          await settle(mounted);
          expect(sentEnabled(backend)).toBe(!before);
          expect(backend.draft.enabled).toBe(!before);
        });

        it('sends no 放弃未应用草稿, 恢复上一个版本 or 应用设置 while 总开关 saves its change', async () => {
          const hold = holdAnswers('save_draft');
          const mounted = await mountReady(hold);
          const { backend, container } = mounted;
          await click(mounted, '文本指令参数设置');
          const original = separators(container);
          await click(mounted, '文本指令管理');
          const before = enabled(container);
          hold.arm();
          await toggle(mounted);
          await waitFor(mounted, () => {
            expect(hold.held[0]?.read).toBe(true);
          });
          // 应用设置 is disabled while busy too: another command done meanwhile ends that.
          await click(mounted, '文本指令参数设置');
          await mounted.act(() => {
            checkbox(container, '开机时启动')?.click();
          });
          await waitFor(mounted, () => {
            expect(callsOf(backend, 'set_autostart')).toHaveLength(1);
          });
          await settle(mounted);
          expect(statusText(container)).not.toBe('正在处理…');
          const apply = () => button(container, '应用设置');
          expect(apply()?.disabled).toBe(true);
          // Nothing unsaved: it would publish the draft at once.
          await callHandler(mounted, apply(), 'onClick');
          await settle(mounted);
          expect(errorShown(container)).toContain(PENDING);
          // An edit, then the commands that would undo the save, its change made again over them.
          await type(mounted, field(container, '词分隔符'), ' |');
          await click(mounted, '运行状态与恢复');
          for (const [text, title, confirm] of [DISCARD, ROLLBACK]) {
            expect(button(container, text)?.disabled).toBe(true);
            await callHandler(mounted, button(container, text), 'onClick');
            await waitFor(mounted, () => {
              expect(dialog(container, title)).not.toBeNull();
            });
            await clickIn(mounted, dialog(container, title), confirm);
            await settle(mounted);
          }
          for (const command of ['apply_settings', 'discard_draft', 'rollback'])
            expect(callsOf(backend, command)).toEqual([]);
          expect(errorShown(container)).toContain(PENDING);
          await answerHeld(mounted, hold, 0);
          for (const text of ['放弃未应用草稿', '恢复上一个版本', '应用设置'])
            expect(button(container, text)?.disabled).toBe(false);
          // Sent now, 放弃未应用草稿 puts in the editor the draft as discarded: the active config.
          await confirmOnStatus(mounted, DISCARD);
          await waitFor(mounted, () => {
            expect(separators(container)).toBe(original);
          });
          await click(mounted, '文本指令管理');
          expect(enabled(container)).toBe(before);
          expect(await exportSaves(mounted)).toBe(false);
        });

        it('keeps the import dialog open until its save is done, the editor then the import', async () => {
          const hold = holdAnswers('save_draft');
          const mounted = await mountReady(hold);
          const { backend, container } = mounted;
          const strategy = '遇到重复的指令时怎么处理？';
          await click(mounted, '导入');
          await waitFor(mounted, () => {
            expect(dialog(container, strategy)).not.toBeNull();
          });
          await clickIn(mounted, dialog(container, strategy), '导入');
          await waitFor(mounted, () => {
            expect(button(container, '确认导入草稿')).toBeDefined();
          });
          hold.arm();
          await click(mounted, '确认导入草稿');
          await waitFor(mounted, () => {
            expect(hold.held[0]?.read).toBe(true);
          });
          // Closed, it would let the editor be edited, and the import replace the edits.
          const report = () => dialog(container, '导入预览 · 确认后写入草稿');
          const close = () =>
            [...(report()?.querySelectorAll('button') ?? [])].find((b) => b.textContent === '关闭');
          expect(close()?.disabled).toBe(true);
          expect(
            report()?.querySelector<HTMLButtonElement>('[aria-label="关闭报告"]')?.disabled,
          ).toBe(true);
          await clickIn(mounted, report(), '关闭');
          expect(report()?.open).toBe(true);
          // Esc.
          await mounted.act(() => {
            report()?.dispatchEvent(new Event('cancel', { cancelable: true }));
          });
          expect(report()?.open).toBe(true);
          // A close the browser does not let be cancelled (Esc again, no user activation since).
          await mounted.act(() => {
            const modal = report();
            modal?.close();
            modal?.dispatchEvent(new Event('close'));
          });
          expect(report()?.open).toBe(true);
          expect(callsOf(backend, 'save_draft')).toHaveLength(1);
          await answerHeld(mounted, hold, 0);
          await waitFor(mounted, () => {
            expect(report()).toBeNull();
          });
          expect(errorShown(container)).toBe('');
          expect(container.querySelectorAll('tbody tr')).toHaveLength(5);
          expect(revisionText(container)).toContain('草稿待应用');
          // As saved: nothing unsaved.
          expect(await exportSaves(mounted)).toBe(false);
        });

        it('lets the import dialog be closed once its save fails', async () => {
          const hold = holdFailures('save_draft', FAILURE);
          const mounted = await mountReady(hold);
          const { container } = mounted;
          const strategy = '遇到重复的指令时怎么处理？';
          await click(mounted, '导入');
          await waitFor(mounted, () => {
            expect(dialog(container, strategy)).not.toBeNull();
          });
          await clickIn(mounted, dialog(container, strategy), '导入');
          await waitFor(mounted, () => {
            expect(button(container, '确认导入草稿')).toBeDefined();
          });
          hold.arm();
          await click(mounted, '确认导入草稿');
          await waitFor(mounted, () => {
            expect(hold.held[0]?.read).toBe(true);
          });
          const report = () => dialog(container, '导入预览 · 确认后写入草稿');
          await answerHeld(mounted, hold, 0);
          expect(errorShown(container)).toContain(FAILURE);
          // Still open, to try again or close.
          expect(button(container, '确认导入草稿')?.disabled).toBe(false);
          await clickIn(mounted, report(), '关闭');
          expect(report()).toBeNull();
          expect(container.querySelectorAll('tbody tr')).toHaveLength(3);
        });
      });
    });
  });
});

describe('action types by capability status (S-16, principle 5)', () => {
  const EDITOR = '编辑文本指令';
  const rule = (action: Record<string, unknown>) => ({
    id: 'rule-a-capability',
    enabled: true,
    group: '默认',
    description: '',
    ignore_case: false,
    match: { kind: 'literal', value: ';a-capability' },
    trigger_mode: 'immediate',
    terminator: null,
    ime_erasure: null,
    action,
    parameter: { type: 'default_match' },
    notification: { enabled: false, message: '' },
  });
  const component = (name: Component, state: HealthState, reason: ReasonCode) => ({
    component: name,
    state,
    reason,
    since_ms: 0,
  });
  // Lowered like capabilities.rs: executor degraded makes keys / open degraded, clipboard failed
  // makes paste / html unavailable (the worse one wins).
  const lowered = {
    state: 'degraded' as const,
    components: [
      component('executor', 'degraded', 'executor_timeout'),
      component('clipboard', 'failed', 'clipboard_busy'),
    ],
  };

  /** Mount with the rule (listed first) and the lowered health, and open the rule. */
  async function editRule(platform: 'windows' | 'macos', action: Record<string, unknown>) {
    const mounted = await mount({ platform }, (backend) => {
      backend.draft.rules.push(structuredClone(rule(action)));
      backend.health = structuredClone(lowered);
    });
    await waitFor(mounted, () => {
      expect(mounted.container.querySelectorAll('tbody tr')).toHaveLength(4);
    });
    await click(mounted, '编辑');
    await mounted.act(() => {
      typeSelect(mounted).click();
    });
    return mounted;
  }

  const typeSelect = (mounted: Mounted) => {
    const select = dialog(mounted.container, EDITOR)?.querySelector<HTMLButtonElement>(
      '[role="combobox"][aria-label="操作类型"]',
    );
    if (!select) throw new Error('no picker 「操作类型」');
    return select;
  };
  const offered = (mounted: Mounted): Record<string, { text: string | null; disabled: boolean }> =>
    Object.fromEntries<{ text: string | null; disabled: boolean }>(
      [
        ...(dialog(mounted.container, EDITOR)?.querySelectorAll<HTMLElement>('[role="option"]') ??
          []),
      ].map((o) => [
        o.dataset.value ?? '',
        { text: o.textContent, disabled: o.getAttribute('aria-disabled') === 'true' },
      ]),
    );

  /** 保存 goes through; the action saved for the rule. */
  async function save(mounted: Mounted): Promise<unknown> {
    await clickIn(mounted, dialog(mounted.container, EDITOR), '保存');
    await waitFor(mounted, () => {
      expect(callsOf(mounted.backend, 'save_draft')).toHaveLength(1);
      expect(dialog(mounted.container, EDITOR)).toBeNull();
    });
    const { rules } = callsOf(mounted.backend, 'save_draft')[0]?.args.config as {
      rules: { id: string; action: unknown }[];
    };
    return rules.find((r) => r.id === 'rule-a-capability')?.action;
  }

  it('offers every action type by the status of the capability it needs', async () => {
    const mounted = await editRule('macos', {
      type: 'text',
      method: 'keys',
      source: { type: 'inline', value: 'hi' },
    });
    const options = offered(mounted);
    const degraded = `（降级：${reasonNames.executor_timeout ?? ''}）`;
    const unavailable = `（暂不可用：${reasonNames.clipboard_busy ?? ''}）`;
    // Degraded: selectable, with the reason.
    expect(options.text_keys).toEqual({
      text: `键入纯文本${degraded}`,
      disabled: false,
    });
    expect(options.hotkey).toEqual({
      text: `按快捷键${degraded}`,
      disabled: false,
    });
    expect(options.open).toEqual({
      text: `打开网页、文件或程序${degraded}`,
      disabled: false,
    });
    // Unavailable for now: listed, disabled, with the reason.
    expect(options.text_paste).toEqual({
      text: `粘贴纯文本${unavailable}`,
      disabled: true,
    });
    expect(options.html).toEqual({
      text: `粘贴带格式的文字${unavailable}`,
      disabled: true,
    });
    // Experimental (macOS): selectable, tagged.
    expect(options.replay).toEqual({
      text: '重放鼠标和键盘操作（实验性）',
      disabled: false,
    });
    // Never provided: not listed.
    expect(options.quicker).toBeUndefined();
    // Needing no capability: plain.
    expect(options.steps).toEqual({
      text: '按顺序执行多个操作',
      disabled: false,
    });
    expect(typeSelect(mounted).value).toBe('text_keys');
  });

  it('keeps an action type that is unavailable for now when the rule is opened and saved', async () => {
    const action = { type: 'text', method: 'paste', source: { type: 'inline', value: 'hi' } };
    const mounted = await editRule('windows', action);
    const select = typeSelect(mounted);
    expect(select.value).toBe('text_paste');
    expect(offered(mounted).text_paste?.disabled).toBe(false);
    expect(offered(mounted).text_paste?.text).toMatch(/^粘贴纯文本（暂不可用：.+）$/);
    expect(offered(mounted).html?.disabled).toBe(true);
    expect(dialog(mounted.container, EDITOR)?.querySelector('.capability-warning')).not.toBeNull();
    expect(await save(mounted)).toEqual(action);
  });

  it('keeps an action type the platform never provides when the rule is opened and saved', async () => {
    const action = { type: 'quicker', action_id: 'abc', parameter: '' };
    const mounted = await editRule('windows', action);
    expect(typeSelect(mounted).value).toBe('quicker');
    expect(offered(mounted).quicker).toEqual({
      text: '保留 Quicker 动作信息（暂不能执行）',
      disabled: false,
    });
    expect(await save(mounted)).toEqual(action);
  });
});

describe('tolerant reading (B-43)', () => {
  // Rules are [;s1 (with an unknown field), unreadable with id, ;s2, ;s3, 'not a rule', null].
  type Saved = Record<string, unknown> & {
    globals: Record<string, unknown>;
    rules: unknown[];
  };
  const lastSaved = (backend: MockBackend) =>
    callsOf(backend, 'save_draft').at(-1)?.args.config as Saved;

  async function mountSample(): Promise<Mounted> {
    const mounted = await mount({ unreadable: true });
    await waitFor(mounted, () => {
      expect(mounted.container.querySelectorAll('tbody tr')).toHaveLength(6);
    });
    return mounted;
  }

  it('lists unreadable entries read-only without crashing', async () => {
    const mounted = await mountSample();
    const { container } = mounted;
    const rows = [...container.querySelectorAll('tbody tr.unreadable-row')];
    expect(rows.map((r) => r.querySelector('strong')?.textContent)).toEqual([
      '无法读取的规则',
      '无法读取的规则',
      '无法读取的规则',
    ]);
    expect(rows.map((r) => r.querySelector('.unreadable-id')?.textContent ?? null)).toEqual([
      'mock-unreadable-1',
      null,
      null,
    ]);
    // Read-only only says this UI cannot edit them; the backend may still run them, so no row
    // claims they are not in effect (only a backend rule_unreadable diagnostic may say that).
    expect(rows.map((r) => r.querySelector('.muted')?.textContent)).toEqual(
      rows.map(() => '这条规则含有当前界面无法编辑的内容，只能查看；已保留在配置中。'),
    );
    expect(rows.every((r) => !/生效/.test(r.textContent))).toBe(true);
    // Nothing to select, edit or delete on them.
    expect(rows.every((r) => !r.querySelector('input, button'))).toBe(true);
    expect(container.querySelector('.batch span')?.textContent).toBe(
      '共 6 条（3 条无法读取） · 已选 0 条',
    );
    // Only readable rules belong to a group.
    await click(mounted, '问候1');
    expect(container.querySelectorAll('tbody tr')).toHaveLength(1);
    expect(container.querySelectorAll('tbody tr.unreadable-row')).toHaveLength(0);
  });

  it('writes raw entries and unknown keys back as the values received when rules change', async () => {
    const mounted = await mountSample();
    const { backend, container } = mounted;
    const before = structuredClone(backend.draft);
    const toggle = container.querySelector<HTMLInputElement>('input[aria-label="停用 ;s2"]');
    await mounted.act(() => {
      toggle?.click();
    });
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    const saved = lastSaved(backend);
    expect(saved.rules).toHaveLength(6);
    expect([saved.rules[1], saved.rules[4], saved.rules[5]]).toEqual([
      before.rules[1],
      'not a rule',
      null,
    ]);
    expect(saved.rules[2]).toMatchObject({ id: 'mock-rule-2', enabled: false });
    expect(saved.rules[0]).toHaveProperty('future_rule_field', [1, 2]);
    expect(saved.future_setting).toEqual({ sample: true });
    expect(saved.globals.future_global).toBe('kept');

    // Deleting a rule removes it in place: every other entry keeps its position relative to the rest.
    const remove = [...container.querySelectorAll('tbody tr')]
      .find((r) => r.querySelector('strong')?.textContent === ';s1')
      ?.querySelector<HTMLButtonElement>('.row-tools button:last-child');
    await mounted.act(() => {
      remove?.click();
    });
    const question = '删除指令“;s1”？';
    await waitFor(mounted, () => {
      expect(dialog(container, question)).not.toBeNull();
    });
    await clickIn(mounted, dialog(container, question), '删除');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(2);
    });
    const ids = lastSaved(backend).rules.map((r) =>
      typeof r === 'object' && r !== null ? (r as { id?: unknown }).id : r,
    );
    expect(ids).toEqual(['mock-unreadable-1', 'mock-rule-2', 'mock-rule-3', 'not a rule', null]);
    expect(lastSaved(backend).rules[0]).toEqual(before.rules[1]);
  });

  it('deletes selected rules in place around unreadable entries', async () => {
    const mounted = await mountSample();
    const { backend, container } = mounted;
    for (const label of ['选择 ;s1', '选择 ;s3']) {
      const box = container.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`);
      await mounted.act(() => {
        box?.click();
      });
    }
    await clickIn(mounted, container.querySelector('.batch'), '删除');
    const question = '删除所选 2 条指令？';
    await waitFor(mounted, () => {
      expect(dialog(container, question)).not.toBeNull();
    });
    await clickIn(mounted, dialog(container, question), '删除');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    const ids = lastSaved(backend).rules.map((r) =>
      typeof r === 'object' && r !== null ? (r as { id?: unknown }).id : r,
    );
    expect(ids).toEqual(['mock-unreadable-1', 'mock-rule-2', 'not a rule', null]);
  });

  it('lists entries with a malformed nested field read-only and writes them back as received', async () => {
    const rule = (id: string) => ({
      id,
      enabled: true,
      group: '默认',
      description: '',
      ignore_case: false,
      match: { kind: 'literal', value: `;${id}` },
      trigger_mode: 'immediate',
      terminator: null,
      ime_erasure: null,
      action: { type: 'none' },
      parameter: { type: 'default_match' },
      notification: { enabled: false, message: '' },
    });
    // One per nested field the list or the editor reads; each would throw if rendered as a rule.
    const malformed = [
      {
        ...rule('bad-terminator'),
        trigger_mode: 'terminator',
        terminator: { keys: 42, consume: true },
      },
      { ...rule('bad-match'), match: { kind: 'literal', value: 5 } },
      { ...rule('bad-ime'), ime_erasure: 7 },
      { ...rule('bad-text'), action: { type: 'text', method: 'keys', source: null } },
      { ...rule('bad-hotkey'), action: { type: 'hotkey', keys: 'ctrl+s' } },
      {
        ...rule('bad-steps'),
        action: { type: 'steps', plan: { steps: [{ operation: { type: 'delay', ms: -1.5 } }] } },
      },
      {
        ...rule('bad-plan'),
        action: { type: 'steps', plan: { version: 'future', dialect: { bad: true }, source: 42 } },
      },
      {
        ...rule('nested-unknown-key'),
        action: {
          type: 'send_keys',
          source: { type: 'inline', value: 'hello', future_option: { keep: true } },
        },
      },
      { ...rule('bad-open'), action: { type: 'open', target: 'x', args: 'a b' } },
      { ...rule('bad-future'), action: { type: 'from_the_future', payload: 1 } },
      { ...rule('bad-parameter'), parameter: { type: 'fixed', value: 3 } },
      { ...rule('bad-notification'), notification: null },
    ];
    const mounted = await mount({}, (backend) => {
      backend.draft.rules.splice(1, 0, ...structuredClone(malformed));
    });
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(container.querySelectorAll('tbody tr')).toHaveLength(3 + malformed.length);
    });
    const rows = [...container.querySelectorAll('tbody tr.unreadable-row')];
    expect(rows.map((r) => r.querySelector('strong')?.textContent)).toEqual(
      malformed.map(() => '无法读取的规则'),
    );
    expect(rows.map((r) => r.querySelector('.unreadable-id')?.textContent)).toEqual(
      malformed.map((m) => m.id),
    );
    expect(rows.every((r) => !r.querySelector('input, button'))).toBe(true);

    const toggle = container.querySelector<HTMLInputElement>('input[aria-label="停用 ;s2"]');
    await mounted.act(() => {
      toggle?.click();
    });
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    const saved = lastSaved(backend).rules;
    expect(saved).toHaveLength(3 + malformed.length);
    expect(saved.slice(1, 1 + malformed.length)).toEqual(malformed);
    expect(saved.at(-2)).toMatchObject({ id: 'mock-rule-2', enabled: false });
  });

  it('keeps unknown keys of the edited rule, the globals and the config', async () => {
    const mounted = await mountSample();
    const { backend, container } = mounted;
    const before = structuredClone(backend.draft);
    await click(mounted, '编辑');
    expect(container.querySelector('.extra-keys')?.textContent).toBe(
      '含有本版本不认识的字段（future_rule_field），保存时会保留。',
    );
    await type(mounted, field(container, '说明'), '新说明');
    await clickIn(mounted, dialog(container, '编辑文本指令'), '保存');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    // Warnings about the unreadable entries do not show up as the edit's field errors.
    expect(container.querySelector('.field-errors')).toBeNull();
    const saved = lastSaved(backend);
    expect(saved.rules[0]).toMatchObject({ description: '新说明', future_rule_field: [1, 2] });
    expect(saved.rules.slice(1)).toEqual(before.rules.slice(1));

    await click(mounted, '文本指令参数设置');
    await click(mounted, '保存参数草稿');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(2);
    });
    expect(lastSaved(backend).globals.future_global).toBe('kept');
    expect(lastSaved(backend).future_setting).toEqual({ sample: true });
  });

  it('keeps a nested unknown key (same value, IPC number contract) and rule-level keys in place', async () => {
    // Codex's case: 「添加普通文字」 rebuilds `source`, so a rule with an unknown key in it must not
    // reach the editor. Rule-level keys of a newer version are edited around, in place.
    const nested = {
      id: 'nested-unknown',
      enabled: true,
      group: '默认',
      description: '',
      ignore_case: false,
      match: { kind: 'literal', value: ';nested' },
      trigger_mode: 'immediate',
      terminator: null,
      ime_erasure: null,
      action: {
        type: 'send_keys',
        source: { type: 'inline', value: 'hello', future_option: { keep: true } },
      },
      parameter: { type: 'default_match' },
      notification: { enabled: false, message: '' },
    };
    const mounted = await mount({ unreadable: true }, (backend) => {
      const [first] = backend.draft.rules as Record<string, unknown>[];
      // A rule-level key of a newer version in the middle of the known ones.
      backend.draft.rules[0] = Object.fromEntries(
        Object.entries({
          ...first,
          action: { type: 'send_keys', source: { type: 'inline', value: 'hi' } },
        }).flatMap(([k, v]) =>
          k === 'group'
            ? [
                ['future_middle', { deep: [1, 'x'] }],
                [k, v],
              ]
            : [[k, v]],
        ),
      );
      backend.draft.rules.splice(2, 0, structuredClone(nested));
    });
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(container.querySelectorAll('tbody tr')).toHaveLength(7);
    });
    const before = structuredClone(backend.draft);
    const raw = [...container.querySelectorAll('tbody tr.unreadable-row')].find(
      (r) => r.querySelector('.unreadable-id')?.textContent === 'nested-unknown',
    );
    // Listed read-only: nothing to open it in the editor with.
    expect(raw).toBeDefined();
    expect(raw?.querySelector('input, button')).toBeNull();

    // Edit ;s1 through the send_keys builder that dropped the key, then save from the editor.
    await click(mounted, '编辑');
    const editor = dialog(container, '编辑文本指令');
    const literal = editor?.querySelector<HTMLInputElement>('input[aria-label="要输入的普通文字"]');
    if (!literal) throw new Error('no send_keys builder');
    await type(mounted, literal, '!');
    await clickIn(mounted, editor, '添加普通文字');
    await clickIn(mounted, editor, '保存');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    const saved = lastSaved(backend);
    const edited = saved.rules[0] as Record<string, unknown>;
    expect(edited.action).toStrictEqual({
      type: 'send_keys',
      source: { type: 'inline', value: 'hi!' },
      // Absent in the draft; filled in with its serde default.
      syntax_variables: [],
    });
    expect(Object.keys(edited)).toEqual(Object.keys(before.rules[0] as object));
    expect(edited.future_middle).toEqual({ deep: [1, 'x'] });
    // Every other entry, the nested-unknown one included, is written back as the same value: all
    // their numbers are within the IPC number contract (docs/ipc-contract-0.4.md §6).
    expect(saved.rules[2]).toEqual(nested);
    expect(JSON.stringify(saved.rules.slice(1))).toBe(JSON.stringify(before.rules.slice(1)));

    // The globals and the config keep their unknown keys, in place, through a settings edit.
    await click(mounted, '文本指令参数设置');
    await type(mounted, field(container, '词分隔符'), ' ;');
    await click(mounted, '保存参数草稿');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(2);
    });
    const settings = lastSaved(backend);
    expect(settings.globals.word_separators).toBe(' ;');
    expect(Object.keys(settings.globals)).toEqual(Object.keys(before.globals));
    expect(settings.globals.future_global).toBe('kept');
    expect(Object.keys(settings)).toEqual(Object.keys(before));
    expect(settings.future_setting).toEqual({ sample: true });
    expect(JSON.stringify(settings.rules[2])).toBe(JSON.stringify(nested));
  });

  it('flags an entry holding a number past the IPC contract, from JSON text, without blocking a save', async () => {
    // JSON text as the IPC carries it (docs/ipc-contract-0.4.md §6). JSON.parse rounds
    // 9007199254740993 to 2^53 before any frontend code runs; Codex's case is span.line.
    const hugeText =
      '{"id":"huge-number","match":{"kind":"literal","value":";huge"},"action":{"type":"steps",' +
      '"plan":{"steps":[{"operation":{"type":"delay","ms":1},' +
      '"span":{"line":9007199254740993,"column":1,"length":0}}]}}}';
    // Another unreadable entry, with numbers right at the edge of the contract. Written in the
    // form JSON.stringify writes, so the text itself can be compared.
    const safeText =
      '{"id":"safe-raw","match":42,"future_kind":{"counts":[9007199254740991,-9007199254740991,2.5],"note":null}}';
    // A rule the editor could open but for the number in a form-editable reusable parameter: it
    // would show the rounded value as the real one, so it is listed read-only too.
    const parameterText =
      '{"id":"huge-parameter","match":{"kind":"literal","value":";hp"},' +
      '"action":{"type":"reusable","action_id":"a","parameters":{"count":9007199254740993}}}';
    const mounted = await mount({}, (backend) => {
      const raw = JSON.parse(`[${hugeText},${safeText},${parameterText}]`) as unknown[];
      backend.draft.rules.splice(1, 0, ...raw);
    });
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(container.querySelectorAll('tbody tr.unreadable-row')).toHaveLength(3);
    });
    const row = (id: string) =>
      [...container.querySelectorAll('tbody tr.unreadable-row')].find(
        (r) => r.querySelector('.unreadable-id')?.textContent === id,
      );
    expect(row('huge-number')?.querySelector('.unreadable-reason')?.textContent).toBe(
      '含超出可安全保存范围的数值，保存其他内容时该数值可能被改写。',
    );
    expect(row('huge-parameter')?.querySelector('.unreadable-reason')?.textContent).toBe(
      '含超出可安全保存范围的数值，保存其他内容时该数值可能被改写。',
    );
    expect(row('huge-parameter')?.querySelector('input, button')).toBeNull();
    expect(row('safe-raw')?.querySelector('.unreadable-reason')).toBeNull();
    // Every row says the same thing otherwise: no claim that the entry is or is not in effect,
    // and none that it is kept byte for byte.
    for (const id of ['huge-number', 'huge-parameter', 'safe-raw']) {
      expect(row(id)?.querySelector('.muted')?.textContent).toBe(
        '这条规则含有当前界面无法编辑的内容，只能查看；已保留在配置中。',
      );
      expect(row(id)?.textContent).not.toMatch(/生效|原样|不会改动/);
    }

    // An unrelated save goes through.
    const toggle = container.querySelector<HTMLInputElement>('input[aria-label="停用 ;s2"]');
    await mounted.act(() => {
      toggle?.click();
    });
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    await waitFor(mounted, () => {
      expect(backend.draft.rules.at(-2)).toMatchObject({ id: 'mock-rule-2', enabled: false });
    });
    expect(container.querySelector('.alert.error')).toBeNull();
    const saved = lastSaved(backend).rules;
    // The safe entry goes out as the JSON it came in as.
    expect(saved[2]).toEqual(JSON.parse(safeText));
    expect(JSON.stringify(saved[2])).toBe(safeText);
    // The flagged ones go out with the value JSON.parse rounded them to, as the list says.
    expect(JSON.stringify(saved[1])).toBe(hugeText.replace('9007199254740993', '9007199254740992'));
    expect(JSON.stringify(saved[3])).toBe(
      parameterText.replace('9007199254740993', '9007199254740992'),
    );
  });

  // Config- and globals-level keys of a newer version: the config stays readable; the settings
  // page says a number in it may be rewritten. JSON text again, as the IPC carries it.
  describe.each(['config', 'globals'] as const)('an unknown key of the %s', (level) => {
    const text = (n: string) => `{"future_big":{"ids":[1,${n}],"note":null}}`;
    async function mountWith(n: string): Promise<Mounted> {
      const mounted = await mount({}, (backend) => {
        Object.assign(
          level === 'config' ? backend.draft : backend.draft.globals,
          JSON.parse(text(n)),
        );
      });
      await waitFor(mounted, () => {
        expect(mounted.container.querySelectorAll('tbody tr')).toHaveLength(3);
      });
      await click(mounted, '文本指令参数设置');
      return mounted;
    }
    async function saveSettings(mounted: Mounted): Promise<string> {
      await click(mounted, '保存参数草稿');
      await waitFor(mounted, () => {
        expect(callsOf(mounted.backend, 'save_draft')).toHaveLength(1);
      });
      expect(mounted.container.querySelector('.alert.error')).toBeNull();
      const saved = lastSaved(mounted.backend);
      return JSON.stringify({
        future_big: (level === 'config' ? saved : saved.globals).future_big,
      });
    }

    it('holding a number past the IPC contract: the settings page says so; saving goes through', async () => {
      const mounted = await mountWith('9007199254740993');
      expect(mounted.container.querySelector('.unsafe-number-note')?.textContent).toBe(
        '配置中含超出可安全保存范围的数值，保存时可能被改写。',
      );
      // Out with the value JSON.parse rounded it to, as the note says.
      expect(await saveSettings(mounted)).toBe(text('9007199254740992'));
    });

    it('holding only safe numbers: no note; it goes out as the JSON it came in as', async () => {
      const mounted = await mountWith('9007199254740991');
      expect(mounted.container.querySelector('.unsafe-number-note')).toBeNull();
      expect(await saveSettings(mounted)).toBe(text('9007199254740991'));
    });
  });

  it('reads absent fields as their serde defaults and edits such a rule', async () => {
    // Rust reads this as a complete rule; the step editor reads `target.type` right away.
    const sparse = {
      id: 'sparse',
      match: { kind: 'literal', value: ';a-sparse' },
      action: { type: 'steps', plan: { steps: [{ operation: { type: 'delay', ms: 5 } }] } },
    };
    const mounted = await mount({}, (backend) => {
      backend.draft.rules.push(structuredClone(sparse));
    });
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(container.querySelectorAll('tbody tr')).toHaveLength(4);
    });
    expect(container.querySelectorAll('tbody tr.unreadable-row')).toHaveLength(0);
    // ;a-sparse sorts first.
    await click(mounted, '编辑');
    const editor = dialog(container, '编辑文本指令');
    expect(field(editor ?? container, '说明').value).toBe('');
    await type(mounted, field(editor ?? container, '说明'), '补全');
    await clickIn(mounted, editor, '保存');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    expect(lastSaved(backend).rules.at(-1)).toStrictEqual({
      id: 'sparse',
      match: { kind: 'literal', value: ';a-sparse' },
      action: {
        type: 'steps',
        plan: {
          steps: [
            {
              operation: { type: 'delay', ms: 5 },
              span: { line: 0, column: 0, length: 0 },
              target: { type: 'locked' },
            },
          ],
          version: 1,
          dialect: 'structured',
          source: null,
        },
      },
      enabled: true,
      group: '',
      description: '补全',
      ignore_case: false,
      trigger_mode: 'terminator',
      terminator: null,
      ime_erasure: null,
      parameter: { type: 'default_match' },
      notification: { enabled: false, message: '' },
    });
  });
});

// The editor never writes or sends a number its Rust type and validation refuse, nor one past the
// IPC number contract (Codex round 4): the text typed stays with an inline error, and 保存 and the
// tests refuse before anything leaves the editor.
describe('number fields of the editor', () => {
  const rule = (match: string, action: Record<string, unknown>) => ({
    id: `rule-${match.slice(1)}`,
    enabled: true,
    group: '默认',
    description: '',
    ignore_case: false,
    match: { kind: 'literal', value: match },
    trigger_mode: 'immediate',
    terminator: null,
    ime_erasure: null,
    action,
    parameter: { type: 'default_match' },
    notification: { enabled: false, message: '' },
  });
  const steps = (...operations: Record<string, unknown>[]) => ({
    type: 'steps',
    plan: {
      version: 1,
      dialect: 'structured',
      source: null,
      steps: operations.map((operation) => ({
        operation,
        span: { line: 0, column: 0, length: 0 },
        target: { type: 'locked' },
      })),
    },
  });
  const EDITOR = '编辑文本指令';
  const JSON_RANGE = '-9007199254740991 到 9007199254740991 之间的数字';

  function input(root: ParentNode | null, label: string): HTMLInputElement {
    const found = root?.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`);
    if (!found) throw new Error(`no input 「${label}」`);
    return found;
  }

  async function choose(mounted: Mounted, label: string, value: string): Promise<void> {
    if (label === '操作类型') {
      await mounted.act(() => {
        dialog(mounted.container, EDITOR)
          ?.querySelector<HTMLButtonElement>('[role="combobox"]')
          ?.click();
      });
      await mounted.act(() => {
        dialog(mounted.container, EDITOR)
          ?.querySelector<HTMLElement>(`[role="option"][data-value="${value}"]`)
          ?.click();
      });
      return;
    }
    const select = dialog(mounted.container, EDITOR)?.querySelector<HTMLSelectElement>(
      `select[aria-label="${label}"]`,
    );
    if (!select) throw new Error(`no select 「${label}」`);
    await mounted.act(() => {
      select.value = value;
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
  }

  /** Mount with `extra` rules (matched `;a-…`, so listed first) and open the first one. */
  async function editFirst(...extra: ReturnType<typeof rule>[]): Promise<Mounted> {
    const mounted = await mount({}, (backend) => {
      backend.draft.rules.push(...structuredClone(extra));
    });
    await waitFor(mounted, () => {
      expect(mounted.container.querySelectorAll('tbody tr')).toHaveLength(3 + extra.length);
    });
    expect(mounted.container.querySelectorAll('tbody tr.unreadable-row')).toHaveLength(0);
    await click(mounted, '编辑');
    return mounted;
  }

  /** The field keeps the text typed and says what it takes. */
  function expectInvalid(field: HTMLInputElement, text: string, error: string): void {
    expect(field.value).toBe(text);
    expect(field.getAttribute('aria-invalid')).toBe('true');
    const message = document.getElementById(field.getAttribute('aria-describedby') ?? '');
    expect(message?.textContent).toBe(error);
  }

  /** 保存 is refused before anything is sent; the editor stays open and lists the problem. */
  async function expectSaveRefused(mounted: Mounted, listed: string): Promise<void> {
    const { backend, container } = mounted;
    await clickIn(mounted, dialog(container, EDITOR), '保存');
    await waitFor(mounted, () => {
      expect(container.querySelector('.alert.error')?.textContent).toMatch(/有设置需要修改/);
    });
    const items = [...container.querySelectorAll('.action-diagnostics button')].map(
      (b) => b.textContent,
    );
    expect(items).toContain(listed);
    expect(dialog(container, EDITOR)).not.toBeNull();
    expect(callsOf(backend, 'validate_config')).toHaveLength(0);
    expect(callsOf(backend, 'save_draft')).toHaveLength(0);
  }

  /** No call reached the backend with the editor's invalid-number marker in it. */
  function expectNoMarkerSent(backend: MockBackend): void {
    expect(JSON.stringify(backend.calls)).not.toContain('invalidNumber');
  }

  /** Select step `index` (from 0) in the step list. */
  async function openStep(mounted: Mounted, index: number): Promise<void> {
    const steps = dialog(mounted.container, EDITOR)?.querySelectorAll('ol.steps > li > button');
    const target = steps?.[index];
    if (!(target instanceof HTMLButtonElement)) throw new Error(`no step ${String(index + 1)}`);
    await mounted.act(() => {
      target.click();
    });
  }

  /** The heading of the step form: 「步骤 2 · 等待」. */
  const stepHeading = (mounted: Mounted) =>
    dialog(mounted.container, EDITOR)?.querySelector('.step-form h4')?.textContent;

  /** 保存 goes through; the action of the rule saved last. */
  async function save(mounted: Mounted): Promise<unknown> {
    await clickIn(mounted, dialog(mounted.container, EDITOR), '保存');
    await waitFor(mounted, () => {
      expect(callsOf(mounted.backend, 'save_draft')).toHaveLength(1);
    });
    await waitFor(mounted, () => {
      expect(dialog(mounted.container, EDITOR)).toBeNull();
    });
    const { rules } = callsOf(mounted.backend, 'save_draft')[0]?.args.config as {
      rules: { action: unknown }[];
    };
    return rules.at(-1)?.action;
  }

  it('refuses a reusable number parameter past 2^53−1 in a new rule, keeping it editable', async () => {
    const mounted = await mount();
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(container.querySelectorAll('tbody tr')).toHaveLength(3);
    });
    await click(mounted, '＋ 添加指令');
    const editor = dialog(container, EDITOR);
    await type(mounted, field(editor ?? container, '缩写词'), ';big');
    await choose(mounted, '操作类型', 'reusable');
    await choose(mounted, '复用已保存指令', 'mock-rule-1');
    await clickIn(mounted, editor, '＋ 添加参数');
    await choose(mounted, '参数类型', 'number');
    await type(mounted, input(editor, '参数值'), '9007199254740993');
    // Not turned into 9007199254740992 by Number().
    expectInvalid(input(editor, '参数值'), '9007199254740993', `请输入 ${JSON_RANGE}。`);

    await clickIn(mounted, editor, '测试动作（真实执行）');
    await waitFor(mounted, () => {
      expect(container.querySelector('.alert.error')?.textContent).toMatch(/有数值需要修改/);
    });
    expect(callsOf(backend, 'execute_action')).toHaveLength(0);
    const listed = `操作设置 · 参数「param1」的值应为 ${JSON_RANGE}。`;
    await expectSaveRefused(mounted, listed);
    expect(backend.draft.rules).toHaveLength(3);
    expectNoMarkerSent(backend);
    // Still editable: the largest safe integer goes through as typed.
    expectInvalid(input(editor, '参数值'), '9007199254740993', `请输入 ${JSON_RANGE}。`);
    await type(mounted, input(editor, '参数值'), '9007199254740991');
    expect(input(editor, '参数值').getAttribute('aria-invalid')).toBeNull();
    expect(await save(mounted)).toEqual({
      type: 'reusable',
      action_id: 'mock-rule-1',
      parameters: { param1: 9007199254740991 },
    });
  });

  it('refuses the same when editing an existing reusable rule, which stays editable', async () => {
    const reuse = rule(';a-reuse', {
      type: 'reusable',
      action_id: 'mock-rule-1',
      parameters: { count: 5 },
    });
    const mounted = await editFirst(reuse);
    const { backend, container } = mounted;
    const editor = dialog(container, EDITOR);
    expect(input(editor, '参数值').value).toBe('5');
    await type(mounted, input(editor, '参数值'), '9007199254740993');
    expectInvalid(input(editor, '参数值'), '9007199254740993', `请输入 ${JSON_RANGE}。`);
    await expectSaveRefused(mounted, `操作设置 · 参数「count」的值应为 ${JSON_RANGE}。`);
    expect(backend.draft.rules.at(-1)).toEqual(reuse);
    // 检查这段文字能否触发 sends the rule too: refused before it asks for the text.
    await clickIn(mounted, editor, '检查这段文字能否触发（不会执行操作）');
    await mounted.act(() => new Promise((resolve) => setTimeout(resolve, 20)));
    expect(dialog(container, '用哪段文字检查？')).toBeNull();
    expect(callsOf(backend, 'test_rule')).toHaveLength(0);
    expectNoMarkerSent(backend);

    // The rule is not turned read-only: it can be fixed and saved, and is listed as editable.
    await type(mounted, input(editor, '参数值'), '-12');
    expect(await save(mounted)).toEqual({ ...reuse.action, parameters: { count: -12 } });
    expect(container.querySelectorAll('tbody tr.unreadable-row')).toHaveLength(0);
  });

  it('refuses a delay past MAX_DELAY_MS', async () => {
    const mounted = await editFirst(rule(';a-delay', steps({ type: 'delay', ms: 100 })));
    const editor = dialog(mounted.container, EDITOR);
    const ms = input(editor, '等待时间（毫秒）');
    expect(ms.value).toBe('100');
    await type(mounted, ms, '3600001');
    expectInvalid(ms, '3600001', '请输入 0 到 3600000 之间的整数。');
    await expectSaveRefused(mounted, '步骤 1 · 等待时间（毫秒）应为 0 到 3600000 之间的整数。');
    await type(mounted, ms, '3600000');
    expect(await save(mounted)).toEqual(steps({ type: 'delay', ms: 3600000 }));
  });

  it('refuses a coordinate past i32, for 保存 and 单步测试', async () => {
    const move = { type: 'mouse_move', x: 10, y: 20, relative: false, percent: false };
    const mounted = await editFirst(rule(';a-move', steps(move)));
    const { backend, container } = mounted;
    const editor = dialog(container, EDITOR);
    const x = input(editor, '左右位置 X');
    await type(mounted, x, '2147483648');
    expectInvalid(x, '2147483648', '请输入 -2147483648 到 2147483647 之间的数字。');
    await clickIn(mounted, editor, '单步测试（真实执行）');
    await waitFor(mounted, () => {
      expect(container.querySelector('.alert.error')?.textContent).toMatch(/有数值需要修改/);
    });
    expect(callsOf(backend, 'execute_action')).toHaveLength(0);
    await expectSaveRefused(
      mounted,
      '步骤 1 · 左右位置 X应为 -2147483648 到 2147483647 之间的数字。',
    );
    // Back in range, the step runs and saves.
    await type(mounted, x, '-2147483648');
    await clickIn(mounted, editor, '单步测试（真实执行）');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'execute_action')).toHaveLength(1);
    });
    expect(await save(mounted)).toEqual(steps({ ...move, x: -2147483648 }));
  });

  it('refuses a Windows key code past 1..=255, keeping the text', async () => {
    const press = { type: 'key_press', key: { type: 'named', name: 'Enter' } };
    const mounted = await editFirst(rule(';a-key', steps(press)));
    const editor = dialog(mounted.container, EDITOR);
    const name = input(editor, '按键名称');
    await type(mounted, name, '#300');
    expectInvalid(name, '#300', '以 # 开头表示 Windows 键码，请输入 1 到 255 之间的整数。');
    await expectSaveRefused(mounted, '步骤 1 · 键码应为 1 到 255 之间的整数。');
    await type(mounted, name, '#13');
    expect(await save(mounted)).toEqual(
      steps({ type: 'key_press', key: { type: 'windows_code', code: 13 } }),
    );
  });

  // Codex round 5: the text is part of the rule being edited, not of the field's component, which
  // is mounted anew for each step.
  it('keeps invalid number text across switching steps, reordering them and undo', async () => {
    const delay = { type: 'delay', ms: 100 };
    const text = { type: 'text', text: 'hi', method: 'keys' };
    const mounted = await editFirst(rule(';a-steps', steps(delay, text)));
    const { backend, container } = mounted;
    const editor = dialog(container, EDITOR);
    const label = '等待时间（毫秒）';
    const error = '请输入 0 到 3600000 之间的整数。';
    await type(mounted, input(editor, label), '3600001');
    expectInvalid(input(editor, label), '3600001', error);

    await openStep(mounted, 1);
    expect(editor?.querySelector(`input[aria-label="${label}"]`)).toBeNull();
    await openStep(mounted, 0);
    expectInvalid(input(editor, label), '3600001', error);

    await clickIn(mounted, editor, '下移');
    expect(stepHeading(mounted)).toBe('步骤 2 · 等待');
    expectInvalid(input(editor, label), '3600001', error);
    expect(editor?.querySelectorAll('ol.steps small')[1]?.textContent).toBe('3600001');
    await expectSaveRefused(mounted, '步骤 2 · 等待时间（毫秒）应为 0 到 3600000 之间的整数。');

    // Undo takes back the move first, then the edit: the last valid value, without the error.
    await clickIn(mounted, editor, '撤销');
    expect(stepHeading(mounted)).toBe('步骤 1 · 等待');
    expectInvalid(input(editor, label), '3600001', error);
    await clickIn(mounted, editor, '撤销');
    expect(input(editor, label).value).toBe('100');
    expect(input(editor, label).getAttribute('aria-invalid')).toBeNull();
    expect(editor?.querySelector('.number-error')).toBeNull();
    expectNoMarkerSent(backend);
  });

  it('keeps an invalid key code (#300) across switching steps, reordering them and undo', async () => {
    const press = { type: 'key_press', key: { type: 'named', name: 'Enter' } };
    const delay = { type: 'delay', ms: 100 };
    const mounted = await editFirst(rule(';a-keys', steps(press, delay)));
    const { backend, container } = mounted;
    const editor = dialog(container, EDITOR);
    const error = '以 # 开头表示 Windows 键码，请输入 1 到 255 之间的整数。';
    await type(mounted, input(editor, '按键名称'), '#300');

    await openStep(mounted, 1);
    expect(editor?.querySelector('input[aria-label="按键名称"]')).toBeNull();
    await openStep(mounted, 0);
    expectInvalid(input(editor, '按键名称'), '#300', error);

    await clickIn(mounted, editor, '下移');
    expect(stepHeading(mounted)).toBe('步骤 2 · 按一下键');
    expectInvalid(input(editor, '按键名称'), '#300', error);
    await expectSaveRefused(mounted, '步骤 2 · 键码应为 1 到 255 之间的整数。');

    await clickIn(mounted, editor, '撤销');
    expectInvalid(input(editor, '按键名称'), '#300', error);
    await clickIn(mounted, editor, '撤销');
    expect(input(editor, '按键名称').value).toBe('Enter');
    expect(input(editor, '按键名称').getAttribute('aria-invalid')).toBeNull();
    expectNoMarkerSent(backend);

    // Fixed after all that, it saves as typed.
    await type(mounted, input(editor, '按键名称'), '#013');
    expect(input(editor, '按键名称').value).toBe('#013');
    expect(await save(mounted)).toEqual(
      steps({ type: 'key_press', key: { type: 'windows_code', code: 13 } }, delay),
    );
  });

  it('saves an edit next to kept arrays of any width (Codex round 6)', async () => {
    // Wide enough that a call with one argument per element throws a RangeError; every save sends
    // them through the last check before sending, which must not block an unrelated edit.
    const wide = (): number[] => new Array<number>(200_000).fill(0);
    const kept = { ...rule(';a-wide', steps({ type: 'delay', ms: 100 })), future_list: wide() };
    const raw = { id: 'raw-wide', match: 42, future_list: wide() };
    const mounted = await mount({}, (backend) => {
      backend.draft.future_setting = wide();
      backend.draft.rules.push(structuredClone(kept), structuredClone(raw));
    });
    const { backend, container } = mounted;
    await waitFor(mounted, () => {
      expect(container.querySelectorAll('tbody tr')).toHaveLength(5);
    });
    expect(container.querySelectorAll('tbody tr.unreadable-row')).toHaveLength(1);
    await click(mounted, '编辑');
    const editor = dialog(container, EDITOR);
    expect(editor?.querySelector('.extra-keys')?.textContent).toBe(
      '含有本版本不认识的字段（future_list），保存时会保留。',
    );
    await type(mounted, input(editor, '等待时间（毫秒）'), '200');
    await clickIn(mounted, editor, '保存');
    await waitFor(mounted, () => {
      expect(callsOf(backend, 'save_draft')).toHaveLength(1);
    });
    await waitFor(mounted, () => {
      expect(dialog(container, EDITOR)).toBeNull();
    });
    const saved = callsOf(backend, 'save_draft')[0]?.args.config as {
      rules: unknown[];
      future_setting: unknown;
    };
    expect(saved.rules).toHaveLength(5);
    expect(saved.rules.at(-2)).toEqual({ ...kept, action: steps({ type: 'delay', ms: 200 }) });
    expect(saved.rules.at(-1)).toEqual(raw);
    expect(saved.future_setting).toEqual(wide());
    expect(container.querySelector('.alert.error')).toBeNull();
  });
});

describe('production build outside the desktop app', () => {
  it('shows 「未连接到桌面后端」 instead of falling back to sample data', async () => {
    // What a release bundle sees when opened in a plain browser: no Tauri and no mock.
    vi.stubEnv('DEV', false);
    vi.resetModules();
    const react = await import('react');
    const { createRoot } = await import('react-dom/client');
    const { default: App } = await import('./App');
    const container = document.createElement('div');
    document.body.append(container);
    const root = createRoot(container);
    const act: Act = async (callback) => {
      await react.act(async () => {
        await callback();
      });
    };
    cleanups.push(async () => {
      await act(() => {
        root.unmount();
      });
      container.remove();
      vi.unstubAllEnvs();
    });
    await act(() => {
      root.render(react.createElement(App));
    });

    await waitFor({ act }, () => {
      expect(container.querySelector('[role="alert"]')?.textContent).toContain('未连接到桌面后端');
    });
    expect(headerStatus(container)).toBe('未连接');
    expect((globalThis as { quickerMock?: unknown }).quickerMock).toBeUndefined();
  });
});
