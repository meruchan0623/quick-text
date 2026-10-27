// The key recorder of 「文字与按键组合」 (src/KeyRecorder.tsx) shows a send_keys source as a row of
// text, key and placeholder blocks and writes the blocks back as the same SendKeys source the
// backend reads (parse_sendkeys in crates/domain/src/parser.rs, after render_template in
// crates/domain/src/content.rs fills in the placeholders). The rule stores only the source, so
// rules from an import or an older version, and the advanced source box, keep working as before.
import { MAX_STEPS } from './ruleShape';

export type Modifier = 'Ctrl' | 'Shift' | 'Alt';
export type Recorded =
  | { kind: 'text'; text: string }
  | { kind: 'key'; mods: Modifier[]; key: string; repeat: number }
  /** A render_template placeholder such as `cliptext` or `0:yyyy-MM-dd`, without braces. */
  | { kind: 'token'; token: string };

/** Characters with a meaning in SendKeys; as text they are written in braces (`{+}`). */
const SPECIAL = '+^%~()[]{}';
const PREFIX: Record<string, Modifier> = { '^': 'Ctrl', '+': 'Shift', '%': 'Alt' };
const MOD_PREFIX: Record<Modifier, string> = { Ctrl: '^', Shift: '+', Alt: '%' };

const isToken = (body: string) =>
  body === 'cliptext' ||
  body === 'selection' ||
  (body.startsWith('param:') && body.length > 6) ||
  body.startsWith('0:');

// parse_key's names minus those sendkeys_key refuses (Win, media, browser and launch keys,
// F17 and up, key codes): what a SendKeys source may name.
const NAMED = new Set(
  (
    'CTRL CONTROL CONTROLKEY LCONTROLKEY RCONTROLKEY SHIFT SHIFTKEY LSHIFTKEY RSHIFTKEY ALT MENU ' +
    'LMENU RMENU ENTER RETURN SPACE TAB ESC ESCAPE BACK BACKSPACE BS LEFT RIGHT UP DOWN HOME END ' +
    'PGUP PGDN PRIOR NEXT DELETE DEL INSERT INS CAPSLOCK CAPITAL NUMLOCK SCROLLLOCK SCROLL PAUSE ' +
    'BREAK PRINTSCREEN PRTSC ADD SUBTRACT MULTIPLY DIVIDE DECIMAL APPS HELP CLEAR SELECT EXECUTE ' +
    'OEMSEMICOLON OEMPLUS OEMCOMMA OEMMINUS OEMPERIOD OEMQUESTION OEMTILDE OEMOPENBRACKETS ' +
    'OEMPIPE OEMCLOSEBRACKETS OEMQUOTES OEMBACKSLASH'
  ).split(' '),
);
export const isSendKey = (name: string): boolean => {
  const upper = name.toUpperCase();
  const f = /^F(\d{1,2})$/.exec(upper);
  return (
    /^[A-Za-z]$/.test(name) ||
    /^D\d$/.test(upper) ||
    /^NUMPAD\d$/.test(upper) ||
    (f !== null && Number(f[1]) >= 1 && Number(f[1]) <= 16) ||
    NAMED.has(upper)
  );
};

const pushText = (items: Recorded[], text: string) => {
  const last = items[items.length - 1];
  if (last?.kind === 'text') items[items.length - 1] = { kind: 'text', text: last.text + text };
  else items.push({ kind: 'text', text });
};

/**
 * The blocks of a SendKeys source, or null when it uses what the blocks cannot show (a group such
 * as `^(ac)`, a modifier on a placeholder) or what the backend would refuse (an unknown key name,
 * an unclosed brace); the advanced source box edits those.
 */
