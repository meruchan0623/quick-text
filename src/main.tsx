import React from 'react';
import { createRoot } from 'react-dom/client';
import App from './App';
import { ErrorBoundary } from './ErrorBoundary';
import { installGlobalErrorHandlers, rootErrorOptions } from './errorLog';
import './style.css';
installGlobalErrorHandlers();
const root = document.getElementById('root');
if (!root) throw new Error('index.html is missing #root');
createRoot(root, rootErrorOptions()).render(
  <React.StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);
