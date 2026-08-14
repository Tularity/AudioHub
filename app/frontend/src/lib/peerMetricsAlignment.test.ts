import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const SRC_ROOT = new URL('../', import.meta.url);
const COMPONENT = readFileSync(
  fileURLToPath(new URL('components/PeerMetrics.tsx', SRC_ROOT)),
  'utf8',
);
const CSS = readFileSync(
  fileURLToPath(new URL('styles.css', SRC_ROOT)),
  'utf8',
);

function component(name: string, nextName: string): string {
  const start = COMPONENT.indexOf(`function ${name}`);
  const end = COMPONENT.indexOf(`function ${nextName}`, start);
  expect(start, `${name} must remain present`).toBeGreaterThanOrEqual(0);
  expect(end, `${nextName} must follow ${name}`).toBeGreaterThan(start);
  return COMPONENT.slice(start, end);
}

function rule(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return CSS.match(new RegExp(`${escaped}\\s*\\{([^}]*)\\}`))?.[1] ?? '';
}

describe('peer-card metric alignment', () => {
  it('gives only the quality cell the right-alignment modifier', () => {
    const latency = component('LatencyCell', 'LatencyBand');
    const quality = component('QualityCell', 'QualityParts');

    expect(quality).toContain('className="metric-cell metric-cell-quality"');
    expect(latency).toContain('className="metric-cell"');
    expect(latency).not.toContain('metric-cell-quality');
  });

  it('right-aligns quality as a whole without changing the base metric cell', () => {
    const quality = rule('.metric-cell-quality');
    const base = rule('.metric-cell');

    expect(quality).toMatch(/justify-content\s*:\s*flex-end/);
    expect(quality).toMatch(/text-align\s*:\s*right/);
    expect(base).not.toMatch(/justify-content\s*:\s*flex-end/);
    expect(base).not.toMatch(/text-align\s*:\s*right/);
  });
});
