// The editor's number contract (Codex rounds 4 and 5): the editor never produces a number the
// backend cannot read, or one past the IPC number contract (docs/ipc-contract-0.4.md §6), or one
// Rust's validation refuses. Each number field checks its raw text against the `ranges` entry of
// its Rust type (./ruleShape.ts, the table the shape check uses too) before converting it.
//
// Text that is not such a number is kept, as typed, in the edited rule itself: an InvalidNumber
// marker stands where the number goes. The rule is the editor's state, so the text survives
// whatever the field's component does not (switching steps, reordering them, undo), and the field
// shows it again with its inline error. Keeping the last valid value instead would save a number
// the user did not mean.
//
// The marker never leaves the editor. Every path that writes the edited rule or sends the edited
// action runs the checks below first (`ruleEditDiagnostics`, `actionDiagnostics`), which refuse it
// with a message at the field. The marker also carries a NaN, so the transport's last check
// (../ipc/sendable.ts) refuses any command still holding one: JSON would turn it into an object,
// and an object is a valid `serde_json::Value` (a reusable parameter, for one).
import { unsendablePath } from '../ipc/sendable';
import { hasUnsafeNumber, isReadableRule } from './rules';
import { inRange, isAction, isRecord, ranges, type NumberRange } from './ruleShape';

declare const invalidBrand: unique symbol;

/** Number text that is no number in its field's range, kept as typed where the number goes. */
export interface InvalidNumber {
  readonly invalidNumber: string;
  /**
   * Always NaN. It makes the marker unsendable (../ipc/sendable.ts), and it tells the marker apart
   * from anything the backend sends: JSON has no NaN. structuredClone (the editor's history) keeps
   * it.
   */
  readonly nan: number;
  readonly [invalidBrand]: true;
}

export const invalidNumber = (text: string): InvalidNumber =>
  ({ invalidNumber: text, nan: NaN }) as InvalidNumber;

export const isInvalidNumber = (value: unknown): value is InvalidNumber =>
  isRecord(value) && typeof value.invalidNumber === 'string' && Number.isNaN(value.nan);

