/// <reference types="vite/client" />

interface ImportMetaEnv {
  /**
   * `1` forces the in-memory mock backend even inside Tauri (FE-18). Development only: a production
   * build ignores it and always uses Tauri.
   */
  readonly VITE_MOCK?: string;
}
