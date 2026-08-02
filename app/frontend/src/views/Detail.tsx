// 对端详情：完整指纹、别名、虚拟设备、地址历史、会话列表、解除配对。

import { useEffect, useRef, useState } from 'react';
import { Icon } from '../components/Icon';
import { confirmDialog } from '../components/ConfirmDialog';
import { toast } from '../components/Toasts';
import { volumeText } from '../components/VolumeControl';
import { fmt, sessionFlow, dirLabel } from '../lib/fmt';
import { t, joinPhrases } from '../i18n';
import { actions, useStore } from '../state/store';
import { peerDeviceRows, halReasonText, halDeviceOf, deviceStateLabel, isModeB } from '../state/mode';
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

function AliasCard({ peer }: { peer: PeerState }) {
  const fp = peer.fingerprint;
  const [busy, setBusy] = useState(false);
  const ref = useRef<HTMLInputElement>(null);

  // 输入框只在用户没有在编辑时跟随 daemon，否则每秒一帧会把正在输入的字冲掉。
  useEffect(() => {
    const node = ref.current;
    if (node && document.activeElement !== node) node.value = peer.alias || '';
  }, [peer.alias]);

  async function setAlias(value: string | null) {
    if (busy) return;
    setBusy(true);
    try {
      const res = await rpc<{ display_name?: string }>('peers.set_alias', { peer: fp, alias: value });
      toast(value
        ? t('detail.alias.renamed', { name: (res && res.display_name) || value })
        : t('detail.alias.restored'), 'ok');
      await refreshPeers();
    } catch { /* rpc 已 toast */ } finally {
      setBusy(false);
    }
  }

  return (
    <section className="card block" data-testid="detail-alias">
      <h3 className="block-title">{t('detail.alias.title')}</h3>
      <div className="form-row">
        <label className="field grow">
          <span className="field-label">{t('detail.alias.field')}</span>
          <input
            ref={ref}
            className="input"
            data-testid="detail-alias-input"
            maxLength={48}
            placeholder={peer.name || t('detail.alias.placeholder')}
            autoComplete="off"
            spellCheck="false"
            defaultValue={peer.alias || ''}
            onKeyDown={(e) => {
              if (e.key !== 'Enter') return;
              e.preventDefault();
              void setAlias(e.currentTarget.value.trim() || null);
            }}
          />
        </label>
        <span className="field-btn">
          <button
            className="btn primary small" type="button" data-testid="detail-alias-save" disabled={busy}
            onClick={() => void setAlias((ref.current?.value || '').trim() || null)}
          >
            {t('common.save')}
          </button>
          <button
            className="btn ghost small" type="button" data-testid="detail-alias-clear"
            disabled={busy || !peer.alias}
            onClick={() => { if (ref.current) ref.current.value = ''; void setAlias(null); }}
          >
            {t('common.clear')}
          </button>
        </span>
      </div>
      {/* 改名走「同 UID 就地更新」（spec-m5b §3.5）：AudioObjectID 不变、设备列表不变，
          任何应用已记住的设备选择完全不受影响。这一点必须在界面上说出来，否则用户会
          因为怕搞乱 Zoom 里的选择而不敢改名。 */}
      <p className="muted small" data-testid="detail-alias-note">
        {peer.alias
          ? t('detail.alias.noteSet', { alias: peer.alias ?? '', name: peer.name || t('common.dash') })
          : t('detail.alias.noteEmpty')}
      </p>
    </section>
  );
}

function DevicesCard({ peer }: { peer: PeerState }) {
  const daemon = useStore((s) => s.daemon);
  const modeB = useStore(isModeB);
  const fp = peer.fingerprint;
  const rows = peerDeviceRows(peer, daemon);
  const info = halDeviceOf(daemon, fp);
  const dev = peer.hal_device;
  const published = !!dev && dev.state === 'bound' && !!dev.observed;

  let note: string;
  if (!rows.length) {
    note = modeB ? halReasonText(peer.hal_reason) : t('detail.devices.modeA');
  } else if (published) {
    note = peer.online ? t('detail.devices.published') : t('detail.devices.offline');
  } else {
    // 「已列出 / 尚未列出」是两句独立的话，不是一句里换一个词：别的语言可能整句改写。
    const state = deviceStateLabel(dev?.state) || t('common.dash');
    note = dev && dev.observed
      ? t('detail.devices.stateListed', { state })
      : t('detail.devices.stateUnlisted', { state });
  }

  return (
    <section className="card block" data-testid="detail-hal-devices">
      <div className="dev-inv-head">
        <h3 className="block-title">{t('detail.devices.title')}</h3>
        <code className="mono dim" data-testid="detail-hal-meta">
          {info ? t('device.slotGen', { slot: String(info.slot ?? ''), gen: String(info.generation ?? '') }) : ''}
        </code>
      </div>
      <div className="dev-list" hidden={rows.length === 0}>
        {rows.map((r) => (
          <div key={r.dir} className="dev-row" data-testid={`detail-device-${r.dir}`}>
            <Icon name={r.icon} cls="ico dev-ico" />
            <div className="dev-text">
              <span className="dev-name">{r.name || t('common.dash')}</span>
              <code className="dev-uid mono">{r.uid || ''}</code>
            </div>
            <span className="dev-frames mono">
              {joinPhrases([
                t('device.frames', { n: fmt.count(r.frames) }),
                r.dropped ? t('device.dropped', { n: fmt.count(r.dropped) }) : null,
              ])}
            </span>
            <span className={`dev-state ${r.io ? 'live' : published ? 'idle' : 'pending'}`}>
              {r.io ? t('device.inUse') : published ? t('device.idle') : t('device.awaiting')}
            </span>
          </div>
        ))}
      </div>
      <p className="muted small" data-testid="detail-hal-note">{note}</p>
    </section>
  );
}

export function DetailView() {
  const fp = useStore((s) => s.route.peerFp);
  const peer = useStore((s) => s.peers.find((p) => p.fingerprint === fp) || null);
  const sessions = useStore((s) => s.sessions);
  const addrHistory = useStore((s) => (fp ? s.addrHistory[fp] : undefined));
  const [unpairing, setUnpairing] = useState(false);

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
    const dev = cur.hal_device;
    const body = [
      t('detail.unpair.confirmLead', { name: cur.display_name || cur.name || peerFp }),
      dev
        ? t('detail.unpair.confirmDevices', { out: dev.out_name || '', in: dev.in_name || '' })
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
          <span className={`detail-online ${peer.online ? 'ok' : 'dim'}`} data-testid="detail-online">
            {peer.online ? t('common.online') : reconnecting ? t('detail.reconnecting') : t('common.offline')}
          </span>
        </div>
      </div>

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

      <AliasCard peer={peer} />
      <DevicesCard peer={peer} />

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
        <p className="muted small">{t('detail.addrs.note')}</p>
      </section>

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

      <section className="card block danger-block">
        <h3 className="block-title danger-title">{t('detail.danger.title')}</h3>
        <p className="muted">{t('detail.danger.desc')}</p>
        <div className="field-btn">
          <button
            className="btn danger" type="button" data-testid="detail-unpair"
            disabled={unpairing} onClick={() => void unpair()}
          >
            {t('detail.unpair')}
          </button>
        </div>
        <p className="muted small">{t('detail.danger.foot')}</p>
      </section>
    </>
  );
}
