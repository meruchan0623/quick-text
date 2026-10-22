/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** `1` forces the in-memory mock backend even inside Tauri (FE-18). */
  readonly VITE_MOCK?: string;
}
