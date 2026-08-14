// 对端详情：连接（档位 / 连通方式 / 虚拟设备 / 隧道地址）、活跃会话、地址历史、身份。
//
// # 2026-08-10 的重排（用户 22 条指令的 C 组）
//
// 顶部那一行现在带**两枚图标按钮**：别名（✎）与危险操作（⚠），各自开一个 Sheet。
// 它们从前是页面中段的一整张卡和页尾的一整块红色区域。搬上去的收益是页面主体只剩
// 「这台对端此刻怎么样」——连接、会话、地址、身份四块自上而下就是一条读下来的线；
// 代价是危险操作的**可发现性减弱**了，补偿是它多了一层 Sheet 再加原有的确认框，
// 误触反而更难。

import { useCallback, useState } from 'react';
import { Icon } from '../components/Icon';
import { confirmDialog } from '../components/ConfirmDialog';
import { Help } from '../components/Controls';
import { Sheet } from '../components/Sheet';
import { WIKI } from '../lib/external';
import { toast } from '../components/Toasts';
import { volumeText } from '../components/VolumeControl';
import { fmt, sessionFlow, dirLabel } from '../lib/fmt';
import { listFormat, t } from '../i18n';
import { actions, useStore } from '../state/store';
import { peerDeviceRows } from '../state/mode';
import { PeerTransportCard } from '../components/PeerTransport';
import { refreshPeers, rpc } from '../state/connection';
import type { PeerState, SessionInfo, VolumeState } from '../ipc/types';

function VolumeCell({ info }: { info: SessionInfo }) {
  const v = volumeText(info.stats?.volume as VolumeState | null | undefined);
  if (!v) return <span className="dim">{t('common.dash')}</span>;
  // dir=send 是本机在驱动对端设备，dir=recv 是对端在驱动本机设备——标题要分清。
  return (
    <span
      className={`vol-cell${v.muted ? ' muted' : ''}`}
      title={info.dir === 'recv' ? t('detail.volume.localOut') : t('detail.volume.remoteOut')}
    >
      <span>{v.text}</span>
      {v.adjustable ? null : <span className="tag warn">{t('volume.notAdjustable.tag')}</span>}
    </span>
  );
}

function VerdictCell({ v }: { v: { detected?: boolean; snr_db?: number } | null | undefined }) {
  if (!v) return <span className="dim">{t('common.dash')}</span>;
  return v.detected
    ? <span className="tag ok">{t('detail.verdict.pass', { snr: fmt.decimal1(v.snr_db) })}</span>
    : <span className="tag danger">{t('detail.verdict.fail')}</span>;
}

/** 别名的二级菜单（用户第 16 条）。入口是详情头上那枚 ✎。 */
function AliasSheet({ peer, onClose }: { peer: PeerState; onClose: () => void }) {
  const fp = peer.fingerprint;
  const [busy, setBusy] = useState(false);
  // Sheet 是一次编辑事务：轮询回来的 peer 对象继续刷新，但不能覆盖已经敲下的草稿。
  // 关闭 / Esc / 遮罩统一丢弃它，只有底部那一枚保存按钮会写 daemon。
  const [draft, setDraft] = useState(peer.alias || '');
  const saved = peer.alias || '';
  const normalized = draft.trim();
  const dirty = normalized !== saved;

  async function saveAlias() {
    if (busy || !dirty) return;
    const value = normalized || null;
    setBusy(true);
    try {
      const res = await rpc<{ display_name?: string }>('peers.set_alias', { peer: fp, alias: value });
      toast(value
        ? t('detail.alias.renamed', { name: (res && res.display_name) || value })
        : t('detail.alias.restored'), 'ok');
      await refreshPeers();
      onClose();
    } catch { /* rpc 已 toast */ } finally {
      setBusy(false);
    }
  }

  return (
    <Sheet
      testid="detail-alias-sheet"
      title={t('detail.alias.title')}
      /* 改名走「同 UID 就地更新」（spec-m5b §3.5）：AudioObjectID 不变，任何应用
         已记住的设备选择完全不受影响。这件事必须能查到，否则用户会因为怕搞乱
         Zoom 里的选择而不敢改名——细节在 wiki，由这枚 `?` 指过去。 */
      help={<Help label={t('wiki.deviceNaming')} url={WIKI.deviceNaming} testid="detail-alias-help" />}
      dismissLabel={t('common.cancel')}
      dismissDisabled={busy}
      onClose={onClose}
      footer={(
        <button
          className="btn ghost small" type="button" data-testid="detail-alias-clear"
          disabled={busy || (!draft && !saved)}
          onClick={() => setDraft('')}
        >
          {t('detail.alias.default')}
        </button>
      )}
      primaryAction={(
        <button
          className="btn primary" type="button" data-testid="detail-alias-save"
          disabled={busy || !dirty}
          onClick={() => void saveAlias()}
        >
          {t('common.save')}
        </button>
      )}
    >
      <div className="sheet-form-row">
        <label className="field">
          <span className="field-label">{t('detail.alias.field')}</span>
          <input
            className="input"
            data-testid="detail-alias-input"
            maxLength={48}
            placeholder={peer.name || t('detail.alias.placeholder')}
            autoComplete="off"
            spellCheck="false"
            value={draft}
            disabled={busy}
            onChange={(e) => setDraft(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key !== 'Enter' || !dirty) return;
              e.preventDefault();
              void saveAlias();
            }}
          />
        </label>
      </div>
      {/* 后果句。plan §3.1 第 4 类（后果）允许留在界面上——这个名字不只影响本页，
          它会改掉这台对端在系统设备列表里实际存在的虚拟设备名称，而那正是用户不敢按
          「保存」的原因。 */}
      <p className="muted small sheet-effect" data-testid="detail-alias-effect">{t('detail.alias.effect')}</p>
    </Sheet>
  );
}

