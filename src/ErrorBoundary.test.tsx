import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ErrorBoundary } from './ErrorBoundary';
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
  // React reports caught render errors on the console; keep the test output readable.
  vi.spyOn(console, 'error').mockImplementation(() => undefined);
  const host = document.createElement('div');
  const root = createRoot(host);
  await act(async () => {
    root.render(
      <ErrorBoundary>
        <Boom />
      </ErrorBoundary>,
    );
    await settle();
  });
  return { host, root };
}

afterEach(() => {
  setTransport(null);
  broken = true;
});

describe('ErrorBoundary', () => {
  it('shows a recoverable page and logs only the error class and component names', async () => {
    const backend = new MockBackend({ rules: 0 });
    const { host, root } = await render(backend);
    expect(host.querySelector('[role="alert"] h1')?.textContent).toBe('界面出错了');
    const reports = backend.calls.filter((c) => c.command === 'log_frontend_error');
    expect(reports).toHaveLength(1);
    const args = reports[0]?.args;
    expect(args?.message).toBe('TypeError');
    expect(String(args?.stack)).toMatch(/^Boom(@|$)/m);
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
