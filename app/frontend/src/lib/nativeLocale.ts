import type { Locale } from '../i18n';

/**
 * Native locale is machine-wide OS state, not ordinary page state. Only the
 * installed Tauri shell may synchronize it; a browser can render in any
 * language without renaming virtual devices for the desktop user.
 */
export function nativeLocaleNeedsSync(
  mode: 'tauri' | 'browser',
  online: boolean,
  stored: string | null | undefined,
  resolved: Locale,
): boolean {
  return mode === 'tauri' && online && stored !== resolved;
}
