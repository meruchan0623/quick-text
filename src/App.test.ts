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
