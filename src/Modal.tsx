import { useEffect, useRef } from 'react';

/** A native modal `<dialog>`; Esc calls `onClose` instead of closing it directly. */
export function Modal({
  children,
  className,
  onClose,
  title,
}: {
  children: React.ReactNode;
  className: string;
  onClose: () => void;
  title: string;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const modal = ref.current;
    modal?.showModal();
    // React does not render the `autofocus` attribute, so showModal() would focus the first
    // focusable element; let a dialog choose its initial focus explicitly.
    modal?.querySelector<HTMLElement>('[data-autofocus]')?.focus();
    return () => modal?.close();
  }, []);
  return (
    <dialog
      aria-label={title}
      ref={ref}
      className={className}
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
    >
      {children}
    </dialog>
  );
}
