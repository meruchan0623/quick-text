import { useLayoutEffect, useRef } from 'react';
import { layout, type HintItem, type HintPayload } from '../lib/hint';

function Abbreviation({ text, typed }: { text: string; typed: number }) {
  const chars = Array.from(text);
  return (
    <span className="hint-abbr">
      <b>{chars.slice(0, typed).join('')}</b>
      {chars.slice(typed).join('')}
    </span>
  );
}

function oneLine(text: string) {
  return text.replace(/\r?\n/g, ' ⏎ ');
}

function Row({
  item,
  typed,
  density,
  first,
}: {
  item: HintItem;
  typed: number;
  density: 'detail' | 'standard' | 'compact';
  first: boolean;
}) {
  const cls = `hint-item hint-${density}${first ? ' hint-top' : ''}`;
  if (density === 'compact')
    return (
      <div className={cls}>
        <Abbreviation text={item.abbreviation} typed={typed} />
        <span className="hint-preview">{oneLine(item.preview)}</span>
      </div>
    );
  return (
    <div className={cls}>
      <div className="hint-line">
        <Abbreviation text={item.abbreviation} typed={typed} />
        <span className={`hint-tag${item.source === 'team' ? '' : ' hint-tag-me'}`}>
          {item.source === 'team' ? '团队' : '我的'}
        </span>
        {item.trigger === 'terminator' && <span className="hint-trigger">空格触发</span>}
        {item.description && <span className="hint-desc">{item.description}</span>}
      </div>
      {density === 'detail' && !item.preview_is_label ? (
        <div className="hint-body-preview">{item.preview}</div>
      ) : (
        <div className="hint-preview">{oneLine(item.preview)}</div>
      )}
    </div>
  );
}

export function HintPanel({
  payload,
  onLaidOut,
}: {
  payload: HintPayload;
  onLaidOut: (height: number) => void;
}) {
  const panel = useRef<HTMLDivElement>(null);
  const { hint } = payload;
  useLayoutEffect(() => {
    if (hint && panel.current) onLaidOut(panel.current.getBoundingClientRect().height);
  }, [hint, onLaidOut]);
  if (!hint) return null;
  const { density, shown } = layout(hint.total, hint.items.length, payload.max_height);
  const typed = Array.from(hint.typed).length;
  const more = hint.total - shown;
  return (
    <div
      ref={panel}
      className="hint-panel"
      style={{ width: payload.width, maxHeight: payload.max_height }}
    >
      <div className="hint-inner">
        <div className="hint-head">
          <span className="hint-dot" aria-hidden="true" />
          快捷输入
          <span className="hint-typed">{hint.typed}</span>
          <span className="hint-count">{hint.total} 项</span>
        </div>
        {hint.items.slice(0, shown).map((item, i) => (
          <Row key={item.rule_id} item={item} typed={typed} density={density} first={i === 0} />
        ))}
        {more > 0 && <div className="hint-more">还有 {more} 项，继续输入以缩小范围</div>}
        <div
          key={payload.seq}
          className="hint-timer"
          style={{ animationDuration: `${payload.duration_ms}ms` }}
        />
      </div>
    </div>
  );
}
