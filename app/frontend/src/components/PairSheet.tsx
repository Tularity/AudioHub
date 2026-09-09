// 「添加对端」与「让对方找到我」——主面板上两枚按钮各自的二级菜单。
//
// 这里原本是**一整页**（`views/Pair.tsx`，导航第二格「配对向导」）。用户
// 2026-08-11 的第 1 条指令把那一页整个撤了：主面板上本来就有一枚「添加对端」
// 按钮，配对却要先切到另一页去做——一件偶尔做一次的事占着一格常驻导航，而每次
// 都得先离开自己真正在看的那一页。版式照搬原页（扫描行 → 发现列表 → 手动地址
// + PIN → 进度），只是宿主从 `view` 换成了 `Sheet`。
//
// 「让对方找到我」不再挤在扫描行右端，升为主面板上自己的一枚按钮：它是配对的
// **另一半**（我等别人来），不是扫描行的附属动作，而且开启期间必须在主面板上
// 一眼可见——一个悄悄开着的、允许局域网内任意主机发起配对的窗口，不该藏在两层
// 菜单背后。
//
// 扫描循环与配对窗口的到期熄灯**不在这个文件里**（见 state/discovery.ts）：
// 它们不该随着关面板、切页面而停。这里只剩订阅与渲染。

import { useEffect, useId, useReducer, useRef, useState } from 'react';
import { Icon } from './Icon';
import { Help } from './Controls';
import { Sheet } from './Sheet';
import { toast } from './Toasts';
import { fmt } from '../lib/fmt';
import { WIKI } from '../lib/external';
import { classifyPeerAddr } from '../lib/peerAddr';
import { t, joinPhrases } from '../i18n';
import { discoveredAddress } from '../lib/pairing';
import { useTick } from '../lib/hooks';
import { discoverKey, isStale, remainingSecs, visibleResults } from '../lib/discovery';
import {
  maybeAutoScan, notePairPanelLeft, pruneResults, scanDeadlineAt, toggleScan,
} from '../state/discovery';
import { actions, useStore } from '../state/store';
import { ensureDaemon, refreshPeers, rpc } from '../state/connection';
import { isTauri } from '../ipc/endpoint';
import type { PeerState } from '../ipc/types';

const PAIR_TTL_S = 120;
const RING_R = 30;
const RING_C = 2 * Math.PI * RING_R;
/** 重算「陈旧」并清掉过期条目的节拍。比 RESULT_STALE_MS 密得多，够它准点变灰。 */
const STALE_TICK_MS = 15_000;


/**
 * 「让对方找到我」——配对窗口。
 *
 * ⚠ 这里**没有**到期熄灯的 effect。它搬去了 state/discovery.ts——留在这个组件里的话，
 * 关掉面板就等于关掉了那个判定，store 里会留一个早已过期的 pairing。
 */
export function BeDiscoveredSheet({ onClose }: { onClose: () => void }) {
  const pairing = useStore((s) => s.pairing);
  const [busy, setBusy] = useState(false);
  useTick(250, !!pairing);

  const remain = pairing ? Math.max(0, (pairing.expiresAt - Date.now()) / 1000) : 0;
  const offset = pairing ? RING_C * (1 - remain / pairing.ttlS) : 0;

  const footer = pairing ? (
    <button
      className="btn" type="button" data-testid="pairing-disable"
      onClick={async () => {
        try { await rpc('pairing.disable', {}); } catch { /* ignore */ }
        actions.setPairing(null);
      }}
    >
      {t('pair.left.disable')}
    </button>
  ) : (
    <button
      className="btn primary" type="button" data-testid="pairing-enable" disabled={busy}
      onClick={async () => {
        setBusy(true);
        try {
          const res = await rpc<{ pin?: string | number }>('pairing.enable', { ttl_s: PAIR_TTL_S });
          actions.setPairing({
            pin: String((res && res.pin) ?? ''),
            ttlS: PAIR_TTL_S,
            expiresAt: Date.now() + PAIR_TTL_S * 1000,
          });
        } catch { /* rpc 已 toast */ } finally {
          setBusy(false);
        }
      }}
    >
      {t('pair.left.enable')}
    </button>
  );

  return (
    <Sheet
      testid="pair-armed-sheet"
      title={t('pair.left.title')}
      help={<Help label={t('wiki.discovery')} url={WIKI.discovery} testid="pair-armed-help" />}
      primaryAction={footer}
      dismissDisabled={busy}
      onClose={onClose}
    >
      <div className="pair-active" hidden={!pairing}>
        <div className="ring-wrap">
          <svg className="ring" viewBox="0 0 72 72" data-testid="pin-countdown">
            <circle className="ring-bg" cx={36} cy={36} r={RING_R} />
            <circle
              className="ring-fg" cx={36} cy={36} r={RING_R}
              strokeDasharray={RING_C.toFixed(2)}
              style={{ strokeDashoffset: String(offset) }}
            />
            <text className="ring-text" x={36} y={41} textAnchor="middle">
              {pairing ? fmt.int(Math.ceil(remain)) : '--'}
            </text>
          </svg>
        </div>
        {/* PIN 是**按下按钮之后才出现**的内容，而按钮本身随之被换掉、焦点无处可归。
            读屏用户因此不会被带到这几位数字上——所以这里是个 live 区域。 */}
        <div className="pin-display" data-testid="pin-display" aria-live="polite">
          {pairing ? [...pairing.pin].map((ch, i) => (
            // CSSOM 变量而不是内联 style 属性字符串（CSP）
            <span className="pin-digit" key={i} style={{ ['--i' as string]: String(i) } as React.CSSProperties}>{ch}</span>
          )) : null}
        </div>
      </div>
      {/* 后果句。§3.1 允许留在界面上的三类之一，所以它不搬 wiki——开着这扇窗户
          期间会发生什么，必须在按下开关的地方说。 */}
      <p className="muted" data-testid="pair-armed-consequence">{t('pair.left.desc')}</p>
    </Sheet>
  );
}

