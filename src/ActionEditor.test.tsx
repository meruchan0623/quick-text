import { act, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';
import ActionEditor from './ActionEditor';
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
    expect(host.querySelector('[aria-label="Cmd"]')?.getAttribute('aria-pressed')).toBe('true');
    click('录制快捷键');
    act(() => {
      host
        .querySelector('input[aria-label="快捷键录制"]')
        ?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    });
    expect(change).toHaveBeenCalledTimes(2);
    expect(host.querySelector('input[aria-label="快捷键录制"]')).toBeNull();
  });
});
