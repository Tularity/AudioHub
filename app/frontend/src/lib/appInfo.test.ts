import { afterEach, describe, expect, it } from 'vitest';
import { appVersion } from './appInfo';

// Under vitest there is no `define`, so `__APP_VERSION__` is a plain global
// lookup -- which is exactly what lets both branches be exercised here: assign
// the global to simulate an applied define, delete it to simulate a build that
// never got one.
const G = globalThis as unknown as Record<string, unknown>;
const KEY = '__APP_VERSION__';

afterEach(() => {
  delete G[KEY];
});

describe('appVersion', () => {
  it('returns null when the build-time define was never applied', () => {
    expect(appVersion()).toBeNull();
  });

  it('returns the injected version verbatim', () => {
    G[KEY] = '0.1.0';
    expect(appVersion()).toBe('0.1.0');
  });

  it('trims incidental whitespace around the injected value', () => {
    G[KEY] = '  1.2.3\n';
    expect(appVersion()).toBe('1.2.3');
  });

  // A blank define is the same failure as no define at all. It must not reach
  // the About block as an empty "version" row, and it must not be papered over
  // with a fabricated number.
  it('treats an empty or whitespace-only define as absent', () => {
    G[KEY] = '   ';
    expect(appVersion()).toBeNull();
  });

  // A numeric 0 must come back as null, not as the string "0". The inverse
  // mistake -- folding an absent value into a falsy-but-real one -- has bitten
  // this project before.
  it('treats a non-string define as absent', () => {
    G[KEY] = 0;
    expect(appVersion()).toBeNull();
  });
});
