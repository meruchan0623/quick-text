import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, describe, expect, it } from 'vitest';
import CandidateHintSection from './CandidateHintSection';
import { HintPanel } from './hint/HintPanel';
import { MockBackend, createMockTransport } from './ipc/mock';
import { setTransport } from './ipc/transport';
import { layout, parseSeconds, type HintItem, type HintPayload } from './lib/hint';

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const mounted: { root: Root; host: HTMLElement }[] = [];
afterEach(() => {
  for (const { root, host } of mounted.splice(0)) {
    act(() => root.unmount());
    host.remove();
  }
  setTransport(null);
});

function render(element: React.ReactElement) {
  const host = document.createElement('div');
  document.body.append(host);
  const root = createRoot(host);
  act(() => root.render(element));
  mounted.push({ root, host });
  return host;
}

const flush = () => act(async () => new Promise((resolve) => setTimeout(resolve, 0)));

function input(host: HTMLElement, selector: string): HTMLInputElement {
  const found = host.querySelector<HTMLInputElement>(selector);
  if (!found) throw new Error(`missing ${selector}`);
  return found;
}

/** Types `value` into a controlled input the way React notices. */
function typeInto(input: HTMLInputElement, value: string) {
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set?.call(input, value);
  input.dispatchEvent(new Event('input', { bubbles: true }));
}

describe('layout', () => {
  it('uses the richest density that fits everything, else compact with a remainder', () => {
    expect(layout(3, 3, 480)).toEqual({ density: 'detail', shown: 3 });
    expect(layout(6, 6, 480)).toEqual({ density: 'standard', shown: 6 });
    expect(layout(12, 12, 480)).toEqual({ density: 'compact', shown: 12 });
    const many = layout(40, 40, 480);
    expect(many.density).toBe('compact');
    expect(many.shown).toBeLessThan(40);
    // More rules than were sent: never all shown.
    expect(layout(50, 3, 480).shown).toBe(3);
    expect(layout(1, 1, 10)).toEqual({ density: 'compact', shown: 1 });
  });

  it('reads seconds between 1 and 10 in half seconds', () => {
    expect(parseSeconds('2')).toBe(2);
    expect(parseSeconds('2.5')).toBe(2.5);
    expect(parseSeconds('0.5')).toBeNull();
    expect(parseSeconds('11')).toBeNull();
    expect(parseSeconds('')).toBeNull();
    expect(parseSeconds('abc')).toBeNull();
  });
});

describe('CandidateHintSection', () => {
  it('shows the duration only when on and saves both to prefs', async () => {
    const backend = new MockBackend({ rules: 1 });
    setTransport(createMockTransport(backend));
    const host = render(
      <CandidateHintSection stored={{ enabled: false, duration_ms: 2000 }} disabled={false} />,
    );
    expect(host.textContent).toContain('输入时显示候选提示');
    expect(host.textContent).not.toContain('显示时长');
    const toggle = input(host, 'input[type=checkbox]');
    act(() => {
      toggle.click();
    });
    await flush();
    expect(backend.candidateHint).toEqual({ enabled: true, duration_ms: 2000 });
    const seconds = input(host, 'input[type=number]');
    expect(seconds.value).toBe('2');

    act(() => typeInto(seconds, '12'));
    act(() => {
      seconds.dispatchEvent(new FocusEvent('focusout', { bubbles: true }));
    });
    expect(host.querySelector('[role=alert]')?.textContent).toContain('1 到 10');
    expect(backend.candidateHint.duration_ms).toBe(2000);

    act(() => typeInto(seconds, '3.5'));
    act(() => {
      seconds.dispatchEvent(new FocusEvent('focusout', { bubbles: true }));
    });
    await flush();
    expect(backend.candidateHint).toEqual({ enabled: true, duration_ms: 3500 });
    expect(host.querySelector('[role=alert]')).toBeNull();
  });

  it('is hidden on a backend without the setting', () => {
    const host = render(<CandidateHintSection stored={undefined} disabled={false} />);
    expect(host.textContent).toBe('');
  });
});

describe('HintPanel', () => {
  const item = (id: string, abbreviation: string, preview: string): HintItem => ({
    rule_id: id,
    abbreviation,
    description: `说明 ${id}`,
    source: id === 'b' ? 'team' : 'personal',
    trigger: 'terminator',
    preview,
    preview_is_label: false,
  });
  const payload = (items: HintItem[], total = items.length): HintPayload => ({
    seq: 1,
    hint: { typed: 'th', total, items },
    width: 320,
    max_height: 480,
    duration_ms: 2000,
  });

  it('highlights the typed part, marks the source and reports its height', () => {
    const heights: number[] = [];
    const host = render(
      <HintPanel
        payload={payload([item('a', 'thx', 'Thanks'), item('b', 'thks', '谢谢\n再见')])}
        onLaidOut={(h) => heights.push(h)}
      />,
    );
    expect(host.querySelector('.hint-abbr b')?.textContent).toBe('th');
    expect(host.textContent).toContain('团队');
    expect(host.textContent).toContain('我的');
    expect(host.querySelectorAll('.hint-detail')).toHaveLength(2);
    expect(heights).toHaveLength(1);
  });

  it('goes compact with a remainder line when too many match', () => {
    const items = Array.from({ length: 40 }, (_, i) =>
      item(`r${i}`, `th${i}`, `正文 ${i}\n第二行`),
    );
    const host = render(<HintPanel payload={payload(items, 55)} onLaidOut={() => {}} />);
    expect(host.querySelector('.hint-detail')).toBeNull();
    expect(host.querySelector('.hint-compact .hint-preview')?.textContent).toBe('正文 0 ⏎ 第二行');
    expect(host.querySelector('.hint-more')?.textContent).toMatch(/还有 \d+ 项/);
  });

  it('renders nothing for a hide', () => {
    const host = render(
      <HintPanel payload={{ ...payload([]), hint: null }} onLaidOut={() => {}} />,
    );
    expect(host.textContent).toBe('');
  });
});
