import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ErrorBoundary } from './ErrorBoundary';
import { rootErrorOptions } from './errorLog';
import { createMockTransport, MockBackend } from './ipc/mock';
import { setTransport } from './ipc/transport';

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const SECRET = ';secret-phrase';
let broken = true;

function Boom() {
  if (broken) throw new TypeError(`Cannot read properties of ${SECRET}`);
  return <p>界面已恢复</p>;
}

const settle = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

async function render(backend: MockBackend) {
  setTransport(createMockTransport(backend));
  // Spied on, not silenced: what reaches the console is checked.
  const printed = [vi.spyOn(console, 'error'), vi.spyOn(console, 'warn')];
  const host = document.createElement('div');
  // The root as a release build makes it (main.tsx): React's own error output replaced.
  const root = createRoot(host, rootErrorOptions(false));
  await act(async () => {
    root.render(
      <ErrorBoundary>
        <Boom />
      </ErrorBoundary>,
    );
    await settle();
  });
  return { host, root, printed };
}

afterEach(() => {
  setTransport(null);
  broken = true;
});

describe('ErrorBoundary', () => {
  it('shows a recoverable page and logs only the error class and stack frames', async () => {
    const backend = new MockBackend({ rules: 0 });
    const { host, root, printed } = await render(backend);
    expect(host.querySelector('[role="alert"] h1')?.textContent).toBe('界面出错了');
    // Reported once, by the boundary; the root's onCaughtError only prints.
    const reports = backend.calls.filter((c) => c.command === 'log_frontend_error');
    expect(reports).toHaveLength(1);
    // One line of our own on the console: the class and the component names, no message.
    const [consoleError, consoleWarn] = printed;
    expect(consoleWarn?.mock.calls).toEqual([]);
    expect(consoleError?.mock.calls).toHaveLength(1);
    const printedArgs: unknown[] = consoleError?.mock.calls[0] ?? [];
    const [line, ...rest] = printedArgs;
    expect(rest).toEqual([]);
    expect(typeof line).toBe('string');
    expect(line).toContain('a render error, caught by the error boundary (TypeError)');
    expect(line).toMatch(/^Boom@ErrorBoundary\.test\.tsx:\d+:\d+$/m);
    expect(line).toMatch(/^ErrorBoundary(@|$)/m);
    expect(line).not.toContain('secret');
    const args = reports[0]?.args;
    expect(args?.message).toBe('TypeError');
    // The frames of the error's own stack as the browser wrote them, without the message line.
    const frames = String(args?.stack).split('\n');
    expect(frames[0]).toMatch(/^at \S+ErrorBoundary\.test\.tsx:\d+:\d+$/);
    expect(frames.every((frame) => /^at \S+:\d+:\d+$/.test(frame))).toBe(true);
    expect(args?.source).toBe(frames[0]?.slice(3));
    expect(Object.keys(args ?? {}).sort()).toEqual(['message', 'source', 'stack']);
    expect(JSON.stringify(args)).not.toContain('secret');

    // 重新加载界面 mounts the children again.
    broken = false;
    const retry = [...host.querySelectorAll('button')].find(
      (b) => b.textContent === '重新加载界面',
    );
    await act(async () => {
      retry?.click();
      await settle();
    });
    expect(host.textContent).toBe('界面已恢复');
    act(() => root.unmount());
  });

  it('still shows the page when the backend cannot take the report (0.3)', async () => {
    const backend = new MockBackend({ rules: 0, legacy: true });
    const { host, root } = await render(backend);
    expect(host.querySelector('[role="alert"] h1')?.textContent).toBe('界面出错了');
    expect(backend.calls.map((c) => c.command)).toEqual(['log_frontend_error']);
    act(() => root.unmount());
  });
});
