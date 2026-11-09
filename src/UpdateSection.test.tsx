import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { UpdateCheck, UpdateProgress, UpdateRelease } from './lib/update';
import UpdateSection, {
  AUTO_CHECK_KEY,
  PRERELEASE_KEY,
  releaseDate,
  sizeText,
  updateAvailable,
  useUpdates,
} from './UpdateSection';

vi.mock('./lib/update', async (importOriginal) => {
  const original = await importOriginal<typeof import('./lib/update')>();
  return {
    ...original,
    checkUpdate: vi.fn(),
    downloadUpdate: vi.fn(),
    revealUpdate: vi.fn(),
    onUpdateProgress: vi.fn(),
  };
});

const update = await import('./lib/update');
const checkUpdate = vi.mocked(update.checkUpdate);
const downloadUpdate = vi.mocked(update.downloadUpdate);
const revealUpdate = vi.mocked(update.revealUpdate);
const onUpdateProgress = vi.mocked(update.onUpdateProgress);

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const release = (version: string, extra: Partial<UpdateRelease> = {}): UpdateRelease => ({
  version,
  tag: `v${version}`,
  prerelease: version.includes('-'),
  published_at: '2026-11-02T08:00:00Z',
  notes: '- 新增：应用内检查更新',
  html_url: `https://github.com/meruchan0623/quick-text/releases/tag/v${version}`,
  asset: { name: `Quicktext_${version}_x64.zip`, size: 12_582_912 },
  ...extra,
});

const found = (current: string, latest: UpdateRelease): UpdateCheck => ({
  current,
  latest,
  update_available: true,
});
const upToDate = (current: string): UpdateCheck => ({
  current,
  latest: release(current),
  update_available: false,
});

const cleanups: (() => void)[] = [];
beforeEach(() => {
  localStorage.clear();
  checkUpdate.mockReset();
  downloadUpdate.mockReset();
  revealUpdate.mockReset();
  onUpdateProgress.mockReset();
  onUpdateProgress.mockResolvedValue(() => undefined);
});
afterEach(() => {
  cleanups.splice(0).forEach((cleanup) => cleanup());
  vi.useRealTimers();
});

/** What App marks 设置 with, rendered for the tests to read. */
function Harness({ delay }: { delay: number | null }) {
  const updates = useUpdates(delay);
  return (
    <>
      <i data-mark={updateAvailable(updates)} />
      <UpdateSection updates={updates} />
    </>
  );
}

const mark = () => document.querySelector('[data-mark]')?.getAttribute('data-mark') === 'true';

function mount(delay: number | null = null) {
  const host = document.createElement('div');
  document.body.append(host);
  const root = createRoot(host);
  act(() => root.render(<Harness delay={delay} />));
  cleanups.push(() => {
    act(() => root.unmount());
    host.remove();
  });
  const button = (text: string) =>
    [...host.querySelectorAll('button')].find((b) => b.textContent.startsWith(text));
  const click = async (text: string) => {
    const target = button(text);
    if (!target) throw new Error(`no button 「${text}」`);
    await act(async () => {
      target.click();
      await Promise.resolve();
    });
  };
  const box = (text: string) =>
    [...host.querySelectorAll('label')]
      .find((l) => l.textContent.trim() === text)
      ?.querySelector('input') ?? null;
  const flush = () => act(() => new Promise((resolve) => setTimeout(resolve, 0)));
  return { host, button, click, box, flush };
}

