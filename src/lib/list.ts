/** Returns a copy of `xs` with the item at `from` moved to index `to`. */
export function moveItem<T>(xs: readonly T[], from: number, to: number): T[] {
  const out = [...xs];
  if (from < 0 || from >= out.length) return out;
  const [item] = out.splice(from, 1) as [T];
  out.splice(to, 0, item);
  return out;
}

/** Returns a copy of `record` without `key`, keeping the order of the other keys. */
export function withoutKey<T>(record: Readonly<Record<string, T>>, key: string): Record<string, T> {
  return Object.fromEntries(Object.entries(record).filter(([k]) => k !== key));
}
