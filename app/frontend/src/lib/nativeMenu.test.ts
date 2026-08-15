import { describe, expect, it, vi } from 'vitest';
import {
  installNativeSettingsMenu, installTrayVolume,
  NATIVE_SETTINGS_EVENT, TRAY_VOLUME_EVENT, trayVolumeScalar,
} from './nativeMenu';

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

describe('tray volume bridge', () => {
  it('routes the exact native event and releases its listener', () => {
    let handler: ((e: unknown) => void) | null = null;
    const addEventListener = vi.fn((_event: string, next: (e: unknown) => void) => {
      handler = next;
    });
    const removeEventListener = vi.fn();
    const onScalar = vi.fn();

    const cleanup = installTrayVolume({ addEventListener, removeEventListener }, onScalar);

    expect(addEventListener).toHaveBeenCalledWith(TRAY_VOLUME_EVENT, expect.any(Function));
    (handler as unknown as (e: unknown) => void)({ detail: { scalar: 0.42 } });
    expect(onScalar).toHaveBeenCalledWith(0.42);

    cleanup();
    expect(removeEventListener).toHaveBeenCalledWith(TRAY_VOLUME_EVENT, handler);
  });

  // A native surface is still an input boundary. Dropping a malformed payload
  // matters more here than elsewhere: clamping an absent field to 0 would mute
  // the peer, and the user would have no idea what did it.
  it('drops a malformed payload instead of writing a plausible value', () => {
    const onScalar = vi.fn();
    let handler: ((e: unknown) => void) | null = null;
    installTrayVolume(
      { addEventListener: (_e, n) => { handler = n as (e: unknown) => void; }, removeEventListener: () => {} },
      onScalar,
    );
    for (const bad of [{}, { detail: {} }, { detail: { scalar: 'x' } }, { detail: { scalar: 1.5 } },
      { detail: { scalar: -0.1 } }, { detail: { scalar: NaN } }, null]) {
      (handler as unknown as (e: unknown) => void)(bad);
    }
    expect(onScalar).not.toHaveBeenCalled();
  });

  it('accepts the closed unit interval', () => {
    expect(trayVolumeScalar({ detail: { scalar: 0 } })).toBe(0);
    expect(trayVolumeScalar({ detail: { scalar: 1 } })).toBe(1);
  });

  it('is a no-op outside Tauri', () => {
    expect(() => installTrayVolume(undefined, vi.fn())()).not.toThrow();
  });
});
