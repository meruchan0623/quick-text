import { describe, expect, it } from 'vitest';
import {
  componentFrames,
  errorClass,
  framePosition,
  frontendErrorReport,
  scriptLocation,
  stackPositions,
} from './errorReport';

const SECRET = ';secret-phrase';
const CHUNK = 'http://tauri.localhost/assets/index-D3f9Kq_x.js';

/** An error whose stack is exactly `stack`, as an engine would have written it. */
function withStack(error: Error, stack: string): Error {
  error.stack = stack;
  return error;
}

describe('frontendErrorReport', () => {
  it('names the error class, never the message', () => {
    expect(errorClass(new TypeError(SECRET))).toBe('TypeError');
    const odd = new Error(SECRET);
    odd.name = `Bad ${SECRET}`;
    expect(errorClass(odd)).toBe('Error');
    expect(errorClass(SECRET)).toBe('Error');
    expect(errorClass(null)).toBe('Error');
  });

  it('reads the position of V8, JavaScriptCore and SpiderMonkey frames as written', () => {
    expect(framePosition(`    at Boom (${CHUNK}:40:5)`)).toBe(`${CHUNK}:40:5`);
    expect(framePosition(`at ${CHUNK}:1:2`)).toBe(`${CHUNK}:1:2`);
    expect(framePosition(`at new ErrorBoundary (tauri://localhost/assets/a-12345678.js:3:4)`)).toBe(
      'tauri://localhost/assets/a-12345678.js:3:4',
    );
    expect(framePosition(`at async Object.load (${CHUNK}:7:8)`)).toBe(`${CHUNK}:7:8`);
    expect(framePosition(`Boom@${CHUNK}:1:2`)).toBe(`${CHUNK}:1:2`);
    expect(framePosition(`@${CHUNK}:5:6`)).toBe(`${CHUNK}:5:6`);
    expect(framePosition('http://localhost:1420/src/App.tsx?t=17:12:7')).toBeNull();
    for (const line of [
      'at Array.filter (<anonymous>)',
      'at eval (eval at f (http://x/a.js:1:2), <anonymous>:1:1)',
      'at async Promise.all (index 0)',
      'forEach@[native code]',
      `TypeError: ${SECRET}`,
      'at x (http://x/a b.js:1:2)',
      `at ${CHUNK}`,
      '',
    ])
      expect(framePosition(line), line).toBeNull();
  });

  it('sends the frames of a V8 stack without the message, origin and path included', () => {
    // The message spans lines and holds a line shaped like a frame.
    const error = new TypeError(`at evil (http://x/${SECRET}.js:1:2)\nsecond @http://x/s.js:3:4`);
    withStack(
      error,
      [
        `TypeError: ${error.message}`,
        '    at Array.filter (<anonymous>)',
        `    at TerminatorList (${CHUNK}:9:99)`,
        `    at Object.render (tauri://localhost/assets/vendor-a1B2-c_D.js:1:18)`,
      ].join('\n'),
    );
    expect(frontendErrorReport(error)).toEqual({
      message: 'TypeError',
      stack: `at ${CHUNK}:9:99\nat tauri://localhost/assets/vendor-a1B2-c_D.js:1:18`,
      source: `${CHUNK}:9:99`,
    });
  });

  it('cuts the header off whatever class name it has, and a bare one for an empty message', () => {
    // A subclass that sets its name after super(): V8 wrote `Error: …` in the stack.
    const named = withStack(new Error(SECRET), `Error: ${SECRET}\n    at f (${CHUNK}:1:2)`);
    named.name = 'CommandError';
    expect(frontendErrorReport(named)).toEqual({
      message: 'CommandError',
      stack: `at ${CHUNK}:1:2`,
      source: `${CHUNK}:1:2`,
    });
    const empty = withStack(new RangeError(), `RangeError\n    at g (${CHUNK}:3:4)`);
    expect(stackPositions(empty)).toEqual([`${CHUNK}:3:4`]);
    // The message set after the stack was made: the bare header is all there is to cut.
    empty.message = SECRET;
    expect(stackPositions(empty)).toEqual([`${CHUNK}:3:4`]);
  });

  it('reads a JavaScriptCore or SpiderMonkey stack, which has frames only', () => {
    const error = withStack(
      new TypeError(SECRET),
      `TerminatorList@${CHUNK}:9:99\nfilter@[native code]\n@${CHUNK}:1:1\n`,
    );
    expect(frontendErrorReport(error)).toEqual({
      message: 'TypeError',
      stack: `at ${CHUNK}:9:99\nat ${CHUNK}:1:1`,
      source: `${CHUNK}:9:99`,
    });
  });

  it('reads nothing of a stack that shows the message in an unexpected form', () => {
    const message = `at evil (http://x/${SECRET}.js:1:2)`;
    for (const stack of [
      // Not the header V8 writes.
      `Uncaught ${message}\n    at X (${CHUNK}:1:1)`,
      // The message changed after the stack was made: the header holds another one.
      `TypeError: old @${CHUNK}:1:1\n${message}\n    at X (${CHUNK}:1:1)`,
      // The header matches only a head of the message.
      `TypeError: ${message} and more\n    at X (${CHUNK}:1:1)`,
      // The message inside a frames-only stack.
      `X@${CHUNK}:1:1\n${message}`,
    ]) {
      const error = withStack(new TypeError(message), stack);
      expect(frontendErrorReport(error), stack).toEqual({
        message: 'TypeError',
        stack: null,
        source: null,
      });
    }
  });

  it('reports a real error without its message, and nothing for a value that is not one', () => {
    const report = frontendErrorReport(new TypeError(`Cannot read ${SECRET}`));
    expect(report.message).toBe('TypeError');
    expect(report.stack?.split('\n').every((line) => /^at \S+:\d+:\d+$/.test(line))).toBe(true);
    expect(report.stack).toContain('errorReport.test.ts');
    expect(report.source).toBe(report.stack?.split('\n')[0]?.slice(3));
    expect(JSON.stringify(report)).not.toContain('secret');

    expect(frontendErrorReport(SECRET)).toEqual({ message: 'Error', stack: null, source: null });
    const stackless = new Error(SECRET);
    Object.defineProperty(stackless, 'stack', { value: 42 });
    expect(frontendErrorReport(stackless)).toEqual({ message: 'Error', stack: null, source: null });
  });

  it('sends at most 32 frames', () => {
    const frames = Array.from(
      { length: 40 },
      (_, i) => `    at f${String(i)} (${CHUNK}:${String(i + 1)}:1)`,
    );
    const error = withStack(new Error('x'), ['Error: x', ...frames].join('\n'));
    const positions = stackPositions(error);
    expect(positions).toHaveLength(32);
    expect(positions.at(-1)).toBe(`${CHUNK}:32:1`);
  });
});