const INTEGER = /^[+-]?\d+$/;
const DECIMAL = /^[+-]?(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?$/i;

/**
 * The number `text` stands for, when it is one in `range`; `null` otherwise. An integer range
 * takes digits only (no fraction, no exponent). The range is checked on the converted value, so a
 * literal past 2^53−1 that converts to a rounded double is out of every range that ends there.
 */
export function parseNumber(text: string, range: NumberRange): number | null {
  const trimmed = text.trim();
  if (!(range.integer ? INTEGER : DECIMAL).test(trimmed)) return null;
  const value = Number(trimmed);
  return inRange(range, value) ? value : null;
}

/** What a number field stores for `text`: its number in `range`, or the text itself, marked. */
export const readNumber = (text: string, range: NumberRange): number | InvalidNumber =>
  parseNumber(text, range) ?? invalidNumber(text);

/** The text a number field shows for a stored `value`: the marked text, or the number. */
export function numberText(value: unknown): string {
  if (isInvalidNumber(value)) return value.invalidNumber;
  return typeof value === 'number' && Number.isFinite(value) ? String(value) : '';
}

/** `range` in words: 「0 到 3600000 之间的整数」, 「大于 0 的数字」. */
export function rangeText(range: NumberRange): string {
  const kind = range.integer ? '整数' : '数字';
  if (range.max >= Number.MAX_VALUE) return range.min > 0 ? `大于 0 的${kind}` : `有限的${kind}`;
  return `${String(range.min)} 到 ${String(range.max)} 之间的${kind}`;
}

/** `before` and `text` joined, with the space this UI puts between Chinese and a number. */
const phrase = (before: string, text: string): string =>
  /^[-\d]/.test(text) ? `${before} ${text}` : before + text;

/** The inline error of a number field holding no number in `range`. */
export const rangeError = (range: NumberRange): string => `${phrase('请输入', rangeText(range))}。`;

/** A number field of an action that holds no number in its range. */
export interface NumberProblem {
  /** JSON path of the value (`rules[0].action.plan.steps[2].operation.ms`). */
  path: string;
  /** The field, as the editor names it. */
  label: string;
  range: NumberRange;
}

const join = (path: string, key: string): string => (path ? `${path}.${key}` : key);

function check(
  value: unknown,
  range: NumberRange,
  path: string,
  label: string,
  out: NumberProblem[],
): void {
  if (!inRange(range, value)) out.push({ path, label, range });
}

function keyProblems(key: unknown, path: string, out: NumberProblem[]): void {
  if (isRecord(key) && key.type === 'windows_code')
    check(key.code, ranges.windowsKeyCode, join(path, 'code'), '键码', out);
}

function keysProblems(keys: unknown, path: string, out: NumberProblem[]): void {
  if (Array.isArray(keys)) keys.forEach((key, i) => keyProblems(key, `${path}[${String(i)}]`, out));
}

/** Numbers of `reusable.parameters`: any JSON number within the IPC number contract. */
function parameterProblems(parameters: unknown, path: string, out: NumberProblem[]): void {
  if (!isRecord(parameters)) return;
  for (const [name, value] of Object.entries(parameters))
    if (typeof value === 'number' || isInvalidNumber(value))
      check(value, ranges.json, join(path, name), `参数「${name}」的值`, out);
}

function operationProblems(operation: unknown, path: string, out: NumberProblem[]): void {
  if (!isRecord(operation)) return;
  switch (operation.type) {
    case 'delay':
      check(operation.ms, ranges.delayMs, join(path, 'ms'), '等待时间（毫秒）', out);
      break;
    case 'mouse_move': {
      const range = operation.percent === true ? ranges.percent : ranges.coordinate;
      check(operation.x, range, join(path, 'x'), '左右位置 X', out);
      check(operation.y, range, join(path, 'y'), '上下位置 Y', out);
      break;
    }
    case 'wheel':
      check(operation.amount, ranges.wheelAmount, join(path, 'amount'), '滚动量', out);
      break;
    case 'key_down':
    case 'key_up':
    case 'key_press':
      keyProblems(operation.key, join(path, 'key'), out);
      break;
    case 'hotkey':
      keysProblems(operation.keys, join(path, 'keys'), out);
      break;
    case 'reusable':
      parameterProblems(operation.parameters, join(path, 'parameters'), out);
      break;
  }
}

/** The number fields of `action` that hold no number in their range; paths start at `path`. */
export function actionNumberProblems(action: unknown, path = ''): NumberProblem[] {
  const out: NumberProblem[] = [];
  if (!isRecord(action)) return out;
  switch (action.type) {
    case 'hotkey':
      keysProblems(action.keys, join(path, 'keys'), out);
      break;
    case 'steps':
    case 'replay': {
      const steps = isRecord(action.plan) ? action.plan.steps : undefined;
      if (Array.isArray(steps))
        steps.forEach((step, i) => {
          const at = join(path, `plan.steps[${String(i)}]`);
          operationProblems(isRecord(step) ? step.operation : undefined, `${at}.operation`, out);
        });
      if (action.type === 'replay')
        check(action.speed, ranges.replaySpeed, join(path, 'speed'), '倍速', out);
      break;
    }
    case 'reusable':
      parameterProblems(action.parameters, join(path, 'parameters'), out);
      break;
  }
  return out;
}

/** A diagnostic of the editor's own checks, shaped like the backend's (ipc-contract §2). */
export interface EditDiagnostic {
  code: 'number_range' | 'not_savable';
  message: string;
  path: string;
  span: { line: number; column: number; length: number };
  severity: 'error';
}

const diagnostic = (
  code: EditDiagnostic['code'],
  message: string,
  path: string,
): EditDiagnostic => ({
  code,
  message,
  path,
  span: { line: 0, column: 0, length: 0 },
  severity: 'error',
});

const toDiagnostic = ({ path, label, range }: NumberProblem): EditDiagnostic =>
  diagnostic('number_range', `${phrase(`${label}应为`, rangeText(range))}。`, path);

/** For a value the specific checks miss: the backend could not read it as the editor means it. */
const NOT_SAVABLE = '含有无法识别的内容，请检查后再试。';

/** Where `value` holds a number JSON cannot carry. A marker is named by its own path. */
const unsendableAt = (value: unknown, root: string): string | null =>
  unsendablePath(value, root)?.replace(/\.nan$/, '') ?? null;

/**
 * Why the edited rule, at position `index` of the config, cannot be saved or tested as it is:
 * number fields holding no number in their range; else an invalid-number marker or other number
 * JSON cannot carry anywhere else in it; else a shape the backend would not read as the editor
 * means it (`isReadableRule`). Empty when it can.
 */
export function ruleEditDiagnostics(rule: unknown, index: number): EditDiagnostic[] {
  const base = `rules[${String(index)}]`;
  const found = actionNumberProblems(isRecord(rule) ? rule.action : undefined, `${base}.action`);
  if (found.length > 0) return found.map(toDiagnostic);
  const unsendable = unsendableAt(rule, base);
  if (unsendable !== null) return [diagnostic('not_savable', NOT_SAVABLE, unsendable)];
  return isReadableRule(rule) ? [] : [diagnostic('not_savable', NOT_SAVABLE, base)];
}

/** `ruleEditDiagnostics` for an action on its own (「测试动作」); paths start at the action. */
export function actionDiagnostics(action: unknown): EditDiagnostic[] {
  const found = actionNumberProblems(action);
  if (found.length > 0) return found.map(toDiagnostic);
  const unsendable = unsendableAt(action, '');
  if (unsendable !== null) return [diagnostic('not_savable', NOT_SAVABLE, unsendable)];
  return isAction(action) && !hasUnsafeNumber(action)
    ? []
    : [diagnostic('not_savable', NOT_SAVABLE, '')];
}
