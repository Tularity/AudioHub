/** Event emitted by the native macOS application-menu Settings item. */
export const NATIVE_SETTINGS_EVENT = 'audiohub://navigate-settings';

/**
 * Event emitted by the volume slider in the macOS menu-bar (tray) menu.
 *
 * Unlike the Settings item this one carries a value, so it is a `CustomEvent`
 * with `detail.scalar` in 0..1. Rust throttles it to one dispatch per 100 ms
 * and always re-sends the final value when the drag ends — the same 100 ms the
 * peer-card slider uses (`components/VolumeControl.tsx` THROTTLE_MS), so the
 * two cannot fight over the wire.
 *
 * Rust deliberately does NOT call the daemon itself: the write goes back out
 * through the frontend's existing `session.set_volume` path so that intent
 * suppression, error reporting and the card's own slider all stay on one code
 * path. A second writer would race the first.
 */
export const TRAY_VOLUME_EVENT = 'audiohub://tray-volume';

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

/** Numeric payload of {@link TRAY_VOLUME_EVENT}, or null when it is malformed. */
export function trayVolumeScalar(event: unknown): number | null {
  const detail = (event as { detail?: unknown } | null)?.detail;
  const raw = (detail as { scalar?: unknown } | null)?.scalar;
  const n = Number(raw);
  // A native surface is still an untrusted input boundary: a missing or
  // out-of-range value must be dropped, never clamped into a plausible write.
  // Silently sending 0 because the field was absent would mute the peer.
  if (!Number.isFinite(n) || n < 0 || n > 1) return null;
  return n;
}

/**
 * Connect the menu-bar volume slider to the frontend's own `session.set_volume`
 * path. Same shape as {@link installNativeSettingsMenu}: browser mode installs
 * a listener that simply never fires.
 */
export function installTrayVolume(
  target: NativeEventTarget | null | undefined,
  onScalar: (scalar: number) => void,
): () => void {
  if (!target) return () => {};
  const handler = (event: unknown) => {
    const scalar = trayVolumeScalar(event);
    if (scalar != null) onScalar(scalar);
  };
  target.addEventListener(TRAY_VOLUME_EVENT, handler as () => void);
  return () => {
    target.removeEventListener(TRAY_VOLUME_EVENT, handler as () => void);
  };
}
