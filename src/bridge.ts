import { invoke } from '@tauri-apps/api/core';
// eslint-disable-next-line @typescript-eslint/no-explicit-any -- legacy call sites rely on `any`
export async function call<T = any>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!('__TAURI_INTERNALS__' in window))
    throw new Error('当前为浏览器预览，请在桌面应用中连接真实输入引擎。');
  return invoke<T>(command, args);
}
