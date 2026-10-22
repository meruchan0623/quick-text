import { useState, type TextareaHTMLAttributes } from 'react';
import { parseLines, sameItems } from './lib/text';

type Props = Omit<TextareaHTMLAttributes<HTMLTextAreaElement>, 'value' | 'onChange'> & {
  value: readonly string[];
  onChange: (lines: string[]) => void;
};

/**
 * A textarea holding one entry per line (F-04): the typed text is kept as is, so a trailing
 * newline survives and the next line can be appended; only the parsed list goes to `onChange`.
 */
export function LinesInput({ value, onChange, ...rest }: Props) {
  const [text, setText] = useState(() => value.join('\n'));
  const [seen, setSeen] = useState(value);
  // A list that is not what the text parses to came from elsewhere (reload, 「选择当前窗口」).
  if (seen !== value) {
    setSeen(value);
    if (!sameItems(parseLines(text), value)) setText(value.join('\n'));
  }
  return (
    <textarea
      {...rest}
      value={text}
      onChange={(e) => {
        setText(e.target.value);
        onChange(parseLines(e.target.value));
      }}
    />
  );
}
