import { describe, expect, it } from 'vitest';
import {
  capabilityNames,
  capabilityReasonText,
  capabilityStatusNames,
  componentNames,
  healthNames,
  label,
  logLevelNames,
  other,
  outcomeNames,
  pauseNames,
  platformReasonNames,
  reasonNames,
  selfTestNames,
  triggerNames,
} from './labels';

// Copied from the Rust sources on purpose: a code added there without a Chinese name here fails.
// crates/platform/src/health.rs `ReasonCode` (contract §3 lists the same 23).
const REASON_CODES = [
  'ok',
  'dropped_busy',
  'cancelled',
  'paused',
  'engine_down',
  'focus_changed',
  'stale_input',
  'protected_control',
  'modifier_held',
  'inject_partial',
  'clipboard_busy',
  'clipboard_changed',
  'clipboard_format_skipped',
  'clipboard_too_large',
  'resolve_failed',
  'preflight_failed',
  'executor_panic',
  'executor_timeout',
  'hook_missing',
  'raw_input_missing',
  'hook_install_failed',
  'unsupported',
  'internal',
];
// crates/platform/src/capabilities.rs `PlatformReason`, `CapabilityStatus`, `WINDOWS` and `MACOS`.
const PLATFORM_REASONS = ['experimental_platform', 'permission_missing', 'not_implemented'];
const CAPABILITY_STATUSES = ['available', 'unavailable', 'experimental', 'degraded'];
const CAPABILITY_IDS = [
  'keyboard_hook',
  'keys',
  'paste',
  'html',
  'file_clipboard',
  'image_clipboard',
  'mouse',
  'recording',
  'foreground',
  'open',
  'ime_committed_text',
  'quicker_private_action',
  'wheel_delta',
  'media_system_actions',
  'window_ime_actions',
];

const CJK = /[一-鿿]/;

/** Every code has its own Chinese name, never the raw code or the 「其他」 fallback. */
function expectNamed(names: Readonly<Record<string, string>>, codes: readonly string[]) {
  for (const code of codes) {
    const text = label(names, code);
    expect(text, code).toMatch(CJK);
    expect(text, code).not.toContain(code);
    expect(text, code).not.toMatch(/^其他/);
  }
}

describe('labels', () => {
  it('names every ReasonCode', () => {
    expect(REASON_CODES).toHaveLength(23);
    expectNamed(reasonNames, REASON_CODES);
  });

  it('names every capability id, status and platform reason', () => {
    expectNamed(capabilityNames, CAPABILITY_IDS);
    expectNamed(capabilityStatusNames, CAPABILITY_STATUSES);
    expectNamed(platformReasonNames, PLATFORM_REASONS);
  });

  it('names the other health.rs enums and the self-test checks', () => {
    expectNamed(healthNames, ['starting', 'healthy', 'degraded', 'recovering', 'failed']);
    expectNamed(componentNames, [
      'executor',
      'keyboard_hook',
      'mouse_hook',
      'raw_input',
      'supervisor',
      'focus_events',
      'clipboard',
      'protection',
    ]);
    expectNamed(pauseNames, [
      'manual',
      'timed',
      'fullscreen',
      'recording',
      'safe_mode',
      'engine_down',
      'self_test',
    ]);
    expectNamed(outcomeNames, [
      'succeeded',
      'unknown',
      'failed',
      'skipped',
      'cancelled',
      'dropped_busy',
    ]);
    expectNamed(triggerNames, ['immediate', 'terminator', 'manual']);
    expectNamed(selfTestNames, ['raw_input', 'hook_heartbeat', 'foreground', 'clipboard']);
    expectNamed(logLevelNames, ['error', 'warn', 'info', 'debug', 'trace']);
  });

  it('shows an unknown value as 其他 with the raw text, and never throws', () => {
    expect(label(reasonNames, 'quantum_tunnel')).toBe('其他（quantum_tunnel）');
    expect(label(capabilityNames, 'telepathy')).toBe('其他（telepathy）');
    // Inherited properties are not names.
    expect(label(reasonNames, 'constructor')).toBe('其他（constructor）');
    expect(label(reasonNames, '__proto__')).toBe('其他（__proto__）');
    expect(label(reasonNames, 'toString')).toBe('其他（toString）');
    // Not even a string: still no exception.
    expect(label(reasonNames, 7)).toBe('其他（7）');
    expect(label(reasonNames, null)).toBe('其他');
    expect(label(reasonNames, undefined)).toBe('其他');
    expect(label(reasonNames, { code: 'ok' })).toBe('其他');
    expect(label(reasonNames, '')).toBe('其他');
    expect(other('x')).toBe('其他（x）');
  });
});

describe('capabilityReasonText', () => {
  it('has Chinese text for every reason a capability can carry', () => {
    for (const code of [...REASON_CODES.filter((c) => c !== 'ok'), ...PLATFORM_REASONS]) {
      const text = capabilityReasonText(code);
      expect(text, code).toMatch(CJK);
      expect(text, code).not.toContain(code);
    }
  });

  it('is null when there is nothing to explain', () => {
    for (const nothing of [null, undefined, '', 'ok', 3, {}])
      expect(capabilityReasonText(nothing)).toBeNull();
  });

  it('shows an unknown code as 其他 and keeps the prose of a 0.3 backend', () => {
    expect(capabilityReasonText('solar_wind')).toBe('其他（solar_wind）');
    expect(capabilityReasonText('浏览器预览：模拟数据')).toBe('浏览器预览：模拟数据');
    expect(capabilityReasonText('constructor')).toBe('其他（constructor）');
  });
});
