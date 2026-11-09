// 「已启用 X / Y 条指令」 in the header: what of the active configuration and the team phrases is in
// effect, counted from the state the backend answered (never the editor's unsaved copy).
import type { CloudState } from '../ipc/types';
import { completeConfig, type Config } from '../model';
import { overridingPhrases } from './cloud';
import { isReadableRule } from './rules';

export interface RuleCount {
  /** Rules that trigger: enabled, and not taken over by an active team phrase. */
  active: number;
  /** All of them. */
  total: number;
}

export interface RuleSummary {
  /**
   * 我的指令 the UI can read. Entries it cannot read (B-43) are left out of both numbers: whether
   * the backend runs them is not known here, and the list names them on their own.
   */
  mine: RuleCount;
  /** Team phrases of a subscription (status `active` of all in the table); null without one. */
  team: RuleCount | null;
}

/** Count `config` (the active configuration) and the team phrases `cloud` reports. */
export function ruleSummary(config: Config, cloud: CloudState | null): RuleSummary {
  const rules = completeConfig(config).rules.filter(isReadableRule);
  const subscribed = cloud?.settings.enabled ? cloud : null;
  // An active team phrase takes the place of the 我的指令 rules with its abbreviation (§4).
  const overridden = overridingPhrases(subscribed);
  const mine = {
    active: rules.filter((r) => r.enabled && !overridden.has(r.id)).length,
    total: rules.length,
  };
  const team =
    subscribed === null
      ? null
      : {
          active: subscribed.rules.filter((e) => e.status === 'active').length,
          total: subscribed.rules.length,
        };
  return { mine, team };
}

/**
 * The header summary: empty until the configuration is read, then what is in effect. In safe mode
 * the engine runs nothing (and in Locked the active configuration is a placeholder without rules).
 */
export function summaryText({
  config,
  cloud,
  loaded,
  safeMode,
}: {
  config: Config | null;
  cloud: CloudState | null;
  loaded: boolean;
  safeMode: string | null;
}): string {
  if (!loaded || config === null) return '';
  if (safeMode !== null) return '安全模式 · 指令暂未生效';
  if (!config.enabled) return '文本指令总开关已关闭';
  const { mine, team } = ruleSummary(config, cloud);
  const text = `已启用 ${mine.active} / ${mine.total} 条指令`;
  return team === null ? text : `${text} · 团队话术 ${team.active} / ${team.total} 条`;
}
