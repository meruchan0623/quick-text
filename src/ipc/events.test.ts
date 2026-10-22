import { act, createElement, StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it } from 'vitest';
import {
  HISTORY_LIMIT,
  RETRY_CAP_MS,
  retryDelay,
  RuntimeSync,
  SeqLedger,
  useRuntime,
  type RuntimeIo,
  type Schedule,
} from './events';
import { createMockTransport, MockBackend, type MockOptions } from './mock';
import { BackendUnavailableError } from './transport';
import { EVENT_NAMES, type RuntimeSnapshot } from './types';

describe('SeqLedger', () => {
  it('applies consecutive events', () => {
    const ledger = new SeqLedger();
    ledger.install(10);
    expect(ledger.accept(11)).toBe('apply');
    expect(ledger.accept(12)).toBe('apply');
    expect(ledger.mark).toBe(12);
  });

  it('drops duplicates and out-of-order events', () => {
    const ledger = new SeqLedger();
    ledger.install(10);
    expect(ledger.accept(10)).toBe('drop');
    expect(ledger.accept(9)).toBe('drop');
    expect(ledger.accept(11)).toBe('apply');
    expect(ledger.accept(11)).toBe('drop');
    expect(ledger.mark).toBe(11);
  });

  it('reports a gap without moving past it', () => {
    const ledger = new SeqLedger();
    ledger.install(10);
    expect(ledger.accept(13)).toBe('gap');
    expect(ledger.mark).toBe(10);
    expect(ledger.accept(12)).toBe('gap');
    expect(ledger.accept(11)).toBe('apply');
    expect(ledger.accept(12)).toBe('apply');
    expect(ledger.accept(13)).toBe('apply');
  });

  it('treats any event without a baseline as a gap', () => {
    expect(new SeqLedger().accept(1)).toBe('gap');
  });

  it('installs a snapshot only at or above the mark', () => {
    const ledger = new SeqLedger();
    expect(ledger.install(10)).toBe(true);
    expect(ledger.accept(11)).toBe('apply');
    // An older snapshot changes nothing: covered events stay covered.
    expect(ledger.install(9)).toBe(false);
    expect(ledger.mark).toBe(11);
    expect(ledger.accept(10)).toBe('drop');
    expect(ledger.accept(11)).toBe('drop');
    expect(ledger.install(11)).toBe(true);
    expect(ledger.install(15)).toBe(true);
    expect(ledger.accept(12)).toBe('drop');
    expect(ledger.accept(16)).toBe('apply');
  });

  it('backs off retries up to a cap', () => {
    expect([1, 2, 3, 4].map(retryDelay)).toEqual([250, 500, 1000, 2000]);
    expect(retryDelay(10)).toBe(RETRY_CAP_MS);
    expect(retryDelay(1000)).toBe(RETRY_CAP_MS);
  });
});

/** Retries run only when a test fires them. */
function manualClock() {
  const timers: { run: () => void; ms: number; cancelled: boolean }[] = [];
  const schedule: Schedule = (run, ms) => {
    const timer = { run, ms, cancelled: false };
    timers.push(timer);
    return () => {
      timer.cancelled = true;
    };
  };
  const live = () => timers.filter((t) => !t.cancelled);
  return {
    schedule,
    /** Delays of the retries waiting to run. */
    waiting: () => live().map((t) => t.ms),
    /** Run every waiting retry. */
    fire: () => {
      const due = live();
      timers.length = 0;
      for (const timer of due) timer.run();
    },
  };
}

const syncs: RuntimeSync[] = [];
afterEach(() => {
  for (const sync of syncs.splice(0)) sync.dispose();
});

function setup(options: MockOptions = {}, io?: (backend: MockBackend) => RuntimeIo) {
  const backend = new MockBackend({ rules: 0, ...options });
  const focus = new EventTarget();
  const clock = manualClock();
  const sync = new RuntimeSync(
    io ? io(backend) : createMockTransport(backend),
    focus,
    clock.schedule,
  );
  syncs.push(sync);
  const snapshots = () => backend.calls.filter((c) => c.command === 'get_runtime_snapshot').length;
  return { backend, focus, sync, snapshots, clock };
}

/**
 * Mock IPC whose snapshot responses can be held back (they are taken when asked for, like the
 * backend does, and delivered on `release`) and rewritten.
 */
