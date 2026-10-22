// Runtime events (B-44, S-19): the four broadcast events replace polling. Every payload carries the
// global `seq`; stale and duplicate events are dropped, and a gap or the window regaining focus
// realigns everything with one `get_runtime_snapshot` (docs/ipc-contract-0.4.md §3).
import { useEffect, useSyncExternalStore } from 'react';
import { isMissingCommand } from './errors';
import { invoke, listen, type Transport, type Unlisten } from './transport';
import type {
  EngineState,
  EventName,
  EventPayloads,
  HealthSnapshot,
  HistoryEntry,
  RuntimeSnapshot,
  SafeModeReason,
  StartupProblem,
} from './types';
import { EVENT_NAMES } from './types';

export type SeqVerdict = 'apply' | 'drop' | 'gap';

/** Pure seq bookkeeping: what to do with an incoming event. */
export class SeqLedger {
  #last: number | null = null;

  get last(): number | null {
    return this.#last;
  }

  /**
   * `drop`: not newer than what was already processed. `gap`: newer, but at least one event in
   * between was missed — apply it and realign. Without a baseline every event is a gap.
   */
  accept(seq: number): SeqVerdict {
    const last = this.#last;
    if (last !== null && seq <= last) return 'drop';
    this.#last = seq;
    return last !== null && seq === last + 1 ? 'apply' : 'gap';
  }

  /** New baseline from a snapshot. */
  rebase(seq: number): void {
    this.#last = seq;
  }
}

export interface HistoryRow {
  entry: HistoryEntry;
  /** Entries merged into this push by rate limiting (「另有 N 条」). */
  suppressed: number;
}

export interface RuntimeView {
  /**
   * `connecting` until the first snapshot; `unsupported` when the backend predates 0.4 (no
   * `get_runtime_snapshot`), in which case the legacy `load_state` fields are all there is.
   */
  status: 'connecting' | 'ready' | 'unsupported';
  seq: number | null;
  health: HealthSnapshot | null;
  engine: EngineState | null;
  revision: number | null;
  safeMode: SafeModeReason | null;
  problems: StartupProblem[];
  /** Newest first, at most HISTORY_LIMIT rows. */
  history: HistoryRow[];
  /** False when `list_history` is missing or failed; rows then only come from events. */
  historyAvailable: boolean;
}

export const HISTORY_LIMIT = 200;

export const initialView: RuntimeView = {
  status: 'connecting',
  seq: null,
  health: null,
  engine: null,
  revision: null,
  safeMode: null,
  problems: [],
  history: [],
  historyAvailable: false,
};

export type RuntimeIo = Pick<Transport, 'invoke' | 'listen'>;

type AnyEvent = { [K in EventName]: { name: K; payload: EventPayloads[K] } }[EventName];

const problemKey = (p: StartupProblem) => `${p.kind}:${JSON.stringify(p.args)}`;

function mergeProblems(current: StartupProblem[], incoming: StartupProblem[]): StartupProblem[] {
  const seen = new Set(current.map(problemKey));
  const added = incoming.filter((p) => !seen.has(problemKey(p)));
  return added.length > 0 ? [...current, ...added] : current;
}

function mergeHistory(current: HistoryRow[], incoming: HistoryRow[]): HistoryRow[] {
  if (incoming.length === 0) return current;
  const bySeq = new Map(current.map((row) => [row.entry.seq, row]));
  for (const row of incoming) bySeq.set(row.entry.seq, row);
  return [...bySeq.values()].sort((a, b) => b.entry.seq - a.entry.seq).slice(0, HISTORY_LIMIT);
}

/**
 * Keeps a RuntimeView in sync with the backend. One instance per app (see `useRuntime`); tests
 * create their own with a mock `RuntimeIo` and an `EventTarget` standing in for `window`.
 */
export class RuntimeSync {
  readonly #io: RuntimeIo;
  readonly #focusTarget: EventTarget | null;
  readonly #ledger = new SeqLedger();
  readonly #listeners = new Set<() => void>();
  #view: RuntimeView = initialView;
  #started = false;
  #disposed = false;
  #unlisten: Unlisten[] = [];
  /** Events that arrive while a snapshot is being fetched; replayed on top of it. */
  #buffer: AnyEvent[] | null = [];
  #resync: Promise<void> | null = null;
  /** History is known without holes up to this seq; `list_history` continues from here. */
  #historySeq: number | null = null;

  constructor(io: RuntimeIo, focusTarget: EventTarget | null = null) {
    this.#io = io;
    this.#focusTarget = focusTarget;
  }

  getView = (): RuntimeView => this.#view;

  subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  /** Idempotent: subscribe to every broadcast event, then take the first snapshot. */
  async start(): Promise<void> {
    if (this.#started) return this.#resync ?? Promise.resolve();
    this.#started = true;
    const subscriptions = await Promise.allSettled(
      EVENT_NAMES.map((name) =>
        this.#io.listen<EventPayloads[typeof name]>(name, (payload) => {
          this.#receive({ name, payload } as AnyEvent);
        }),
      ),
    );
    for (const s of subscriptions) if (s.status === 'fulfilled') this.#unlisten.push(s.value);
    if (this.#disposed) {
      this.dispose();
      return;
    }
    this.#focusTarget?.addEventListener('focus', this.#onFocus);
    return this.resync();
  }

  dispose(): void {
    this.#disposed = true;
    this.#focusTarget?.removeEventListener('focus', this.#onFocus);
    for (const unlisten of this.#unlisten.splice(0)) unlisten();
    this.#listeners.clear();
  }

  #onFocus = () => {
    if (this.#view.status !== 'unsupported') void this.resync();
  };

  /** Realign with the backend; concurrent calls share one round trip. */
  resync(): Promise<void> {
    this.#resync ??= this.#doResync().finally(() => {
      this.#resync = null;
    });
    return this.#resync;
  }

  async #doResync(): Promise<void> {
    this.#buffer ??= [];
    let snapshot: RuntimeSnapshot;
    try {
      snapshot = await this.#io.invoke<RuntimeSnapshot>('get_runtime_snapshot');
    } catch (e) {
      const buffered = this.#buffer;
      this.#buffer = null;
      if (isMissingCommand(e)) {
        this.#set({ ...this.#view, status: 'unsupported' });
      } else {
        // Keep what we have and apply what arrived meanwhile; the next focus or gap retries.
        for (const event of buffered)
          if (this.#ledger.accept(event.payload.seq) !== 'drop') this.#apply(event);
      }
      return;
    }
    this.#ledger.rebase(snapshot.seq);
    let view: RuntimeView = {
      ...this.#view,
      status: 'ready',
      seq: snapshot.seq,
      health: snapshot.health,
      engine: snapshot.state,
      revision: snapshot.revision,
      safeMode: snapshot.safe_mode,
    };
    const [history, problems] = await Promise.allSettled([
      this.#io.invoke<HistoryEntry[]>('list_history', { since_seq: this.#historySeq }),
      this.#io.invoke<StartupProblem[]>('startup_problems'),
    ]);
    if (history.status === 'fulfilled') {
      // The list is complete up to now, so earlier 「另有 N 条」 notes are covered by real rows.
      const rows = view.history.map((row) => (row.suppressed ? { ...row, suppressed: 0 } : row));
      view = {
        ...view,
        historyAvailable: true,
        history: mergeHistory(
          rows,
          history.value.map((entry) => ({ entry, suppressed: 0 })),
        ),
      };
      // Called after the snapshot, so the list covers everything up to at least its history_seq.
      this.#noteHistorySeq([...history.value.map((e) => e.seq), snapshot.history_seq]);
    }
    if (problems.status === 'fulfilled')
      view = { ...view, problems: mergeProblems(view.problems, problems.value) };
    this.#set(view);
    // Replay what arrived during the round trip; anything not newer than the snapshot is stale.
    const buffered = this.#buffer.sort((a, b) => a.payload.seq - b.payload.seq);
    this.#buffer = null;
    for (const event of buffered) {
      const last = this.#ledger.last;
      if (last !== null && event.payload.seq <= last) continue;
      this.#ledger.rebase(event.payload.seq);
      this.#apply(event);
    }
  }

  #receive(event: AnyEvent): void {
    if (this.#disposed) return;
    if (this.#buffer) {
      this.#buffer.push(event);
      return;
    }
    if (this.#view.status === 'unsupported') return;
    const verdict = this.#ledger.accept(event.payload.seq);
    if (verdict === 'drop') return;
    this.#apply(event);
    if (verdict === 'gap') void this.resync();
  }

  #noteHistorySeq(seqs: number[]): void {
    for (const seq of seqs)
      if (this.#historySeq === null || seq > this.#historySeq) this.#historySeq = seq;
  }

  #apply(event: AnyEvent): void {
    const view = this.#view;
    switch (event.name) {
      case 'engine-health': {
        const { state, components } = event.payload;
        this.#set({ ...view, seq: event.payload.seq, health: { state, components } });
        return;
      }
      case 'engine-state': {
        const { listening, paused_reasons, busy, current_process } = event.payload;
        this.#set({
          ...view,
          seq: event.payload.seq,
          engine: { listening, paused_reasons, busy, current_process },
        });
        return;
      }
      case 'execution-result': {
        const { entry, suppressed } = event.payload;
        // Only a contiguous push extends the complete range; rows merged away by rate limiting
        // (suppressed) are fetched by the next `list_history`.
        if (suppressed === 0 && this.#historySeq !== null && entry.seq === this.#historySeq + 1)
          this.#historySeq = entry.seq;
        this.#set({
          ...view,
          seq: event.payload.seq,
          history: mergeHistory(view.history, [{ entry, suppressed }]),
        });
        return;
      }
      case 'startup-problem': {
        const { kind, severity, args } = event.payload;
        this.#set({
          ...view,
          seq: event.payload.seq,
          problems: mergeProblems(view.problems, [{ kind, severity, args }]),
        });
        return;
      }
    }
  }

  #set(view: RuntimeView): void {
    this.#view = view;
    for (const listener of this.#listeners) listener();
  }
}

let shared: RuntimeSync | null = null;

/** The app-wide instance, bound to the real transport and the window's focus event. */
export function runtimeSync(): RuntimeSync {
  shared ??= new RuntimeSync({ invoke, listen }, typeof window === 'undefined' ? null : window);
  return shared;
}

/** Live runtime state for React. Starts the shared subscription on first use (app lifetime). */
export function useRuntime(sync: RuntimeSync = runtimeSync()): RuntimeView {
  useEffect(() => {
    void sync.start();
  }, [sync]);
  return useSyncExternalStore(sync.subscribe, sync.getView);
}
