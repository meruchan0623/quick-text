// Structural check of a `rules` entry (B-43). It mirrors the serde model in
// crates/domain/src/model.rs: an entry is a readable rule only when every nested field the list and
// the editor read has the shape the backend produces. Anything else is an unreadable entry, listed
// read-only and written back unchanged, so the UI never calls a string or array method on a value
// that is not one. That includes an action, step operation or key type this version does not
// know: the backend could not have read it either, and the editor has no form for it.
//
// The backend serialises every field of a rule it read, so a field is required here unless the UI
// reads it with a fallback (`?? []`) and serde has a default for it (`optional`).

/** A runtime type check for one JSON value. */
export type Guard = (value: unknown) => boolean;

export const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

const str: Guard = (value) => typeof value === 'string';
const bool: Guard = (value) => typeof value === 'boolean';
/** JSON numbers are finite; NaN or Infinity cannot have come from the backend. */
const num: Guard = (value) => typeof value === 'number' && Number.isFinite(value);
const record: Guard = isRecord;

const oneOf =
  (...values: string[]): Guard =>
  (value) =>
    typeof value === 'string' && values.includes(value);

/** `Option<T>`: `null` or a T. */
const nullable =
  (guard: Guard): Guard =>
  (value) =>
    value === null || guard(value);

/** A field with a serde default that the UI reads with a fallback: absent, or a valid value. */
const optional =
  (guard: Guard): Guard =>
  (value) =>
    value === undefined || guard(value);

const arrayOf =
  (guard: Guard): Guard =>
  (value) =>
    Array.isArray(value) && value.every(guard);

/** An object with at least these fields (unknown extra keys are a newer version's and kept). */
function shape(fields: Record<string, Guard>): Guard {
  const entries = Object.entries(fields);
  return (value) => isRecord(value) && entries.every(([name, guard]) => guard(value[name]));
}

/** A serde enum tagged by `type`; an unknown `type` does not pass. */
function tagged(variants: Record<string, Record<string, Guard>>): Guard {
  // A Map, so that `type: "constructor"` and friends are not found on Object.prototype.
  const byType = new Map(Object.entries(variants).map(([type, fields]) => [type, shape(fields)]));
  return (value) => {
    if (!isRecord(value) || typeof value.type !== 'string') return false;
    const guard = byType.get(value.type);
    return guard !== undefined && guard(value);
  };
}

const strings = arrayOf(str);
const outputMethod = oneOf('keys', 'paste');

const key = tagged({
  named: { name: str },
  windows_code: { code: num },
  platform_primary: {},
});
const keys = arrayOf(key);

const contentSource = tagged({
  inline: { value: str },
  file: { path: str, encoding: optional(oneOf('utf8', 'utf16_le', 'utf16_be', 'gbk')) },
});

const operation = tagged({
  text: { text: str, method: outputMethod },
  key_down: { key },
  key_up: { key },
  key_press: { key },
  hotkey: { keys },
  delay: { ms: num },
  mouse_move: { x: num, y: num, relative: optional(bool), percent: optional(bool) },
  mouse_button: {
    button: oneOf('left', 'right', 'middle', 'x1', 'x2'),
    state: oneOf('click', 'down', 'up', 'double_click'),
  },
  wheel: { axis: oneOf('vertical', 'horizontal'), amount: num, unit: oneOf('clicks', 'delta') },
  paste_files: { paths: strings },
  paste_image: { path: str },
  html: { html: str, plain_text: str },
  open: { target: str, args: strings, working_directory: nullable(str) },
  common: { function: str },
  reusable: { action_id: str, parameters: record },
});

/** `target` has a serde default, but the step editor reads `target.type`, so it must be there. */
const step = shape({
  operation,
  target: tagged({ locked: {}, declared: { process: str } }),
});

const plan = shape({ steps: arrayOf(step) });

export const isAction = tagged({
  none: {},
  text: { method: outputMethod, source: contentSource },
  html: { html: str, plain_text: str },
  hotkey: { keys },
  send_keys: { source: contentSource, syntax_variables: optional(strings) },
  steps: { plan },
  replay: { plan, speed: num },
  open: { target: str, args: strings, working_directory: optional(nullable(str)) },
  reusable: { action_id: str, parameters: optional(record) },
  common: { function: str },
  quicker: { action_id: str, parameter: str },
});

const parameter = tagged({
  default_match: {},
  fixed: { value: str },
  first_capture: {},
  capture_template: { template: str },
  null: {},
});

/** Every field of a rule as the list and the editor read it. */
export const isRuleShape = shape({
  id: str,
  enabled: bool,
  group: str,
  description: str,
  ignore_case: bool,
  match: shape({ kind: oneOf('literal', 'regex'), value: str }),
  trigger_mode: oneOf('terminator', 'immediate'),
  terminator: nullable(shape({ keys: strings, consume: bool })),
  ime_erasure: nullable(oneOf('escape', 'backspace')),
  action: isAction,
  parameter,
  notification: shape({ enabled: bool, message: str }),
});
