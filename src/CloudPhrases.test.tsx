import { act, type ReactElement } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import CloudPhrases, { CloudSettingsSection } from './CloudPhrases';
import type { CloudView } from './ipc/events';
import {
  createMockTransport,
  MockBackend,
  sampleCloudRules,
  type MockOptions,
  type MockRule,
} from './ipc/mock';
import { setTransport } from './ipc/transport';
import type { CloudSettingsInput, CloudState } from './ipc/types';
import type { CloudRow } from './lib/cloud';

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

beforeEach(() => {
  // jsdom has no modal dialogs; the detail dialog only needs the open state.
  HTMLDialogElement.prototype.showModal = function showModal(this: HTMLDialogElement) {
    this.setAttribute('open', '');
  };
  HTMLDialogElement.prototype.close = function close(this: HTMLDialogElement) {
    this.removeAttribute('open');
  };
});

const roots: { root: Root; host: HTMLElement }[] = [];
afterEach(() => {
  for (const { root, host } of roots.splice(0)) {
    act(() => {
      root.unmount();
    });
    host.remove();
  }
});

async function cloudState(options: MockOptions = { cloud: 'sample' }): Promise<CloudState> {
  const backend = new MockBackend({ rules: 3, ...options });
  return (await backend.handle('get_cloud_state')) as CloudState;
}

const ready = (state: CloudState, pending: CloudView['pending'] = null): CloudView => ({
  status: 'ready',
  state,
  pending,
  loadError: null,
});

function render(element: ReactElement) {
  const host = document.createElement('div');
  document.body.append(host);
  const root = createRoot(host);
  act(() => {
    root.render(element);
  });
  roots.push({ root, host });
  return {
    host,
    rerender: (next: ReactElement) => {
      act(() => {
        root.render(next);
      });
    },
  };
}

const button = (root: ParentNode, text: string) =>
  [...root.querySelectorAll('button')].find((b) => b.textContent === text);

function click(element: Element | null | undefined) {
  if (!(element instanceof HTMLElement)) throw new Error('nothing to click');
  act(() => {
    element.click();
  });
}

