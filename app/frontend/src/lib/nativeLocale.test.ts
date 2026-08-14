import { describe, expect, it } from 'vitest';
import { nativeLocaleNeedsSync } from './nativeLocale';

describe('native locale ownership', () => {
  it('lets only an online Tauri App rename machine-wide native surfaces', () => {
    expect(nativeLocaleNeedsSync('tauri', true, 'zh-CN', 'en-US')).toBe(true);
    expect(nativeLocaleNeedsSync('browser', true, 'zh-CN', 'en-US')).toBe(false);
    expect(nativeLocaleNeedsSync('tauri', false, 'zh-CN', 'en-US')).toBe(false);
  });

  it('does not rewrite a locale the daemon already holds', () => {
    expect(nativeLocaleNeedsSync('tauri', true, 'en-US', 'en-US')).toBe(false);
  });
});
