import { useEffect, useState } from 'react';

export type Theme = 'light' | 'dark';

const KEY = 'quicker-text.theme';

function systemQuery(): MediaQueryList | null {
  return typeof window.matchMedia === 'function'
    ? window.matchMedia('(prefers-color-scheme: dark)')
    : null;
}

/** The theme chosen with the header toggle, if any. */
function savedTheme(): Theme | null {
  try {
    const value = localStorage.getItem(KEY);
    return value === 'light' || value === 'dark' ? value : null;
  } catch {
    return null;
  }
}

/** The saved choice, else the system's. */
export function currentTheme(): Theme {
  return savedTheme() ?? (systemQuery()?.matches ? 'dark' : 'light');
}

export function applyTheme(theme: Theme) {
  document.documentElement.dataset.theme = theme;
}

/** The theme in use, and a setter that also remembers it across launches. Until the user picks
    one, the system's is followed, including when it changes while the window is open. */
export function useTheme(): [Theme, (theme: Theme) => void] {
  const [theme, setThemeState] = useState<Theme>(currentTheme);
  useEffect(() => applyTheme(theme), [theme]);
  useEffect(() => {
    const query = systemQuery();
    if (!query) return;
    const follow = () => {
      if (!savedTheme()) setThemeState(query.matches ? 'dark' : 'light');
    };
    query.addEventListener('change', follow);
    return () => query.removeEventListener('change', follow);
  }, []);
  const setTheme = (next: Theme) => {
    try {
      localStorage.setItem(KEY, next);
    } catch {
      // Not remembered, still applied for this window.
    }
    setThemeState(next);
  };
  return [theme, setTheme];
}
