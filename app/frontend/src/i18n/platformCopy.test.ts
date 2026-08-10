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
//
// # As of 2026-08-10 the catalogue contains no such pair
//
// The last one was `settings.startup.autostartDesc{Mac,Win}` — two sentences
// under the autostart switch saying where the login item gets registered and
// that turning it off leaves nothing behind. Those are **descriptions**, and
// the user's restructure (instruction 8, plus the standing §3.1 ruling) moved
// the startup rows into 「杂项」 where every row is label + value + `?`. The
// text went to the wiki; the `?` beside that row points at
// Settings-Reference#startup-at-login.
//
// The rules below are therefore kept as a **standing** guard for the next pair
// rather than deleted along with the last one. A guard whose loops all iterate
// over nothing is a test that cannot fail, which is exactly the kind of quiet
// lie this project keeps paying for — so the emptiness is asserted out loud in
// the first case. When somebody adds a pair, that case fails and tells them the
// other three have just woken up.

import { describe, it, expect } from 'vitest';
import { zhCN } from './zh-CN';

const keys = Object.keys(zhCN) as Array<keyof typeof zhCN>;
const suffixed = (suffix: string) => keys.filter((k) => k.endsWith(suffix));

describe('per-platform copy', () => {
  it('states out loud that no platform-split pair exists right now', () => {
    // Read the failure message before changing this number: adding a pair is
    // allowed, it just means the three checks below now have something to say.
    expect(
      [...suffixed('Mac'), ...suffixed('Win')],
      'a platform-split pair reappeared; the checks below now apply to it',
    ).toEqual([]);
  });

  it('has a Mac half for every Win half, and vice versa', () => {
    const wins = suffixed('Win').map((k) => k.slice(0, -3));
    const macs = suffixed('Mac').map((k) => k.slice(0, -3));
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
});
