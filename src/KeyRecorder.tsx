import { useEffect, useRef, useState } from 'react';
import {
  appendKey,
  appendText,
  itemLabel,
  keyFromEvent,
  parseRecording,
  recordingSource,
  TOKEN_LABELS,
  undoLast,
  type Recorded,
} from './lib/sendKeys';

const TOKENS = ['cliptext', 'selection', '0:yyyy-MM-dd', '0:HH:mm:ss'];

/**
 * 「文字与按键组合」 as a recording: what is typed and pressed in the box is kept as text and key
 * blocks, until 「停止录制」. Every key is kept, Esc and Backspace included; only the button
 * stops. The blocks are read from `value`, the SendKeys source (./lib/sendKeys.ts), each time.
 */
export function KeyRecorder({
  value,
  onChange,
}: {
  value: string;
  onChange: (source: string) => void;
}) {
  const items = parseRecording(value);
  const [recording, setRecording] = useState(false);
  const [focused, setFocused] = useState(false);
  const [notice, setNotice] = useState('');
  const box = useRef<HTMLInputElement>(null);
  const composing = useRef(false);
  useEffect(() => {
    if (recording) box.current?.focus();
  }, [recording]);
  const write = (next: Recorded[]) => onChange(recordingSource(next));
  // Text the box got, moved into the blocks; the box itself stays empty.
  const take = (input: HTMLInputElement) => {
    const text = input.value;
    input.value = '';
    if (text && items) write(appendText(items, text));
  };
  const resume = () => {
    if (recording) box.current?.focus();
  };
  return (
    <div className="recorder">
      <div className="recorder-blocks" aria-label="录下的内容">
        {items === null ? (
          <p className="recorder-note" role="note">
            这条指令用了录制框无法显示的写法，请在下方“高级”里修改，或点“清空”后重新录制。
          </p>
        ) : items.length === 0 ? (
          <span className="recorder-empty">
            还没有内容。点“开始录制”，然后像平时一样打字、按键。
          </span>
        ) : (
          items.map((item, i) => {
            const label = itemLabel(item);
            return (
              <span key={i} className={`recorded ${item.kind}`}>
                <span className="recorded-label">{label}</span>
                <button
                  type="button"
                  aria-label={`删除“${label}”`}
                  onClick={() => {
                    write(items.filter((_, j) => j !== i));
                    resume();
                  }}
                >
                  ×
                </button>
              </span>
            );
          })
        )}
      </div>
      {recording && (
        <div className="recorder-capture">
          <input
            ref={box}
            aria-label="录制区"
            placeholder="在这里打字、按键；回车、Tab、退格、Esc 也会录下"
            onFocus={() => setFocused(true)}
            onBlur={() => setFocused(false)}
            onCompositionStart={() => {
              composing.current = true;
            }}
            onCompositionEnd={(e) => {
              composing.current = false;
              take(e.currentTarget);
            }}
            onChange={(e) => {
              // An input method's text arrives when it is committed (compositionend).
              if (!composing.current) take(e.currentTarget);
            }}
            onKeyDown={(e) => {
              if (composing.current || e.nativeEvent.isComposing || e.key === 'Process') return;
              const pressed = keyFromEvent(e);
              if (pressed === null || pressed === 'text') return;
              // Kept from the page: Tab stays here, and Esc does not close the editor.
              e.preventDefault();
              e.stopPropagation();
              if (pressed === 'unsupported') {
                setNotice(
                  'Windows 键、媒体键和 F17 以上的功能键录不了；需要时请改用“按顺序执行多个操作”。',
                );
                return;
              }
              setNotice('');
              if (items) write(appendKey(items, pressed));
            }}
          />
          {!focused && (
            <button type="button" className="recorder-paused" onClick={resume}>
              已暂停，点这里继续录制
            </button>
          )}
        </div>
      )}
      {notice && (
        <small className="recorder-notice" role="status">
          {notice}
        </small>
      )}
      <div className="inline">
        <button
          type="button"
          className={recording ? 'danger' : 'primary'}
          disabled={!recording && items === null}
          onClick={() => {
            setRecording(!recording);
            setNotice('');
          }}
        >
          {recording ? '■ 停止录制' : '● 开始录制'}
        </button>
        <button
          type="button"
          disabled={!items?.length}
          onClick={() => {
            if (items) write(undoLast(items));
            resume();
          }}
        >
          撤销上一步
        </button>
        <button
          type="button"
          disabled={!value}
          onClick={() => {
            onChange('');
            resume();
          }}
        >
          清空
        </button>
      </div>
      <div className="inline">
        {TOKENS.map((token) => (
          <button
            type="button"
            key={token}
            onClick={() => {
              onChange(value + `{${token}}`);
              resume();
            }}
          >
            插入{TOKEN_LABELS[token]}
          </button>
        ))}
      </div>
      <small>
        这些按钮会加入一块标记，执行指令时再换成当时的文字、日期或时间。录制时按下的每个键都会照录，执行时也会照按；录错了点块上的
        × 删除，或点“撤销上一步”。
      </small>
    </div>
  );
}