type PairTarget = { name: string; address: string; fingerprint?: string; paired?: boolean };

export function AddPeerSheet({ onClose }: { onClose: () => void }) {
  const running = useStore((s) => s.discover.running);
  const results = useStore((s) => s.discover.results);
  const peers = useStore((s) => s.peers);
  const [target, setTarget] = useState<PairTarget | null>(null);
  const [reconnect, setReconnect] = useState<PairTarget | null>(null);
  const [closing, setClosing] = useState(false);
  const [, bump] = useReducer((x: number) => x + 1, 0);
  useTick(1000, running);

  useEffect(() => { maybeAutoScan(); return notePairPanelLeft; }, []);
  useEffect(() => useStore.subscribe((s, prev) => {
    if (s.conn === 'online' && prev.conn !== 'online') maybeAutoScan();
  }), []);
  useEffect(() => {
    const id = setInterval(() => { pruneResults(); bump(); }, STALE_TICK_MS);
    return () => clearInterval(id);
  }, []);

  const now = Date.now();
  const list = visibleResults(results, now);
  const finishPair = () => { setTarget(null); setReconnect(null); setClosing(true); };

  return (
    <Sheet testid="add-peer-sheet" title={t('peers.addManual')}
      help={<Help label={t('wiki.discovery')} url={WIKI.discovery} testid="pair-help" />}
      onClose={onClose} closeRequested={closing} wide>
      <div className="pair-panel" data-testid="pair-block">
        <div className="scan-row">
          <button className={`btn${running ? ' primary' : ''}`} type="button" data-testid="discover-run" onClick={toggleScan}>
            <Icon name="scan" />{running ? t('pair.right.stopScan') : t('pair.right.scan')}
          </button>
          <span className="spinner" hidden={!running} />
          <span className="scan-remain" data-testid="discover-deadline" hidden={!running}>
            {t('pair.right.remain', { n: fmt.int(remainingSecs(scanDeadlineAt(), now)) })}
          </span>
        </div>
        <p className="muted small">{t('pair.selectDevice')}</p>
        <div className="disc-list" data-testid="discover-list">
          {list.map((d) => {
            const key = discoverKey(d);
            const stale = isStale(d, now);
            const address = discoveredAddress(d.addrs?.[0], d.port);
            const known = peers.find(p => p.fingerprint === d.fingerprint);
            const paired = !!known || !!d.paired;
            const name = known?.display_name || d.name || d.instance || t('pair.right.unknownHost');
            return (
              <button key={key} className={`disc-item card${stale ? ' stale' : ''}`} type="button"
                data-testid={`discover-item-${key}`} disabled={!address || (paired && !d.fingerprint)}
                onClick={() => {
                  if (!address) return;
                  const selected = { name, address, fingerprint: d.fingerprint, paired };
                  if (paired) setReconnect(selected); else setTarget(selected);
                }}>
                <div className="disc-main">
                  <strong>{name}</strong>
                  <span className="disc-tags">
                    <span className={`tag${paired ? ' ok' : ''}`}>{t(paired ? 'pair.right.paired' : 'pair.right.unpaired')}</span>
                    {stale ? <span className="tag" data-testid={`discover-item-stale-${key}`}>{t('pair.right.stale')}</span> : null}
                  </span>
                </div>
                <div className="disc-sub">{joinPhrases([address || t('pair.noAddress'), d.fingerprint ? fmt.fp(d.fingerprint, 12) : null])}</div>
              </button>
            );
          })}
        </div>
        <p className="muted" data-testid="discover-empty" hidden={list.length > 0}>{t('pair.right.empty')}</p>
        <div className="divider" />
        <div className="pair-alternatives">
          <button className="btn" type="button" data-testid="manual-pair-open"
            onClick={() => setTarget({ name: '', address: '' })}>{t('pair.manual')}</button>
          <button className="btn ghost" type="button" data-testid="reconnect-open"
            onClick={() => setReconnect({ name: '', address: '' })}>{t('peers.form.reconnectTitle')}</button>
        </div>
      </div>
      {target ? <PairingSheet target={target} onCancel={() => setTarget(null)} onSuccess={finishPair} /> : null}
      {reconnect ? <ReconnectSheet target={reconnect} onCancel={() => setReconnect(null)} onSuccess={finishPair} /> : null}
    </Sheet>
  );
}

