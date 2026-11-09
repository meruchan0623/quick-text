// 「新手提示」 in 设置: turns the typing hint overlay on or off and sets how long it shows. Saved
// to prefs.json at once (set_candidate_hint), like 「启动时自动开始监听」; not part of the
// configuration, exports or team phrases.
import { useState } from 'react';
import { call } from './ipc/transport';
import { errorText } from './ipc/errors';
import { DEFAULT_HINT, HINT_SECONDS, parseSeconds, type CandidateHint } from './lib/hint';

export default function CandidateHintSection({
  stored,
  disabled,
}: {
  /** From load_state; undefined on a backend without the setting (the card is hidden). The
      parent keys the card by it, so a newer stored value starts it afresh. */
  stored: CandidateHint | undefined;
  disabled: boolean;
}) {
  const [value, setValue] = useState<CandidateHint>(stored ?? DEFAULT_HINT);
  const [seconds, setSeconds] = useState(String((stored ?? DEFAULT_HINT).duration_ms / 1000));
  const [error, setError] = useState('');
  const [saving, setSaving] = useState(false);
  if (!stored) return null;

  const save = async (next: CandidateHint) => {
    setSaving(true);
    setError('');
    try {
      const answer = await call<CandidateHint>('set_candidate_hint', {
        enabled: next.enabled,
        duration_ms: next.duration_ms,
      });
      setValue(answer);
      setSeconds(String(answer.duration_ms / 1000));
    } catch (e) {
      setError(errorText(e) ?? '保存失败');
    } finally {
      setSaving(false);
    }
  };
  const commitSeconds = () => {
    const parsed = parseSeconds(seconds);
    if (parsed === null) {
      setError(`请输入 ${HINT_SECONDS.min} 到 ${HINT_SECONDS.max} 之间的秒数`);
      return;
    }
    const duration_ms = parsed * 1000;
    if (duration_ms !== value.duration_ms) void save({ ...value, duration_ms });
    else setError('');
  };
  const preview = () => {
    setError('');
    call('preview_candidate_hint').catch((e: unknown) => setError(errorText(e) ?? '预览失败'));
  };

  return (
    <section className="hint-card" aria-label="新手提示">
      <h3>新手提示</h3>
      <div className="hint-row">
        <label className="switch">
          <input
            type="checkbox"
            checked={value.enabled}
            disabled={disabled || saving}
            onChange={(e) => void save({ ...value, enabled: e.target.checked })}
          />
          <span />
          输入时显示候选提示
        </label>
        <span className="hint-badge">适合新手</span>
        <p className="muted">
          记不住缩写时打开。输入缩写的开头几个字母后，屏幕右侧会短暂列出可用的指令，只提示，不会替你选择。
        </p>
      </div>
      {value.enabled && (
        <div className="hint-row hint-duration">
          <label>
            显示时长
            <input
              type="number"
              inputMode="decimal"
              min={HINT_SECONDS.min}
              max={HINT_SECONDS.max}
              step={HINT_SECONDS.step}
              value={seconds}
              disabled={disabled || saving}
              aria-invalid={error !== '' || undefined}
              onChange={(e) => {
                setSeconds(e.target.value);
                setError('');
              }}
              onBlur={commitSeconds}
              onKeyDown={(e) => {
                if (e.key === 'Enter') commitSeconds();
              }}
            />
            秒
          </label>
          <button type="button" disabled={disabled} onClick={preview}>
            预览效果
          </button>
          <p className="muted">停止输入后保留的时间，1–10 秒，默认 2 秒。</p>
        </div>
      )}
      {error && (
        <p className="hint-error" role="alert">
          {error}
        </p>
      )}
    </section>
  );
}
