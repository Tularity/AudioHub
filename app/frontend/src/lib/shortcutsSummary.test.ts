import { describe, it, expect } from 'vitest';
import { customizedCount, defaultBindings, SHORTCUT_ACTIONS } from './shortcuts';

// 设置 › 杂项 › 快捷键那一行的值。
describe('counting the shortcuts the user has actually changed', () => {
  it('counts nothing on a stock install', () => {
    expect(customizedCount({}, 'mac')).toBe(0);
    expect(customizedCount({}, 'win')).toBe(0);
  });

  it('does not count an override that happens to equal the default', () => {
    // ShortcutRow writes an override even when the user lands back on the
    // stock key. Counting stored keys would report "1 customised" while every
    // binding on screen is the factory one.
    const d = defaultBindings('mac');
    const id = SHORTCUT_ACTIONS[0];
    expect(customizedCount({ [id]: d[id] }, 'mac')).toBe(0);
  });

  it('counts a cleared binding, which is a real change', () => {
    const id = SHORTCUT_ACTIONS[0];
    expect(customizedCount({ [id]: null }, 'mac')).toBe(1);
  });

  it('counts a genuinely rebound action', () => {
    const id = SHORTCUT_ACTIONS[0];
    expect(customizedCount({ [id]: 'CommandOrControl+9' }, 'mac')).toBe(1);
  });

  it('never exceeds the number of actions', () => {
    const all = Object.fromEntries(SHORTCUT_ACTIONS.map((id) => [id, null]));
    expect(customizedCount(all, 'mac')).toBe(SHORTCUT_ACTIONS.length);
  });
});
