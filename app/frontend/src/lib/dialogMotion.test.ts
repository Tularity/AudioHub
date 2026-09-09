import { describe, expect, it } from 'vitest';
import { dialogTransform } from './dialogMotion';

describe('dialog travel connects the trigger and final layout', () => {
  const target = { left: 580, top: 220, width: 440, height: 600 };

  it.each([
    { left: 1250, top: 84, width: 110, height: 36 },
    { left: 24, top: 84, width: 40, height: 36 },
    { left: 760, top: 900, width: 80, height: 36 },
  ])('starts at the source centre without clamping the trajectory: %j', (source) => {
    const pose = dialogTransform(target, source);
    expect(target.left + target.width / 2 + pose.x).toBe(source.left + source.width / 2);
    expect(target.top + target.height / 2 + pose.y).toBe(source.top + source.height / 2);
    expect(pose.scale).toBeGreaterThan(0);
    expect(pose.scale).toBeLessThan(1);
  });

  it('re-aims correctly after asynchronous content changes the dialog layout', () => {
    const source = { left: 1250, top: 84, width: 110, height: 36 };
    const resized = { ...target, top: 150, height: 740 };
    const pose = dialogTransform(resized, source);
    expect(resized.left + resized.width / 2 + pose.x).toBe(1305);
    expect(resized.top + resized.height / 2 + pose.y).toBe(102);
  });

  it('uses a centred fallback when there is no source', () => {
    const pose = dialogTransform(target, null);
    expect(pose.x).toBe(0);
    expect(pose.y).toBe(0);
    expect(Number.isFinite(pose.scale)).toBe(true);
  });

  it('does not divide by an unlaid-out target', () => {
    const pose = dialogTransform({ left: 0, top: 0, width: 0, height: 0 }, target);
    expect(Object.values(pose).every(Number.isFinite)).toBe(true);
  });
});
