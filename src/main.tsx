import React from 'react';
import { createRoot } from 'react-dom/client';
import App from './App';
import { ErrorBoundary } from './ErrorBoundary';
import { installGlobalErrorHandlers, rootErrorOptions } from './errorLog';
import { applyTheme, currentTheme } from './theme';
import './style.css';
installGlobalErrorHandlers();
// Before the first paint, so a dark window never flashes light.
applyTheme(currentTheme());
const root = document.getElementById('root');
if (!root) throw new Error('index.html is missing #root');
createRoot(root, rootErrorOptions()).render(
  <React.StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);
