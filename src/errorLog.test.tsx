// What errors leave on the console and in `log_frontend_error` in a production build: never the
// message. Every case throws an error carrying a sentinel and checks the arguments the console and
// the backend actually received; the console is spied on, not silenced.
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { installGlobalErrorHandlers, rootErrorOptions } from './errorLog';
import { createMockTransport, MockBackend } from './ipc/mock';
import { setTransport } from './ipc/transport';

const SENTINEL = 'SENTINEL-7f3a;secret-phrase';

function Boom(): never {
  throw new TypeError(`Cannot read properties of ${SENTINEL}`);
}

const settle = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

const spyOnConsole = () => [
  vi.spyOn(console, 'error'),
  vi.spyOn(console, 'warn'),
  vi.spyOn(console, 'log'),
  vi.spyOn(console, 'info'),
  vi.spyOn(console, 'debug'),
];
let spies: ReturnType<typeof spyOnConsole> = [];
let backend: MockBackend;
const cleanups: (() => void)[] = [];

function setUp(): void {
  backend = new MockBackend({ rules: 0 });
  setTransport(createMockTransport(backend));
  spies = spyOnConsole();
}

/** Everything printed on the console: our own strings only, never an error object. */
function printed(): string {
  const args = spies.flatMap((spy): unknown[] => spy.mock.calls.flat());
  for (const arg of args) expect(typeof arg).toBe('string');
  return args.join('\n');
}

function reports(): Record<string, unknown>[] {
  return backend.calls
    .filter((call) => call.command === 'log_frontend_error')
    .map((call) => call.args);
}

/**
 * Dispatches an uncaught-error event and a rejection event that no handler of ours should touch.
 * The error event has no `error`: Vitest's own window listener turns an error event with one that
 * nobody else listens to into a failure of the test.
 */
function leftAlone(): Event[] {
  const events = [
    new ErrorEvent('error', { cancelable: true }),
    Object.assign(new Event('unhandledrejection', { cancelable: true }), { reason: null }),
  ];
  for (const event of events) window.dispatchEvent(event);
  return events;
}

afterEach(() => {
  for (const cleanup of cleanups.splice(0)) cleanup();
  setTransport(null);
  vi.unstubAllEnvs();
});

describe('production build', () => {
  it('reports a render error no boundary caught, once and without its message', async () => {
    setUp();
    const root = createRoot(document.createElement('div'), rootErrorOptions(false));
    // Not in act(), which hands an uncaught error to the test instead of onUncaughtError.
    root.render(<Boom />);
    await vi.waitFor(() => {
      expect(reports()).toHaveLength(1);
    });
    await settle();
    expect(reports()).toHaveLength(1);
    expect(reports()[0]?.message).toBe('TypeError');
    expect(JSON.stringify(reports())).not.toContain(SENTINEL);
    const output = printed();
    expect(output).toContain('a render error no error boundary caught (TypeError)');
    expect(output).toMatch(/^Boom(@|$)/m);
    expect(output).not.toContain(SENTINEL);
    root.unmount();
  });

  it('reports an error React recovered from without its message or its cause', async () => {
    setUp();
    const error = new Error(`React recovered: ${SENTINEL}`, { cause: new TypeError(SENTINEL) });
    rootErrorOptions(false).onRecoverableError?.(error, {
      componentStack: `\n    at Editor (http://tauri.localhost/assets/index-D3f9Kq_x.js:1:2)`,
    });
    await settle();
    expect(reports()).toHaveLength(1);
    expect(reports()[0]?.message).toBe('Error');
    expect(JSON.stringify(reports())).not.toContain(SENTINEL);
    const output = printed();
    expect(output).toContain('a render error React recovered from (Error)');
    expect(output).toContain('Editor@index-D3f9Kq_x.js:1:2');
    expect(output).not.toContain(SENTINEL);
  });

  it('handles uncaught errors and rejections outside React instead of the webview', async () => {
    setUp();
    cleanups.push(installGlobalErrorHandlers(window, false));
    const error = new RangeError(SENTINEL);
    const uncaught = new ErrorEvent('error', { error, message: SENTINEL, cancelable: true });
    window.dispatchEvent(uncaught);
    const rejection = Object.assign(new Event('unhandledrejection', { cancelable: true }), {
      reason: new TypeError(SENTINEL),
      promise: Promise.resolve(),
    });
    window.dispatchEvent(rejection);
    await settle();
    // Cancelled: the webview prints nothing of its own.
    expect([uncaught.defaultPrevented, rejection.defaultPrevented]).toEqual([true, true]);
    expect(reports().map((report) => report.message)).toEqual(['RangeError', 'TypeError']);
    expect(JSON.stringify(reports())).not.toContain(SENTINEL);
    const output = printed();
    expect(output).toContain('an uncaught error (RangeError)');
    expect(output).toContain('an unhandled promise rejection (TypeError)');
    expect(output).not.toContain(SENTINEL);

    // Removed: the events are left to the webview again.
    cleanups.splice(0).forEach((cleanup) => {
      cleanup();
    });
    const later = leftAlone();
    await settle();
    expect(later.map((event) => event.defaultPrevented)).toEqual([false, false]);
    expect(reports()).toHaveLength(2);
  });

  it('is what a build without import.meta.env.DEV gets', () => {
    expect(rootErrorOptions()).toEqual({});
    vi.stubEnv('DEV', false);
    const options = rootErrorOptions();
    expect(Object.keys(options).sort()).toEqual([
      'onCaughtError',
      'onRecoverableError',
      'onUncaughtError',
    ]);
  });
});

describe('development', () => {
  it("keeps React's and the webview's own error output", async () => {
    setUp();
    expect(rootErrorOptions(true)).toEqual({});
    cleanups.push(installGlobalErrorHandlers(window, true));
    const events = leftAlone();
    await settle();
    expect(events.map((event) => event.defaultPrevented)).toEqual([false, false]);
    expect(reports()).toEqual([]);
  });
});
