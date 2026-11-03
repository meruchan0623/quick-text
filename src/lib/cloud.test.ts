import { describe, expect, it } from 'vitest';
import type { CloudRuleEntry, CloudState } from '../ipc/types';
import {
  actionBody,
  AUTO_SYNC_MINUTES,
  autoSyncLabel,
  cloudErrorNames,
  cloudErrorText,
  cloudRows,
  cloudRuleName,
  cloudStatusClass,
  cloudStatusNames,
  overriddenIds,
  overridingPhrases,
  phraseCopy,
  syncTime,
  triggerText,
} from './cloud';
import type { ReadableRule } from './rules';

const rule = (id: string, value: string, extra: Partial<ReadableRule> = {}): ReadableRule => ({
  id,
  enabled: true,
  group: '',
  description: '',
  ignore_case: false,
  match: { kind: 'literal', value },
  trigger_mode: 'terminator',
  terminator: null,
  ime_erasure: null,
  action: { type: 'text', method: 'paste', source: { type: 'inline', value: '正文' } },
  parameter: { type: 'default_match' },
  notification: { enabled: false, message: '' },
  ...extra,
});

function state(rules: CloudRuleEntry[]): CloudState {
  return {
    settings: { enabled: true, base_url: 'https://x', has_key: true, auto_sync_minutes: 30 },
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

describe('labels', () => {
  it('names every status of §4 and tags a newer one as other', () => {
    expect(Object.keys(cloudStatusNames)).toEqual([
      'active',
      'disabled_remote',
      'disabled_local',
      'invalid',
    ]);
    // A phrase that takes a personal rule's place is simply in effect: `shadowed` is gone.
    expect(cloudStatusClass('shadowed')).toBe('other');
    expect(cloudStatusClass('invalid')).toBe('invalid');
    expect(cloudStatusClass('paused_by_admin')).toBe('other');
    expect(cloudStatusClass('constructor')).toBe('other');
  });

  it('offers the §2 intervals in words', () => {
    expect(AUTO_SYNC_MINUTES.map(autoSyncLabel)).toEqual([
      '仅手动同步',
      '每 15 分钟',
      '每 30 分钟',
      '每 1 小时',
      '每 2 小时',
      '每 4 小时',
    ]);
    expect(autoSyncLabel(90)).toBe('每 90 分钟');
  });
});

describe('cloudErrorText', () => {
  it('has a sentence for every §3 error code', () => {
    for (const code of [
      'not_configured',
      'invalid_url',
      'unauthorized',
      'network',
      'timeout',
      'http_status',
      'too_large',
      'too_many_rules',
      'invalid_format',
      'schema_newer',
      'io',
    ])
      expect(cloudErrorNames[code], code).toMatch(/[一-鿿]/);
    expect(cloudErrorText({ code: 'schema_newer', message: 'schema 2' })).toMatch(/升级/);
    expect(cloudErrorText({ code: 'unauthorized', message: 'HTTP 401' })).not.toMatch(/HTTP/);
  });

  it('uses the backend message where it tells more, and names an unknown code', () => {
    expect(
      cloudErrorText({
        code: 'http_status',
        message: '服务器上没有团队话术接口（HTTP 404），请检查服务器地址是否填对',
      }),
    ).toBe('服务器上没有团队话术接口（HTTP 404），请检查服务器地址是否填对。');
    expect(cloudErrorText({ code: 'io', message: '缓存无法写入。' })).toBe('缓存无法写入。');
    expect(cloudErrorText({ code: 'http_status', message: '' })).toBe(cloudErrorNames.http_status);
    expect(cloudErrorText({ code: 'rate_limited', message: 'slow down' })).toBe(
      '同步失败（错误代码 rate_limited）：slow down',
    );
    expect(cloudErrorText({ code: 'toString', message: '' })).toBe(
      '同步失败（错误代码 toString）。',
    );
  });
});

describe('syncTime', () => {
  const now = new Date(2026, 9, 28, 18, 0).getTime();
  it('says today, a date this year or a date with its year, in local time', () => {
    expect(syncTime(new Date(2026, 9, 28, 9, 5).toISOString(), now)).toBe('今天 09:05');
    expect(syncTime(new Date(2026, 9, 27, 23, 59).toISOString(), now)).toBe('10月27日 23:59');
    expect(syncTime(new Date(2025, 0, 2, 8, 0).toISOString(), now)).toBe('2025年1月2日 08:00');
  });

  it('is null without a time or for one that does not parse', () => {
    expect(syncTime(null, now)).toBeNull();
    expect(syncTime('yesterday', now)).toBeNull();
  });
});

describe('cloudRows', () => {
  it('reads each entry as the list reads 我的指令, defaults filled in', () => {
    const partial = { id: 'b', match: { kind: 'literal', value: 'tb' } };
    const rows = cloudRows(
      state([
        { id: 'a', rule: rule('a', 'ta'), status: 'active', disabled_local: false },
        { id: 'b', rule: partial, status: 'active', disabled_local: false },
        {
          id: 'c',
          rule: { id: 'c', action: { type: 'from_0_9' } },
          status: 'invalid',
          disabled_local: false,
        },
      ]),
    );
    expect(rows.map((r) => r.rule?.match.value ?? null)).toEqual(['ta', 'tb', null]);
    expect(rows[1]?.rule?.action).toEqual({ type: 'none' });
    expect(rows[1]?.rule?.trigger_mode).toBe('terminator');
    expect(cloudRows(null)).toEqual([]);
  });
});

describe('overridingPhrases and cloudRuleName', () => {
  const s = state([
    {
      id: 'a',
      rule: rule('a', 'ta', { description: '开场' }),
      status: 'active',
      disabled_local: false,
    },
    {
      id: 'b',
      rule: rule('b', 'tb'),
      status: 'active',
      disabled_local: false,
      overrides: ['mine-1'],
    },
    {
      id: 'c',
      rule: rule('c', 'tc'),
      status: 'active',
      disabled_local: false,
      overrides: ['mine-1', 'mine-2'],
    },
    // Only a phrase that is in effect takes anything's place.
    {
      id: 'd',
      rule: rule('d', 'td'),
      status: 'disabled_local',
      disabled_local: true,
      overrides: ['mine-3'],
    },
    { id: 'e', rule: rule('e', 'te'), status: 'active', disabled_local: false, overrides: [] },
  ]);

  it('lists the phrases that take the place of each personal rule', () => {
    expect([...overridingPhrases(s)]).toEqual([
      ['mine-1', ['tb', 'tc']],
      ['mine-2', ['tc']],
    ]);
    expect(overridingPhrases(null).size).toBe(0);
  });

  it('reads the replaced ids of an active phrase, and nothing malformed', () => {
    const entry = (extra: Partial<CloudRuleEntry>): CloudRuleEntry => ({
      id: 'x',
      rule: rule('x', 'tx'),
      status: 'active',
      disabled_local: false,
      ...extra,
    });
    expect(overriddenIds(entry({ overrides: ['a', 'b'] }))).toEqual(['a', 'b']);
    expect(overriddenIds(entry({}))).toEqual([]);
    expect(overriddenIds(entry({ status: 'invalid', overrides: ['a'] }))).toEqual([]);
    expect(overriddenIds(entry({ overrides: 'a' as unknown as string[] }))).toEqual([]);
    expect(overriddenIds(entry({ overrides: ['a', 3] as unknown as string[] }))).toEqual(['a']);
  });

  it('names a cloud: id from the table, and nothing else', () => {
    expect(cloudRuleName('cloud:a', s)).toBe('ta（团队话术：开场）');
    expect(cloudRuleName('cloud:b', s)).toBe('tb（团队话术）');
    expect(cloudRuleName('cloud:gone', s)).toBeNull();
    expect(cloudRuleName('a', s)).toBeNull();
    expect(cloudRuleName('cloud:a', null)).toBeNull();
  });
});

describe('detail texts', () => {
  it('describes the trigger with regex and case notes', () => {
    expect(triggerText(rule('a', 'x'))).toBe('输入缩写后，再按触发键执行');
    expect(
      triggerText(
        rule('a', 'x', {
          trigger_mode: 'immediate',
          ignore_case: true,
          match: { kind: 'regex', value: 'x+' },
        }),
      ),
    ).toBe('缩写输入完成后立即执行（按正则表达式匹配，英文不区分大小写）');
  });

  it('shows the whole text of a text action and the summary of others', () => {
    expect(
      actionBody({ type: 'text', method: 'keys', source: { type: 'inline', value: '一\n二' } }),
    ).toBe('一\n二');
    expect(actionBody({ type: 'send_keys', source: { type: 'file', path: 'a.txt' } })).toBe(
      '读取文件：a.txt',
    );
    expect(actionBody({ type: 'html', html: '', plain_text: '纯文本' })).toBe('纯文本');
    expect(actionBody({ type: 'open', target: 'https://example.com' })).toBe('https://example.com');
    expect(actionBody({ type: 'none' })).toBe('—');
    expect(actionBody(null)).toBe('—');
  });
});

describe('phraseCopy (「复制到我的指令」 whitelist)', () => {
  const row = (r: unknown, status: CloudRuleEntry['status'] = 'active') => {
    const [only] = cloudRows(state([{ id: 'x', rule: r, status, disabled_local: false }]));
    if (!only) throw new Error('no row');
    return only;
  };
  const text = (value: string, method = 'paste') => ({
    type: 'text',
    method,
    source: { type: 'inline', value },
  });

  it('takes over only the safe fields, rebuilt, and drops the rest', () => {
    const hostile = {
      ...rule('x', 'tk', { group: '售后', description: '说明', ignore_case: true }),
      enabled: false,
      terminator: { keys: ['enter', 'semicolon', 'tab', 'space', 'semicolon'], consume: false },
      ime_erasure: 'backspace',
      action: text('您好 {0:yyyy-MM-dd}', 'keys'),
      parameter: { type: 'fixed', value: 'p' },
      notification: { enabled: true, message: '提示' },
      future_field: { run: 'calc.exe' },
    };
    const { copy, reason } = phraseCopy(row(hostile, 'disabled_remote'));
    expect(reason).toBeNull();
    expect(copy).toEqual({
      match: { kind: 'literal', value: 'tk' },
      ignore_case: true,
      trigger_mode: 'terminator',
      terminator: { keys: ['semicolon', 'space'], consume: false },
      group: '售后',
      description: '说明',
      action: {
        type: 'text',
        method: 'keys',
        source: { type: 'inline', value: '您好 {0:yyyy-MM-dd}' },
      },
    });
    // Rebuilt, not shared with what the server sent.
    expect(copy?.action).not.toBe(hostile.action);
  });

  it('falls back to the default keys without 空格 or 分号, and for an immediate phrase', () => {
    const only = (r: ReadableRule) => phraseCopy(row(r)).copy?.terminator;
    expect(
      only(rule('x', 'a', { terminator: { keys: ['enter', 'tab'], consume: true } })),
    ).toBeNull();
    expect(
      only(
        rule('x', 'a', {
          trigger_mode: 'immediate',
          terminator: { keys: ['space'], consume: true },
        }),
      ),
    ).toBeNull();
    expect(only(rule('x', 'a'))).toBeNull();
  });

  it('copies fixed text whatever the status says, except invalid', () => {
    for (const status of ['active', 'disabled_remote', 'disabled_local'] as const)
      expect(phraseCopy(row(rule('x', 'a'), status)).reason, status).toBeNull();
    expect(phraseCopy(row(rule('x', 'a'), 'invalid')).reason).toMatch(/无法使用/);
  });

  it('refuses anything but fixed inline text, judged by the action, not the status', () => {
    const refused = (r: unknown, status: CloudRuleEntry['status'] = 'active') =>
      phraseCopy(row(r, status)).reason;
    // An enabled open action that the merge refused: never copied.
    expect(
      refused(
        { ...rule('x', 'a'), action: { type: 'open', target: 'cmd.exe', args: [] } },
        'invalid',
      ),
    ).toMatch(/无法使用/);
    // Statuses decided before the action is checked do not let it through either.
    for (const status of ['active', 'disabled_remote', 'disabled_local'] as const)
      expect(
        refused(
          { ...rule('x', 'a'), action: { type: 'open', target: 'cmd.exe', args: [] } },
          status,
        ),
      ).toMatch(/打开网页、文件或程序/);
    expect(
      refused({
        ...rule('x', 'a'),
        action: { type: 'send_keys', source: { type: 'inline', value: '^a' } },
      }),
    ).toMatch(/文字与按键组合/);
    expect(
      refused({
        ...rule('x', 'a'),
        action: { type: 'text', method: 'paste', source: { type: 'file', path: 'C:/secret.txt' } },
      }),
    ).toMatch(/不是直接写在话术里/);
    expect(refused(rule('x', 'a', { match: { kind: 'regex', value: 'a+' } }))).toMatch(/正则/);
    expect(refused(rule('x', ''))).toMatch(/没有缩写/);
    expect(refused({ id: 'x', action: { type: 'from_0_9' } })).toMatch(/无法读取/);
  });

  it('refuses text that reads the clipboard, the selection or a parameter', () => {
    for (const value of ['订单 {cliptext}', '{selection}', '你好 {param:name}'])
      expect(phraseCopy(row({ ...rule('x', 'a'), action: text(value) })).reason, value).toMatch(
        /剪贴板/,
      );
    expect(phraseCopy(row({ ...rule('x', 'a'), action: text('{0:HH:mm} 见') })).reason).toBeNull();
  });
});
