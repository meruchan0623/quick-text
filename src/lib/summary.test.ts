import { describe, expect, it } from 'vitest';
import type { CloudRuleEntry, CloudState } from '../ipc/types';
import { emptyConfig, type Config } from '../model';
import type { ReadableRule } from './rules';
import { ruleSummary, summaryText } from './summary';

const rule = (id: string, enabled = true): ReadableRule => ({
  id,
  enabled,
  group: '',
  description: '',
  ignore_case: false,
  match: { kind: 'literal', value: id },
  trigger_mode: 'terminator',
  terminator: null,
  ime_erasure: null,
  action: { type: 'text', method: 'paste', source: { type: 'inline', value: '正文' } },
  parameter: { type: 'default_match' },
  notification: { enabled: false, message: '' },
});

const config = (rules: unknown[], enabled = true): Config => ({ ...emptyConfig(), enabled, rules });

function cloud(rules: CloudRuleEntry[], enabled = true): CloudState {
  return {
    settings: { enabled, base_url: 'https://x', has_key: true, auto_sync_minutes: 30 },
    status: {
      syncing: false,
      last_attempt_at: null,
      last_success_at: null,
      last_error: null,
      revision: 3,
      updated_at: null,
      unreadable_count: 0,
    },
    rules,
  };
}

const phrase = (id: string, status: CloudRuleEntry['status'], overrides?: string[]) => ({
  id,
  rule: rule(id),
  status,
  disabled_local: status === 'disabled_local',
  ...(overrides ? { overrides } : {}),
});

describe('ruleSummary', () => {
  it('counts readable rules only, enabled of all', () => {
    const c = config([rule('a'), rule('b', false), rule('c'), { id: 'broken', match: 7 }]);
    expect(ruleSummary(c, null)).toEqual({ mine: { active: 2, total: 3 }, team: null });
  });

  it('reads a rule with absent serde defaults as the backend does', () => {
    const partial: Record<string, unknown> = { ...rule('a') };
    delete partial.notification;
    expect(ruleSummary(config([partial]), null).mine).toEqual({ active: 1, total: 1 });
  });

  it('counts team phrases of a subscription, and not the rules they take the place of', () => {
    const c = config([rule('a'), rule('b')]);
    const team = cloud([
      phrase('p1', 'active', ['a']),
      phrase('p2', 'disabled_local'),
      phrase('p3', 'disabled_remote'),
      phrase('p4', 'invalid'),
      phrase('p5', 'active'),
    ]);
    expect(ruleSummary(c, team)).toEqual({
      mine: { active: 1, total: 2 },
      team: { active: 2, total: 5 },
    });
  });

  it('counts no team phrase without a subscription', () => {
    const c = config([rule('a')]);
    expect(ruleSummary(c, cloud([phrase('p1', 'active', ['a'])], false))).toEqual({
      mine: { active: 1, total: 1 },
      team: null,
    });
  });
});

describe('summaryText', () => {
  const base = { cloud: null, loaded: true, safeMode: null };
  it('says nothing before the configuration is read', () => {
    expect(summaryText({ ...base, config: config([rule('a')]), loaded: false })).toBe('');
    expect(summaryText({ ...base, config: null })).toBe('');
  });

  it('names what is in effect, team phrases apart', () => {
    expect(summaryText({ ...base, config: config([rule('a'), rule('b', false)]) })).toBe(
      '已启用 1 / 2 条指令',
    );
    expect(
      summaryText({
        ...base,
        config: config([rule('a')]),
        cloud: cloud([phrase('p1', 'active'), phrase('p2', 'disabled_remote')]),
      }),
    ).toBe('已启用 1 / 1 条指令 · 团队话术 1 / 2 条');
  });

  it('says the master switch is off, and safe mode runs nothing', () => {
    expect(summaryText({ ...base, config: config([rule('a')], false) })).toBe(
      '文本指令总开关已关闭',
    );
    expect(summaryText({ ...base, config: config([]), safeMode: 'locked' })).toBe(
      '安全模式 · 指令暂未生效',
    );
  });
});
