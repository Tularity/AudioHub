// 模式 A「送对方扬声器」的共享来源选择器（plan §7.1）+ 系统音频捕获后端（plan §6）。
//
// 存在的理由：在这个控件出现之前，「送对方扬声器」写死发 `source:'mic'`，送过去的是
// 本机**麦克风**——而 plan §7.1 与设置页文案说的都是「捕获本机系统音频送到对端播放」。
// 界面上没有任何地方能改，也没有任何地方能看出来它送的其实是别的东西。
//
// 两条不能松的规矩：
//   1. **不猜可用性**。daemon 没上报后端清单时，available 是 null，控件只说「要到真正
//      开启时才知道」，绝不本地推断 OS 版本伪造一个绿灯（lib/sysaudio.ts 有详述）。
//   2. **不静默失败**。开不起来时 daemon 的原话留在框里（fault），不只弹一条会消失的
//      toast——用户看到的不能只是一个自己弹回去的开关。
//
// 体验红线（plan §6）：这里所有后端都是旁路读取本机正在播放的内容，用户的输出设备
// 保持原样。任何文案都不得出现「请把系统输出切到某某虚拟设备」。

import { Icon } from './Icon';
import { Segmented } from './Controls';
import {
  BACKEND_AUTO, SOURCE_MIC, SOURCE_SYSAUDIO,
  backendKnown, backendOptions, backendsReported, noBackendAvailable,
} from '../lib/sysaudio';
import { t } from '../i18n';
import type { DaemonInfo } from '../ipc/types';
import type { PermissionState } from '../state/permissions';

export function ShareSourceControl({
  testid, daemon, source, backend, pending = false, perm, fault,
  onSource, onBackend, onGrant,
}: {
  testid: string;
  daemon: DaemonInfo | null;
  /** 'sysaudio' | 'mic' */
  source: string;
  /** 后端 id，'' / 'auto' = 自动 */
  backend: string;
  pending?: boolean;
  /** daemon.permissions 里的 system_audio 那一条，拿不到就传 null。 */
  perm: PermissionState | null;
  /** 上一次开启失败的原因（daemon 原话），没有就传 ''。 */
  fault: string;
  onSource: (v: string) => void;
  onBackend: (v: string) => void;
  /** 「前往授权」：跳设置页的权限卡片，不在这里直接触发系统弹窗。 */
  onGrant: () => void;
}) {
  const sys = source === SOURCE_SYSAUDIO;
  const options = backendOptions(daemon);
  const reported = backendsReported(daemon);
  const dead = noBackendAvailable(daemon);
  // 选中的后端已经不在清单里（换了平台、daemon 降级）：留一个如实标注的条目，
  // 别让选择悄悄跳回「自动」——那会让用户以为自己仍然指定着某个后端。
  const stale = !!backend && backend !== BACKEND_AUTO && !backendKnown(daemon, backend);

  // 权限提示只在**确实不是 granted** 且 daemon 真的报了这一项时出现（Windows 侧
  // daemon 直接回 granted，于是这一行永远不出现——判据是数据，不是平台分支）。
  // 「查不到」不等于「没授权」：macOS 的这一项永远查不到，所以提示说的是「首次开启
  // 时会问」，而不是「你还没授权」。
  const needGrant = sys && !!perm && perm.status !== 'granted';
  // 真出了问题（拒绝 / 受限）时，daemon 那句话指得比我们准（它带着确切的系统设置路径）。
  const denied = !!perm && (perm.status === 'denied' || perm.status === 'restricted');
  const grantText = (denied && perm && perm.note) || t('share.perm.hint');

  let note: string;
  if (!sys) {
    note = t('share.mic.note');
  } else if (dead) {
    note = t('share.sys.none');
  } else if (stale) {
    note = t('share.backend.stale', { id: backend });
  } else if (!reported) {
    // daemon 侧缺口：清单没上报，可用性只有开启那一刻才知道。如实说，别装作知道。
    note = t('share.backend.unknown');
  } else {
    const cur = options.find((b) => b.id === backend);
    note = cur ? t('share.backend.selected', { name: cur.label, note: cur.note })
      : t('share.backend.auto');
  }

  return (
    <div
      className={`share-box${dead ? ' unavailable' : ''}`}
      data-testid={`${testid}-box`}
      // 对端卡片整体可点（进入详情）：控件里的点击不能冒泡上去。
      onClick={(e) => e.stopPropagation()}
    >
      <div className="share-row">
        <Icon name="wave" />
        <span className="share-label">{t('share.label')}</span>
        <Segmented<string>
          testid={testid}
          value={sys ? SOURCE_SYSAUDIO : SOURCE_MIC}
          onSelect={onSource}
          options={[
            {
              value: SOURCE_SYSAUDIO,
              label: t('share.source.sysaudio'),
              // 只有 daemon 明确说「一个可用后端都没有」时才真禁用。
              disabled: dead || pending,
              why: dead ? t('share.sys.none') : '',
            },
            { value: SOURCE_MIC, label: t('share.source.mic'), disabled: pending },
          ]}
        />
      </div>

      {/* 后端选择是二级：绝大多数用户不该关心它，但 plan §6 要求「允许配置强制指定」，
          而 probe 的 A/B 实测也只有在这里能被复现。 */}
      <label className="share-row sub" hidden={!sys}>
        <Icon name="plug" />
        <span className="share-label">{t('share.backend.label')}</span>
        <select
          className="select"
          data-testid={`${testid}-backend`}
          aria-label={t('share.backend.label')}
          value={backend || BACKEND_AUTO}
          disabled={pending || dead}
          onChange={(e) => onBackend(e.currentTarget.value)}
        >
          <option value={BACKEND_AUTO}>{t('share.backend.autoOption')}</option>
          {options.map((b) => (
            <option key={b.id} value={b.id} disabled={b.available === false}>
              {b.available === false
                ? t('share.backend.optionUnavailable', { name: b.label })
                : b.label}
            </option>
          ))}
          {stale ? (
            <option value={backend}>{t('share.backend.staleOption', { id: backend })}</option>
          ) : null}
        </select>
      </label>

      <p className="share-note" data-testid={`${testid}-note`}>{note}</p>

      {/* 系统音频录制授权：只指路，**不在这里触发系统弹窗**——权限的状态判定、
          「授权 / 去系统设置」的分支全在设置页的 PermissionRow 里，两处各写一遍
          迟早各说各话。 */}
      <div className="share-perm" data-testid={`${testid}-perm`} hidden={!needGrant}>
        <Icon name="shield" />
        <span className="share-perm-text">{grantText}</span>
        <button
          className="link-btn" type="button" data-testid={`${testid}-grant`}
          onClick={onGrant}
        >
          {t('share.perm.goto')}
        </button>
      </div>

      {/* 开不起来的原话。它比任何我们自己写的说明都准确，所以原样呈现。 */}
      <p className="share-fault" data-testid={`${testid}-fault`} hidden={!fault}>
        {t('share.fault', { reason: fault })}
      </p>
    </div>
  );
}
