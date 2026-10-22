import { useEffect, useRef } from 'react';

/**
 * A native modal `<dialog>`; Esc calls `onClose` instead of closing it directly: it closes when it
 * is no longer rendered, so `onClose` may keep it open. Its backdrop does not close it.
 */
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
      // A close request the browser does not let be cancelled (Chromium: Esc again, with no user
      // activation since the last one) closes it anyway: open it again, and ask as for Esc. Not
      // when unmounted (the effect's cleanup closes it), nor already open again (StrictMode).
      onClose={(e) => {
        const modal = e.currentTarget;
        if (modal.open || !modal.isConnected) return;
        modal.showModal();
        onClose();
      }}
    >
      {children}
    </dialog>
  );
}
