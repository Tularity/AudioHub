// Copy that differs per platform must actually differ per platform.
//
// The bug this exists for: the settings page's permission block opened with
// "macOS 的规则是：一项权限被拒绝后…" on BOTH platforms, while every row
// underneath it on Windows reported "本平台无需授权" (which is what
// `core/audiohub-core/src/permissions.rs` returns on the non-macOS branch). A
// Windows user therefore read a paragraph about a rule that does not apply,
// immediately above a list contradicting it.
//
// Nothing type-checks that. `t('settings.perm.desc')` is a valid key on every
// platform; only its CONTENT was wrong on one of them. So the convention is a
// `…Mac` / `…Win` pair chosen at the call site by `IS_MAC`, and this file
// enforces the two properties that make the convention worth having:
// the pair is complete, and neither half talks about the other's operating
// system.

import { describe, it, expect } from 'vitest';
import { zhCN } from './zh-CN';

const keys = Object.keys(zhCN) as Array<keyof typeof zhCN>;
const suffixed = (suffix: string) => keys.filter((k) => k.endsWith(suffix));

describe('per-platform copy', () => {
  it('has a Mac half for every Win half, and vice versa', () => {
    const wins = suffixed('Win').map((k) => k.slice(0, -3));
    const macs = suffixed('Mac').map((k) => k.slice(0, -3));
    expect(wins.length).toBeGreaterThan(0);
    expect([...wins].sort()).toEqual([...macs].sort());
  });

  it('never leaves the unsuffixed key behind once a pair exists', () => {
    // A leftover `foo` next to `fooMac`/`fooWin` is how a call site keeps
    // rendering the platform-blind text while looking updated.
    for (const base of suffixed('Win').map((k) => k.slice(0, -3))) {
      expect(keys, `${base} should have been replaced by the pair`).not.toContain(base);
    }
  });

  it('does not describe the other platform in either half', () => {
    for (const k of suffixed('Win')) {
      expect(zhCN[k], `${k} mentions macOS`).not.toMatch(/macOS|Mac\b|系统设置 → 隐私/);
    }
    for (const k of suffixed('Mac')) {
      expect(zhCN[k], `${k} mentions Windows`).not.toMatch(/Windows|计划任务|%APPDATA%/);
    }
  });

  it('still tells Windows users where the states come from', () => {
    // The half that changed must keep the sentence that carries the actual
    // information -- the statuses are probed live, not remembered.
    expect(zhCN['settings.perm.descWin']).toContain('实时探测');
    expect(zhCN['settings.perm.descMac']).toContain('实时探测');
  });
});
