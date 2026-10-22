// The last check before anything goes over IPC (Codex round 5): no number JSON cannot carry. JSON
// has no NaN or ±Infinity; JSON.stringify writes them as `null`, which the backend reads as a valid
// value (`serde_json::Value`, an `Option`), so the value would change silently on the way.
//
// It also refuses the editor's marker for invalid number text (InvalidNumber in
// src/lib/numberFields.ts): the marker always carries a NaN for this reason. The editor's own
// checks refuse such values first, with a message at the field; this check holds for every
// command, whatever path its arguments took.
//
// Every save sends the whole config through here, with whatever it keeps as it is: unknown fields
// and unreadable entries can hold arrays of any size, at any depth (Codex round 6). So the walk
// takes no recursion and no call with one argument per element (`push(...items)` throws a
// RangeError past some 100 000 arguments): it keeps one frame per open array or object, and builds
// a path string only for a container it enters or for the number it reports.

interface Frame {
  readonly path: string;
  /** The array's items, or the object's values. */
  readonly values: readonly unknown[];
  /** The object's keys, in the order of `values`; `null` for an array (the key is the index). */
  readonly keys: readonly string[] | null;
  /** The position of the next child to look at. */
  next: number;
}

const frame = (node: object, path: string): Frame =>
  Array.isArray(node)
    ? { path, values: node, keys: null, next: 0 }
    : { path, values: Object.values(node), keys: Object.keys(node), next: 0 };

function childPath(parent: Frame, index: number): string {
  const key = parent.keys?.[index];
  if (key === undefined) return `${parent.path}[${String(index)}]`;
  return parent.path ? `${parent.path}.${key}` : key;
}

/** The path of the first number in `value` that JSON cannot carry, or `null` when there is none. */
export function unsendablePath(value: unknown, root = ''): string | null {
  if (typeof value === 'number') return Number.isFinite(value) ? null : root;
  if (typeof value !== 'object' || value === null) return null;
  const seen = new Set<object>([value]);
  const stack: Frame[] = [frame(value, root)];
  for (let top = stack.at(-1); top; top = stack.at(-1)) {
    if (top.next >= top.values.length) {
      stack.pop();
      continue;
    }
    const index = top.next++;
    const child = top.values[index];
    if (typeof child === 'number') {
      if (!Number.isFinite(child)) return childPath(top, index);
    } else if (typeof child === 'object' && child !== null && !seen.has(child)) {
      // Depth first, children in order: the first problem in document order is the one reported.
      seen.add(child);
      stack.push(frame(child, childPath(top, index)));
    }
  }
  return null;
}

/** A command not sent because its arguments hold a number JSON cannot carry. */
export class UnsendableArgsError extends Error {
  override readonly name = 'UnsendableArgsError';
  constructor(
    readonly command: string,
    readonly path: string,
  ) {
    super(`要发送的内容含有无效的数值（${path}），已停止发送；已填写的内容会保留。`);
  }
}
