// Beginner typing hint (设置 「新手提示」, src-tauri/src/hint.rs): the setting and the list layout.

/** `load_state.candidate_hint` and the answer of `set_candidate_hint`. */
export interface CandidateHint {
  enabled: boolean;
  duration_ms: number;
}

export const DEFAULT_HINT: CandidateHint = { enabled: false, duration_ms: 2000 };
export const HINT_SECONDS = { min: 1, max: 10, step: 0.5 } as const;

/** One rule in the list (`quicker_domain::HintItem`). */
export interface HintItem {
  rule_id: string;
  abbreviation: string;
  description: string;
  source: 'personal' | 'team';
  trigger: 'immediate' | 'terminator';
  preview: string;
  preview_is_label: boolean;
}

export interface HintMatch {
  typed: string;
  total: number;
  items: HintItem[];
}

/** `candidate-hint` event payload; `hint` null hides the list. */
export interface HintPayload {
  seq: number;
  hint: HintMatch | null;
  width: number;
  max_height: number;
  duration_ms: number;
}

export function isCandidateHint(value: unknown): value is CandidateHint {
  if (typeof value !== 'object' || value === null) return false;
  const v = value as Record<string, unknown>;
  return typeof v.enabled === 'boolean' && typeof v.duration_ms === 'number';
}

/** Seconds typed in the field, or null when not a valid duration. */
export function parseSeconds(text: string): number | null {
  if (text.trim() === '') return null;
  const value = Number(text);
  if (!Number.isFinite(value) || value < HINT_SECONDS.min || value > HINT_SECONDS.max) return null;
  return Math.round(value * 2) / 2;
}

export type Density = 'detail' | 'standard' | 'compact';

/** Approximate row heights in CSS pixels, matching src/hint.css. */
export const ROW_HEIGHT: Record<Density, number> = { detail: 118, standard: 58, compact: 34 };
export const HEADER_HEIGHT = 34;
export const MORE_HEIGHT = 24;
/** Frame: 2px border, 3px gap, 1px inner border, each side. */
export const FRAME_HEIGHT = 12;

/**
 * The richest density that fits every item in `maxHeight`, else the compact one with as many
 * items as fit (at least one) and a 「还有 N 项」 line for the rest.
 */
export function layout(total: number, available: number, maxHeight: number) {
  const room = maxHeight - HEADER_HEIGHT - FRAME_HEIGHT;
  const count = Math.min(total, available);
  for (const density of ['detail', 'standard'] as const) {
    if (count * ROW_HEIGHT[density] <= room && count === total) return { density, shown: count };
  }
  if (count * ROW_HEIGHT.compact <= room && count === total)
    return { density: 'compact' as const, shown: count };
  const shown = Math.max(1, Math.floor((room - MORE_HEIGHT) / ROW_HEIGHT.compact));
  return { density: 'compact' as const, shown: Math.min(shown, count) };
}