/** Type into a controlled input the way React sees it (native setter + input event). */
function type(control: HTMLInputElement | null, value: string) {
  if (!control) throw new Error('no input');
  act(() => {
    Reflect.set(HTMLInputElement.prototype, 'value', value, control);
    control.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

function phrases(view: CloudView, overrides: Partial<Parameters<typeof CloudPhrases>[0]> = {}) {
  const props = {
    view,
    platform: 'windows',
    terminators: ['space', 'semicolon'],
    ruleName: (id: string) => (id === 'mock-rule-2' ? ';s2' : id),
    canCopy: true,
    onCopy: vi.fn<(row: CloudRow) => void>(),
    onSync: vi.fn<() => void>(),
    onSetDisabled: vi.fn<(ids: string[], disabled: boolean) => void>(),
    onRetry: vi.fn<() => void>(),
    onOpenSettings: vi.fn<() => void>(),
    ...overrides,
  };
  return { props, ...render(<CloudPhrases {...props} />) };
}

const rowOf = (host: HTMLElement, abbreviation: string) =>
  [...host.querySelectorAll<HTMLTableRowElement>('tbody tr')].find(
    (tr) => tr.querySelector('strong')?.textContent === abbreviation,
  );

describe('团队话术 list', () => {
  it('shows the source, the same columns plus 状态, and every status in words', async () => {
    const { host } = phrases(ready(await cloudState()));
    expect(host.querySelector('.cloud-source')?.textContent).toMatch(
      /^来自 TSim 客服系统 · v12 · 上次同步 \S+ \d\d:\d\d（1 条当前版本无法读取，已跳过）立即同步$/,
    );
    expect([...host.querySelectorAll('th')].map((th) => th.textContent)).toEqual([
      '',
      '缩写词',
      '触发字符',
      '说明',
      '分组',
      '操作类型',
      '摘要',
      '本机停用',
      '状态',
      '操作',
    ]);
    const status = (abbreviation: string) =>
      rowOf(host, abbreviation)?.querySelector('.cloud-status')?.textContent;
    expect(status('tnh')).toBe('生效');
    expect(status('tjh')).toBe('云端已停用');
    expect(status('tfh')).toBe('本机停用');
    // The same abbreviation as a rule of 我的指令: the phrase is in effect and says what it replaces.
    expect(status(';s2')).toBe('生效');
    expect(rowOf(host, ';s2')?.textContent).toContain('覆盖我的指令「;s2」');
    expect(rowOf(host, 'tnh')?.textContent).not.toContain('覆盖我的指令');
    expect(status('(DD[0-9')).toBe('无法使用');
    expect(rowOf(host, '(DD[0-9')?.querySelector('.cloud-problem')?.textContent).toBe(
      '缩写的正则表达式写法有误，无法使用。',
    );
    // No add, edit, delete or import here.
    for (const text of ['＋ 添加指令', '编辑', '删除', '导入', '导出'])
      expect(button(host, text)).toBeUndefined();
  });

  it('inherits the 触发字符 of 我的指令 and shows a phrase’s own keys', async () => {
    const { host } = phrases(ready(await cloudState()));
    expect(rowOf(host, 'tnh')?.querySelectorAll('td')[2]?.textContent).toBe('空格 / 分号继承');
    expect(rowOf(host, 'ttk')?.querySelectorAll('td')[2]?.textContent).toBe('分号');
    expect(rowOf(host, 'tjs')?.querySelectorAll('td')[2]?.textContent).toBe('即时');
  });

  it('searches abbreviation, description, group and summary, with counted group tabs', async () => {
    const { host } = phrases(ready(await cloudState()));
    const tabs = [...host.querySelectorAll('.group-tabs button')].map((b) => b.textContent);
    expect(tabs).toEqual(['全部8', '问候2', '售后2', '物流3', '默认1']);
    click(button(host, '物流3'));
    expect(host.querySelectorAll('tbody tr')).toHaveLength(3);
    click(button(host, '全部8'));
    const search = host.querySelector<HTMLInputElement>('input[aria-label="搜索团队话术"]');
    type(search, '退款');
    expect([...host.querySelectorAll('tbody tr strong')].map((s) => s.textContent)).toEqual([
      'ttk',
    ]);
    type(search, 'EXAMPLE.COM/HELP');
    expect([...host.querySelectorAll('tbody tr strong')].map((s) => s.textContent)).toEqual([
      'tbz',
    ]);
    type(search, '没有这条');
    expect(host.textContent).toContain('没有匹配的团队话术');
  });

  it('switches phrases off on this computer, one by one or selected', async () => {
    const { host, props } = phrases(ready(await cloudState()));
    const box = (abbreviation: string) =>
      host.querySelector<HTMLInputElement>(`input[aria-label="本机停用 ${abbreviation}"]`);
    expect(box('tfh')?.checked).toBe(true);
    expect(box('tnh')?.checked).toBe(false);
    // A phrase the table switched off has this computer's choice too (§5 disabled_local).
    expect(box('tjh')?.checked).toBe(false);
    click(box('tjh'));
    expect(props.onSetDisabled).toHaveBeenLastCalledWith(['mock-cloud-4'], true);
    click(box('tnh'));
    expect(props.onSetDisabled).toHaveBeenLastCalledWith(['mock-cloud-1'], true);
    click(box('tfh'));
    expect(props.onSetDisabled).toHaveBeenLastCalledWith(['mock-cloud-5'], false);

    expect(button(host, '本机停用')?.disabled).toBe(true);
    click(host.querySelector('input[aria-label="选择 tnh"]'));
    click(host.querySelector('input[aria-label="选择 tbz"]'));
    expect(host.querySelector('.batch span')?.textContent).toContain('已选 2 条');
    click(button(host, '本机停用'));
    expect(props.onSetDisabled).toHaveBeenLastCalledWith(['mock-cloud-1', 'mock-cloud-8'], true);
    click(button(host, '本机启用'));
    expect(props.onSetDisabled).toHaveBeenLastCalledWith(['mock-cloud-1', 'mock-cloud-8'], false);
  });

  it('shows this computer’s choice for a phrase the table switched off', async () => {
    const backend = new MockBackend({ rules: 3, cloud: 'sample' });
    const state = (await backend.handle('set_cloud_rules_disabled', {
      ids: ['mock-cloud-4'],
      disabled: true,
    })) as CloudState;
    const { host } = phrases(ready(state));
    const row = rowOf(host, 'tjh');
    expect(row?.querySelector('.cloud-status')?.textContent).toBe('云端已停用');
    expect(row?.textContent).toContain('本机也已停用');
    expect(row?.querySelector<HTMLInputElement>('input[aria-label="本机停用 tjh"]')?.checked).toBe(
      true,
    );
  });

  it('disables local switches and 立即同步 while a command or a sync runs', async () => {
    const state = await cloudState();
    const { host, rerender, props } = phrases(ready(state, 'disabled'));
    expect(host.querySelector<HTMLInputElement>('input[aria-label="本机停用 tnh"]')?.disabled).toBe(
      true,
    );
    expect(button(host, '立即同步')?.disabled).toBe(true);
    const syncing = { ...state, status: { ...state.status, syncing: true } };
    rerender(<CloudPhrases {...props} view={ready(syncing)} />);
    expect(button(host, '正在同步…')?.disabled).toBe(true);
    rerender(<CloudPhrases {...props} view={ready(state)} />);
    click(button(host, '立即同步'));
    expect(props.onSync).toHaveBeenCalledTimes(1);
  });

  it('shows a phrase read-only and copies it into 我的指令', async () => {
    const { host, props } = phrases(ready(await cloudState()));
    click(rowOf(host, 'ttk')?.querySelector('.row-tools button'));
    const detail = host.querySelector('dialog[aria-label="查看团队话术"]');
    const text = detail?.textContent ?? '';
    for (const part of [
      '触发方式输入缩写后，再按触发键执行（英文不区分大小写）',
      '结束键分号执行时去掉按下的触发键',
      '说明退款时效（示例）',
      '分组售后',
      '操作类型粘贴纯文本',
      '示例：退款一般在 3 至 5 个工作日内原路退回。',
    ])
      expect(text).toContain(part);
    expect(detail?.querySelectorAll('input, textarea, select')).toHaveLength(0);
    click(button(detail ?? host, '复制到我的指令'));
    expect(vi.mocked(props.onCopy).mock.lastCall?.[0].entry.id).toBe('mock-cloud-3');
    expect(host.querySelector('dialog[aria-label="查看团队话术"]')).toBeNull();
  });

  describe('an image phrase', () => {
    const asset = `${'a'.repeat(64)}.png`;
    const flush = () => act(() => new Promise<void>((resolve) => setTimeout(resolve, 0)));
    afterEach(() => setTransport(null));

    async function detailOf(action: Record<string, unknown>, fail = false) {
      const backend = new MockBackend({ rules: 3, cloud: 'sample' });
      backend.cloudCache?.rules.push({
        ...sampleCloudRules()[0],
        id: 'mock-cloud-image',
        match: { kind: 'literal', value: 'timg' },
        action,
      } as MockRule);
      const state = (await backend.handle('get_cloud_state')) as CloudState;
      const transport = createMockTransport(backend);
      setTransport(
        fail
          ? {
              ...transport,
              invoke: () => Promise.reject(new Error('missing')),
            }
          : transport,
      );
      const { host } = phrases(ready(state));
      click(rowOf(host, 'timg')?.querySelector('.row-tools button'));
      await flush();
      return {
        host,
        calls: backend.calls.filter((c) => c.command === 'preview_image_asset'),
        detail: host.querySelector('dialog[aria-label="查看团队话术"]'),
      };
    }

    it('shows the picture and its name in the detail view, and offers copying', async () => {
      const { host, detail, calls } = await detailOf({ type: 'image', asset, name: '二维码.png' });
      expect(detail?.querySelector('pre')?.textContent).toBe('二维码.png');
      expect(calls[0]?.args).toEqual({ asset });
      expect(detail?.querySelector('img.image-preview')?.getAttribute('src')).toMatch(/^data:/);
      expect(button(detail ?? host, '复制到我的指令')?.disabled).toBe(false);
    });

    it('says so when the picture cannot be shown', async () => {
      const { detail } = await detailOf({ type: 'image', asset, name: '二维码.png' }, true);
      expect(detail?.querySelector('img')).toBeNull();
      expect(detail?.textContent).toContain('这张图片暂时无法预览');
    });

    it('never asks for an asset that is not one', async () => {
      const { detail, calls } = await detailOf({ type: 'image', asset: '../a.png', name: 'x' });
      expect(calls).toHaveLength(0);
      expect(detail?.querySelector('img')).toBeNull();
    });
  });

  it('does not call the table old when only a picture failed to download', async () => {
    const state = await cloudState();
    state.status.last_error = { code: 'asset_download', message: '' };
    const { host } = phrases(ready(state));
    const alert = host.querySelector('[role="alert"]');
    expect(alert?.textContent).toContain('有图片没能下载');
    expect(alert?.textContent).not.toContain('目前仍使用上次同步的内容');
  });

  it('offers copying fixed text only, and says why not otherwise', async () => {
    const backend = new MockBackend({ rules: 3, cloud: 'sample' });
    backend.cloudCache?.rules.push({
      ...sampleCloudRules()[0],
      id: 'mock-cloud-clip',
      match: { kind: 'literal', value: 'tclip' },
      action: {
        type: 'text',
        method: 'paste',
        source: { type: 'inline', value: '订单 {cliptext}' },
      },
    } as MockRule);
    const state = (await backend.handle('get_cloud_state')) as CloudState;
    const { host, props } = phrases(ready(state));
    const copy = (abbreviation: string) =>
      [...(rowOf(host, abbreviation)?.querySelectorAll('button') ?? [])].find(
        (b) => b.textContent === '复制到我的指令',
      );
    expect(copy('tnh')?.disabled).toBe(false);
    // Disabled by the table or on this computer, or replacing a rule: still fixed text, still copyable.
    for (const ok of ['tjh', 'tfh', ';s2']) expect(copy(ok)?.disabled, ok).toBe(false);
    for (const [abbreviation, why] of [
      ['(DD[0-9', '无法使用'],
      ['tbz', '打开网页、文件或程序'],
      ['tclip', '{cliptext}'],
    ] as const) {
      expect(copy(abbreviation)?.disabled, abbreviation).toBe(true);
      expect(copy(abbreviation)?.title).toContain(why);
      expect(copy(abbreviation)?.parentElement?.title).toContain(why);
    }
    click(rowOf(host, 'tclip')?.querySelector('.row-tools button'));
    const detail = host.querySelector('dialog[aria-label="查看团队话术"]');
    expect(detail?.querySelector('.modal-footer span')?.textContent).toMatch(
      /^不能复制：正文会读取本机/,
    );
    expect(button(detail ?? host, '复制到我的指令')?.disabled).toBe(true);
    expect(props.onCopy).not.toHaveBeenCalled();
  });

  it('offers copying only while 我的指令 can be saved', async () => {
    const { host } = phrases(ready(await cloudState()), { canCopy: false });
    const copies = [...host.querySelectorAll('button')].filter(
      (b) => b.textContent === '复制到我的指令',
    );
    expect(copies.length).toBeGreaterThan(0);
    expect(copies.every((b) => b.disabled)).toBe(true);
  });

  it('explains a failed sync by its code and keeps the cached list', async () => {
    const { host, props } = phrases(ready(await cloudState({ cloud: 'error' })));
    const alert = host.querySelector('[role="alert"]');
    expect(alert?.textContent).toMatch(/^同步失败（\S+ \d\d:\d\d）：同步密钥无效/);
    expect(alert?.textContent).toContain('目前仍使用上次同步的内容');
    expect(host.querySelectorAll('tbody tr').length).toBeGreaterThan(0);
    click(button(alert ?? host, '去设置'));
    expect(props.onOpenSettings).toHaveBeenCalledTimes(1);
  });

  it('leads to the settings when not subscribed, and says when nothing could be read', async () => {
    const { host, props } = phrases(ready(await cloudState({ cloud: 'off' })));
    expect(host.textContent).toContain('还没有订阅团队话术');
    expect(host.querySelector('table')).toBeNull();
    click(button(host, '去设置开启'));
    expect(props.onOpenSettings).toHaveBeenCalledTimes(1);

    const failed = phrases({
      status: 'connecting',
      state: null,
      pending: null,
      loadError: '未连接到桌面后端',
    });
    expect(failed.host.textContent).toContain('未能读取团队话术未连接到桌面后端');
    click(button(failed.host, '重新读取'));
    expect(failed.props.onRetry).toHaveBeenCalledTimes(1);
  });
});

describe('团队话术同步 settings', () => {
  function section(
    view: CloudView,
    onSave: (input: CloudSettingsInput) => Promise<CloudState>,
    onSync: () => Promise<CloudState>,
  ) {
    return render(<CloudSettingsSection view={view} onSave={onSave} onSync={onSync} />);
  }

  const settle = () =>
    act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });

  it('saves without the key unless it is replaced or cleared, then syncs', async () => {
    const backend = new MockBackend({ rules: 0, cloud: 'sample' });
    const state = (await backend.handle('get_cloud_state')) as CloudState;
    const onSave = vi.fn(
      (input: CloudSettingsInput) =>
        backend.handle('set_cloud_settings', { settings: input }) as Promise<CloudState>,
    );
    const onSync = vi.fn(() => backend.handle('sync_cloud_now') as Promise<CloudState>);
    const { host } = section(ready(state), onSave, onSync);
    expect(host.querySelector('.key-state')?.textContent).toBe('已设置替换清除');
    expect(host.querySelector('input[type="password"]')).toBeNull();
    const interval = host.querySelector('select');
    expect([...(interval?.options ?? [])].map((o) => o.textContent)).toEqual([
      '仅手动同步',
      '每 15 分钟',
      '每 30 分钟',
      '每 1 小时',
      '每 2 小时',
      '每 4 小时',
    ]);

    click(button(host, '保存并同步'));
    await settle();
    expect(onSave).toHaveBeenLastCalledWith({
      enabled: true,
      base_url: 'https://cs.example.com',
      auto_sync_minutes: 30,
    });
    expect(onSync).toHaveBeenCalledTimes(1);
    expect(host.querySelector('[role="status"]')?.textContent).toBe('已保存并同步，团队话术 v12。');

    click(button(host, '替换'));
    type(host.querySelector('input[aria-label="同步密钥"]'), 'new-key');
    click(button(host, '保存并同步'));
    await settle();
    expect(onSave.mock.lastCall?.[0].key).toBe('new-key');
    expect(backend.cloudPrefs.key).toBe('new-key');
    // Saved: the key typed is gone from the page.
    expect(host.querySelector('input[aria-label="同步密钥"]')).toBeNull();

    click(button(host, '清除'));
    expect(host.querySelector('.key-state')?.textContent).toBe('保存后清除撤销');
    click(button(host, '保存并同步'));
    await settle();
    expect(onSave.mock.lastCall?.[0].key).toBe('');
    expect(host.querySelector('[role="alert"]')?.textContent).toMatch(
      /^已保存，但同步失败：团队话术同步还没有设置完整/,
    );
  });

  it('says which field the backend refused, and does not sync', async () => {
    const backend = new MockBackend({ rules: 0, cloud: 'off' });
    const state = (await backend.handle('get_cloud_state')) as CloudState;
    const onSave = (input: CloudSettingsInput) =>
      backend.handle('set_cloud_settings', { settings: input }) as Promise<CloudState>;
    const onSync = vi.fn(() => backend.handle('sync_cloud_now') as Promise<CloudState>);
    const { host } = section(ready(state), onSave, onSync);
    expect(host.querySelector('input[aria-label="同步密钥"]')).not.toBeNull();
    click(
      [...host.querySelectorAll('label')]
        .find((l) => l.textContent.trim() === '订阅团队话术')
        ?.querySelector('input'),
    );
    const url = [...host.querySelectorAll('label.field')]
      .find((l) => l.querySelector('span')?.textContent === '服务器地址')
      ?.querySelector('input');
    type(url ?? null, 'http://cs.example.com');
    click(button(host, '保存并同步'));
    await settle();
    expect(host.querySelector('[role="alert"]')?.textContent).toBe(
      '保存失败：服务器地址：服务器地址必须以 https:// 开头（本机调试可用 http://localhost）',
    );
    expect(onSync).not.toHaveBeenCalled();
    expect(backend.cloudPrefs.enabled).toBe(false);
  });

  it('follows settings changed elsewhere until edited', async () => {
    const backend = new MockBackend({ rules: 0, cloud: 'sample' });
    const state = (await backend.handle('get_cloud_state')) as CloudState;
    const never = () => new Promise<CloudState>(() => undefined);
    const { host, rerender } = section(ready(state), never, never);
    const url = () =>
      [...host.querySelectorAll('label.field')]
        .find((l) => l.querySelector('span')?.textContent === '服务器地址')
        ?.querySelector('input') ?? null;
    const moved = { ...state, settings: { ...state.settings, base_url: 'https://b.example' } };
    rerender(<CloudSettingsSection view={ready(moved)} onSave={never} onSync={never} />);
    expect(url()?.value).toBe('https://b.example');
    type(url(), 'https://typed.example');
    const again = { ...state, settings: { ...state.settings, base_url: 'https://c.example' } };
    rerender(<CloudSettingsSection view={ready(again)} onSave={never} onSync={never} />);
    expect(url()?.value).toBe('https://typed.example');
  });
});
