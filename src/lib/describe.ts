import type { Data } from '../model';

// Known engine messages (English, from crates/platform) shown as plain Chinese.
const messageHints: [string, string][] = [
  [
    'Physical key conflicts',
    '检测到你正按着某个键（如 Ctrl、Shift、Alt），与要发送的按键冲突，为避免误操作已停止。请松开所有按键后再试。',
  ],
  [
    'target consumption cannot be confirmed',
    '按键或文字已经发出，但无法确认目标窗口是否收到，请到目标窗口查看效果。',
  ],
  [
    'SendInput accepted',
    '只有部分输入被系统接受，目标窗口可能以管理员身份运行。请以相同权限运行本软件后再试。',
  ],
  ['already running', '已有一个操作正在执行，请等它结束或先停止。'],
];

export function plainMessage(message: unknown): string {
  if (typeof message !== 'string' || message === '') return '';
  const hint = messageHints.find(([k]) => message.includes(k));
  return hint ? hint[1] : message;
}

export function describeExecution(result: Data | null | undefined): string {
  if (!result) return '没有收到执行结果。';
  const lines: string[] = [];
  switch (result.status) {
    case 'success':
      lines.push('✅ 执行完成。');
      break;
    case 'failed':
    case 'failure':
      lines.push('❌ 执行失败。');
      break;
    case 'cancelled':
    case 'canceled':
      lines.push('⏹ 已停止。');
      break;
    case 'unknown':
      lines.push('⚠️ 已经发送，但不确定目标窗口是否收到。');
      break;
    default:
      lines.push(`状态：${result.status ?? '未知'}`);
  }
  const msg = plainMessage(result.message);
  if (msg) lines.push(msg);
  if (result.side_effects) lines.push('注意：已经实际输入或执行了部分操作，停止后不会撤销。');
  else if (result.status !== 'success') lines.push('没有对其他窗口造成任何影响。');
  return lines.join('\n\n');
}

export function describeMatch(match: Data | null | undefined, input: string): string {
  if (!match)
    return `❌ 输入「${input}」不会触发这条指令。\n\n请检查缩写是否写对；如果设置了“按结束键触发”，实际使用时还需要在缩写后按结束键。`;
  const lines = [`✅ 输入「${input}」会触发这条指令。`, `识别到的缩写：「${match.matched}」`];
  if (match.erase_chars > 0)
    lines.push(`触发时会先删除刚输入的 ${match.erase_chars} 个字符，再执行操作。`);
  if (match.parameter) lines.push(`附带内容：「${match.parameter}」`);
  const caps = ((match.captures ?? []) as (string | null)[]).filter((c) => c);
  if (caps.length) lines.push(`捕获到的内容：${caps.map((c) => `「${c}」`).join('、')}`);
  if (match.consume_terminator) lines.push('触发时会吞掉你按下的结束键（不会多出空格或回车）。');
  if ((match.tied_rule_ids as string[] | undefined)?.length)
    lines.push('提示：还有其他指令也能被这段文字触发，实际只会执行列表中靠前的那条。');
  return lines.join('\n\n');
}
