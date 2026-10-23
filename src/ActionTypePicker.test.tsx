import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ActionTypePicker } from './ActionTypePicker';

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
const cleanups: (() => void)[] = [];
afterEach(() => cleanups.splice(0).forEach((cleanup) => cleanup()));

function mount() {
  const host = document.createElement('dialog');
  document.body.append(host);
  const root = createRoot(host);
  const change = vi.fn();
  act(() =>
    root.render(
      <ActionTypePicker
        value="text_keys"
        onChange={change}
        options={[
          { value: 'text_keys', label: '键入纯文本', disabled: false },
          { value: 'html', label: '粘贴带格式的文字（暂不可用）', disabled: true },
          { value: 'open', label: '打开网页、文件或程序', disabled: false },
        ]}
      />,
    ),
  );
  cleanups.push(() => {
    act(() => root.unmount());
    host.remove();
  });
  const trigger = host.querySelector<HTMLButtonElement>('[role="combobox"]');
  if (!trigger) throw new Error('missing picker');
  const press = (key: string) =>
    act(() => {
      trigger.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true }));
    });
  return { host, trigger, change, press };
}

describe('ActionTypePicker', () => {
  it('only shows guidance while expanded, previews disabled options without changing the action', () => {
    const { host, trigger, change, press } = mount();
    expect(host.querySelector('.action-guide')).toBeNull();
    act(() => {
      trigger.click();
    });
    expect(host.querySelector('.action-type-popup')?.parentElement).toBe(host);
    const option = host.querySelector<HTMLElement>('[data-value="html"]');
    act(() => {
      option?.dispatchEvent(new MouseEvent('mousemove', { bubbles: true }));
    });
    expect(host.querySelector('.action-guide')?.textContent).toContain(
      '把带加粗、颜色或链接的内容粘贴',
    );
    act(() => {
      option?.click();
    });
    expect(change).not.toHaveBeenCalled();
    expect(trigger.value).toBe('text_keys');
    press('Escape');
    expect(host.querySelector('.action-guide')).toBeNull();
    expect(trigger.getAttribute('aria-expanded')).toBe('false');
  });

  it('browses with arrows and Home/End, confirms with Enter, and dismisses on Tab and outside pointer', () => {
    const { host, trigger, change, press } = mount();
    act(() => {
      trigger.focus();
    });
    press('ArrowDown');
    press('ArrowDown');
    expect(host.querySelector('[data-active="true"]')?.getAttribute('aria-disabled')).toBe('true');
    press('Enter');
    expect(change).not.toHaveBeenCalled();
    press('End');
    expect(host.querySelector('.action-guide')?.textContent).toContain('打开常用网页');
    press('Home');
    expect(host.querySelector('[data-active="true"]')?.getAttribute('aria-selected')).toBe('true');
    press('ArrowUp');
    press('Enter');
    expect(change).toHaveBeenCalledExactlyOnceWith('open');
    expect(document.activeElement).toBe(trigger);
    expect(host.querySelector('.action-type-popup')).toBeNull();
    act(() => {
      trigger.click();
    });
    press('Tab');
    expect(host.querySelector('.action-type-popup')).toBeNull();
    act(() => {
      trigger.click();
    });
    act(() => {
      document.body.dispatchEvent(new Event('pointerdown', { bubbles: true }));
    });
    expect(host.querySelector('.action-type-popup')).toBeNull();
  });
});
