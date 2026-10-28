// Hand-written mirror of the 0.4 IPC contract (docs/ipc-contract-0.4.md, crates/platform/src/health.rs).
// 0.5 replaces this file with tauri-specta output (B-46). The backend may add fields and enum values:
// every union below is open in practice, so code that switches on them needs a fallback branch.

export type HealthState = 'starting' | 'healthy' | 'degraded' | 'recovering' | 'failed';
export type SafeModeReason = 'crash_loop' | 'locked' | 'newer_schema' | 'no_valid_config';
export type IoOp =
  'read' | 'write' | 'rename' | 'create_dir' | 'remove' | 'open' | 'dialog' | 'other';
export type EntityKind = 'rule' | 'group' | 'revision' | 'file' | 'history' | 'window';

export type Component =
  | 'executor'
  | 'keyboard_hook'
  | 'mouse_hook'
  | 'raw_input'
  | 'supervisor'
  | 'focus_events'
  | 'clipboard'
  | 'protection';

export type ReasonCode =
  | 'ok'
  | 'dropped_busy'
  | 'cancelled'
  | 'paused'
  | 'engine_down'
  | 'focus_changed'
  | 'stale_input'
  | 'protected_control'
  | 'modifier_held'
  | 'inject_partial'
  | 'clipboard_busy'
  | 'clipboard_changed'
  | 'clipboard_format_skipped'
  | 'clipboard_too_large'
  | 'resolve_failed'
  | 'preflight_failed'
  | 'executor_panic'
  | 'executor_timeout'
  | 'hook_missing'
  | 'raw_input_missing'
  | 'hook_install_failed'
  | 'unsupported'
  | 'internal';

export interface ComponentHealth {
  component: Component;
  state: HealthState;
  reason: ReasonCode | null;
  since_ms: number;
}

export interface HealthSnapshot {
  state: HealthState;
  components: ComponentHealth[];
}

export type PauseReason =
  | 'manual'
  | 'timed'
  | 'fullscreen'
  | 'recording'
  | 'safe_mode'
  | 'engine_down'
  | 'self_test'
  | 'platform';

export interface EngineState {
  listening: boolean;
  paused_reasons: PauseReason[];
  busy: boolean;
  current_process: string | null;
}

export type TriggerKind = 'immediate' | 'terminator' | 'manual';
export type Outcome = 'succeeded' | 'unknown' | 'failed' | 'skipped' | 'cancelled' | 'dropped_busy';

export interface StageTimings {
  queue_us: number | null;
  resolve_us: number | null;
  erase_us: number | null;
  output_us: number | null;
  restore_us: number | null;
  total_us: number | null;
}

export interface HistoryEntry {
  /** Sequence number of the history ring; not the global event seq. */
  seq: number;
  at_ms: number;
  rule_id: string;
  trigger: TriggerKind;
  outcome: Outcome;
  reason: ReasonCode;
  process: string | null;
  completed_steps: number;
  side_effects: boolean;
  skipped_formats?: number[];
  timings: StageTimings;
}

export type Severity = 'info' | 'warning' | 'error';

/** `kind` is open: unknown kinds are shown verbatim with a generic text. */
export interface StartupProblem {
  kind: string;
  severity: Severity;
  args: Record<string, unknown>;
}

export interface Diagnostic {
  code: string;
  message: string;
  path: string;
  span?: { line: number; column: number; length: number } | null;
  severity: 'error' | 'warning';
}

// ---- events (§3) ----

export interface EngineHealthEvent extends HealthSnapshot {
  seq: number;
}
export interface EngineStateEvent extends EngineState {
  seq: number;
}
export interface ExecutionResultEvent {
  seq: number;
  entry: HistoryEntry;
  /** Entries merged into this push by rate limiting; details via `list_history`. */
  suppressed: number;
}
export interface StartupProblemEvent extends StartupProblem {
  seq: number;
}
/**
 * Sent whenever the store revision or safe mode changes, whoever caused it (this window's own
 * save/rollback/import too, or a background Locked retry that succeeded). Not rate limited, never
 * repeated for unchanged values. An older backend never sends it.
 */
export interface StoreStateEvent {
  seq: number;
  revision: number;
  safe_mode: SafeModeReason | null;
}

export interface EventPayloads {
  'engine-health': EngineHealthEvent;
  'engine-state': EngineStateEvent;
  'execution-result': ExecutionResultEvent;
  'startup-problem': StartupProblemEvent;
  'store-state': StoreStateEvent;
}
export type EventName = keyof EventPayloads;
export const EVENT_NAMES: readonly EventName[] = [
  'engine-health',
  'engine-state',
  'execution-result',
  'startup-problem',
  'store-state',
];

// ---- 0.4 commands (§5) ----

export interface RuntimeSnapshot {
  /** Global event seq at the time of the snapshot: the new baseline. */
  seq: number;
  health: HealthSnapshot;
  state: EngineState;
  revision: number;
  /** Latest history seq; `list_history(since_seq)` continues from here. */
  history_seq: number;
  safe_mode: SafeModeReason | null;
}

export type CapabilityStatus = 'available' | 'unavailable' | 'experimental' | 'degraded';
/**
 * One row of get_capabilities / load_state.capabilities, as crates/platform/src/capabilities.rs
 * serializes it. Kept looser than the Rust type on purpose (lib/capabilities.ts reads it):
 * - `status` is always sent by a 0.4 backend; a 0.3 one sends only `available`.
 * - `reason` is untagged in Rust: a ReasonCode (`unsupported`, or one lowered from health) or a
 *   PlatformReason (`experimental_platform`, `permission_missing`, `not_implemented`), and null
 *   exactly when `status` is `available`. A 0.3 backend sends prose; an unknown code is 「其他」.
 * - `available` is `status !== 'unavailable'`.
 */
export interface Capability {
  id: string;
  status?: CapabilityStatus;
  reason: string | null;
  available: boolean;
}

export type Redaction = 'summary' | 'full';
export type LogLevel = 'error' | 'warn' | 'info' | 'debug' | 'trace';

export interface SelfTestCheck {
  name: string;
  passed: boolean;
  reason: ReasonCode | null;
  elapsed_ms: number;
}
export interface SelfTestResult {
  passed: boolean;
  checks: SelfTestCheck[];
}

export interface LastMiss {
  seq: number;
  at_ms: number;
  rule_id: string;
  outcome: Outcome;
  reason: ReasonCode;
}
