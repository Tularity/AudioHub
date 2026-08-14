import { describe, expect, it } from 'vitest';
import { createInertRegistry } from './modalInert';

class FakeElement {
  private attrs = new Map<string, string>();

  constructor(initial: Record<string, string> = {}) {
    for (const [name, value] of Object.entries(initial)) this.attrs.set(name, value);
  }

  hasAttribute(name: string): boolean { return this.attrs.has(name); }
  getAttribute(name: string): string | null { return this.attrs.get(name) ?? null; }
  setAttribute(name: string, value: string): void { this.attrs.set(name, value); }
  removeAttribute(name: string): void { this.attrs.delete(name); }
}

describe('overlapping top-level modal inert ownership', () => {
  it('keeps the background blocked until the final owner releases it', () => {
    const registry = createInertRegistry<FakeElement>();
    const background = new FakeElement();

    registry.acquire(background); // confirmation
    registry.acquire(background); // recovery overlay supersedes it
    registry.release(background); // confirmation closes first

    expect(background.hasAttribute('inert')).toBe(true);
    expect(background.getAttribute('aria-hidden')).toBe('true');

    registry.release(background);
    expect(background.hasAttribute('inert')).toBe(false);
    expect(background.getAttribute('aria-hidden')).toBeNull();
  });

  it('also survives release in the opposite order', () => {
    const registry = createInertRegistry<FakeElement>();
    const background = new FakeElement();

    registry.acquire(background);
    registry.acquire(background);
    registry.release(background); // recovery overlay closes first
    expect(background.hasAttribute('inert')).toBe(true);

    registry.release(background);
    expect(background.hasAttribute('inert')).toBe(false);
  });

  it('restores attributes that existed before either modal opened', () => {
    const registry = createInertRegistry<FakeElement>();
    const background = new FakeElement({ inert: '', 'aria-hidden': 'menu-state' });

    registry.acquire(background);
    expect(background.getAttribute('aria-hidden')).toBe('true');
    registry.release(background);

    expect(background.hasAttribute('inert')).toBe(true);
    expect(background.getAttribute('aria-hidden')).toBe('menu-state');
  });

  it('ignores duplicate or late releases', () => {
    const registry = createInertRegistry<FakeElement>();
    const background = new FakeElement();

    registry.release(background);
    registry.acquire(background);
    registry.release(background);
    registry.release(background);

    expect(background.hasAttribute('inert')).toBe(false);
    expect(background.getAttribute('aria-hidden')).toBeNull();
  });
});