/** The selected target stays fixed while discovery refreshes behind this layer. */
function PairingSheet({ target, onCancel, onSuccess }: {
  target: PairTarget; onCancel: () => void; onSuccess: () => void;
}) {
  const [addr, setAddr] = useState(target.address);
  const [pin, setPin] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [completed, setCompleted] = useState(false);
  const busyRef = useRef(false);
  const pinRef = useRef<HTMLInputElement>(null);
  const addrRef = useRef<HTMLInputElement>(null);
  const manual = !target.address;
  const formId = useId();

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (busyRef.current || completed) return;
    const address = addr.trim(), code = pin.trim();
    if (!address) { setError(t('pair.right.needAddr')); addrRef.current?.focus(); return; }
    const shape = classifyPeerAddr(address);
    if (shape.kind !== 'direct') {
      setError(shape.kind === 'badUrl' ? t(`addr.badUrl.${shape.reason}`, { addr: address }) : t('addr.pairNotOverWs'));
      addrRef.current?.focus(); return;
    }
    if (!code) { setError(t('pair.right.needPin')); pinRef.current?.focus(); return; }
    busyRef.current = true;
    setBusy(true); setError('');
    try {
      if (isTauri()) await ensureDaemon();
      const peer = await rpc<PeerState>('peers.pair', { addr: address, pin: code }, { silent: true });
      toast(t('pair.right.done', { name: peer?.display_name || peer?.name || address }), 'ok');
      setPin(''); setCompleted(true);
      // Trust is established once peers.pair returns. A later list poll must
      // not keep a successful PIN request waiting on another network round trip.
      void refreshPeers();
    } catch {
      setError(t('pair.right.failed'));
      pinRef.current?.focus();
    } finally {
      busyRef.current = false; setBusy(false);
    }
  }

  return (
    <Sheet testid="pair-device-sheet" title={target.name ? t('pair.withDevice', { name: target.name }) : t('pair.manual')}
      onClose={completed ? onSuccess : onCancel} closeRequested={completed} dismissDisabled={busy || completed}
      dismissLabel={t('common.cancel')} initialFocusRef={manual ? addrRef : pinRef}
      primaryAction={<button className="btn primary" type="submit" form={formId} data-testid="manual-pair-btn" disabled={busy || completed}>
        {busy ? t('pair.right.going') : t('pair.right.go')}
      </button>}>
      <form id={formId} className="pair-entry" onSubmit={submit} aria-busy={busy}>
        {manual ? <label className="field">
          <span className="field-label">{t('pair.right.addrLabel')}</span>
          <input ref={addrRef} className="input" data-testid="manual-pair-addr" value={addr}
            placeholder={t('pair.right.addrPlaceholder')} onChange={e => setAddr(e.currentTarget.value)}
            disabled={busy || completed} autoComplete="off" spellCheck={false} />
        </label> : <p className="pair-target-address mono" data-testid="pair-selected-address">{target.address}</p>}
        <p className="muted small">{t('pair.pinInstruction')}</p>
        <label className="field">
          <span className="field-label">{t('pair.right.pinLabel')}</span>
          <input ref={pinRef} className="input pin-input" data-testid="manual-pair-pin" value={pin}
            placeholder={t('pair.right.pinPlaceholder')} onChange={e => setPin(e.currentTarget.value)}
            inputMode="numeric" maxLength={8} autoComplete="one-time-code" disabled={busy || completed} />
        </label>
        <p className="pair-request-state" data-testid="pair-progress" role="status" hidden={!busy}>{t('pair.right.going')}</p>
        <p className="form-error" data-testid="pair-cli-hint" role="alert" hidden={!error}>{error}</p>
      </form>
    </Sheet>
  );
}

