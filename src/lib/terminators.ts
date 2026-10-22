// Terminator keys per platform (FE-12, S-12 / B-12). The Windows input backend does not support Tab
// and Enter as terminators: old configs keep them (the backend only warns, behaviour unchanged), so
// the UI shows them as unsupported instead of dropping them.

const UNSUPPORTED: Readonly<Record<string, readonly string[]>> = {
  windows: ['tab', 'enter'],
};

/** Default terminators for new configs (B-12). */
export const DEFAULT_TERMINATORS: readonly string[] = ['space', 'semicolon'];

/** Terminator keys the platform cannot use; empty when the platform is unknown. */
export const unsupportedTerminators = (platform: string | null | undefined): readonly string[] =>
  (platform && UNSUPPORTED[platform]) || [];

export const isTerminatorSupported = (key: string, platform: string | null | undefined): boolean =>
  !unsupportedTerminators(platform).includes(key);

/** The keys that actually end an abbreviation on this platform. */
export const effectiveTerminators = (
  keys: readonly string[],
  platform: string | null | undefined,
): string[] => keys.filter((key) => isTerminatorSupported(key, platform));
