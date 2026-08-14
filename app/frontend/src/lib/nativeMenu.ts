/** Event emitted by the native macOS application-menu Settings item. */
export const NATIVE_SETTINGS_EVENT = 'audiohub://navigate-settings';

export interface NativeEventTarget {
  addEventListener(event: string, handler: () => void): void;
  removeEventListener(event: string, handler: () => void): void;
}

/**
 * Connect the native menu to the frontend router without making browser mode
 * depend on Tauri. Rust dispatches this fixed DOM event directly into its own
 * webview; no plugin API or extra frontend dependency is required.
 */
export function installNativeSettingsMenu(
  target: NativeEventTarget | null | undefined,
  open: () => void,
): () => void {
  if (!target) return () => {};
  const handler = () => open();
  target.addEventListener(NATIVE_SETTINGS_EVENT, handler);
  return () => {
    target.removeEventListener(NATIVE_SETTINGS_EVENT, handler);
  };
}
