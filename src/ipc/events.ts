// Runtime events (B-44, S-19): the four broadcast events replace polling. Every payload carries the
// global `seq`; stale and duplicate events are dropped, and a gap or the window regaining focus
// realigns everything with one `get_runtime_snapshot` (docs/ipc-contract-0.4.md §3).
//
// The view is always a snapshot plus the events after it without a hole. The seq it reflects (the
// ledger's high-water mark) only moves forward: a snapshot older than the mark is discarded, and an
// event behind a gap waits until a snapshot covers the gap. At most one snapshot request is in
// flight; whatever else asks for one meanwhile shares it or, when it does not settle things (stale,
// failed, or followed by a gap), one retry scheduled with a capped backoff.
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

/**
 * Pure seq bookkeeping around a high-water mark that only moves forward: the view reflects a
 * snapshot taken at some seq plus every event after it, without a hole, up to `mark`.
 */
export class SeqLedger {
  #mark: number | null = null;

  /** The newest seq the view reflects; null before the first snapshot. */
  get mark(): number | null {
    return this.#mark;
  }

  /**
   * `drop`: already covered (a duplicate or an older event). `apply`: the next one; the mark moves
   * to it. `gap`: newer, but at least one event in between is missing; the mark stays where it is,
   * so the event has to wait for a snapshot. Without a baseline every event is a gap.
   */
  accept(seq: number): SeqVerdict {
    const mark = this.#mark;
    if (mark !== null && seq <= mark) return 'drop';
    if (mark === null || seq !== mark + 1) return 'gap';
    this.#mark = seq;
    return 'apply';
  }

  /**
   * Install a snapshot taken at `seq`. Refused (false, nothing changes) when it is older than the
   * mark: it would roll the view back and make events already covered acceptable again.
   */
  install(seq: number): boolean {
    if (this.#mark !== null && seq < this.#mark) return false;
    this.#mark = seq;
    return true;
  }
}

/** Runs `run` after `ms` milliseconds; returns a function that cancels it. */
export type Schedule = (run: () => void, ms: number) => () => void;

const later: Schedule = (run, ms) => {
  const id = setTimeout(run, ms);
  return () => {
    clearTimeout(id);
  };
};

export const RETRY_BASE_MS = 250;
export const RETRY_CAP_MS = 10_000;

/** Delay before retry `attempt` (1, 2, …) after snapshots that did not settle: doubling, capped. */
export const retryDelay = (attempt: number): number =>
  Math.min(RETRY_BASE_MS * 2 ** Math.max(attempt - 1, 0), RETRY_CAP_MS);

/**
 * Events kept while waiting for a snapshot. Past this the oldest are dropped: the snapshot covers
 * them, or the gap they leave asks for another one.
 */
const HELD_LIMIT = 1000;

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
 * create their own with a mock `RuntimeIo`, an `EventTarget` standing in for `window` and their
 * own `Schedule` for retries.
 */
export class RuntimeSync {
  readonly #io: RuntimeIo;
  readonly #focusTarget: EventTarget | null;
  readonly #schedule: Schedule;
  readonly #ledger = new SeqLedger();
  readonly #listeners = new Set<() => void>();
  #view: RuntimeView = initialView;
  #started = false;
  #disposed = false;
  #unlisten: Unlisten[] = [];
  /**
   * Events that cannot be applied yet: they arrived while a snapshot was in flight, or behind a
   * gap. Replayed in seq order on top of the next snapshot (or of the current view).
   */
  #held: AnyEvent[] = [];
  /** The one snapshot round trip in flight. */
  #inFlight: Promise<void> | null = null;
  /** Cancels the scheduled retry, while one is scheduled. */
  #retry: (() => void) | null = null;
  /** Snapshots in a row that did not settle; sets the retry delay. */
  #attempt = 0;
  /** History is known without holes up to this seq; `list_history` continues from here. */
  #historySeq: number | null = null;

  constructor(io: RuntimeIo, focusTarget: EventTarget | null = null, schedule: Schedule = later) {
    this.#io = io;
    this.#focusTarget = focusTarget;
    this.#schedule = schedule;
  }

  getView = (): RuntimeView => this.#view;

  subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  /** Idempotent: subscribe to every broadcast event, then take the first snapshot. */
  async start(): Promise<void> {
    if (this.#started) return this.#inFlight ?? Promise.resolve();
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
    this.#cancelRetry();
    this.#held = [];
    this.#focusTarget?.removeEventListener('focus', this.#onFocus);
    for (const unlisten of this.#unlisten.splice(0)) unlisten();
    this.#listeners.clear();
  }

  #onFocus = () => {
    if (this.#view.status !== 'unsupported') void this.resync();
  };

  /**
   * Realign with the backend now. Never more than one request at a time: a call while one is in
   * flight shares it, and a scheduled retry is folded into this one.
   */
  resync(): Promise<void> {
    if (this.#inFlight) return this.#inFlight;
    if (this.#disposed) return Promise.resolve();
    this.#cancelRetry();
    const request = this.#snapshot().finally(() => {
      this.#inFlight = null;
    });
    this.#inFlight = request;
    return request;
  }

  /** Ask again later, unless a retry is already scheduled (they coalesce). */
  #retryLater(): void {
    if (this.#disposed || this.#retry) return;
    this.#attempt += 1;
    this.#retry = this.#schedule(() => {
      this.#retry = null;
      void this.resync();
    }, retryDelay(this.#attempt));
  }

  #cancelRetry(): void {
    this.#retry?.();
    this.#retry = null;
  }

  async #snapshot(): Promise<void> {
    let snapshot: RuntimeSnapshot;
    try {
      snapshot = await this.#io.invoke<RuntimeSnapshot>('get_runtime_snapshot');
    } catch (e) {
      if (this.#disposed) return;
      if (isMissingCommand(e)) {
        // A 0.3 backend: no snapshots and no events, now or later.
        this.#held = [];
        this.#set({ ...this.#view, status: 'unsupported' });
      } else {
        // Keep the view (still consistent) and go on from it; try again later.
        this.#settle(false);
      }
      return;
    }
    if (this.#disposed) return;
    if (!this.#ledger.install(snapshot.seq)) {
      // Older than what the view already reflects: discard it, keep the view, try again later.
      this.#settle(false);
      return;
    }
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
    // (Disposed meanwhile: nobody listens any more, nothing is held and no retry is scheduled.)
    this.#set(view);
    this.#settle(true);
  }

  /**
   * End of a snapshot round trip: apply the held events that continue the mark. Settled only when
   * the snapshot was installed and no event is left behind a gap; otherwise ask again later.
   */
  #settle(installed: boolean): void {
    const gap = this.#replay();
    if (installed && !gap) this.#attempt = 0;
    else this.#retryLater();
  }

  /**
   * Apply held events in seq order while they continue the mark; covered ones are dropped. Stops at
   * the first gap and keeps the rest held; returns whether there was one.
   */
  #replay(): boolean {
    const held = this.#held.sort((a, b) => a.payload.seq - b.payload.seq);
    this.#held = [];
    for (const [index, event] of held.entries()) {
      const verdict = this.#ledger.accept(event.payload.seq);
      if (verdict === 'apply') this.#apply(event);
      else if (verdict === 'gap') {
        this.#held = held.slice(index);
        return true;
      }
    }
    return false;
  }

  #hold(event: AnyEvent): void {
    this.#held.push(event);
    if (this.#held.length > HELD_LIMIT) this.#held.splice(0, this.#held.length - HELD_LIMIT);
  }

  #receive(event: AnyEvent): void {
    if (this.#disposed || this.#view.status === 'unsupported') return;
    const mark = this.#ledger.mark;
    if (mark !== null && event.payload.seq <= mark) return; // a duplicate or an older event
    // A snapshot is on its way, or earlier events already wait behind a gap: wait with them.
    if (this.#inFlight || this.#held.length > 0) {
      this.#hold(event);
      return;
    }
    if (this.#ledger.accept(event.payload.seq) === 'apply') {
      this.#apply(event);
      return;
    }
    // Something was missed: hold this until a snapshot covers the hole. A scheduled retry will.
    this.#hold(event);
    if (!this.#retry) void this.resync();
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
