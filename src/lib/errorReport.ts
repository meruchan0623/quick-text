// What an error leaves of itself outside the page. Its message can quote rule content (a phrase, a
// path, a regex), so the message never goes anywhere.
//
// - `log_frontend_error` gets the error class and the frames of the error's own stack, the message
//   line(s) cut off, each as `at url:line:col`. The backend (S-08) keeps a frame only when `url` is
//   a chunk embedded in the binary, served from the app's origin under `/assets/`, at a line and
//   column inside that chunk, and logs `hash.js:line:col` of it; so the URL goes as the browser wrote
//   it, origin and path included, and the filtering is the backend's. Function names are left out
//   already here: the backend drops them anyway, and a name can come from data at run time (a
//   computed property key).
// - What a production build prints on the console (errorLog.ts) is the class and the component
//   names of React's component stack, each with `file.js:line:col` of the script it is in.

/** `log_frontend_error` arguments (docs/ipc-contract-0.4.md §5). */
export type FrontendErrorReport = {
  /** The error class only, e.g. `TypeError`; never the message. */
  message: string;
  /** One `at url:line:col` per frame of the error's stack, innermost first; no message line. */
  stack: string | null;
  /** `url:line:col` of the innermost frame. */
  source: string | null;
};

const MAX_FRAMES = 32;
const IDENTIFIER = /^[A-Za-z_$][\w$]{0,63}$/;
const SCRIPT = /^[\w.-]{1,128}\.(?:js|mjs|cjs|jsx|ts|tsx|mts|html)$/;
/** A frame's position as engines print it: `url:line:col`, without spaces or parentheses. */
const POSITION = /^[^\s()]+:\d+:\d+$/;

const isIdentifier = (name: string) => IDENTIFIER.test(name);
/** V8 qualifies methods (`Object.render`), so a function name may be identifiers joined by dots. */
const isFunctionName = (name: string) => name.length <= 64 && name.split('.').every(isIdentifier);

/** The class of a thrown value: `error.name` when it is an identifier, else `Error`. */
export function errorClass(error: unknown): string {
  return error instanceof Error && isIdentifier(error.name) ? error.name : 'Error';
}

/**
 * The position of one raw stack line, as written: V8 (`at fn (url:1:2)`, `at url:1:2`),
 * JavaScriptCore and SpiderMonkey (`fn@url:1:2`). Null for any other line and for a position that is
 * not `url:line:col` (`<anonymous>`, `native`, an eval origin).
 */
export function framePosition(raw: string): string | null {
  const line = raw.trim();
  let position: string;
  if (line.startsWith('at ')) {
    const rest = line.slice(3);
    position = /^.*? \((.*)\)$/.exec(rest)?.[1] ?? rest;
  } else {
    const at = line.lastIndexOf('@');
    if (at === -1) return null;
    position = line.slice(at + 1);
  }
  return POSITION.test(position) ? position : null;
}

/**
 * The stack without the message. A V8 stack starts with `${name}: ${message}` (just `${name}` for an
 * empty message), the message possibly spanning lines; a JavaScriptCore or SpiderMonkey stack has
 * frames only. A stack that shows the message, or a message, in any other way is not read at all,
 * so that text of a message can never pass as a frame.
 */
function withoutMessage(stack: string, message: string): string | null {
  const name = /^[\w$.]*/.exec(stack)?.[0] ?? '';
  const rest = stack.slice(name.length);
  const isEnd = (text: string) => text === '' || text.startsWith('\n');
  if (name !== '') {
    if (message !== '' && rest.startsWith(`: ${message}`)) {
      const frames = rest.slice(message.length + 2);
      if (isEnd(frames)) return frames;
    }
    if (isEnd(rest)) return rest;
  }
  const first = stack.split('\n', 1)[0] ?? '';
  const framesOnly = !first.includes(': ') && (message === '' || !stack.includes(message));
  return framesOnly ? stack : null;
}

/** The frame positions of an error's stack, innermost first (see `framePosition`). */
export function stackPositions(error: unknown): string[] {
  if (!(error instanceof Error) || typeof error.stack !== 'string') return [];
  const positions: string[] = [];
  for (const line of (withoutMessage(error.stack, error.message) ?? '').split('\n')) {
    const position = framePosition(line);
    if (position) positions.push(position);
    if (positions.length === MAX_FRAMES) break;
  }
  return positions;
}

/** The report of an error for `log_frontend_error`. */
export function frontendErrorReport(error: unknown): FrontendErrorReport {
  const positions = stackPositions(error);
  return {
    message: errorClass(error),
    stack: positions.length > 0 ? positions.map((position) => `at ${position}`).join('\n') : null,
    source: positions[0] ?? null,
  };
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
 * One component stack line: V8 (`at Component (url:1:2)`), JavaScriptCore / SpiderMonkey
 * (`Component@url:1:2`) or React's older `in Component (at file.js:12)`. Names that are not
 * identifiers are dropped.
 */
function componentFrame(raw: string): Frame | null {
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
    const frame = componentFrame(line);
    if (!frame?.name) continue;
    frames.push(frame.location ? `${frame.name}@${frame.location}` : frame.name);
    if (frames.length === MAX_FRAMES) break;
  }
  return frames.length > 0 ? frames.join('\n') : null;
}
