import { act, createElement } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it } from 'vitest';
import { HISTORY_LIMIT, RuntimeSync, SeqLedger, useRuntime, type RuntimeIo } from './events';
import { createMockTransport, MockBackend, type MockOptions } from './mock';

describe('SeqLedger', () => {
  it('applies consecutive events', () => {
    const ledger = new SeqLedger();
    ledger.rebase(10);
    expect(ledger.accept(11)).toBe('apply');
    expect(ledger.accept(12)).toBe('apply');
    expect(ledger.last).toBe(12);
  });

  it('drops duplicates and out-of-order events', () => {
    const ledger = new SeqLedger();
    ledger.rebase(10);
    expect(ledger.accept(10)).toBe('drop');
    expect(ledger.accept(9)).toBe('drop');
    expect(ledger.accept(11)).toBe('apply');
    expect(ledger.accept(11)).toBe('drop');
    expect(ledger.last).toBe(11);
  });

  it('reports a gap and moves past it', () => {
    const ledger = new SeqLedger();
    ledger.rebase(10);
    expect(ledger.accept(13)).toBe('gap');
    expect(ledger.last).toBe(13);
    expect(ledger.accept(12)).toBe('drop');
    expect(ledger.accept(14)).toBe('apply');
  });

  it('treats any event without a baseline as a gap', () => {
    expect(new SeqLedger().accept(1)).toBe('gap');
  });
});

const syncs: RuntimeSync[] = [];
afterEach(() => {
  for (const sync of syncs.splice(0)) sync.dispose();
});

function setup(options: MockOptions = {}, io?: (backend: MockBackend) => RuntimeIo) {
  const backend = new MockBackend({ rules: 0, ...options });
  const focus = new EventTarget();
  const sync = new RuntimeSync(io ? io(backend) : createMockTransport(backend), focus);
  syncs.push(sync);
  const snapshots = () => backend.calls.filter((c) => c.command === 'get_runtime_snapshot').length;
  return { backend, focus, sync, snapshots };
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
    const { backend, focus, sync, snapshots } = setup({ legacy: true });
    await sync.start();
    expect(sync.getView().status).toBe('unsupported');
    focus.dispatchEvent(new Event('focus'));
    await settle();
    expect(snapshots()).toBe(1);
    expect(backend.calls.map((c) => c.command)).toEqual(['get_runtime_snapshot']);
  });

  it('keeps connecting after a failed snapshot and retries on focus', async () => {
    const { backend, focus, sync } = setup();
    backend.failures.set('get_runtime_snapshot', { code: 'internal', detail: { trace_id: 't' } });
    await sync.start();
    expect(sync.getView().status).toBe('connecting');
    focus.dispatchEvent(new Event('focus'));
    await settle();
    await settle();
    expect(sync.getView().status).toBe('ready');
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
});
