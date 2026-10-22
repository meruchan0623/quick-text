// A number input that takes only what its Rust type and validation accept (src/lib/numberFields.ts).
import { useId, useState, type InputHTMLAttributes } from 'react';
import { parseNumber, rangeError } from './lib/numberFields';
import { inRange, type NumberRange } from './lib/ruleShape';

type Props = Omit<InputHTMLAttributes<HTMLInputElement>, 'type' | 'value' | 'onChange'> & {
  /** The number held in the rule; NaN marks text that is no number in `range`. */
  value: unknown;
  range: NumberRange;
  /** Called on every edit: the number typed, or NaN while the text is no number in `range`. */
  onChange: (value: number) => void;
};

/** A bound worth offering to the browser's spinner; the f64 extremes are left out. */
const bound = (n: number): number | undefined => (Number.isSafeInteger(n) ? n : undefined);

/**
 * The text typed stays as typed while it still stands for `value` (「1e3」, 「0.50」, or invalid text
 * held as NaN); a value set from elsewhere (「使用鼠标现在的位置」) replaces it. A value out of
 * `range`, typed or loaded, shows the inline error.
 */
export function NumberField({ value, range, onChange, ...input }: Props) {
  const errorId = useId();
  const [typed, setTyped] = useState<string | null>(null);
  const shown =
    typed !== null && Object.is(parseNumber(typed, range) ?? NaN, value)
      ? typed
      : typeof value === 'number' && !Number.isNaN(value)
        ? String(value)
        : '';
  const valid = inRange(range, value);
  return (
    <>
      <input
        type="number"
        min={bound(range.min)}
        max={bound(range.max)}
        step={range.integer ? 1 : 'any'}
        aria-invalid={valid ? undefined : true}
        aria-describedby={valid ? undefined : errorId}
        {...input}
        value={shown}
        onChange={(e) => {
          setTyped(e.target.value);
          onChange(parseNumber(e.target.value, range) ?? NaN);
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