function controlled() {
  const gates: (() => void)[] = [];
  let holding = false;
  let rewrite: ((snapshot: RuntimeSnapshot) => RuntimeSnapshot) | null = null;
  const io = (backend: MockBackend): RuntimeIo => {
    const mock = createMockTransport(backend);
    return {
      listen: (event, handler) => mock.listen(event, handler),
      invoke: async (command, args) => {
        let result = await mock.invoke<unknown>(command, args);
        if (command === 'get_runtime_snapshot') {
          if (rewrite) result = rewrite(result as RuntimeSnapshot);
          rewrite = null;
          if (holding) await new Promise<void>((resolve) => gates.push(resolve));
        }
        return result as never;
      },
    };
  };
  return {
    io,
    hold: () => {
      holding = true;
    },
    release: () => {
      holding = false;
      for (const open of gates.splice(0)) open();
    },
    rewriteNext: (fn: (snapshot: RuntimeSnapshot) => RuntimeSnapshot) => {
      rewrite = fn;
    },
  };
}

/** Let queued promise callbacks (mock IPC round trips) run. */
const settle = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

describe('RuntimeSync', () => {
  it('starts from one snapshot plus history and startup problems', async () => {
    const { backend, sync, snapshots } = setup({
      health: 'degraded',
      safeMode: 'locked',
    });
    backend.recordExecution({ rule_id: 'a' });
    await sync.start();
    const view = sync.getView();
    expect(view.status).toBe('ready');
    expect(view.seq).toBe(backend.seq);
    expect(view.health?.state).toBe('degraded');
    expect(view.engine?.paused_reasons).toEqual(['safe_mode']);
    expect(view.safeMode).toBe('locked');
    expect(view.revision).toBe(1);
    expect(view.problems).toEqual([
      { kind: 'safe_mode', severity: 'error', args: { reason: 'locked' } },
    ]);
    expect(view.history.map((r) => r.entry.rule_id)).toEqual(['a']);
    expect(view.historyAvailable).toBe(true);
    expect(snapshots()).toBe(1);
    await sync.start();
    expect(snapshots()).toBe(1);
  });

  it('applies consecutive events without asking for a snapshot', async () => {
    const { backend, sync, snapshots } = setup();
    await sync.start();
    backend.setHealth('degraded');
    backend.setBusy(true);
    backend.recordExecution({ outcome: 'dropped_busy', reason: 'dropped_busy' });
    await settle();
    const view = sync.getView();
    expect(view.health?.state).toBe('degraded');
    expect(view.engine?.busy).toBe(true);
    expect(view.history[0]?.entry.outcome).toBe('dropped_busy');
    expect(view.seq).toBe(backend.seq);
    expect(snapshots()).toBe(1);
  });

  it('drops duplicate and out-of-order events', async () => {
    const { backend, sync, snapshots } = setup();
    await sync.start();
    backend.setBusy(true); // seq n
    const seq = backend.seq;
    backend.deliver('engine-state', {
      seq,
      listening: false,
      paused_reasons: ['manual'],
      busy: false,
      current_process: null,
    });
    backend.deliver('engine-health', { seq: seq - 1, state: 'failed', components: [] });
    await settle();
    expect(sync.getView().engine?.busy).toBe(true);
    expect(sync.getView().engine?.paused_reasons).toEqual([]);
    expect(sync.getView().health?.state).toBe('healthy');
    expect(snapshots()).toBe(1);
  });

  it('realigns with one snapshot when a seq is skipped', async () => {
    const { backend, sync, snapshots } = setup();
    await sync.start();
    backend.skipSeq(2); // two events never arrive…
    backend.engine = { ...backend.engine, current_process: 'code.exe' }; // …one of them changed this
    backend.setHealth('degraded'); // arrives with a gap
    await settle();
    await settle();
    expect(snapshots()).toBe(2);
    const view = sync.getView();
    expect(view.health?.state).toBe('degraded');
    expect(view.engine?.current_process).toBe('code.exe');
    expect(view.seq).toBe(backend.seq);
  });

  it('realigns when the window regains focus', async () => {
    const { backend, focus, sync, snapshots } = setup();
    await sync.start();
    backend.health = { ...backend.health, state: 'recovering' }; // changed without an event
    focus.dispatchEvent(new Event('focus'));
    await settle();
    await settle();
    expect(snapshots()).toBe(2);
    expect(sync.getView().health?.state).toBe('recovering');
  });

  it('buffers events during a snapshot and replays only newer ones', async () => {
    let release: () => void = () => undefined;
    let gate: Promise<void> | null = null;
    const { backend, focus, sync } = setup({}, (b) => {
      const mock = createMockTransport(b);
      return {
        listen: (event, handler) => mock.listen(event, handler),
        invoke: async (command, args) => {
          const result = await mock.invoke(command, args);
          if (command === 'get_runtime_snapshot' && gate) await gate;
          return result as never;
        },
      };
    });
    await sync.start();
    gate = new Promise<void>((resolve) => (release = resolve));
    focus.dispatchEvent(new Event('focus'));
    await settle(); // snapshot taken at the current seq, response held back
    const snapshotSeq = backend.seq;
    backend.deliver('engine-health', { seq: snapshotSeq, state: 'failed', components: [] }); // stale
    backend.setBusy(true); // newer than the snapshot
    expect(sync.getView().engine?.busy).toBe(false); // held until the snapshot lands
    release();
    await settle();
    await settle();
    const view = sync.getView();
    expect(view.health?.state).toBe('healthy');
    expect(view.engine?.busy).toBe(true);
    expect(view.seq).toBe(snapshotSeq + 1);
  });

  it('shows 「另有 N 条」 until list_history fills in the details', async () => {
    const { backend, focus, sync } = setup();
    await sync.start();
    backend.recordExecution({ rule_id: 'x' }, 3);
    await settle();
    expect(sync.getView().history).toHaveLength(1);
    expect(sync.getView().history[0]?.suppressed).toBe(3);
    focus.dispatchEvent(new Event('focus'));
    await settle();
    await settle();
    const rows = sync.getView().history;
    expect(rows).toHaveLength(4);
    expect(rows.every((r) => r.suppressed === 0)).toBe(true);
    expect(rows.map((r) => r.entry.seq)).toEqual([4, 3, 2, 1]);
    // A contiguous push afterwards extends the known range, so the next fetch starts after it.
    backend.recordExecution();
    await settle();
    await sync.resync();
    const listCalls = backend.calls.filter((c) => c.command === 'list_history');
    expect(listCalls.map((c) => c.args.since_seq)).toEqual([null, 0, 5]);
  });

  it(`keeps at most ${HISTORY_LIMIT} rows, newest first`, async () => {
    const { backend, sync } = setup();
    for (let i = 0; i < HISTORY_LIMIT + 5; i += 1) backend.recordExecution();
    await sync.start();
    const rows = sync.getView().history;
    expect(rows).toHaveLength(HISTORY_LIMIT);
    expect(rows[0]?.entry.seq).toBe(HISTORY_LIMIT + 5);
  });

  it('merges startup problems from events and the command without duplicates', async () => {
    const { backend, sync } = setup();
    await sync.start();
    backend.addProblem({
      kind: 'rules_skipped',
      severity: 'warning',
      args: { count: 1, ids: ['r'] },
    });
    await settle();
    await sync.resync();
    expect(sync.getView().problems).toHaveLength(1);
  });

  it('marks a 0.3 backend as unsupported and stops resyncing', async () => {
    const { backend, focus, sync, snapshots, clock } = setup({ legacy: true });
    await sync.start();
    expect(sync.getView().status).toBe('unsupported');
    focus.dispatchEvent(new Event('focus'));
    await settle();
    expect(snapshots()).toBe(1);
    expect(backend.calls.map((c) => c.command)).toEqual(['get_runtime_snapshot']);
    expect(clock.waiting()).toEqual([]);
  });

  it('keeps connecting after a failed snapshot and retries on focus', async () => {
    const { backend, focus, sync, snapshots, clock } = setup();
    backend.failures.set('get_runtime_snapshot', { code: 'internal', detail: { trace_id: 't' } });
    await sync.start();
    expect(sync.getView().status).toBe('connecting');
    expect(clock.waiting()).toEqual([250]);
    // Focus asks right away and takes the place of the scheduled retry.
    focus.dispatchEvent(new Event('focus'));
    await settle();
    await settle();
    expect(sync.getView().status).toBe('ready');
    expect(clock.waiting()).toEqual([]);
    expect(snapshots()).toBe(2);
  });

  it('retries failed snapshots with a growing delay until one lands', async () => {
    const { backend, sync, clock } = setup();
    const failure = { code: 'internal', detail: { trace_id: 't' } };
    backend.failures.set('get_runtime_snapshot', failure);
    await sync.start();
    expect(clock.waiting()).toEqual([250]);
    backend.failures.set('get_runtime_snapshot', failure);
    clock.fire();
    await settle();
    expect(clock.waiting()).toEqual([500]);
    clock.fire();
    await settle();
    await settle();
    expect(sync.getView().status).toBe('ready');
    expect(clock.waiting()).toEqual([]);
  });

  it('does not keep retrying when there is no desktop backend at all', async () => {
    const focus = new EventTarget();
    const clock = manualClock();
    const asked: string[] = [];
    const nothing: RuntimeIo = {
      listen: () => Promise.reject(new BackendUnavailableError()),
      invoke: (command) => {
        asked.push(command);
        return Promise.reject(new BackendUnavailableError());
      },
    };
    const sync = new RuntimeSync(nothing, focus, clock.schedule);
    syncs.push(sync);
    await sync.start();
    expect(sync.getView().status).toBe('connecting');
    expect(clock.waiting()).toEqual([]);
    // Focus still asks once (cheap, and harmless), but schedules nothing either.
    focus.dispatchEvent(new Event('focus'));
    await settle();
    expect(clock.waiting()).toEqual([]);
    expect(asked).toEqual(['get_runtime_snapshot', 'get_runtime_snapshot']);
  });
});

