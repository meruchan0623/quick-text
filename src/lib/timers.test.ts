import { afterEach, describe, expect, it, vi } from 'vitest';
import { every } from './timers';

describe('every', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('repeats until the returned function is called', () => {
    vi.useFakeTimers();
    const fn = vi.fn();
    const stop = every(100, fn);
    vi.advanceTimersByTime(350);
    expect(fn).toHaveBeenCalledTimes(3);
    stop();
    vi.advanceTimersByTime(1000);
    expect(fn).toHaveBeenCalledTimes(3);
  });
});
