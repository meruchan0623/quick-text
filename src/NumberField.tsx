// A number input that takes only what its Rust type and validation accept (src/lib/numberFields.ts).
import { useId, useState, type InputHTMLAttributes } from 'react';
import {
  numberText,
  parseNumber,
  rangeError,
  readNumber,
  type InvalidNumber,
} from './lib/numberFields';
import { inRange, type NumberRange } from './lib/ruleShape';

type Props = Omit<InputHTMLAttributes<HTMLInputElement>, 'type' | 'value' | 'onChange'> & {
  /** The value held in the rule: a number, or an InvalidNumber marker holding the text typed. */
  value: unknown;
  range: NumberRange;
  /** Called on every edit: the number typed, or the text marked while it is no number in `range`. */
  onChange: (value: number | InvalidNumber) => void;
};

/**
 * A text input, not `type="number"`: a number input reports "" for text it cannot read, and the
 * text typed must be kept as typed. Invalid text is kept in the rule (the marker), so the field
 * shows it again after a remount. Valid text stays as typed while it still stands for `value`
 * (「1e3」, 「0.50」); a value set from elsewhere (「使用鼠标现在的位置」, undo) replaces it. A
 * value out of `range`, typed or loaded, shows the inline error.
 */
export function NumberField({ value, range, onChange, ...input }: Props) {
  const errorId = useId();
  const [typed, setTyped] = useState<string | null>(null);
  const shown =
    typed !== null && typeof value === 'number' && parseNumber(typed, range) === value
      ? typed
      : numberText(value);
  const valid = inRange(range, value);
  return (
    <>
      <input
        type="text"
        inputMode={range.integer ? 'numeric' : 'decimal'}
        autoComplete="off"
        spellCheck={false}
        aria-invalid={valid ? undefined : true}
        aria-describedby={valid ? undefined : errorId}
        {...input}
        value={shown}
        onChange={(e) => {
          setTyped(e.target.value);
          onChange(readNumber(e.target.value, range));
        }}
      />
      {!valid && (
        <small id={errorId} className="number-error">
          {rangeError(range)}
        </small>
      )}
    </>
  );
}
