// The only module allowed to use setInterval (see eslint.config.js). Prefer event
// subscriptions; use these helpers only for genuinely periodic work.

/** Calls `fn` every `ms` milliseconds; returns a function that stops it. */
export function every(ms: number, fn: () => void): () => void {
  const id = setInterval(fn, ms);
  return () => {
    clearInterval(id);
  };
}
