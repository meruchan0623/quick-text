import { useEffect, useId, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { actionGuides } from './actionHelp';

type Option = { value: string; label: string; disabled: boolean };
type Props = {
  options: Option[];
  value: string;
  onChange: (value: string) => void;
  platform?: string | undefined;
};

/** Keep keyboard focus on the combobox while browsing; selection only changes on confirmation. */
export function ActionTypePicker({ options, value, onChange, platform }: Props) {
  const id = useId();
  const trigger = useRef<HTMLButtonElement>(null);
  const popup = useRef<HTMLDivElement>(null);
  const [portalHost, setPortalHost] = useState<Element>(document.body);
  const [placement, setPlacement] = useState<{
    left: number;
    top: number;
    width: number;
    height: number;
  } | null>(null);
  const [active, setActive] = useState(value);
  const selected = options.find((option) => option.value === value);
  const guide = actionGuides[active] ?? actionGuides.none;
  const open = placement !== null;
  const close = () => setPlacement(null);
  const expand = () => {
    const rect = trigger.current?.getBoundingClientRect();
    if (!rect) return;
    setPortalHost(trigger.current?.closest('dialog') ?? document.body);
    const width = Math.min(860, window.innerWidth - 16);
    const height = Math.min(520, window.innerHeight - 24);
    setPlacement({
      left: Math.max(8, Math.min(rect.right - width, window.innerWidth - width - 8)),
      top: Math.max(8, Math.min(rect.bottom + 6, window.innerHeight - height - 8)),
      width,
      height,
    });
    setActive(value);
  };
  const confirm = (option: Option) => {
    if (option.disabled) return;
    onChange(option.value);
    close();
    trigger.current?.focus();
  };
  useEffect(() => {
    if (!open) return;
    const outside = (event: PointerEvent) => {
      if (
        event.target instanceof Node &&
        !popup.current?.contains(event.target) &&
        !trigger.current?.contains(event.target)
      )
        close();
    };
    const dismiss = () => close();
    document.addEventListener('pointerdown', outside, true);
    window.addEventListener('resize', dismiss);
    return () => {
      document.removeEventListener('pointerdown', outside, true);
      window.removeEventListener('resize', dismiss);
    };
  }, [open]);
  useEffect(() => {
    const option = popup.current?.querySelector('[data-active="true"]');
    if (open && option && typeof option.scrollIntoView === 'function')
      option.scrollIntoView({ block: 'nearest' });
  }, [open, active]);
  return (
    <>
      <button
        ref={trigger}
        type="button"
        value={value}
        className="action-type-trigger"
        role="combobox"
        aria-label="操作类型"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        aria-activedescendant={open ? `${id}-${active}` : undefined}
        onClick={() => (open ? close() : expand())}
        onBlur={() => close()}
        onKeyDown={(event) => {
          if (event.key === 'Escape' && open) {
            event.preventDefault();
            event.stopPropagation();
            close();
          } else if (event.key === 'Tab') close();
          else if (['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
            event.preventDefault();
            if (!open) expand();
            else {
              const index = options.findIndex((option) => option.value === active);
              const next =
                event.key === 'Home'
                  ? 0
                  : event.key === 'End'
                    ? options.length - 1
                    : (index + (event.key === 'ArrowDown' ? 1 : -1) + options.length) %
                      options.length;
              if (options[next]) setActive(options[next].value);
            }
          } else if ((event.key === 'Enter' || event.key === ' ') && open) {
            event.preventDefault();
            const option = options.find((item) => item.value === active);
            if (option) confirm(option);
          }
        }}
      >
        <span>{selected?.label ?? value}</span>
        <span aria-hidden="true">▾</span>
      </button>
      {placement &&
        createPortal(
          <div ref={popup} className="action-type-popup" style={placement}>
            <section className="action-guide" aria-label="操作使用说明">
              <h3>{options.find((option) => option.value === active)?.label}</h3>
              <p>
                <strong>作用</strong>
                {guide.purpose}
              </p>
              <strong>怎么设置</strong>
              <ol>
                {guide.steps.map((step) => (
                  <li key={step}>{step}</li>
                ))}
              </ol>
              <p>
                <strong>例子</strong>
                {guide.example}
              </p>
              <strong>特别设置与注意事项</strong>
              <ul>
                {guide.special.map((note) => (
                  <li key={note}>{note}</li>
                ))}
              </ul>
              {platform === 'windows' && (
                <p className="action-windows-note">
                  Windows：切换目标、继续按键或停止动作会中断后续输入；已执行的操作无法自动撤销。输入框不提供正文时，结果可能显示“目标处理结果未知”，请查看目标是否收到。
                </p>
              )}
              <small>设置完成后，点击编辑窗口的“保存”即可立即生效。</small>
            </section>
            <div id={id} role="listbox" aria-label="操作类型选项" className="action-type-options">
              {options.map((option) => (
                <div
                  key={option.value}
                  id={`${id}-${option.value}`}
                  role="option"
                  aria-selected={value === option.value}
                  aria-disabled={option.disabled}
                  data-value={option.value}
                  data-active={active === option.value}
                  onMouseDown={(event) => event.preventDefault()}
                  onMouseMove={() => setActive(option.value)}
                  onClick={() => confirm(option)}
                >
                  {option.label}
                </div>
              ))}
            </div>
          </div>,
          portalHost,
        )}
    </>
  );
}
