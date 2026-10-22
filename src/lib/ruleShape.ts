// Structural check of a `rules` entry (B-43). It mirrors the serde model in
// crates/domain/src/model.rs field by field: an entry is a readable rule only when Rust would read
// it the same way and the editor has a form for every value in it. Anything else is an unreadable
// entry, listed read-only and written back unchanged, so the UI never calls a string or array method
// on a value that is not one and never rebuilds an object it does not fully know.
//
// - Unknown keys. Every serde type has `deny_unknown_fields`; the backend keeps a newer version's
//   keys only on the rule itself (and on the config and the globals, which this file does not
//   check), and the editor keeps rule-level keys in place. A nested object (match, terminator,
//   notification, action, source, plan, step, span, target, operation, key, parameter) with a key
//   this version does not know makes the whole entry unreadable: the editor rebuilds those objects
//   (「添加普通文字」 replaces `source`, for one) and would drop the key on save.
// - Absent fields. A field with a serde default (`#[serde(default)]` on the field or the struct)
//   or of an `Option` type may be absent; `withRuleDefaults` fills in the same default Rust would.
//   A field without one must be there. `null` is only valid for an `Option`.
// - Numbers. Integers are checked against their Rust type (u16, u32, i32, u64/usize) and floats must
//   be finite. A u64 past 2^53 is not readable: JavaScript cannot write it back exactly.

/** A runtime type check for one JSON value. */
export type Guard = (value: unknown) => boolean;

export const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

/** The check for one JSON value, and how to fill in the serde defaults of a value that passed it. */
interface Spec {
  check: Guard;
  /** The value with every absent defaulted field filled in; the same value when none was absent. */
  fill: (value: unknown) => unknown;
}

/** A field of a struct or enum variant; `missing` is its serde default (absent: required). */
interface Field {
  spec: Spec;
  missing?: () => unknown;
}

const same = (value: unknown): unknown => value;
const leaf = (check: Guard): Spec => ({ check, fill: same });

const str = leaf((value) => typeof value === 'string');
const bool = leaf((value) => typeof value === 'boolean');
/** f64: JSON numbers are finite; NaN or Infinity cannot have come from the backend. */
const f64 = leaf((value) => typeof value === 'number' && Number.isFinite(value));
const integer = (min: number, max: number): Spec =>
  leaf(
    (value) => typeof value === 'number' && Number.isInteger(value) && value >= min && value <= max,
  );
const u16 = integer(0, 0xffff);
const u32 = integer(0, 0xffff_ffff);
const i32 = integer(-0x8000_0000, 0x7fff_ffff);
/** u64 and usize, as far as JavaScript can represent them exactly. */
const u64 = integer(0, Number.MAX_SAFE_INTEGER);

const oneOf = (...values: string[]): Spec =>
  leaf((value) => typeof value === 'string' && values.includes(value));

const arrayOf = (spec: Spec): Spec => ({
  check: (value) => Array.isArray(value) && value.every(spec.check),
  fill: (value) => {
    const items = value as unknown[];
    const filled = items.map(spec.fill);
    return filled.every((item, i) => item === items[i]) ? items : filled;
  },
});

/** A JSON object used as a map (`BTreeMap<String, _>`): any keys, values checked by `spec`. */
const mapOf = (spec: Spec): Spec =>
  leaf((value) => isRecord(value) && Object.values(value).every(spec.check));

/** Any JSON value (`serde_json::Value`). */
const json = leaf(() => true);
/** A typed parameter value the reusable-action form can show and edit. */
const scalar = leaf(
  (value) =>
    value === null ||
    typeof value === 'string' ||
    typeof value === 'boolean' ||
    (typeof value === 'number' && Number.isFinite(value)),
);

/** A field with a serde default: absent is read as `missing()`. */
const withDefault = (spec: Spec, missing: () => unknown): Field => ({ spec, missing });

/** An `Option<T>` field: absent or `null` is `None`. */
const option = (spec: Spec): Field => ({
  spec: {
    check: (value) => value === null || spec.check(value),
    fill: (v) => (v === null ? v : spec.fill(v)),
  },
  missing: () => null,
});

type Fields = Record<string, Spec | Field>;

const asField = (field: Spec | Field): Field => ('spec' in field ? field : { spec: field });

/** JSON has no `undefined`; a key holding it is sent (and so read by Rust) as absent. */
const has = (value: Record<string, unknown>, key: string): boolean =>
  Object.hasOwn(value, key) && value[key] !== undefined;

/**
 * A struct with these fields. Unknown keys make it invalid (`deny_unknown_fields`), except with
 * `extras`: then they are a newer version's and kept, in place.
 */
function shape(fields: Fields, { extras = false } = {}): Spec {
  const byName = new Map(Object.entries(fields).map(([name, field]) => [name, asField(field)]));
  return {
    check: (value) =>
      isRecord(value) &&
      (extras || Object.keys(value).every((key) => byName.has(key) || value[key] === undefined)) &&
      [...byName].every(([name, field]) =>
        has(value, name) ? field.spec.check(value[name]) : field.missing !== undefined,
      ),
    fill: (value) => {
      const record = value as Record<string, unknown>;
      let changed = false;
      // Object.fromEntries rather than assignment, so that a `__proto__` key stays a plain key.
      const entries = Object.entries(record).map(([key, item]): [string, unknown] => {
        const field = byName.get(key);
        const filled = field && item !== undefined ? field.spec.fill(item) : item;
        if (filled !== item) changed = true;
        return [key, filled];
      });
      const out = Object.fromEntries(entries);
      for (const [name, field] of byName) {
        if (field.missing && !has(record, name)) {
          out[name] = field.missing();
          changed = true;
        }
      }
      return changed ? out : record;
    },
  };
}

