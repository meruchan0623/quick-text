import { describe, expect, it } from 'vitest';
import {
  capabilityView,
  capabilityViews,
  choiceCapability,
  choiceOffer,
  findCapability,
  fixedImeErasure,
} from './capabilities';
import { platformReasonNames, reasonNames } from '../ipc/labels';

// Entries as crates/platform/src/capabilities.rs serializes them.
const entry = (id: string, status: string, reason: string | null = null) => ({
  id,
  status,
  reason,
  available: status !== 'unavailable',
});

describe('capabilityView', () => {
  it('presents the four statuses of principle 5', () => {
    expect(capabilityView(entry('keys', 'available'))).toEqual({
      id: 'keys',
      name: '逐字键入',
      presentation: 'available',
      tag: '可用',
      reason: null,
      usable: true,
    });
    expect(capabilityView(entry('paste', 'experimental', 'experimental_platform'))).toEqual({
      id: 'paste',
      name: '粘贴文本',
      presentation: 'experimental',
      tag: '实验性',
      reason: platformReasonNames.experimental_platform,
      usable: true,
    });
    expect(capabilityView(entry('keyboard_hook', 'degraded', 'hook_missing'))).toEqual({
      id: 'keyboard_hook',
      name: '键盘监听',
      presentation: 'degraded',
      tag: '降级',
      reason: reasonNames.hook_missing,
      usable: true,
    });
    expect(capabilityView(entry('mouse', 'unavailable', 'permission_missing'))).toEqual({
      id: 'mouse',
      name: '鼠标操作',
      presentation: 'unavailable',
      tag: '不可用',
      reason: platformReasonNames.permission_missing,
      usable: false,
    });
  });

  it('hides what the platform never provides', () => {
    for (const reason of ['not_implemented', 'unsupported']) {
      const view = capabilityView(entry('ime_committed_text', 'unavailable', reason));
      expect(view?.presentation).toBe('hidden');
      expect(view?.usable).toBe(false);
    }
    // Unavailable for now (engine down, missing permission) stays visible, grayed.
    expect(capabilityView(entry('keys', 'unavailable', 'engine_down'))?.presentation).toBe(
      'unavailable',
    );
  });

  it('reads a backend without statuses from `available`; on macOS all of it is experimental', () => {
    const legacy = (available: boolean) => ({ id: 'html', available, reason: '浏览器预览' });
    expect(capabilityView(legacy(true), 'windows')).toMatchObject({
      presentation: 'available',
      tag: '可用',
      reason: '浏览器预览',
    });
    expect(capabilityView(legacy(false), 'windows')).toMatchObject({
      presentation: 'unavailable',
      usable: false,
    });
    expect(capabilityView(legacy(true), 'macos')).toMatchObject({
      presentation: 'experimental',
      tag: '实验性',
      usable: true,
    });
    expect(capabilityView({ id: 'keys', available: true }, 'macos')?.reason).toBe(
      platformReasonNames.experimental_platform,
    );
  });

  it('shows a status or id newer than this frontend as 「其他」 without throwing', () => {
    expect(capabilityView(entry('telepathy', 'flaky', 'solar_wind'))).toEqual({
      id: 'telepathy',
      name: '其他（telepathy）',
      presentation: 'degraded',
      tag: '其他（flaky）',
      reason: '其他（solar_wind）',
      usable: true,
    });
    expect(capabilityView({ id: 'keys', status: 'constructor', available: false })).toMatchObject({
      presentation: 'unavailable',
      tag: '其他（constructor）',
      usable: false,
    });
  });

  it('skips entries without an id', () => {
    for (const bad of [null, undefined, 7, 'keys', [], {}, { id: '' }, { id: 3 }])
      expect(capabilityView(bad)).toBeNull();
    expect(capabilityViews([null, entry('open', 'available'), { status: 'available' }])).toEqual([
      expect.objectContaining({ id: 'open' }),
    ]);
    expect(capabilityViews(undefined)).toEqual([]);
    expect(capabilityViews({ keys: true })).toEqual([]);
  });
});

describe('findCapability', () => {
  const list = [entry('keys', 'available'), entry('recording', 'unavailable', 'not_implemented')];
  it('finds by id and knows nothing of an unreported one', () => {
    expect(findCapability(list, 'recording')?.presentation).toBe('hidden');
    expect(findCapability(list, 'html')).toBeNull();
    expect(findCapability(list, null)).toBeNull();
    expect(findCapability(undefined, 'keys')).toBeNull();
  });
});

describe('choiceCapability', () => {
  it('maps action types to the capability they need', () => {
    expect(choiceCapability('text_keys')).toBe('keys');
    expect(choiceCapability('text_paste')).toBe('paste');
    expect(choiceCapability('replay')).toBe('recording');
    expect(choiceCapability('quicker')).toBe('quicker_private_action');
    for (const none of ['none', 'steps', 'reusable', 'constructor', 'toString'])
      expect(choiceCapability(none)).toBeNull();
  });
});

describe('choiceOffer', () => {
  const offer = (raw: unknown, current = false) => choiceOffer(capabilityView(raw), current);

  it('offers an entry by the status of the capability it needs (principle 5)', () => {
    const plain = { hidden: false, disabled: false, note: '' };
    expect(offer(entry('keys', 'available'))).toEqual(plain);
    expect(offer(entry('paste', 'unavailable', 'clipboard_busy'))).toEqual({
      hidden: false,
      disabled: true,
      note: `（暂不可用：${reasonNames.clipboard_busy}）`,
    });
    expect(offer(entry('keys', 'degraded', 'engine_down'))).toEqual({
      hidden: false,
      disabled: false,
      note: `（降级：${reasonNames.engine_down}）`,
    });
    expect(offer(entry('recording', 'experimental', 'experimental_platform'))).toEqual({
      hidden: false,
      disabled: false,
      note: '（实验性）',
    });
    expect(offer(entry('quicker_private_action', 'unavailable', 'not_implemented'))).toEqual({
      hidden: true,
      disabled: false,
      note: '',
    });
    // Nothing reported, nothing known: offered plainly.
    expect(choiceOffer(null, false)).toEqual(plain);
  });

  it('says only the status when there is no reason', () => {
    expect(offer({ id: 'paste', status: 'unavailable', reason: null, available: false }).note).toBe(
      '（暂不可用）',
    );
    expect(offer({ id: 'paste', status: 'degraded', reason: null, available: true }).note).toBe(
      '（降级）',
    );
  });

  it('keeps the value the rule holds listed and selectable', () => {
    expect(offer(entry('paste', 'unavailable', 'clipboard_busy'), true)).toEqual({
      hidden: false,
      disabled: false,
      note: `（暂不可用：${reasonNames.clipboard_busy}）`,
    });
    expect(offer(entry('quicker_private_action', 'unavailable', 'not_implemented'), true)).toEqual({
      hidden: false,
      disabled: false,
      note: '',
    });
  });
});

describe('fixedImeErasure', () => {
  it('is fixed to Backspace only on Windows', () => {
    expect(fixedImeErasure('windows')).toBe('backspace');
    for (const platform of ['macos', 'linux', null, undefined])
      expect(fixedImeErasure(platform)).toBeNull();
  });
});
