import React from 'react';
import { createRoot } from 'react-dom/client';
import App from './App';
import { ErrorBoundary } from './ErrorBoundary';
import './style.css';
const root = document.getElementById('root');
if (!root) throw new Error('index.html is missing #root');
createRoot(root).render(
  <React.StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);
