import { describe, expect, it, vi } from 'vitest';
import { installNativeSettingsMenu, NATIVE_SETTINGS_EVENT } from './nativeMenu';

describe('native Settings menu bridge', () => {
  it('routes the exact native event and releases its listener', () => {
    let handler: (() => void) | null = null;
    const addEventListener = vi.fn((_event: string, next: () => void) => {
      handler = next;
    });
    const removeEventListener = vi.fn();
    const open = vi.fn();

    const cleanup = installNativeSettingsMenu({ addEventListener, removeEventListener }, open);

    expect(addEventListener).toHaveBeenCalledWith(NATIVE_SETTINGS_EVENT, expect.any(Function));
    expect(handler).not.toBeNull();
    (handler as unknown as () => void)();
    expect(open).toHaveBeenCalledOnce();

    cleanup();
    expect(removeEventListener).toHaveBeenCalledWith(NATIVE_SETTINGS_EVENT, handler);
  });

  it('is a no-op outside Tauri', () => {
    expect(() => installNativeSettingsMenu(undefined, vi.fn())()).not.toThrow();
  });
});
