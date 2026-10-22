// The last check before anything goes over IPC (Codex round 5): no number JSON cannot carry. JSON
// has no NaN or ±Infinity; JSON.stringify writes them as `null`, which the backend reads as a valid
// value (`serde_json::Value`, an `Option`), so the value would change silently on the way.
//
// It also refuses the editor's marker for invalid number text (InvalidNumber in
// src/lib/numberFields.ts): the marker always carries a NaN for this reason. The editor's own
// checks refuse such values first, with a message at the field; this check holds for every
// command, whatever path its arguments took.

/** The path of the first number in `value` that JSON cannot carry, or `null` when there is none. */
export function unsendablePath(value: unknown, root = ''): string | null {
  const pending: [unknown, string][] = [[value, root]];
  const seen = new Set<object>();
  for (let next = pending.pop(); next; next = pending.pop()) {
    const [item, path] = next;
    if (typeof item === 'number') {
      if (!Number.isFinite(item)) return path;
    } else if (typeof item === 'object' && item !== null && !seen.has(item)) {
      seen.add(item);
      const entries = Array.isArray(item)
        ? item.map((child, i): [unknown, string] => [child, `${path}[${String(i)}]`])
        : Object.entries(item).map(([key, child]): [unknown, string] => [
            child,
            path ? `${path}.${key}` : key,
          ]);
      // Reversed onto the stack, so the first problem in document order is the one reported.
      pending.push(...entries.reverse());
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