/** A serde enum tagged by `type`; an unknown `type` does not pass. Variants are strict structs. */
function tagged(variants: Record<string, Fields>): Spec {
  // A Map, so that `type: "constructor"` and friends are not found on Object.prototype.
  const byType = new Map(
    Object.entries(variants).map(([type, fields]) => [type, shape({ type: str, ...fields })]),
  );
  const variant = (value: unknown): Spec | undefined =>
    isRecord(value) && typeof value.type === 'string' ? byType.get(value.type) : undefined;
  return {
    check: (value) => variant(value)?.check(value) ?? false,
    fill: (value) => variant(value)?.fill(value) ?? value,
  };
}

const strings = arrayOf(str);
const outputMethod = oneOf('keys', 'paste');

const key = tagged({
  named: { name: str },
  windows_code: { code: u16 },
  platform_primary: {},
});
const keys = arrayOf(key);

const contentSource = tagged({
  inline: { value: str },
  file: {
    path: str,
    encoding: withDefault(oneOf('utf8', 'utf16_le', 'utf16_be', 'gbk'), () => 'utf8'),
  },
});

// The step editor has no form for the open, common, reusable and html operations; it keeps them
// as they are, so their values are only checked as far as Rust reads them.
const operation = tagged({
  text: { text: str, method: outputMethod },
  key_down: { key },
  key_up: { key },
  key_press: { key },
  hotkey: { keys },
  delay: { ms: u64 },
  mouse_move: {
    x: f64,
    y: f64,
    relative: withDefault(bool, () => false),
    percent: withDefault(bool, () => false),
  },
  mouse_button: {
    button: oneOf('left', 'right', 'middle', 'x1', 'x2'),
    state: oneOf('click', 'down', 'up', 'double_click'),
  },
  wheel: { axis: oneOf('vertical', 'horizontal'), amount: i32, unit: oneOf('clicks', 'delta') },
  paste_files: { paths: strings },
  paste_image: { path: str },
  html: { html: str, plain_text: str },
  open: { target: str, args: strings, working_directory: option(str) },
  common: { function: str },
  reusable: { action_id: str, parameters: mapOf(json) },
});

const span = shape({
  line: withDefault(u64, () => 0),
  column: withDefault(u64, () => 0),
  length: withDefault(u64, () => 0),
});

const step = shape({
  operation,
  span: withDefault(span, () => ({ line: 0, column: 0, length: 0 })),
  target: withDefault(tagged({ locked: {}, declared: { process: str } }), () => ({
    type: 'locked',
  })),
});

const plan = shape({
  version: withDefault(u32, () => 1),
  dialect: withDefault(
    oneOf('structured', 'send_keys', 'inputscript', 'replay'),
    () => 'structured',
  ),
  source: option(str),
  steps: withDefault(arrayOf(step), () => []),
});

const action = tagged({
  none: {},
  text: { method: outputMethod, source: contentSource },
  html: { html: str, plain_text: str },
  hotkey: { keys },
  send_keys: { source: contentSource, syntax_variables: withDefault(strings, () => []) },
  steps: { plan },
  replay: { plan, speed: f64 },
  open: { target: str, args: withDefault(strings, () => []), working_directory: option(str) },
  // Rust takes any JSON value; the form edits text, numbers, yes/no and empty values only.
  reusable: { action_id: str, parameters: withDefault(mapOf(scalar), () => ({})) },
  common: { function: str },
  quicker: { action_id: str, parameter: withDefault(str, () => '') },
});

const parameter = tagged({
  default_match: {},
  fixed: { value: str },
  first_capture: {},
  capture_template: { template: str },
  null: {},
});

/** Rust's `Terminator::default()` keys (not the UI's DEFAULT_TERMINATORS for a new config). */
const serdeTerminators = ['space', 'tab', 'enter', 'semicolon'];

const rule = shape(
  {
    id: withDefault(str, () => ''),
    enabled: withDefault(bool, () => true),
    group: withDefault(str, () => ''),
    description: withDefault(str, () => ''),
    ignore_case: withDefault(bool, () => false),
    match: withDefault(shape({ kind: oneOf('literal', 'regex'), value: str }), () => ({
      kind: 'literal',
      value: '',
    })),
    trigger_mode: withDefault(oneOf('terminator', 'immediate'), () => 'terminator'),
    terminator: option(
      shape({
        keys: withDefault(strings, () => [...serdeTerminators]),
        consume: withDefault(bool, () => true),
      }),
    ),
    ime_erasure: option(oneOf('escape', 'backspace')),
    action: withDefault(action, () => ({ type: 'none' })),
    parameter: withDefault(parameter, () => ({ type: 'default_match' })),
    notification: withDefault(
      shape({ enabled: withDefault(bool, () => false), message: withDefault(str, () => '') }),
      () => ({ enabled: false, message: '' }),
    ),
  },
  { extras: true },
);

/** Whether an action has the shape the editor handles (see the top of this file). */
export const isAction: Guard = action.check;

/** Whether a `rules` entry is a rule Rust reads and the editor handles (see the top of this file). */
export const isRuleShape: Guard = rule.check;

/**
 * A rule that passed `isRuleShape`, with the serde default of every absent field filled in at every
 * level, as Rust reads it. Returns the same object when nothing was absent; rule-level keys keep
 * their order and filled-in fields are appended.
 */
export const withRuleDefaults = <T>(entry: T): T => rule.fill(entry) as T;
