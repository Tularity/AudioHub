import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const CSS = readFileSync(
  fileURLToPath(new URL('../styles.css', import.meta.url)),
  'utf8',
);

function rule(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return CSS.match(new RegExp(`${escaped}\\s*\\{([^}]*)\\}`))?.[1] ?? '';
}

describe('the main view scrolls without a permanent right-hand scrollbar', () => {
  it('keeps the main view scrollable', () => {
    expect(rule('#view-root')).toMatch(/overflow-y\s*:\s*auto/);
  });

  it('hides only the main view scrollbar in both engine families', () => {
    expect(rule('#view-root')).toMatch(/scrollbar-width\s*:\s*none/);
    const webkit = rule('#view-root::-webkit-scrollbar');
    expect(webkit).toMatch(/display\s*:\s*none/);
    expect(webkit).toMatch(/width\s*:\s*0/);
    expect(webkit).toMatch(/height\s*:\s*0/);
    expect(CSS.match(/scrollbar-width\s*:\s*none/g)).toHaveLength(1);
    expect(CSS.match(/::-webkit-scrollbar\s*\{[^}]*display\s*:\s*none/g)).toHaveLength(1);
  });
});