function ReconnectSheet({ target, onCancel, onSuccess }: {
  target: PairTarget; onCancel: () => void; onSuccess: () => void;
}) {
  const [pending, setPending] = useState(false);
  const [completed, setCompleted] = useState(false);
  return (
    <Sheet testid="reconnect-sheet" title={target.name || t('peers.form.reconnectTitle')}
      onClose={completed ? onSuccess : onCancel} closeRequested={completed} dismissDisabled={pending || completed}>
      {target.paired ? <p className="muted small">{t('pair.alreadyTrusted')}</p> : null}
      <ReconnectForm onClose={() => setCompleted(true)} initialPeer={target.fingerprint} initialAddr={target.address} onPending={setPending} />
    </Sheet>
  );
}

/**
 * 已配对主机按指纹重连（`peers.connect`）。
 *
 * 与上面那半张面板是**两条不同的 RPC**，别合并：`peers.pair` 建立信任（要 PIN），
 * 这一条只是对一台早已互签过的主机重新拨号（要指纹，地址可留空走最近一次的）。
 * 把它折进「添加对端」而不是留在主面板上一张常驻表单里，是因为它一年用不到一次，
 * 却在主面板顶上占着一整行。
 */
function ReconnectForm({ onClose, initialPeer = '', initialAddr = '', onPending }: {
  onClose: () => void; initialPeer?: string; initialAddr?: string; onPending: (pending: boolean) => void;
}) {
  const peers = useStore((s) => s.peers);
  const peerRef = useRef<HTMLInputElement>(null);
  const [addr, setAddr] = useState(initialAddr);
  const [peerVal, setPeerVal] = useState(initialPeer);
  const [pending, setPending] = useState(false);
  const pendingRef = useRef(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (pendingRef.current) return;
    const peer = peerVal.trim();
    const a = addr.trim();
    if (!peer) {
      toast(t('peers.form.needFingerprint'), 'warn');
      peerRef.current?.focus();
      return;
    }
    // M8 P6：这一格接受 `ws://…`（Tier 2 over WebSocket），因为地址本身就是
    // 传输选择。拒的只有两种：解析不出来的 URL，和本 build 拨不动的 `wss://`。
    // **拒在这里而不是让 daemon 拒**：一个存得下、拨不动的地址在界面上看起来
    // 完全正常，直到用户去连它。
    const shape = classifyPeerAddr(a);
    if (shape.kind === 'badUrl') {
      toast(t(`addr.badUrl.${shape.reason}`, { addr: a }), 'warn');
      return;
    }
    if (shape.kind === 'wss') {
      toast(t('addr.wssUnsupported'), 'warn');
      return;
    }
    pendingRef.current = true; setPending(true); onPending(true);
    try {
      // 超时由 ipc/client.ts 的方法级表给出（daemon 最坏 TCP 5s + 握手 10s）。
      await rpc('peers.connect', a ? { peer, addr: a } : { peer });
      toast(t('peers.form.done'), 'ok');
      setAddr('');
      onClose();
      void refreshPeers();
    } catch { /* rpc 已 toast */ } finally {
      pendingRef.current = false; setPending(false); onPending(false);
    }
  };

  return (
    <form className="add-peer-form" data-testid="add-peer-form" onSubmit={submit}>
      <h4 className="pair-sub-title">{t('peers.form.reconnectTitle')}</h4>
      <div className="form-row">
        <label className="field">
          <span className="field-label">{t('peers.form.fingerprint')}</span>
          <input
            ref={peerRef}
            className="input"
            data-testid="add-peer-peer"
            list="ah-peer-fps"
            placeholder={t('peers.form.fingerprintPlaceholder')}
            autoComplete="off"
            spellCheck="false"
            value={peerVal}
            onChange={(e) => setPeerVal(e.currentTarget.value)}
          />
          <datalist id="ah-peer-fps">
            {peers.map((p) => <option key={p.fingerprint} value={p.fingerprint}>{p.name || ''}</option>)}
          </datalist>
        </label>
        <label className="field grow">
          <span className="field-label">{t('peers.form.addr')}</span>
          <input
            className="input"
            data-testid="add-peer-input"
            placeholder={t('peers.form.addrPlaceholder2')}
            autoComplete="off"
            spellCheck="false"
            value={addr}
            onChange={(e) => setAddr(e.currentTarget.value)}
          />
        </label>
        <span className="field-btn">
          <button className="btn" type="submit" data-testid="add-peer-connect" disabled={pending}>
            {pending ? t('common.connecting') : t('common.connect')}
          </button>
        </span>
      </div>
    </form>
  );
}
