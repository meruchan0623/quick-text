import { useEffect, useRef, useState } from 'react';

type Rect = { x: number; y: number; w: number; h: number };
type Edge = 'n' | 's' | 'e' | 'w' | 'ne' | 'nw' | 'se' | 'sw';

const EDGES: Edge[] = ['n', 's', 'e', 'w', 'ne', 'nw', 'se', 'sw'];
/** Kept free around a window so its edges can always be grabbed. */
const MARGIN = 8;
/** Below this viewport a window always fills it: there is no room to move or size it. */
const COMPACT_WIDTH = 720;
const COMPACT_HEIGHT = 520;

function viewport() {
  return { vw: window.innerWidth, vh: window.innerHeight };
}

function isCompact() {
  const { vw, vh } = viewport();
  return vw < COMPACT_WIDTH || vh < COMPACT_HEIGHT;
}

function savedSize(key: string): { w: number; h: number } | null {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(key) ?? 'null');
    if (value && typeof value === 'object' && 'w' in value && 'h' in value) {
      const { w, h } = value;
      if (typeof w === 'number' && typeof h === 'number' && w > 0 && h > 0) return { w, h };
    }
  } catch {
    // Not saved, or not readable: use the default size.
  }
  return null;
}

function saveSize(key: string, rect: Rect) {
  try {
    localStorage.setItem(key, JSON.stringify({ w: Math.round(rect.w), h: Math.round(rect.h) }));
  } catch {
    // Not remembered, still applied for this window.
  }
}

/** `rect` shrunk to fit the viewport (not below the minimum), then moved to be inside it. */
function fit(rect: Rect, minW: number, minH: number): Rect {
  const { vw, vh } = viewport();
  const w = Math.max(Math.min(rect.w, vw - 2 * MARGIN), Math.min(minW, vw));
  const h = Math.max(Math.min(rect.h, vh - 2 * MARGIN), Math.min(minH, vh));
  const x = Math.min(Math.max(rect.x, MARGIN), Math.max(vw - w - MARGIN, 0));
  const y = Math.min(Math.max(rect.y, MARGIN), Math.max(vh - h - MARGIN, 0));
  return { x, y, w, h };
}

function centered(w: number, h: number, minW: number, minH: number): Rect {
  const { vw, vh } = viewport();
  const fitted = fit({ x: 0, y: 0, w, h }, minW, minH);
  return fit({ ...fitted, x: (vw - fitted.w) / 2, y: (vh - fitted.h) / 2 }, minW, minH);
}

/**
 * A window inside the app: drag its title bar to move it, its edges and corners to size it,
 * double-click the title bar (or use its button) to maximize or restore it. Its size is
 * remembered under `storageKey`. On a small screen it always fills the viewport.
 *
 * Render it inside a `Modal`, which keeps focus in it and handles Esc.
 */
export function WindowFrame({
  title,
  storageKey,
  onClose,
  closeLabel = '关闭',
  defaultWidth = 1080,
  defaultHeight = 870,
  minWidth = 560,
  minHeight = 420,
  className,
  children,
}: {
  title: React.ReactNode;
  storageKey: string;
  onClose: () => void;
  closeLabel?: string;
  defaultWidth?: number;
  defaultHeight?: number;
  minWidth?: number;
  minHeight?: number;
  className?: string;
  children: React.ReactNode;
}) {
  // The place and size the user chose; drawn fitted to the viewport (`shown`), so a window made
  // smaller by a small screen gets its size back when the screen is large again.
  const [rect, setRect] = useState<Rect>(() => {
    const size = savedSize(storageKey) ?? { w: defaultWidth, h: defaultHeight };
    return centered(size.w, size.h, minWidth, minHeight);
  });
  const [maximized, setMaximized] = useState(false);
  const [, setViewportSize] = useState(viewport);
  // The gesture in progress: where the pointer and the window were when it started.
  const gesture = useRef<{ edge: Edge | 'move'; px: number; py: number; start: Rect } | null>(null);

  useEffect(() => {
    const redraw = () => setViewportSize(viewport());
    window.addEventListener('resize', redraw);
    return () => window.removeEventListener('resize', redraw);
  }, []);

  const compact = isCompact();
  const filled = maximized || compact;
  const shown = fit(rect, minWidth, minHeight);

  const begin = (edge: Edge | 'move', e: React.PointerEvent<HTMLElement>) => {
    if (filled || e.button !== 0) return;
    // A button in the title bar is clicked, not dragged.
    if (edge === 'move' && (e.target as HTMLElement).closest('button')) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    gesture.current = { edge, px: e.clientX, py: e.clientY, start: shown };
  };

  const move = (e: React.PointerEvent<HTMLElement>) => {
    const g = gesture.current;
    if (!g) return;
    const dx = e.clientX - g.px;
    const dy = e.clientY - g.py;
    const { vw, vh } = viewport();
    const s = g.start;
    if (g.edge === 'move') {
      setRect({
        ...s,
        x: Math.min(Math.max(s.x + dx, MARGIN), vw - s.w - MARGIN),
        y: Math.min(Math.max(s.y + dy, MARGIN), vh - s.h - MARGIN),
      });
      return;
    }
    let { x, y, w, h } = s;
    if (g.edge.includes('e')) w = Math.min(Math.max(s.w + dx, minWidth), vw - s.x - MARGIN);
    if (g.edge.includes('s')) h = Math.min(Math.max(s.h + dy, minHeight), vh - s.y - MARGIN);
    if (g.edge.includes('w')) {
      w = Math.min(Math.max(s.w - dx, minWidth), s.x + s.w - MARGIN);
      x = s.x + s.w - w;
    }
    if (g.edge.includes('n')) {
      h = Math.min(Math.max(s.h - dy, minHeight), s.y + s.h - MARGIN);
      y = s.y + s.h - h;
    }
    setRect({ x, y, w, h });
  };

  const end = (e: React.PointerEvent<HTMLElement>) => {
    const g = gesture.current;
    if (!g) return;
    gesture.current = null;
    e.currentTarget.releasePointerCapture(e.pointerId);
    if (g.edge !== 'move') saveSize(storageKey, shown);
  };

  const pointer = { onPointerMove: move, onPointerUp: end, onPointerCancel: end };

  return (
    <div
      className={['window-frame', className].filter(Boolean).join(' ')}
      data-filled={filled || undefined}
      style={filled ? undefined : { left: shown.x, top: shown.y, width: shown.w, height: shown.h }}
    >
      <div
        className="modal-title window-titlebar"
        onPointerDown={(e) => begin('move', e)}
        onDoubleClick={(e) => {
          if (!compact && !(e.target as HTMLElement).closest('button')) setMaximized((m) => !m);
        }}
        {...pointer}
      >
        <h2>{title}</h2>
        {!compact && (
          <button
            aria-label={maximized ? '还原窗口' : '最大化窗口'}
            title={maximized ? '还原' : '最大化'}
            onClick={() => setMaximized((m) => !m)}
          >
            {maximized ? '❐' : '□'}
          </button>
        )}
        <button aria-label={closeLabel} title="关闭" onClick={onClose}>
          ×
        </button>
      </div>
      {children}
      {!filled &&
        EDGES.map((edge) => (
          <div
            key={edge}
            aria-hidden
            className={`window-resize ${edge}`}
            onPointerDown={(e) => begin(edge, e)}
            {...pointer}
          />
        ))}
    </div>
  );
}
