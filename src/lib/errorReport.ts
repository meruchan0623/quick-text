// What the error boundary sends to `log_frontend_error`. An error's message and its stack's text can
// quote rule content (a phrase, a path, a regex), so none of it leaves the page: only the error
// class, the component names of React's component stack and the file names of the app's own
// scripts, with their line and column (the backend only keeps frames of that form, S-08).

/** `log_frontend_error` arguments (docs/ipc-contract-0.4.md §4). */
export type FrontendErrorReport = {
  /** The error class only, e.g. `TypeError`; never the message. */
  message: string;
  /** One `Component@file.js:line:col` (or bare `Component`) per component stack frame. */
  stack: string | null;
  /** `file.js:line:col` of the innermost frame in the app's scripts. */
  source: string | null;
};

const MAX_FRAMES = 32;
const IDENTIFIER = /^[A-Za-z_$][\w$]{0,63}$/;
const SCRIPT = /^[\w.-]{1,128}\.(?:js|mjs|cjs|jsx|ts|tsx|mts|html)$/;

const isIdentifier = (name: string) => IDENTIFIER.test(name);
/** V8 qualifies methods (`Object.render`), so a function name may be identifiers joined by dots. */
const isFunctionName = (name: string) => name.length <= 64 && name.split('.').every(isIdentifier);

/** The class of a thrown value: `error.name` when it is an identifier, else `Error`. */
export function errorClass(error: unknown): string {
  return error instanceof Error && isIdentifier(error.name) ? error.name : 'Error';
}

/**
 * `file.js:line:col` when `location` (`url:line:col`) points at a script: the scheme, directories,
 * query string and fragment are left out.
 */
export function scriptLocation(location: string): string | null {
  const found = /^(.*):(\d+):(\d+)$/.exec(location.trim());
  if (!found) return null;
  const [, url = '', line = '', column = ''] = found;
  const path = url.split(/[?#]/)[0] ?? '';
  const name = path.split(/[/\\]/).at(-1) ?? '';
  return SCRIPT.test(name) ? `${name}:${line}:${column}` : null;
}

interface Frame {
  name: string | null;
  location: string | null;
}

/**
 * One stack line: V8 (`at fn (url:1:2)`, `at url:1:2`), JavaScriptCore / SpiderMonkey
 * (`fn@url:1:2`) or React's older `in Component (at file.js:12)`. Names that are not identifiers
 * are dropped.
 */
function parseFrame(raw: string): Frame | null {
  const line = raw.trim();
  let name = '';
  let location = '';
  if (line.startsWith('at ')) {
    const rest = line.slice(3);
    const call = /^(.*?) \((.*)\)$/.exec(rest);
    if (call) [, name = '', location = ''] = call;
    else location = rest;
  } else if (line.startsWith('in ')) {
    name = line.slice(3).split(' ')[0] ?? '';
  } else if (line.includes('@')) {
    const at = line.indexOf('@');
    name = line.slice(0, at);
    location = line.slice(at + 1);
  } else {
    return null;
  }
  name = name.replace(/^async /, '').replace(/^new /, '');
  return {
    name: isFunctionName(name) ? name : null,
    location: location ? scriptLocation(location) : null,
  };
}

/** React's component stack as `Component@file.js:line:col` lines; nothing else of it. */
export function componentFrames(componentStack: string | null | undefined): string | null {
  const frames: string[] = [];
  for (const line of (componentStack ?? '').split('\n')) {
    const frame = parseFrame(line);
    if (!frame?.name) continue;
    frames.push(frame.location ? `${frame.name}@${frame.location}` : frame.name);
    if (frames.length === MAX_FRAMES) break;
  }
  return frames.length > 0 ? frames.join('\n') : null;
}

/** The innermost location of an error's stack that is one of the app's scripts. */
export function errorSource(error: unknown): string | null {
  if (!(error instanceof Error) || typeof error.stack !== 'string') return null;
  // A V8 stack starts with `${name}: ${message}`, which may span lines; skip it before reading
  // frames, so that text from the message can never pass as a frame.
  let stack = error.stack;
  const head = `${error.name}: ${error.message}`;
  if (stack.startsWith(head)) stack = stack.slice(head.length);
  else if (error.message !== '' && stack.includes(error.message)) return null;
  for (const line of stack.split('\n')) {
    const location = parseFrame(line)?.location;
    if (location) return location;
  }
  return null;
}

/** The report for an error caught while rendering. */
export function frontendErrorReport(
  error: unknown,
  componentStack: string | null | undefined,
): FrontendErrorReport {
  return {
    message: errorClass(error),
    stack: componentFrames(componentStack),
    source: errorSource(error),
  };
}