export function parseRecording(source: string): Recorded[] | null {
  const chars = Array.from(source);
  const items: Recorded[] = [];
  let pos = 0;
  while (pos < chars.length) {
    const mods: Modifier[] = [];
    for (let mod = PREFIX[chars[pos] ?? '']; mod; mod = PREFIX[chars[pos] ?? '']) {
      if (mods.includes(mod)) return null;
      mods.push(mod);
      pos += 1;
    }
    const c = chars[pos];
    pos += 1;
    if (c === undefined || c === '(' || ')[]}'.includes(c)) return null;
    if (c === '~') {
      items.push({ kind: 'key', mods, key: 'Enter', repeat: 1 });
    } else if (c === '{') {
      // `{{}` and `{}}` are the braces themselves.
      const brace = chars[pos];
      if ((brace === '{' || brace === '}') && chars[pos + 1] === '}') {
        if (mods.length) return null;
        pushText(items, brace);
        pos += 2;
        continue;
      }
      const end = chars.indexOf('}', pos);
      if (end < 0) return null;
      const body = chars.slice(pos, end).join('');
      pos = end + 1;
      if (isToken(body)) {
        if (mods.length) return null;
        items.push({ kind: 'token', token: body });
        continue;
      }
      const [name, count, ...extra] = body.split(/\s+/).filter(Boolean);
      if (name === undefined || extra.length) return null;
      let repeat = 1;
      if (count !== undefined) {
        if (!/^\d+$/.test(count)) return null;
        repeat = Number(count);
        if (repeat < 1 || repeat > MAX_STEPS) return null;
      }
      if (name.length === 1 && '+^%~()[]'.includes(name)) {
        if (mods.length) return null;
        pushText(items, name.repeat(repeat));
      } else if (isSendKey(name)) {
        items.push({ kind: 'key', mods, key: name, repeat });
      } else {
        return null;
      }
    } else if (mods.length === 0) {
      pushText(items, c);
    } else if (/^\d$/.test(c)) {
      items.push({ kind: 'key', mods, key: `D${c}`, repeat: 1 });
    } else if (/^[A-Za-z]$/.test(c)) {
      items.push({ kind: 'key', mods, key: c, repeat: 1 });
    } else {
      return null;
    }
  }
  return items;
}

/** The SendKeys source of the blocks; parseRecording reads it back as the same blocks. */
export const recordingSource = (items: Recorded[]): string =>
  items
    .map((item) =>
      item.kind === 'text'
        ? Array.from(item.text, (c) => (SPECIAL.includes(c) ? `{${c}}` : c)).join('')
        : item.kind === 'token'
          ? `{${item.token}}`
          : item.mods.map((m) => MOD_PREFIX[m]).join('') +
            (item.repeat > 1 ? `{${item.key} ${item.repeat}}` : `{${item.key}}`),
    )
    .join('');

const sameKey = (a: Recorded, b: Extract<Recorded, { kind: 'key' }>) =>
  a.kind === 'key' &&
  a.key.toUpperCase() === b.key.toUpperCase() &&
  a.mods.length === b.mods.length &&
  a.mods.every((m) => b.mods.includes(m));

/** The blocks with `text` typed at the end. */
export const appendText = (items: Recorded[], text: string): Recorded[] => {
  if (!text) return items;
  const next = [...items];
  pushText(next, text);
  return next;
};

/** The blocks with `key` pressed at the end: pressing the same key again counts it (回车 ×3). */
export const appendKey = (
  items: Recorded[],
  key: Extract<Recorded, { kind: 'key' }>,
): Recorded[] => {
  const last = items[items.length - 1];
  if (last && sameKey(last, key) && last.kind === 'key' && last.repeat < MAX_STEPS)
    return [...items.slice(0, -1), { ...last, repeat: last.repeat + 1 }];
  return [...items, key];
};

/** The blocks without the last character typed or key pressed. */
export const undoLast = (items: Recorded[]): Recorded[] => {
  const last = items[items.length - 1];
  const rest = items.slice(0, -1);
  if (last?.kind === 'text') {
    const text = Array.from(last.text).slice(0, -1).join('');
    return text ? [...rest, { kind: 'text', text }] : rest;
  }
  if (last?.kind === 'key' && last.repeat > 1)
    return [...rest, { ...last, repeat: last.repeat - 1 }];
  return rest;
};

const KEY_LABELS: Record<string, string> = {
  ENTER: '回车',
  RETURN: '回车',
  TAB: 'Tab',
  ESC: 'Esc',
  ESCAPE: 'Esc',
  BACK: '退格',
  BACKSPACE: '退格',
  BS: '退格',
  DELETE: 'Delete',
  DEL: 'Delete',
  INSERT: 'Insert',
  INS: 'Insert',
  SPACE: '空格',
  LEFT: '←',
  RIGHT: '→',
  UP: '↑',
  DOWN: '↓',
  HOME: 'Home',
  END: 'End',
  PGUP: 'PgUp',
  PRIOR: 'PgUp',
  PGDN: 'PgDn',
  NEXT: 'PgDn',
  CTRL: 'Ctrl',
  CONTROL: 'Ctrl',
  SHIFT: 'Shift',
  ALT: 'Alt',
  OEMSEMICOLON: ';',
  OEMPLUS: '=',
  OEMCOMMA: ',',
  OEMMINUS: '-',
  OEMPERIOD: '.',
  OEMQUESTION: '/',
  OEMTILDE: '`',
  OEMOPENBRACKETS: '[',
  OEMCLOSEBRACKETS: ']',
  OEMPIPE: '\\',
  OEMBACKSLASH: '\\',
  OEMQUOTES: "'",
  ADD: '小键盘 +',
  SUBTRACT: '小键盘 -',
  MULTIPLY: '小键盘 *',
  DIVIDE: '小键盘 /',
  DECIMAL: '小键盘 .',
};
const keyLabel = (name: string) => {
  const upper = name.toUpperCase();
  const label = KEY_LABELS[upper];
  if (label) return label;
  if (/^D\d$/.test(upper)) return upper.slice(1);
  if (/^NUMPAD\d$/.test(upper)) return `小键盘 ${upper.slice(6)}`;
  return name.length === 1 ? upper : name;
};
export const TOKEN_LABELS: Record<string, string> = {
  cliptext: '已复制的文字',
  selection: '选中的文字',
  '0:yyyy-MM-dd': '今天的日期',
  '0:HH:mm:ss': '当前时间',
};

