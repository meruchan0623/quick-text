// The beginner typing hint window (src-tauri/src/hint.rs). Shows what the backend sends, measures
// it and reports the height; it never takes the focus or the pointer. Only Tauri IPC: this page
// has no mock backend.
import { createRoot } from 'react-dom/client';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { applyTheme, currentTheme } from '../theme';
import { HintPanel } from './HintPanel';
import type { HintPayload } from '../lib/hint';
import '../style.css';
import '../hint.css';

applyTheme(currentTheme());
// Only the Windows window is transparent (rounded corners); elsewhere the corners would show.
if (!navigator.userAgent.includes('Windows')) document.documentElement.classList.add('hint-opaque');
// The main window's theme toggle writes localStorage; follow it.
window.addEventListener('storage', () => applyTheme(currentTheme()));

const host = document.getElementById('hint-root');
if (!host) throw new Error('hint.html is missing #hint-root');
const root = createRoot(host);

void listen<HintPayload>('candidate-hint', ({ payload }) => {
  applyTheme(currentTheme());
  root.render(
    <HintPanel
      payload={payload}
      onLaidOut={(height) => {
        void invoke('candidate_hint_measured', { seq: payload.seq, height });
      }}
    />,
  );
});
