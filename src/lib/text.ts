// Text helpers for list-valued fields edited in a textarea (FE-12).

/** One entry per non-blank line, trimmed; accepts `\n` and `\r\n`. */
export const parseLines = (text: string): string[] =>
  text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line !== '');

export const sameItems = (a: readonly string[], b: readonly string[]): boolean =>
  a.length === b.length && a.every((item, i) => item === b[i]);
