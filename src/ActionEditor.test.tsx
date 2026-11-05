import { act, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';
import ActionEditor from './ActionEditor';
import { createMockTransport, MockBackend } from './ipc/mock';
import { setTransport } from './ipc/transport';
import type { Data } from './model';

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
const cleanups: (() => void)[] = [];
afterEach(() => cleanups.splice(0).forEach((cleanup) => cleanup()));

function mount(keys: Data[], platform = 'windows') {
  const host = document.createElement('div');
  document.body.append(host);
  const root = createRoot(host);
  const change = vi.fn();
  function Harness() {
    const [action, setAction] = useState<Data>({ type: 'hotkey', keys });
    return (
      <ActionEditor
        platform={platform}
        rules={[]}
        action={action}
        onChange={(next) => {
          change(next.keys);
          setAction(next);
        }}
        run={async (fn) => {
          await fn();
        }}
        show={() => {}}
      />
    );
  }
  act(() => root.render(<Harness />));
  cleanups.push(() => {
    act(() => root.unmount());
    host.remove();
  });
  function click(label: string) {
    const button = Array.from(host.querySelectorAll('button')).find(
      (item) => item.getAttribute('aria-label') === label || item.textContent === label,
    );
    if (!button) throw new Error(`missing button ${label}`);
    act(() => button.click());
  }
  return { host, click, change };
}
const named = (name: string) => ({ type: 'named', name });

describe('粘贴图片', () => {
  const asset = `${'0'.repeat(64)}.png`;
  const flush = () => act(() => new Promise<void>((resolve) => setTimeout(resolve, 0)));

  function mountImage(action: Data, options: ConstructorParameters<typeof MockBackend>[0] = {}) {
    const backend = new MockBackend(options);
    setTransport(createMockTransport(backend));
    cleanups.push(() => setTransport(null));
    const host = document.createElement('div');
    document.body.append(host);
    const root = createRoot(host);
    const change = vi.fn();
    function Harness() {
      const [current, setCurrent] = useState(action);
      return (
        <ActionEditor
          platform="windows"
          rules={[]}
          action={current}
          onChange={(next) => {
            change(next);
            setCurrent(next);
          }}
          run={async (fn) => {
            await fn();
          }}
          show={() => {}}
        />
      );
    }
    act(() => root.render(<Harness />));
    cleanups.push(() => {
      act(() => root.unmount());
      host.remove();
    });
    const button = (label: string) => {
      const found = Array.from(host.querySelectorAll('button')).find(
        (item) => item.textContent === label,
      );
      if (!found) throw new Error(`missing button ${label}`);
      return found;
    };
    const calls = (command: string) => backend.calls.filter((c) => c.command === command);
    return { host, button, change, calls };
  }

  it('copies the chosen file into the library, stores it and previews it', async () => {
    const { host, button, change, calls } = mountImage({ type: 'image', asset: '', name: '' });
    expect(host.textContent).toContain('还没有选择图片');
    expect(button('预览图片').disabled).toBe(true);
    act(() => button('选择图片').click());
    await flush();
    expect(calls('choose_file')[0]?.args).toEqual({ kind: 'image' });
    expect(calls('import_image_asset')[0]?.args).toEqual({ path: 'mock://示例图片.png' });
    expect(change).toHaveBeenLastCalledWith({ type: 'image', asset, name: '示例图片.png' });
    expect(host.textContent).toContain('示例图片.png');
    expect(calls('preview_image_asset')[0]?.args).toEqual({ asset });
    expect(host.querySelector('img.image-preview')?.getAttribute('src')).toMatch(/^data:image/);
  });

  it('leaves the action as it is when the dialog is cancelled', async () => {
    const { button, change, calls } = mountImage(
      { type: 'image', asset: '', name: '' },
      { dialog: 'cancel' },
    );
    act(() => button('选择图片').click());
    await flush();
    expect(calls('import_image_asset')).toHaveLength(0);
    expect(change).not.toHaveBeenCalled();
  });

  it('previews the picture a saved rule holds on request', async () => {
    const { host, button, calls } = mountImage({ type: 'image', asset, name: '' });
    expect(host.textContent).toContain('已选择图片');
    expect(host.querySelector('img.image-preview')).toBeNull();
    act(() => button('预览图片').click());
    await flush();
    expect(calls('preview_image_asset')[0]?.args).toEqual({ asset });
    expect(host.querySelector('img.image-preview')).not.toBeNull();
  });
});

describe('shortcut keyboard', () => {
  it('toggles keys and previews ordered combinations without changing legacy keys', () => {
    const legacy = [{ type: 'windows_code', code: 13 }, named('F24')];
    const { host, click, change } = mount(legacy);
    expect(change).not.toHaveBeenCalled();
    expect(host.querySelector('.shortcut-chord')?.textContent).toContain('#13');
    click('Ctrl');
    click('D1');
    expect(change).toHaveBeenLastCalledWith([...legacy, named('Ctrl'), named('D1')]);
    expect(host.querySelector('[aria-label="D1"]')?.getAttribute('aria-pressed')).toBe('true');
    click('Ctrl');
    expect(change).toHaveBeenLastCalledWith([...legacy, named('D1')]);
    click('移除 #13');
    expect(change).toHaveBeenLastCalledWith([named('F24'), named('D1')]);
    expect(host.querySelector('.shortcut-advanced input')?.getAttribute('value')).toBe('F24');
    click('清空');
    expect(change).toHaveBeenLastCalledWith([]);
    expect(host.querySelector('.shortcut-chord')?.textContent).toContain('还没有设置');
  });

  it('records a Mac shortcut, cancels with Escape, and offers Cmd on the keyboard', () => {
    const { host, click, change } = mount([], 'macos');
    expect(host.querySelector('[aria-label="Win"]')).toBeNull();
    click('Cmd');
    expect(change).toHaveBeenLastCalledWith([named('Cmd')]);
    click('录制快捷键');
    act(() => {
      host
        .querySelector('input[aria-label="快捷键录制"]')
        ?.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', metaKey: true, bubbles: true }));
    });
    expect(change).toHaveBeenLastCalledWith([named('Cmd'), named('A')]);
    // Option and Shift change the character typed, not the key recorded (e.code).
    click('录制快捷键');
    act(() => {
      host
        .querySelector('input[aria-label="快捷键录制"]')
        ?.dispatchEvent(
          new KeyboardEvent('keydown', { key: 'å', code: 'KeyA', altKey: true, bubbles: true }),
        );
    });
    expect(change).toHaveBeenLastCalledWith([named('Alt'), named('A')]);
    click('录制快捷键');
    act(() => {
      host.querySelector('input[aria-label="快捷键录制"]')?.dispatchEvent(
        new KeyboardEvent('keydown', {
          key: '!',
          code: 'Digit1',
          shiftKey: true,
          metaKey: true,
          bubbles: true,
        }),
      );
    });
    expect(change).toHaveBeenLastCalledWith([named('Shift'), named('Cmd'), named('D1')]);
    expect(host.querySelector('[aria-label="Cmd"]')?.getAttribute('aria-pressed')).toBe('true');
    click('录制快捷键');
    act(() => {
      host
        .querySelector('input[aria-label="快捷键录制"]')
        ?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    });
    expect(change).toHaveBeenCalledTimes(4);
    expect(host.querySelector('input[aria-label="快捷键录制"]')).toBeNull();
  });
});