describe('RuntimeSync seq ledger (late, old and overlapping snapshots)', () => {
  it('asks again when an event sent after the snapshot was taken goes missing', async () => {
    const gate = controlled();
    const { backend, focus, sync, snapshots, clock } = setup({}, gate.io);
    await sync.start();
    gate.hold();
    focus.dispatchEvent(new Event('focus'));
    await settle(); // snapshot taken at S, its response held back
    const taken = backend.seq;
    backend.skipSeq(1); // S+1 is lost…
    backend.setBusy(true); // …S+2 arrives while the snapshot is in flight
    gate.release();
    await settle();
    await settle();
    // The snapshot is installed, but S+2 does not continue it: it waits for the next snapshot.
    let view = sync.getView();
    expect(view.seq).toBe(taken);
    expect(view.engine?.busy).toBe(false);
    expect(snapshots()).toBe(2);
    expect(clock.waiting()).toEqual([250]);
    clock.fire();
    await settle();
    await settle();
    view = sync.getView();
    expect(snapshots()).toBe(3);
    expect(view.seq).toBe(backend.seq);
    expect(view.engine?.busy).toBe(true);
    expect(clock.waiting()).toEqual([]);
  });

  it('discards a snapshot older than the view and keeps dropping covered events', async () => {
    const gate = controlled();
    const { backend, focus, sync, snapshots, clock } = setup({}, gate.io);
    await sync.start();
    backend.setHealth('degraded'); // S+1 (health), S+2 (state): applied
    await settle();
    const mark = backend.seq;
    const lists = () => backend.calls.filter((c) => c.command === 'list_history').length;
    const listed = lists();
    // A snapshot from before those events, still saying healthy.
    gate.rewriteNext((s) => ({
      ...s,
      seq: mark - 2,
      health: { state: 'healthy', components: [] },
    }));
    focus.dispatchEvent(new Event('focus'));
    await settle();
    await settle();
    let view = sync.getView();
    expect(view.health?.state).toBe('degraded');
    expect(view.seq).toBe(mark);
    expect(lists()).toBe(listed); // discarded before fetching anything else
    expect(clock.waiting()).toEqual([250]);
    // S+1 again, now claiming failed: still covered, still dropped.
    backend.deliver('engine-health', { seq: mark - 1, state: 'failed', components: [] });
    // The next event still continues the view while the retry waits.
    backend.setBusy(true);
    await settle();
    view = sync.getView();
    expect(view.health?.state).toBe('degraded');
    expect(view.engine?.busy).toBe(true);
    expect(view.seq).toBe(mark + 1);
    // Another old one in a row: the delay doubles.
    gate.rewriteNext((s) => ({ ...s, seq: mark - 2 }));
    clock.fire();
    await settle();
    await settle();
    expect(clock.waiting()).toEqual([500]);
    expect(sync.getView().seq).toBe(mark + 1);
    // Focus takes the place of the waiting retry: one request, and the backoff is reset.
    focus.dispatchEvent(new Event('focus'));
    await settle();
    await settle();
    expect(clock.waiting()).toEqual([]);
    expect(snapshots()).toBe(4);
    expect(sync.getView().seq).toBe(backend.seq);
  });

  it('asks for one follow-up however many gaps arrive while a snapshot is in flight', async () => {
    const gate = controlled();
    const { backend, focus, sync, snapshots, clock } = setup({}, gate.io);
    await sync.start();
    gate.hold();
    focus.dispatchEvent(new Event('focus'));
    await settle();
    backend.skipSeq(1);
    backend.setBusy(true);
    backend.skipSeq(3);
    backend.setHealth('degraded');
    backend.skipSeq(1);
    backend.recordExecution({ rule_id: 'late' });
    gate.release();
    await settle();
    await settle();
    expect(snapshots()).toBe(2);
    expect(clock.waiting()).toEqual([250]);
    // More gaps while the follow-up waits do not ask again.
    backend.skipSeq(2);
    backend.setBusy(false);
    await settle();
    expect(snapshots()).toBe(2);
    expect(clock.waiting()).toEqual([250]);
    clock.fire();
    await settle();
    await settle();
    expect(snapshots()).toBe(3);
    expect(clock.waiting()).toEqual([]);
    const view = sync.getView();
    expect(view.seq).toBe(backend.seq);
    expect(view.health?.state).toBe('degraded');
    expect(view.engine?.busy).toBe(false);
    expect(view.history.map((r) => r.entry.rule_id)).toContain('late');
  });

  it('sends one request for a gap and a focus together', async () => {
    const { backend, focus, sync, snapshots, clock } = setup();
    await sync.start();
    backend.skipSeq(1);
    backend.setBusy(true); // gap: asks for a snapshot
    focus.dispatchEvent(new Event('focus')); // shares it
    focus.dispatchEvent(new Event('focus'));
    await settle();
    await settle();
    expect(snapshots()).toBe(2);
    expect(sync.getView().seq).toBe(backend.seq);
    expect(sync.getView().engine?.busy).toBe(true);
    // The other way round: the gap arrives while the focus snapshot is in flight and is covered.
    focus.dispatchEvent(new Event('focus'));
    backend.skipSeq(1);
    backend.setBusy(false);
    await settle();
    await settle();
    expect(snapshots()).toBe(3);
    expect(sync.getView().seq).toBe(backend.seq);
    expect(sync.getView().engine?.busy).toBe(false);
    expect(clock.waiting()).toEqual([]);
  });
});

