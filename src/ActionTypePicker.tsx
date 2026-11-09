import { useEffect, useId, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { actionNames } from './model';

type Option = { value: string; label: string; disabled: boolean };
type Props = {
  options: Option[];
  value: string;
  onChange: (value: string) => void;
};

/** Keep keyboard focus on the combobox while browsing; selection only changes on confirmation. */
export function ActionTypePicker({ options, value, onChange }: Props) {
  const id = useId();
  const trigger = useRef<HTMLButtonElement>(null);
  const popup = useRef<HTMLDivElement>(null);
  const [portalHost, setPortalHost] = useState<Element>(document.body);
  const [placement, setPlacement] = useState<{
    left: number;
    top: number;
    width: number;
    maxHeight: number;
  } | null>(null);
  const [active, setActive] = useState(value);
  const selected = options.find((option) => option.value === value);
  const open = placement !== null;
  const close = () => setPlacement(null);
  const expand = () => {
    const rect = trigger.current?.getBoundingClientRect();
    if (!rect) return;
    setPortalHost(trigger.current?.closest('dialog') ?? document.body);
    const width = Math.min(Math.max(rect.width, 280), window.innerWidth - 16);
    // About one 40px row per option, so a short list is not lifted above the trigger.
    const height = Math.min(options.length * 40 + 14, 420, window.innerHeight - 24);
    setPlacement({
      left: Math.max(8, Math.min(rect.right - width, window.innerWidth - width - 8)),
      top: Math.max(8, Math.min(rect.bottom + 6, window.innerHeight - height - 8)),
      width,
      maxHeight: height,
    });
    setActive(
      options.some((option) => option.value === value) ? value : (options[0]?.value ?? value),
    );
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
        <span>{selected?.label ?? actionNames[value] ?? value}</span>
        <span aria-hidden="true">▾</span>
      </button>
      {placement &&
        createPortal(
          <div ref={popup} className="action-type-popup" style={placement}>
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
