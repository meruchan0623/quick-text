// How a capability is presented (plan §4 principle 5, S-16 / B-13): what the platform can never do
// is hidden; what is unavailable for now is grayed out with the reason; experimental and degraded
// capabilities stay usable with a 「实验性」 or 「降级」 tag and the reason.
import {
  capabilityNames,
  capabilityReasonText,
  capabilityStatusNames,
  label,
  other,
  platformReasonNames,
} from '../ipc/labels';

export type Presentation = 'available' | 'experimental' | 'degraded' | 'unavailable' | 'hidden';

export interface CapabilityView {
  id: string;
  name: string;
  presentation: Presentation;
  /** 可用 / 不可用 / 实验性 / 降级, or 「其他」 for a status newer than this frontend. */
  tag: string;
  /** Why it is not plainly available, in Chinese; `null` when there is nothing to say. */
  reason: string | null;
  /** Whether it can be used now: experimental and degraded capabilities can. */
  usable: boolean;
}

/** Reasons that never change at run time: no adapter on this platform provides the capability. */
const PERMANENT: readonly string[] = ['not_implemented', 'unsupported'];

const presentations: Readonly<Record<string, Presentation>> = {
  available: 'available',
  experimental: 'experimental',
  degraded: 'degraded',
  unavailable: 'unavailable',
};

/**
 * The presentation of one `get_capabilities` entry; `null` for an entry without an id. Entries of
 * a backend before the four statuses (only `available`) are read from that flag, and on macOS a
 * usable one counts as experimental (ADR-f).
 */
export function capabilityView(raw: unknown, platform?: string | null): CapabilityView | null {
  if (!raw || typeof raw !== 'object') return null;
  const c = raw as Record<string, unknown>;
  if (typeof c.id !== 'string' || c.id === '') return null;
  const available = c.available === true;
  const status =
    typeof c.status === 'string'
      ? c.status
      : !available
        ? 'unavailable'
        : platform === 'macos'
          ? 'experimental'
          : 'available';
  const reason = typeof c.reason === 'string' ? c.reason : null;
  const known = Object.hasOwn(presentations, status) ? presentations[status] : undefined;
  const presentation: Presentation =
    known === 'unavailable' && reason !== null && PERMANENT.includes(reason)
      ? 'hidden'
      : (known ?? (available ? 'degraded' : 'unavailable'));
  const reasonText =
    capabilityReasonText(reason) ??
    (presentation === 'experimental' ? (platformReasonNames.experimental_platform ?? null) : null);
  return {
    id: c.id,
    name: label(capabilityNames, c.id),
    presentation,
    tag: known ? label(capabilityStatusNames, status) : other(status),
    reason: reasonText,
    usable: known ? known !== 'unavailable' : available,
  };
}

/** Every entry of `get_capabilities` that has an id, in the backend's order. */
export const capabilityViews = (list: unknown, platform?: string | null): CapabilityView[] =>
  (Array.isArray(list) ? list : []).flatMap((c) => capabilityView(c, platform) ?? []);

/** The capability `id`; `null` when the backend does not report it (nothing is known then). */
export const findCapability = (
  list: unknown,
  id: string | null,
  platform?: string | null,
): CapabilityView | null =>
  id === null ? null : (capabilityViews(list, platform).find((c) => c.id === id) ?? null);

/** The capability an action type needs to run, by the editor's choice (see `actionChoice`). */
const choiceCapabilities: Readonly<Record<string, string>> = {
  text_keys: 'keys',
  text_paste: 'paste',
  send_keys: 'keys',
  hotkey: 'keys',
  html: 'html',
  replay: 'recording',
  open: 'open',
  quicker: 'quicker_private_action',
};

export const choiceCapability = (choice: string): string | null =>
  (Object.hasOwn(choiceCapabilities, choice) ? choiceCapabilities[choice] : undefined) ?? null;

/** How one entry of a choice list is offered, by the capability it needs. */
export interface ChoiceOffer {
  /** Not listed: the platform never provides it. */
  hidden: boolean;
  /** Listed but cannot be picked: unavailable for now. */
  disabled: boolean;
  /** Appended to the entry's text: 「（暂不可用：原因）」, 「（降级：原因）」, 「（实验性）」 or ''. */
  note: string;
}

/**
 * Principle 5 for a choice list (S-16): never provided is hidden; unavailable for now is listed
 * but disabled, with the reason; degraded stays selectable with its reason (a rule is lasting
 * configuration, not held back by a passing run-time state); experimental stays selectable with
 * its tag. The value the edited rule holds (`current`) is always listed and selectable, so opening
 * and saving a rule never rewrites it. `view` null: nothing is known, offered plainly.
 */
export function choiceOffer(view: CapabilityView | null, current: boolean): ChoiceOffer {
  const plain: ChoiceOffer = { hidden: false, disabled: false, note: '' };
  if (!view) return plain;
  const because = (what: string) => `（${what}${view.reason ? `：${view.reason}` : ''}）`;
  switch (view.presentation) {
    case 'hidden':
      return { ...plain, hidden: !current };
    case 'unavailable':
      return { ...plain, disabled: !current, note: because('暂不可用') };
    case 'degraded':
      return { ...plain, note: because(view.tag) };
    case 'experimental':
      return { ...plain, note: `（${view.tag}）` };
    case 'available':
      return plain;
  }
}

/**
 * The only IME erasure method the platform runs, when it ignores the setting: the Windows input
 * backend always erases by Backspace, so its 「按 Esc 关闭选字框」 is not offered (S-16, R-07).
 */
export const fixedImeErasure = (platform: string | null | undefined): string | null =>
  platform === 'windows' ? 'backspace' : null;
