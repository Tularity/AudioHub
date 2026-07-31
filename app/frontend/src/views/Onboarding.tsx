// 首启授权门（mac app 的通行做法是第一页就把权限要齐再进主界面）。
//
// 挡人的规矩只有一条：**必需 + 状态可知 + 尚未授权** 才挡（state/permissions.ts
// isBlocking）。macOS 不提供本地网络授权的查询接口，它永远不会返回 granted——
// 拿它当门闩会把每个用户永久锁死，所以它只展示、不挡人。可选项同理。
//
// 另一条硬规矩：**不落任何「已看过」标记**。门是每次启动重新探测出来的，
// 用户在系统设置里撤销授权后，下次启动照样会拦住他。

import { useState } from 'react';
import { Icon } from '../components/Icon';
import { PermissionRow } from '../components/PermissionRow';
import { toast } from '../components/Toasts';
import { openExternal } from '../lib/external';
import { t, listFormat } from '../i18n';
import { actions, getState, useStore } from '../state/store';
import { actionOf, isBlocking, requestQueue } from '../state/permissions';
import type { PermissionState } from '../state/permissions';
import { refreshPermissions, rpc } from '../state/connection';

export function OnboardingGate() {
  const perms = useStore((s) => s.permissions);
  // 本次会话内点过「稍后再说」的可选项；只在内存里。
  const [deferred, setDeferred] = useState<ReadonlySet<string>>(() => new Set());

  const items = perms.list;
  const blocking = items.filter(isBlocking);
  const busy = !!perms.busy;

  async function request(p: PermissionState) {
    actions.setPermissionBusy(p.id);
    try {
      // 弹窗停在用户屏幕上时这个请求一直挂着（ipc/client.ts 给了 3 分钟），
      // 所以这里不能拿默认超时去卡它。
      const res = await rpc('daemon.request_permission', { id: p.id });
      await refreshPermissions({ force: true, seed: res });
    } catch {
      // rpc 已经 toast 过；状态以下一轮复查为准。
    } finally {
      actions.setPermissionBusy(null);
    }
  }

  async function runQueue() {
    const queue = requestQueue(getState().permissions.list);
    if (!queue.length) {
      // 一个都请求不了 = 剩下的只能去系统设置。别让按钮点下去毫无反应。
      toast(t('onboarding.noRequestable'), 'warn');
      return;
    }
    actions.setPermissionBusy('*');
    try {
      for (const p of queue) {
        try {
          const res = await rpc('daemon.request_permission', { id: p.id });
          await refreshPermissions({ force: true, seed: res });
        } catch { /* 单项失败不阻断后面的：用户点的是「全部」 */ }
      }
    } finally {
      actions.setPermissionBusy(null);
    }
    await refreshPermissions({ force: true });
    const left = getState().permissions.list.filter(isBlocking);
    if (left.length) {
      toast(t('onboarding.stillMissing', { n: left.length, names: listFormat(left.map((p) => p.name)) }), 'warn');
    } else {
      toast(t('onboarding.allGranted'), 'ok');
    }
  }

  function openSettings(p: PermissionState) {
    if (!p.settingsUrl) {
      toast(p.manual ? t('perm.openManual', { manual: p.manual }) : t('perm.noSettingsUrl'), 'warn');
      return;
    }
    void openExternal(p.settingsUrl);
    // 深链能否打开取决于 webview 与系统，所以路径文案照给不误——
    // 用户不该因为一个链接没反应就无路可走。
    if (p.manual) toast(t('perm.settingsFallback', { manual: p.manual }), 'info');
  }

  function onAction(p: PermissionState) {
    if (actionOf(p) === 'request') void request(p);
    else openSettings(p);
  }

  const hint = busy
    ? t('onboarding.hint.busy')
    : blocking.length
      ? t('onboarding.hint.blocking', { n: blocking.length, names: listFormat(blocking.map((p) => p.name)) })
      : t('onboarding.hint.ready');

  const optionalLeft = items.filter((p) => !p.required && p.status !== 'granted').map((p) => p.name);
  const skipNote = blocking.length
    ? t('onboarding.skipNote.blocking', { names: listFormat(blocking.map((p) => p.name)) })
    : optionalLeft.length
      ? t('onboarding.skipNote.optional', { names: listFormat(optionalLeft) })
      : '';

  return (
    <>
      {/* 门盖住了整个窗口，连带盖掉了外壳里那块拖拽把手：macOS 的
          titleBarStyle=Overlay 下没有系统标题栏，不补一条用户就搬不动这个窗口。 */}
      <div className="gate-drag" data-tauri-drag-region />
      <section className="view onboarding" data-testid="view-onboarding">
        <div className="gate-head">
          <span className="gate-logo"><Icon name="shield" /></span>
          <div>
            <h2 className="gate-title">{t('onboarding.title')}</h2>
            <p className="gate-sub">{t('onboarding.sub')}</p>
          </div>
        </div>

        <div className="perm-list" data-testid="onboarding-list">
          {items.map((p) => (
            <PermissionRow
              key={p.id}
              perm={p}
              prefix="onboarding"
              busy={perms.busy}
              deferred={deferred.has(p.id)}
              onAction={onAction}
              // 可选项才给「稍后再说」：必需项给了就是自相矛盾（它挡着门）。
              onDefer={p.required ? null : (x) => setDeferred((s) => new Set(s).add(x.id))}
            />
          ))}
        </div>

        <div className="gate-actions">
          <button
            className="btn primary" type="button" data-testid="onboarding-grant-all"
            disabled={busy || requestQueue(items).length === 0}
            onClick={() => void runQueue()}
          >
            <Icon name="shield" />{t('onboarding.grantAll')}
          </button>
          <button
            className="btn" type="button" data-testid="onboarding-recheck"
            disabled={busy} onClick={() => void refreshPermissions({ force: true })}
          >
            {t('onboarding.recheck')}
          </button>
          <span className="gate-spacer" />
          <button
            className="btn primary big" type="button" data-testid="onboarding-enter"
            disabled={blocking.length > 0 || busy}
            onClick={() => actions.dismissGate(false)}
          >
            {t('onboarding.enter')}
          </button>
        </div>

        <p className="gate-hint" data-testid="onboarding-hint">{hint}</p>

        <div className="gate-foot">
          <button
            className="btn ghost small" type="button" data-testid="onboarding-skip"
            hidden={blocking.length === 0}
            onClick={() => {
              actions.dismissGate(true);
              toast(t('onboarding.skipToast'), 'warn');
            }}
          >
            {t('onboarding.skip')}
          </button>
          <p className="gate-skip-note" data-testid="onboarding-skip-note">{skipNote}</p>
        </div>
      </section>
    </>
  );
}