describe('UpdateSection', () => {
  it('says the running version is the latest', async () => {
    checkUpdate.mockResolvedValue(upToDate('0.3.1'));
    const { host, click, flush } = mount();
    expect(host.textContent).toContain('当前版本 —');
    await click('检查更新');
    await flush();
    expect(checkUpdate).toHaveBeenCalledExactlyOnceWith(false);
    expect(host.textContent).toContain('当前版本 v0.3.1');
    expect(host.querySelector('.update-latest')?.textContent).toBe('已是最新版本');
    expect(mark()).toBe(false);
  });

  it('shows a newer test build with its date, downloads it with progress and shows it', async () => {
    localStorage.setItem(PRERELEASE_KEY, '1');
    const latest = release('0.4.0-test.12');
    checkUpdate.mockResolvedValue(found('0.3.1', latest));
    let progress: ((p: UpdateProgress) => void) | null = null;
    onUpdateProgress.mockImplementation((handler) => {
      progress = handler;
      return Promise.resolve(() => {
        progress = null;
      });
    });
    let finish: (() => void) | null = null;
    downloadUpdate.mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = () => {
            resolve({ path: '/Users/me/Downloads/Quicktext.zip', sha256: 'ab', verified: true });
          };
        }),
    );
    revealUpdate.mockResolvedValue(undefined);
    const { host, click, flush, button } = mount();
    await click('检查更新');
    await flush();
    expect(checkUpdate).toHaveBeenCalledExactlyOnceWith(true);
    const title = host.querySelector('.update-title')?.textContent ?? '';
    expect(title).toContain('发现新版本 v0.4.0-test.12');
    expect(title).toContain('测试版');
    expect(title).toContain(releaseDate(latest.published_at) ?? 'no date');
    expect(mark()).toBe(true);

    await click('下载');
    await flush();
    expect(downloadUpdate).toHaveBeenCalledExactlyOnceWith('v0.4.0-test.12');
    expect(button('正在下载')?.disabled).toBe(true);
    act(() => {
      progress?.({ tag: 'v0.4.0-test.12', downloaded: 6_291_456, total: 12_582_912 });
      // Another download's progress is not this one's.
      progress?.({ tag: 'v9.9.9', downloaded: 1, total: 2 });
    });
    const bar = host.querySelector('progress');
    expect(bar?.value).toBe(6_291_456);
    expect(bar?.max).toBe(12_582_912);
    expect(host.querySelector('.update-progress')?.textContent).toContain('50%');

    await act(async () => {
      finish?.();
      await Promise.resolve();
    });
    await flush();
    expect(progress).toBeNull();
    expect(host.querySelector('progress')).toBeNull();
    expect(host.querySelector('.update-ok')?.textContent).toContain('校验通过');
    await click('在文件夹中显示');
    expect(revealUpdate).toHaveBeenCalledExactlyOnceWith('/Users/me/Downloads/Quicktext.zip');
  });

  it('shows check, download and reveal errors inline', async () => {
    checkUpdate.mockRejectedValueOnce({
      code: 'update',
      detail: { kind: 'network', message: '无法连接 GitHub' },
    });
    const { host, click, flush } = mount();
    await click('检查更新');
    await flush();
    expect(host.querySelector('.update-error')?.textContent).toContain('检查更新失败');

    checkUpdate.mockResolvedValue(found('0.3.1', release('0.4.0')));
    await click('检查更新');
    await flush();
    expect(host.textContent).not.toContain('检查更新失败');
    downloadUpdate.mockRejectedValueOnce(new Error('磁盘已满'));
    await click('下载');
    await flush();
    expect(host.querySelector('[role="alert"]')?.textContent).toBe('下载失败：磁盘已满');

    downloadUpdate.mockResolvedValue({ path: '/tmp/q.zip', sha256: 'ab', verified: false });
    await click('下载');
    await flush();
    expect(host.querySelector('.update-warning')?.textContent).toContain('未能校验');
    revealUpdate.mockRejectedValueOnce(new Error('文件已被移走'));
    await click('在文件夹中显示');
    await flush();
    expect(host.querySelector('[role="alert"]')?.textContent).toBe('文件已被移走');
  });

  it('says so when the release has no package for this computer', async () => {
    checkUpdate.mockResolvedValue(found('0.3.1', release('0.4.0', { asset: null })));
    const { host, click, flush, button } = mount();
    await click('检查更新');
    await flush();
    expect(host.textContent).toContain('没有适用于本机的安装包');
    expect(button('下载')).toBeUndefined();
  });

  it('includes test builds by default on a test build, and remembers the choice', async () => {
    checkUpdate.mockResolvedValue(upToDate('0.4.0-test.3'));
    const { click, flush, box } = mount();
    expect(box('包含测试版')?.checked).toBe(false);
    await click('检查更新');
    await flush();
    // The first answer told the version: asked again with test builds.
    expect(checkUpdate.mock.calls).toEqual([[false], [true]]);
    expect(box('包含测试版')?.checked).toBe(true);
    expect(localStorage.getItem(PRERELEASE_KEY)).toBeNull();

    await act(async () => {
      box('包含测试版')?.click();
      await Promise.resolve();
    });
    await flush();
    expect(localStorage.getItem(PRERELEASE_KEY)).toBe('0');
    expect(box('包含测试版')?.checked).toBe(false);
    // The answer shown was for the other choice: checked again.
    expect(checkUpdate.mock.calls.at(-1)).toEqual([false]);
  });

  it('checks once, silently, after the delay while 自动检查更新 is on', async () => {
    vi.useFakeTimers();
    checkUpdate.mockRejectedValue(new Error('offline'));
    const { host, box } = mount(3000);
    expect(box('自动检查更新')?.checked).toBe(true);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2999);
    });
    expect(checkUpdate).not.toHaveBeenCalled();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(checkUpdate).toHaveBeenCalledOnce();
    // A failed check nobody asked for says nothing.
    expect(host.querySelector('.update-error')).toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    expect(checkUpdate).toHaveBeenCalledOnce();
  });

  it('does not check by itself once 自动检查更新 is off, or without a delay', async () => {
    vi.useFakeTimers();
    localStorage.setItem(AUTO_CHECK_KEY, '0');
    const first = mount(0);
    expect(first.box('自动检查更新')?.checked).toBe(false);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(checkUpdate).not.toHaveBeenCalled();
    act(() => {
      first.box('自动检查更新')?.click();
    });
    expect(localStorage.getItem(AUTO_CHECK_KEY)).toBe('1');
    localStorage.clear();
    mount(null);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    // The first one, switched on again, checked; the one without a delay did not.
    expect(checkUpdate).toHaveBeenCalledOnce();
  });

  it('works without storage', () => {
    const get = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('denied');
    });
    const set = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('denied');
    });
    const { box } = mount();
    expect(box('自动检查更新')?.checked).toBe(true);
    act(() => {
      box('包含测试版')?.click();
    });
    expect(box('包含测试版')?.checked).toBe(true);
    expect(get).toHaveBeenCalled();
    expect(set).toHaveBeenCalled();
  });
});

describe('formatting', () => {
  it('formats sizes and dates', () => {
    expect(sizeText(12_582_912)).toBe('12.0 MB');
    expect(sizeText(2048)).toBe('2 KB');
    expect(sizeText(0)).toBe('0 KB');
    expect(releaseDate(null)).toBeNull();
    expect(releaseDate('not a date')).toBeNull();
    expect(releaseDate('2026-11-02T12:00:00')).toBe('2026-11-02');
  });
});
