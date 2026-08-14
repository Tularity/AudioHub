import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const SETTINGS = readFileSync(
  fileURLToPath(new URL('../views/Settings.tsx', import.meta.url)),
  'utf8',
);

describe('driver repair action boundary', () => {
  it('keeps repair available when the installed driver is healthy without showing ready copy', () => {
    const modeCard = SETTINGS.slice(
      SETTINGS.indexOf('function ModeCard()'),
      SETTINGS.indexOf('// ---------------------------------------------------------------- ②'),
    );

    expect(modeCard).toContain("const showDriverNote = driverNeedsReboot || st.tone !== 'ok';");
    expect(modeCard).toContain("&& (st.tone !== 'ok' || driverInstaller?.installed === true);");
    expect(modeCard).toContain('hidden={!showDriverNote && !showDriverAction}');
    expect(modeCard).toContain('{showDriverNote ? (');
    expect(modeCard).toContain('{showDriverAction ? (');
    expect(modeCard).toContain("? t('settings.driver.repair')");
    expect(modeCard).not.toContain("hidden={st.tone === 'ok' && !driverNeedsReboot}");
  });
});
