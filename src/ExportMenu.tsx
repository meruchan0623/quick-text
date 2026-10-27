import { useEffect, useRef, useState } from 'react';

export default function ExportMenu({
  disabled,
  excelDisabled,
  onExport,
}: {
  disabled: boolean;
  excelDisabled: boolean;
  onExport: (format: 'json' | 'xlsx') => void;
}) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const toggle = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (!open) return;
    const outside = (event: PointerEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener('pointerdown', outside);
    return () => document.removeEventListener('pointerdown', outside);
  }, [open]);
  const choose = (format: 'json' | 'xlsx') => {
    setOpen(false);
    toggle.current?.focus();
    onExport(format);
  };
  return (
    <div
      className="export-menu"
      ref={root}
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false);
      }}
      onKeyDown={(event) => {
        if (event.key === 'Escape') {
          setOpen(false);
          toggle.current?.focus();
        }
      }}
    >
      <button disabled={disabled} onClick={() => choose('json')} title="导出当前配置为 JSON 文件">
        导出
      </button>
      <button
        ref={toggle}
        disabled={disabled}
        aria-label="选择导出格式"
        aria-expanded={open && !disabled}
        onClick={() => setOpen(!open)}
      >
        ▾
      </button>
      {open && !disabled && (
        <div className="export-options" role="group" aria-label="导出文件格式">
          <button onClick={() => choose('json')}>导出 JSON 文件</button>
          <button disabled={excelDisabled} onClick={() => choose('xlsx')}>
            导出 Excel 文件
          </button>
        </div>
      )}
    </div>
  );
}
