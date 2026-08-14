import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const source = (relative: string) => readFileSync(
  fileURLToPath(new URL(relative, import.meta.url)),
  'utf8',
);

const SHEET = source('../components/Sheet.tsx');
const DETAIL = source('../views/Detail.tsx');
const SETTINGS = source('../views/Settings.tsx');
const SHORTCUTS = source('../components/ShortcutSheet.tsx');
const CHROME = source('../components/Chrome.tsx');
const CONFIRM = source('../components/ConfirmDialog.tsx');
const MODAL_INERT = source('./modalInert.ts');
const CSS = source('../styles.css');

describe('二级菜单动作区', () => {
  it('把辅助动作、取消与菜单级主动作排成固定顺序', () => {
    const actions = SHEET.slice(SHEET.indexOf('<div className="sheet-actions">'));
    const leading = actions.indexOf('sheet-actions-leading');
    const dismiss = actions.indexOf('data-testid={`${testid}-close`}');
    const primary = actions.indexOf('{primaryAction}');
    expect(leading).toBeGreaterThanOrEqual(0);
    expect(dismiss).toBeGreaterThan(leading);
    expect(primary).toBeGreaterThan(dismiss);
  });

  it('别名只在 footer 保存，清除只改草稿', () => {
    const alias = DETAIL.slice(DETAIL.indexOf('function AliasSheet'), DETAIL.indexOf('export function DetailView'));
    expect(alias).toContain('primaryAction={(');
    expect(alias).toContain('data-testid="detail-alias-save"');
    expect(alias).toContain("dismissLabel={t('common.cancel')}");
    expect(alias).toContain("onClick={() => setDraft('')}");
    expect(alias).not.toContain('void setAlias(null)');
  });

  it('快捷键在保存前只写草稿，不碰持久化 store', () => {
    const commitStart = SHORTCUTS.indexOf('function commit');
    const commit = SHORTCUTS.slice(commitStart, SHORTCUTS.indexOf('\n  return (', commitStart));
    expect(commit).toContain('setDraft(');
    expect(commit).not.toContain('setOverrides(');
    expect(SHORTCUTS).toContain('data-testid="shortcuts-save"');
    expect(SHORTCUTS).toContain('setOverrides(draft)');
    expect(SHORTCUTS).toContain("dismissLabel={t('common.cancel')}");
  });

  it('危险动作使用菜单级主动作，而不是留在内容区', () => {
    const danger = DETAIL.slice(DETAIL.indexOf("{sheet === 'danger'"));
    expect(danger).toContain('primaryAction={(');
    expect(danger).toContain('data-testid="detail-unpair"');

    const reset = SETTINGS.slice(SETTINGS.indexOf('function IdentityResetButton'), SETTINGS.indexOf('function StartupRows'));
    expect(reset).toContain('primaryAction={(');
    expect(reset).toContain('data-testid="settings-identity-reset-confirm"');
    expect(reset).not.toContain('danger-slot');
  });

  it('服务恢复层冻结其它根级模态并保持最高层级', () => {
    expect(CHROME).toContain('inertSiblings(overlay)');
    expect(MODAL_INERT).toContain("node.setAttribute('inert', '')");
    expect(MODAL_INERT).toContain("node.setAttribute('aria-hidden', 'true')");
    expect(MODAL_INERT).toContain("observer.observe(host, { childList: true })");
    expect(SHEET).toContain("document.getElementById('overlay')?.hidden === false");
    expect(CONFIRM).toContain("document.getElementById('overlay')?.hidden === false");
    expect(CSS.match(/#overlay\s*\{[^}]*z-index:\s*(\d+)/s)?.[1]).toBe('100');
  });
});
