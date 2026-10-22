import { describe, expect, it } from 'vitest';
import { emptyConfig } from '../model';
import {
  DEFAULT_TERMINATORS,
  effectiveTerminators,
  isTerminatorSupported,
  unsupportedTerminators,
} from './terminators';

describe('terminators (B-12, S-12)', () => {
  it('defaults new configs to space and semicolon', () => {
    expect(DEFAULT_TERMINATORS).toEqual(['space', 'semicolon']);
    expect(emptyConfig().globals.terminators).toEqual(['space', 'semicolon']);
  });

  it('does not support Tab and Enter on Windows', () => {
    expect(unsupportedTerminators('windows')).toEqual(['tab', 'enter']);
    expect(isTerminatorSupported('tab', 'windows')).toBe(false);
    expect(isTerminatorSupported('space', 'windows')).toBe(true);
  });

  it('assumes support on other or unknown platforms', () => {
    expect(unsupportedTerminators('macos')).toEqual([]);
    expect(unsupportedTerminators(undefined)).toEqual([]);
    expect(isTerminatorSupported('enter', null)).toBe(true);
  });

  it('keeps only the keys that end an abbreviation, in order', () => {
    expect(effectiveTerminators(['space', 'tab', 'enter', 'semicolon'], 'windows')).toEqual([
      'space',
      'semicolon',
    ]);
    expect(effectiveTerminators(['enter', 'space'], 'linux')).toEqual(['enter', 'space']);
  });
});
