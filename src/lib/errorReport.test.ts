import { describe, expect, it } from 'vitest';
import {
  componentFrames,
  errorClass,
  errorSource,
  frontendErrorReport,
  scriptLocation,
} from './errorReport';

const SECRET = ';secret-phrase';

describe('frontendErrorReport', () => {
  it('names the error class, never the message', () => {
    expect(errorClass(new TypeError(SECRET))).toBe('TypeError');
    const odd = new Error(SECRET);
    odd.name = `Bad ${SECRET}`;
    expect(errorClass(odd)).toBe('Error');
    expect(errorClass(SECRET)).toBe('Error');
    expect(errorClass(null)).toBe('Error');
  });

  it('keeps only the file name, line and column of a script location', () => {
    expect(scriptLocation('http://tauri.localhost/assets/index-4f2a.js:1:2345')).toBe(
      'index-4f2a.js:1:2345',
    );
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

  it('finds the source in the stack frames, not in the message', () => {
    const error = new TypeError(`at evil (http://x/${SECRET}.js:1:2)\nsecond line`);
    error.stack = [
      `TypeError: ${error.message}`,
      '    at Array.filter (<anonymous>)',
      '    at TerminatorList (http://tauri.localhost/assets/index-4f2a.js:9:99)',
    ].join('\n');
    expect(errorSource(error)).toBe('index-4f2a.js:9:99');
    // A stack that repeats the message in an unexpected form is not read at all.
    error.stack = `Uncaught ${error.message}\n    at X (http://tauri.localhost/assets/i.js:1:1)`;
    expect(errorSource(error)).toBeNull();
    expect(errorSource('not an error')).toBeNull();
  });

  it('carries nothing of the message or the stack text', () => {
    const error = new TypeError(`Cannot read ${SECRET}`);
    const report = frontendErrorReport(error, `\n    at ${SECRET}\n    at App (http://x/a.js:1:1)`);
    expect(report.message).toBe('TypeError');
    expect(report.stack).toBe('App@a.js:1:1');
    expect(JSON.stringify(report)).not.toContain('secret');
  });
});
