import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { enUS } from './en-US';
import { zhCN } from './zh-CN';

const peersSource = readFileSync(
  new URL('../views/Peers.tsx', import.meta.url),
  'utf8',
);

const placeholders = (value: string) =>
  [...value.matchAll(/\{(\w+)\}/g)].map((match) => match[1]).sort();

describe('published language catalogues', () => {
  it('carry the same complete set of keys', () => {
    expect(Object.keys(enUS).sort()).toEqual(Object.keys(zhCN).sort());
  });

  it('preserve every named interpolation placeholder', () => {
    for (const key of Object.keys(zhCN) as Array<keyof typeof zhCN>) {
      expect(placeholders(enUS[key]), key).toEqual(placeholders(zhCN[key]));
    }
  });

  it('ends the peer-card accessible name with its complete fingerprint', () => {
    for (const catalogue of [enUS, zhCN]) {
      expect(catalogue['peers.card.viewDetail']).toContain('{name}');
      expect(catalogue['peers.card.viewDetail']).toMatch(/\{fingerprint\}$/);
    }
    expect(peersSource).toContain(
      "aria-label={t('peers.card.viewDetail', { name: displayName, fingerprint: fp })}",
    );
  });

  it('does not silently leave Chinese UI copy in en-US', () => {
    const untranslated = Object.entries(enUS)
      .filter(([, value]) => /[\u3400-\u9fff]/u.test(value))
      .map(([key]) => key);
    expect(untranslated).toEqual([]);
  });

  it('does not assume that arbitrary runtime counts are plural in English', () => {
    // t() intentionally has no plural grammar. These two keys are only selected
    // behind an explicit `count > 1` branch at their call sites; every other
    // dynamic count must use a number-neutral label (for example `Frames: {n}`).
    const guaranteedPlural = new Set([
      'peers.card.inboundMicN',
      'peers.card.dirMulti',
    ]);
    const nounAfterCount = /\{\w+\}\s+(?:actions|channels|days|frames|items|packets|permissions|probes|samples|seconds|streams)\b/i;
    const unsafe = Object.entries(enUS)
      .filter(([key, value]) => !guaranteedPlural.has(key) && nounAfterCount.test(value))
      .map(([key]) => key);
    expect(unsafe).toEqual([]);
  });
});