export function DetailView() {
  const fp = useStore((s) => s.route.peerFp);
  const peer = useStore((s) => s.peers.find((p) => p.fingerprint === fp) || null);
  const sessions = useStore((s) => s.sessions);
  const daemon = useStore((s) => s.daemon);
  const addrHistory = useStore((s) => (fp ? s.addrHistory[fp] : undefined));
  const [unpairing, setUnpairing] = useState(false);
  // 详情头那两枚图标各开一个 Sheet。**同层只允许一个**（Sheet 的契约），所以这里
  // 是一个三态而不是两个布尔——两个布尔迟早会同时为真，叠出两层遮罩。
  const [sheet, setSheet] = useState<'alias' | 'danger' | null>(null);
  const closeSheet = useCallback(() => setSheet(null), []);

  const back = (
    <button className="btn ghost" type="button" data-testid="detail-back" onClick={() => actions.navigate('peers')}>
      <Icon name="back" />{t('detail.back')}
    </button>
  );

  if (!fp || !peer) {
    return (
      <>
        <div className="detail-top">{back}</div>
        <div className="empty card">
          <h3>{t('detail.notFound.title')}</h3>
          <p>{t('detail.notFound.desc')}</p>
        </div>
      </>
    );
  }

  // 重连中要和「离线」分开说：daemon 还在按退避重拨，不是放弃了。
  // 倒计时留给对端卡片——这张页面 stats 每秒重绘，而 retry_in_s 只随 peers.list 刷新。
  const reconnecting = !peer.online && !!peer.reconnecting;
  const pk = peer.public_key_b64 || '';
  const mine = sessions.filter((x) => x.peer_fingerprint === fp);

  const hist = (addrHistory || []).slice().sort((a, b) => b.seenAt - a.seenAt);
  const addrs: { addr: string; seenAt: number | null }[] = hist.length
    ? hist
    : (peer.last_addr ? [{ addr: peer.last_addr, seenAt: null }] : []);

  // 提前把窄化后的值抓成局部常量：下面两个函数是提升声明，TS 不把外层的
  // `if (!fp || !peer) return` 窄化带进去。
  const peerFp: string = fp;
  const cur: PeerState = peer;

  async function unpair() {
    const devices = peerDeviceRows(cur, daemon);
    const body = [
      t('detail.unpair.confirmLead', { name: cur.display_name || cur.name || peerFp }),
      devices.length
        ? t('detail.unpair.confirmDevices', {
          devices: listFormat(devices.map((device) => device.name || device.role)),
        })
        : t('detail.unpair.confirmNoDevices'),
    ];
    if (!await confirmDialog({
      title: t('detail.unpair.confirmTitle'),
      body,
      confirmText: t('detail.unpair'),
      danger: true,
      testid: 'confirm-unpair',
    })) return;
    setUnpairing(true);
    try {
      await rpc('peers.unpair', { peer: fp });
      toast(t('detail.unpair.done'), 'ok');
      await refreshPeers();
      actions.navigate('peers');
    } catch {
      setUnpairing(false);
    }
  }

  async function closeSession(id: number) {
    try {
      await rpc('session.close', { id });
      actions.removeSession(id);
      toast(t('session.closed', { id }), 'ok');
    } catch { /* rpc 已 toast */ }
  }

  return (
    <>
      <div className="detail-top">
        {back}
        <div className="detail-title">
          <span className={`dot ${peer.online ? 'online' : reconnecting ? 'connecting' : 'offline'}`} />
          <h2 className="detail-name">{peer.display_name || peer.name || t('peers.card.unnamed')}</h2>
          {/* 这两枚按钮没有文字，`aria-label` 是它们唯一说得出的话——而 §3.1 已经
              把 tooltip 当描述禁掉了，所以 `title` 只写「这按钮做什么」。 */}
          <button
            className="icon-btn" type="button"
            data-testid="detail-alias-open"
            aria-label={t('detail.alias.openLabel')} title={t('detail.alias.openLabel')}
            onClick={() => setSheet('alias')}
          >
            <Icon name="pencil" />
          </button>
          {/* 危险色不是装饰：这枚图标从一整块红色区域降级成了标题栏上的一枚小图标，
              不带色的话它读起来就是又一枚设置图标。 */}
          <button
            className="icon-btn danger" type="button"
            data-testid="detail-danger-open"
            aria-label={t('detail.danger.openLabel')} title={t('detail.danger.openLabel')}
            onClick={() => setSheet('danger')}
          >
            <Icon name="unlink" />
          </button>
          <span className={`detail-online ${peer.online ? 'ok' : 'dim'}`} data-testid="detail-online">
            {peer.online ? t('common.online') : reconnecting ? t('detail.reconnecting') : t('common.offline')}
          </span>
        </div>
      </div>

      {/* 「连接」在最前：用户点进详情页最常见的意图是「它现在怎么样、我能不能调」。
          「这是谁」（身份）搬到了页尾——核对指纹是配对当天的事，之后再也用不到。 */}
      <PeerTransportCard peer={peer} />

      <section className="card block">
        <h3 className="block-title">{t('detail.sessions.title')}</h3>
        <div className="table-wrap">
          <table className="table" data-testid="detail-sessions">
            <thead>
              <tr>
                <th>{t('detail.sessions.colSession')}</th>
                <th>{t('detail.sessions.colFlow')}</th>
                <th>{t('detail.sessions.colDir')}</th>
                <th>{t('detail.sessions.colBitrate')}</th>
                <th>{t('detail.sessions.colRung')}</th>
                <th>{t('detail.sessions.colLoss')}</th>
                <th>{t('detail.sessions.colJitter')}</th>
                <th>{t('detail.sessions.colVolume')}</th>
                <th>{t('detail.sessions.colVerdict')}</th>
                <th>{t('detail.sessions.colAction')}</th>
              </tr>
            </thead>
            <tbody>
              {mine.map((info) => {
                const st = info.stats || {};
                const flow = sessionFlow(info);
                // origin=hal 的会话是「某个应用选中了这台对端的虚拟设备」的结果。从背后
                // 把它关掉，应用的设备选择还留在那儿——它会继续对着一台不再出声的设备
                // 播放，而系统里没有任何地方能解释这件事。所以这里不给关闭入口。
                const managed = info.origin === 'hal';
                return (
                  <tr key={info.id} data-testid={`session-row-${info.id}`} className={flow.inbound ? 'inbound' : undefined}>
                    <td><code className="mono">{`#${info.id}`}</code></td>
                    <td data-testid={`session-flow-${info.id}`}>
                      {flow.label}
                      {flow.inbound ? <span className="tag warn">{t('session.tag.peerInitiated')}</span> : null}
                      {managed
                        ? <span className="tag accent" title={info.hal_device || ''}>{t('session.tag.virtualDevice')}</span>
                        : null}
                    </td>
                    <td>{dirLabel(info.dir)}</td>
                    <td>{t('peers.card.kbps', { v: fmt.kbps(st.bitrate_kbps) })}</td>
                    <td>{fmt.int(st.rung)}</td>
                    <td>{`${fmt.pct(st.loss_pct)}${t('stats.unit.pct')}`}</td>
                    <td>{t('stats.rttValue', { v: fmt.ms(st.jitter_ms) })}</td>
                    <td data-testid={`session-volume-${info.id}`}><VolumeCell info={info} /></td>
                    <td><VerdictCell v={st.verdict} /></td>
                    <td>
                      {managed
                        ? (
                          <span className="dim small" data-testid={`session-managed-${info.id}`}>
                            {t('session.managed')}
                          </span>
                        )
                        : (
                          <button
                            className="btn ghost small" type="button"
                            data-testid={`session-close-${info.id}`}
                            onClick={() => void closeSession(info.id)}
                          >
                            <Icon name="close" />{t('common.close')}
                          </button>
                        )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
        <p className="muted" data-testid="detail-sessions-empty" hidden={mine.length > 0}>
          {t('detail.sessions.empty')}
        </p>
      </section>

      {/* 地址历史紧跟会话之后（用户第 21 条）：两者都是「这条链路最近发生了什么」，
          而它从前夹在设备与会话之间，把那条线截成了两段。 */}
      <section className="card block">
        <h3 className="block-title">{t('detail.addrs.title')}</h3>
        <ul className="addr-list" data-testid="detail-addrs">
          {addrs.length
            ? addrs.map((h) => (
              <li key={h.addr}>
                <code className="mono">{h.addr}</code>
                <span className="dim small">
                  {h.seenAt ? t('detail.addrs.seenAt', { time: fmt.clock(h.seenAt) }) : t('detail.addrs.fromDaemon')}
                </span>
              </li>
            ))
            : <li className="muted">{t('detail.addrs.empty')}</li>}
        </ul>
      </section>

      {/* 身份在页尾（用户第 20 条）。§7.6 裁定 2「指纹保留」说的是**对端卡片**上那一段，
          它没有规定位置；核对指纹是配对当天的动作，之后这一块只是存档。 */}
      <section className="card block">
        <h3 className="block-title">{t('detail.identity')}</h3>
        <div className="fp-row">
          <code className="fp-full" data-testid="detail-fingerprint">{fp}</code>
          <button
            className="btn ghost small" type="button" data-testid="detail-copy-fp"
            onClick={async () => {
              try {
                await navigator.clipboard.writeText(fp);
                toast(t('detail.fpCopied'), 'ok');
              } catch {
                toast(t('common.copyFailed'), 'warn');
              }
            }}
          >
            <Icon name="copy" />{t('common.copy')}
          </button>
        </div>
        <div className="kv">
          <div className="kv-row">
            <span className="kv-k">{t('detail.defaultPort')}</span>
            <span>{peer.port != null ? String(peer.port) : t('common.dash')}</span>
          </div>
          <div className="kv-row">
            <span className="kv-k">{t('detail.pairedAt')}</span><span>{fmt.date(peer.added_unix)}</span>
          </div>
          <div className="kv-row">
            <span className="kv-k">{t('detail.publicKey')}</span>
            <code className="mono dim" title={pk}>
              {pk ? pk.slice(0, 24) + (pk.length > 24 ? '…' : '') : t('common.dash')}
            </code>
          </div>
        </div>
      </section>

      {sheet === 'alias' ? <AliasSheet peer={peer} onClose={closeSheet} /> : null}
      {/* 危险操作的二级菜单（用户第 17 条）。按下「解除配对」**仍然**走原来那道
          确认框——两道不是冗余：Sheet 挡的是误触，确认框挡的是误判（它逐条列出
          会从系统里消失的实际虚拟设备）。 */}
      {sheet === 'danger' ? (
        <Sheet
          testid="detail-danger-sheet"
          title={t('detail.danger.title')}
          help={<Help label={t('wiki.unpair')} url={WIKI.unpair} testid="detail-danger-help" />}
          dismissLabel={t('common.cancel')}
          dismissDisabled={unpairing}
          onClose={closeSheet}
          primaryAction={(
            <button
              className="btn danger" type="button" data-testid="detail-unpair"
              disabled={unpairing} onClick={() => void unpair()}
            >
              {t('detail.unpair')}
            </button>
          )}
        >
          <p className="muted small">
            {t('detail.unpair.confirmLead', { name: cur.display_name || cur.name || peerFp })}
          </p>
        </Sheet>
      ) : null}
    </>
  );
}