describe('useRuntime', () => {
  it('re-renders on pushed events', async () => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    const { backend, sync } = setup();
    function Probe() {
      const view = useRuntime(sync);
      return createElement('output', null, `${view.status}:${view.health?.state ?? '-'}`);
    }
    const host = document.createElement('div');
    const root = createRoot(host);
    await act(async () => {
      root.render(createElement(Probe));
      await settle();
    });
    expect(host.textContent).toBe('ready:healthy');
    await act(async () => {
      backend.setHealth('failed');
      await settle();
    });
    expect(host.textContent).toBe('ready:failed');
    act(() => root.unmount());
  });

  it('subscribes and asks once when StrictMode mounts twice', async () => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    const listened: string[] = [];
    const { backend, sync, snapshots } = setup({}, (b) => {
      const mock = createMockTransport(b);
      return {
        invoke: (command, args) => mock.invoke(command, args),
        listen: (event, handler) => {
          listened.push(event);
          return mock.listen(event, handler);
        },
      };
    });
    let renders = 0;
    function Probe() {
      const view = useRuntime(sync);
      renders += 1;
      return createElement('output', null, `${view.status}:${String(view.history.length)}`);
    }
    const host = document.createElement('div');
    const root = createRoot(host);
    await act(async () => {
      root.render(createElement(StrictMode, null, createElement(Probe)));
      await settle();
      await settle();
    });
    expect(renders).toBeGreaterThan(1); // StrictMode did render (and mount) twice
    expect(host.textContent).toBe('ready:0');
    expect(listened).toEqual([...EVENT_NAMES]);
    expect(snapshots()).toBe(1);
    // Pushed events arrive through that one subscription, without another snapshot.
    await act(async () => {
      backend.recordExecution();
      await settle();
    });
    expect(host.textContent).toBe('ready:1');
    expect(snapshots()).toBe(1);
    act(() => root.unmount());
  });
});
