import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { trapIndex } from './sheet';

const source = (relative: string) => readFileSync(
  fileURLToPath(new URL(relative, import.meta.url)),
  'utf8',
);

const CONFIRM = source('../components/ConfirmDialog.tsx');
const SHEET = source('../components/Sheet.tsx');
const CHROME = source('../components/Chrome.tsx');

describe('ConfirmDialog is the active keyboard modal', () => {
  it('wraps Tab and Shift+Tab between its two actions', () => {
    // DOM-free unit tests deliberately stay in node; this is the exact helper
    // wired to ConfirmDialog's key handler.
    expect(trapIndex(2, 0, false)).toBe(1);
    expect(trapIndex(2, 1, false)).toBe(0);
    expect(trapIndex(2, 0, true)).toBe(1);
    expect(trapIndex(2, 1, true)).toBe(0);
    expect(CONFIRM).toContain('if (e.key !== \'Tab\' || !card) return;');
    expect(CONFIRM).toContain('list.indexOf(document.activeElement as HTMLElement)');
    expect(CONFIRM).toContain('list[idx]!.focus()');
  });

  it('makes a containing Sheet yield both Escape and Tab', () => {
    const overlayGuard = SHEET.indexOf("document.getElementById('overlay')?.hidden === false");
    const confirmGuard = SHEET.indexOf('if (isConfirmOpen()) return;', overlayGuard);
    const escapeBranch = SHEET.indexOf('if (isEscape(e))', confirmGuard);
    const tabBranch = SHEET.indexOf("if (e.key !== 'Tab'", escapeBranch);

    expect(overlayGuard).toBeGreaterThanOrEqual(0);
    expect(confirmGuard).toBeGreaterThan(overlayGuard);
    expect(escapeBranch).toBeGreaterThan(confirmGuard);
    expect(tabBranch).toBeGreaterThan(confirmGuard);
  });
});

describe('ConfirmDialog owns and restores modal state', () => {
  it('inerts root siblings while leaving the recovery overlay able to supersede it', () => {
    expect(CONFIRM).toContain('inertSiblings(mask, overlay ? [overlay] : [])');
    expect(CONFIRM).toContain("attributeFilter: ['hidden']");
    expect(CHROME).toContain('inertSiblings(overlay)');
  });

  it('captures the trigger before focusing and restores it after releasing inert', () => {
    const opener = CONFIRM.indexOf('const opener = document.activeElement as HTMLElement | null;');
    const focus = CONFIRM.indexOf('(okRef.current ?? card)?.focus();', opener);
    const release = CONFIRM.indexOf('releaseBackground();', focus);
    const restore = CONFIRM.indexOf("if (opener?.isConnected && !opener.closest('[inert]')) opener.focus();", release);

    expect(opener).toBeGreaterThanOrEqual(0);
    expect(focus).toBeGreaterThan(opener);
    expect(release).toBeGreaterThan(focus);
    expect(restore).toBeGreaterThan(release);
  });

  it('exposes a labelled, focusable alert dialog', () => {
    expect(CONFIRM).toContain('role="alertdialog"');
    expect(CONFIRM).toContain('aria-modal="true"');
    expect(CONFIRM).toContain('aria-labelledby={titleId}');
    expect(CONFIRM).toContain('tabIndex={-1}');
    expect(CONFIRM).toContain('id={titleId}');
  });
});