/** How a block reads in the recorder. */
export const itemLabel = (item: Recorded): string => {
  if (item.kind === 'text') return item.text;
  if (item.kind === 'token')
    return (
      TOKEN_LABELS[item.token] ??
      (item.token.startsWith('param:')
        ? `传入的内容：${item.token.slice(6)}`
        : `日期时间（${item.token.slice(2)}）`)
    );
  const name = [...item.mods, keyLabel(item.key)].join('+');
  return item.repeat > 1 ? `${name} ×${item.repeat}` : name;
};

/** The parts of a keydown event the recorder reads. */
export type KeyPress = Pick<
  KeyboardEvent,
  'key' | 'code' | 'ctrlKey' | 'shiftKey' | 'altKey' | 'metaKey'
>;

const NAMED_EVENT_KEYS: Record<string, string> = {
  Enter: 'Enter',
  Tab: 'Tab',
  Backspace: 'Backspace',
  Escape: 'Escape',
  Delete: 'Delete',
  Insert: 'Insert',
  Home: 'Home',
  End: 'End',
  PageUp: 'PgUp',
  PageDown: 'PgDn',
  ArrowLeft: 'Left',
  ArrowRight: 'Right',
  ArrowUp: 'Up',
  ArrowDown: 'Down',
  ContextMenu: 'Apps',
};
// With Ctrl or Alt held, a character key is named by where it is on the keyboard.
const CODE_KEYS: Record<string, string> = {
  Space: 'Space',
  Comma: 'OEMCOMMA',
  Period: 'OEMPERIOD',
  Minus: 'OEMMINUS',
  Equal: 'OEMPLUS',
  Semicolon: 'OEMSEMICOLON',
  Slash: 'OEMQUESTION',
  Backquote: 'OEMTILDE',
  BracketLeft: 'OEMOPENBRACKETS',
  BracketRight: 'OEMCLOSEBRACKETS',
  Backslash: 'OEMPIPE',
  IntlBackslash: 'OEMBACKSLASH',
  Quote: 'OEMQUOTES',
  NumpadAdd: 'Add',
  NumpadSubtract: 'Subtract',
  NumpadMultiply: 'Multiply',
  NumpadDivide: 'Divide',
  NumpadDecimal: 'Decimal',
};

/**
 * What a key press while recording stands for: a key block; `'text'` for a character the input
 * box types itself (the recorder takes the text it gets, so Shift and input methods work as
 * usual); `'unsupported'` for what SendKeys cannot press (the Windows key, media keys, F17 and
 * up); null for a modifier pressed alone.
 */
export function keyFromEvent(
  e: KeyPress,
): Extract<Recorded, { kind: 'key' }> | 'text' | 'unsupported' | null {
  if (['Control', 'Shift', 'Alt', 'AltGraph', 'CapsLock', 'NumLock'].includes(e.key)) return null;
  if (e.metaKey || e.key === 'Meta' || e.key === 'OS') return 'unsupported';
  const held = e.ctrlKey || e.altKey;
  const mods: Modifier[] = [];
  if (e.ctrlKey) mods.push('Ctrl');
  if (e.shiftKey) mods.push('Shift');
  if (e.altKey) mods.push('Alt');
  let name: string | undefined = NAMED_EVENT_KEYS[e.key];
  const f = /^F(\d{1,2})$/.exec(e.key);
  if (f) {
    if (Number(f[1]) > 16) return 'unsupported';
    name = e.key;
  }
  if (name === undefined) {
    if (e.key.length !== 1 && Array.from(e.key).length !== 1) return 'unsupported';
    if (!held) return 'text';
    const code = e.code;
    name = /^Key[A-Z]$/.test(code)
      ? code.slice(3)
      : /^Digit\d$/.test(code)
        ? `D${code.slice(5)}`
        : /^Numpad\d$/.test(code)
          ? `NumPad${code.slice(6)}`
          : CODE_KEYS[code];
    if (name === undefined) return 'unsupported';
  }
  return { kind: 'key', mods, key: name, repeat: 1 };
}
