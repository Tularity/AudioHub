// 系统权限面板（用户 2026-08-10 第 4 条：改为二级菜单；第 22 条：首次运行自动弹）。
//
// # 为什么它在 components/ 而不是 views/Settings.tsx 里
//
// 入口在「设置 › 杂项」，但**自动弹出必须能发生在任何页面上**（第 22 条）——用户
// 首次启动落在主面板，那时设置页根本没挂载。所以开合状态放在模块级，宿主
// `<PermissionsSheetHost/>` 挂在 `App.tsx` 的根层，设置页那一行只是调 `open…()`。
// 与 `ConfirmDialog` 同一个形状，理由也一样：调用点在事件处理器里。
//
// # 它不是授权门
//
// 授权门（`views/Onboarding.tsx`）只拦「必需 + 状态可知 + 尚未授权」，于是
// `local_network`（永远不可查询）与 `system_audio`（非必需）**永远不会触发门**。
// 这块面板就是那两项唯一的落点，也是跳过授权的用户回头补授权的地方。
// 判据与落盘边界见 `lib/permIntro.ts` 顶部。

import { useSyncExternalStore } from 'react';
import { Help } from './Controls';
import { PermissionRow } from './PermissionRow';
import { Sheet } from './Sheet';
import { toast } from './Toasts';
import { openExternal, WIKI } from '../lib/external';
import { pendingSignature, writePermSeen } from '../lib/permIntro';
import { t } from '../i18n';
import { actionOf } from '../state/permissions';
import type { PermissionState } from '../state/permissions';
import { actions, getState, useStore } from '../state/store';
import { refreshPermissions, rpc } from '../state/connection';

// ---------------------------------------------------------------- 开合

/** null = 关着；`auto` 记的是「这一次是 effect 弹的，不是用户点开的」。 */
let open: { auto: boolean } | null = null;
const listeners = new Set<() => void>();
const emit = () => { for (const fn of [...listeners]) fn(); };
const subscribe = (fn: () => void) => { listeners.add(fn); return () => listeners.delete(fn); };

export function openPermissionsSheet(auto = false): void {
  if (open) return;
  open = { auto };
  emit();
}

export function closePermissionsSheet(): void {
  if (!open) return;
  open = null;
  // 关掉即记下「这一组未授权项我看过了」。手动打开的也记——用户看过就是看过，
  // 只因为他是自己点开的就下次再弹一遍，没有道理。签名随集合变化自动失效，
  // 所以这不是一个「以后别再提醒我」的永久开关（见 lib/permIntro.ts）。
  writePermSeen(pendingSignature(getState().permissions.list));
  emit();
}

export function isPermissionsSheetOpen(): boolean {
  return open !== null;
}

// ---------------------------------------------------------------- 宿主

export function PermissionsSheetHost() {
  const live = useSyncExternalStore(subscribe, () => open);
  if (!live) return null;
  return <PermissionsSheet auto={live.auto} />;
}

function PermissionsSheet({ auto }: { auto: boolean }) {
  const perms = useStore((s) => s.permissions);
  const conn = useStore((s) => s.conn);

  async function onAction(p: PermissionState) {
    if (actionOf(p) === 'request') {
      actions.setPermissionBusy(p.id);
      try {
        const res = await rpc('daemon.request_permission', { id: p.id });
        await refreshPermissions({ force: true, seed: res });
      } catch { /* rpc 已 toast */ } finally {
        actions.setPermissionBusy(null);
      }
      return;
    }
    if (p.settingsUrl) {
      void openExternal(p.settingsUrl);
      if (p.manual) toast(t('perm.settingsFallback', { manual: p.manual }), 'info');
    } else {
      toast(p.manual ? t('perm.openManual', { manual: p.manual }) : t('perm.noSettingsUrl'), 'warn');
    }
  }

  const note = perms.list.length ? ''
    : perms.supported === false ? t('settings.perm.unsupported')
      : perms.error ? t('settings.perm.error', { message: perms.error })
        : conn === 'online' ? t('settings.perm.probing') : t('settings.perm.offline');

  return (
    <Sheet
      testid="settings-perm-sheet"
      auto={auto}
      wide
      title={t('settings.perm.title')}
      help={<Help label={t('wiki.permissions')} url={WIKI.permissions} testid="settings-perm-help" />}
      onClose={closePermissionsSheet}
      footer={(
        <button
          className="btn small" type="button" data-testid="settings-perm-recheck"
          onClick={() => void refreshPermissions({ force: true })}
        >
          {t('settings.perm.recheck')}
        </button>
      )}
    >
      {/* 自动弹出时多一句「为什么现在弹」。用户点开的那次不需要——他知道自己点了。 */}
      <p className="muted small" data-testid="settings-perm-intro" hidden={!auto}>{t('perm.intro')}</p>
      <div className="perm-list" data-testid="settings-perm-list" hidden={perms.list.length === 0}>
        {perms.list.map((p) => (
          <PermissionRow key={p.id} perm={p} prefix="settings-perm" busy={perms.busy} onAction={onAction} />
        ))}
      </div>
      <p className="muted small" data-testid="settings-perm-note" hidden={!note}>{note}</p>
    </Sheet>
  );
}
