// 一行权限。授权门与设置页共用同一份渲染，避免两处文案/状态判定各说各话。

import { Icon } from './Icon';
import {
  actionOf, actionLabel, isBlocking, statusLabel, STATUS_TAG,
} from '../state/permissions';
import { t } from '../i18n';
import type { PermissionState } from '../state/permissions';

export function PermissionRow({
  perm, prefix, busy, deferred = false, onAction, onDefer,
}: {
  perm: PermissionState;
  /** data-testid 前缀（onboarding / settings-perm） */
  prefix: string;
  busy: string | null;
  deferred?: boolean;
  onAction: (p: PermissionState) => void;
  /** 可选项的「稍后再说」；不传则不渲染 */
  onDefer?: ((p: PermissionState) => void) | null;
}) {
  const p = perm;
  const act = actionOf(p);
  const pending = busy === p.id || busy === '*';

  // 说明行的优先级：daemon 的补充 > 不可查询的实话 > 各状态的下一步。
  // 「不知道」的那一行必须明说系统查不到，不能拿一句含糊的提示假装知情。
  let text = p.note || '';
  if (!text && !p.knowable) {
    text = p.unknownNote || t('perm.note.unqueryable');
  }
  if (!text) {
    if (p.status === 'undetermined') text = t('perm.note.undetermined');
    else if (p.status === 'denied') {
      text = p.manual ? t('perm.note.deniedManual', { manual: p.manual }) : t('perm.note.denied');
    } else if (p.status === 'restricted') {
      text = p.manual ? t('perm.note.restrictedManual', { manual: p.manual }) : t('perm.note.restricted');
    }
  }

  const statusText = statusLabel(p.status);

  return (
    <div
      className={`perm-row${p.status === 'granted' ? ' granted' : ''}${isBlocking(p) ? ' blocking' : ''}`}
      data-testid={`${prefix}-row-${p.id}`}
    >
      <span className="perm-ico"><Icon name={p.icon} /></span>
      <div className="perm-text">
        <div className="perm-head">
          <span className="perm-name">{p.name}</span>
          <span className={p.required ? 'tag accent' : 'tag'}>{p.required ? t('common.required') : t('common.optional')}</span>
          <span className={STATUS_TAG[p.status] || 'tag'} data-testid={`${prefix}-status-${p.id}`}>
            {deferred ? t('perm.statusDeferred', { status: statusText }) : statusText}
          </span>
        </div>
        <p className="perm-why">{p.why}</p>
        {/* 琥珀色留给「真的出了问题、需要你去处理」的那两种；「系统查不到」和
            「点了会弹窗」都只是说明，染成警告色等于天天在喊狼来了。 */}
        <p
          className={`perm-note${p.status === 'denied' || p.status === 'restricted' ? ' warn' : ''}`}
          data-testid={`${prefix}-note-${p.id}`}
          hidden={!text}
        >
          {text}
        </p>
      </div>
      <div className="perm-actions">
        <button
          type="button"
          className={`btn small${act === 'request' && p.required ? ' primary' : ''}`}
          data-testid={`${prefix}-action-${p.id}`}
          hidden={act === 'none' || deferred}
          disabled={pending}
          onClick={() => onAction(p)}
        >
          {pending ? t('perm.requesting') : actionLabel(p)}
        </button>
        {onDefer ? (
          <button
            type="button"
            className="btn ghost small"
            data-testid={`${prefix}-defer-${p.id}`}
            // 「稍后再说」只在这一项还没到位、且它确实不挡人时才有意义。
            hidden={act === 'none' || deferred}
            onClick={() => onDefer(p)}
          >
            {t('perm.defer')}
          </button>
        ) : null}
      </div>
    </div>
  );
}
