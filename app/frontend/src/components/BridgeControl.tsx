// 每对端的「桥接到虚拟声卡」选择器。检测到才可选，未检测到置灰并给官网链接。

import { Icon } from './Icon';
import { ExtLink } from './Controls';
import { bridgeCatalog, vendors } from '../lib/bridge';
import type { DaemonInfo } from '../ipc/types';
import { t, listFormat } from '../i18n';

const NONE = '';

export function BridgeControl({
  testid, label, daemon, value, pending = false, onChange,
}: {
  testid: string;
  label?: string;
  daemon: DaemonInfo | null;
  /** 选中的虚拟声卡名，'' = 不桥接。 */
  value: string;
  pending?: boolean;
  onChange: (value: string) => void;
}) {
  const boxLabel = label ?? t('bridge.label');
  const catalog = bridgeCatalog(daemon);
  const list = (catalog || []).filter((c) => c.usable);
  const has = list.length > 0;
  // 选中的卡此刻不在可用名单里 = 这条偏好已经发不出去了（Peers 的 micParams 会
  // 拦下来），文案必须说明白，不能继续讲「将写入…」。
  const stale = !!value && !list.some((c) => c.name === value);

  let note: string;
  if (stale) {
    note = has
      ? t('bridge.stale.reselect', { name: value })
      : t('bridge.stale.reinstall', { name: value });
  } else if (!has) {
    const known = (catalog || []).filter((c) => c.present && !c.usable);
    note = catalog == null
      ? t('bridge.noField')
      : known.length
        ? t('bridge.presentUnusable', { names: listFormat(known.map((c) => c.name)) })
        : t('bridge.nothing');
  } else {
    note = value ? t('bridge.selected', { name: value }) : t('bridge.pick');
  }

  return (
    <div
      className={`bridge-box${has ? '' : ' unavailable'}`}
      data-testid={`${testid}-box`}
      // 对端卡片整体可点击（进入详情）：控件里的点击不能冒泡上去，否则一选就被导航走。
      onClick={(e) => e.stopPropagation()}
    >
      <label className="bridge-row">
        <Icon name="cable" />
        <span className="bridge-label">{boxLabel}</span>
        <select
          className="select"
          data-testid={testid}
          aria-label={boxLabel}
          value={value || NONE}
          disabled={!has || pending}
          onChange={(e) => onChange(e.currentTarget.value)}
        >
          {/* 选中的卡刚被拔掉/改名：留一个如实标注的条目，别让选择悄悄跳回「不桥接」。 */}
          {!list.length
            ? <option value={NONE}>{catalog != null ? t('bridge.undetected') : t('bridge.notReported')}</option>
            : <option value={NONE}>{t('bridge.none')}</option>}
          {list.map((c) => <option key={c.name} value={c.name}>{c.name}</option>)}
          {stale ? <option value={value}>{t('bridge.staleOption', { name: value })}</option> : null}
        </select>
      </label>
      <p className="bridge-note" data-testid={`${testid}-note`}>{note}</p>
      <div className="bridge-links" data-testid={`${testid}-links`} hidden={has}>
        {vendors().map((v) => (
          <ExtLink key={v.id} text={v.label} url={v.url} testid={`${testid}-link-${v.id}`} />
        ))}
      </div>
    </div>
  );
}
