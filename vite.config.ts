import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: {
    target: 'es2022',
    // The main window and the beginner typing hint window (src-tauri/src/hint.rs).
    rollupOptions: { input: { main: 'index.html', hint: 'hint.html' } },
  },
});
