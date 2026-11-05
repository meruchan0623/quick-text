import { useEffect, useRef, useState } from 'react';

export default function ExportMenu({
  disabled,
  excelDisabled,
  onExport,
}: {
  disabled: boolean;
  excelDisabled: boolean;
  onExport: (format: 'zip' | 'xlsx' | 'template') => void;
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
  const choose = (format: 'zip' | 'xlsx' | 'template') => {
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
      <button disabled={disabled} onClick={() => choose('zip')} title="导出当前配置为 ZIP 配置包">
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
          <button onClick={() => choose('zip')}>导出配置包（ZIP）</button>
          <button disabled={excelDisabled} onClick={() => choose('xlsx')}>
            导出 Excel 文件
          </button>
          <button disabled={excelDisabled} onClick={() => choose('template')}>
            导出 Excel 填写模板
          </button>
        </div>
      )}
    </div>
  );
}