describe('componentFrames', () => {
  it('keeps only the file name, line and column of a script location', () => {
    expect(scriptLocation(`${CHUNK}:1:2345`)).toBe('index-D3f9Kq_x.js:1:2345');
    expect(scriptLocation('http://localhost:1420/src/App.tsx?t=17&q=secret:12:7')).toBe(
      'App.tsx:12:7',
    );
    expect(scriptLocation('C:\\Develop\\app\\src\\App.tsx:3:1')).toBe('App.tsx:3:1');
    expect(scriptLocation('<anonymous>')).toBeNull();
    expect(scriptLocation('http://x/secret phrase.txt:1:2')).toBeNull();
    expect(scriptLocation('index.js')).toBeNull();
  });

  it('reads component names from V8, JavaScriptCore and older React component stacks', () => {
    const v8 = [
      '',
      '    at Boom (http://localhost:1420/src/App.tsx?t=1:10:5)',
      '    at div (<anonymous>)',
      '    at new ErrorBoundary (http://tauri.localhost/assets/index-4f2a.js:40:12)',
      `    at ${SECRET} (http://x/a.js:1:1)`,
    ].join('\n');
    expect(componentFrames(v8)).toBe(
      ['Boom@App.tsx:10:5', 'div', 'ErrorBoundary@index-4f2a.js:40:12'].join('\n'),
    );
    expect(componentFrames('Boom@tauri://localhost/assets/index.js:1:2\ndiv@\n')).toBe(
      'Boom@index.js:1:2\ndiv',
    );
    expect(componentFrames('\n    in Boom (at App.tsx:12)\n    in main')).toBe('Boom\nmain');
    expect(componentFrames('')).toBeNull();
    expect(componentFrames(undefined)).toBeNull();
  });
});
