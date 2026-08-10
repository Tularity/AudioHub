// 配对向导：**单栏**。
//
// 从前这里是左右两栏——左「接受配对」、右「发起配对」。左栏一半时间只是一个占了
// 半屏的按钮（空闲态除了一句描述什么都没有），而两栏并列还在暗示这是两件对等的事。
// 实际上「让对方找到我」是个偶尔用一次的开关，「找到对方并发起」才是每次都走的路。
// 所以前者收进二级菜单，后者独占这一页。
//
// 扫描循环与配对窗口的到期熄灯都**不在这个文件里**（见 state/discovery.ts）：
// 它们不该随着切页面、关面板而停。这里只剩订阅与渲染。

import { useEffect, useReducer, useRef, useState } from 'react';
import { Icon } from '../components/Icon';
import { Help } from '../components/Controls';
import { Sheet } from '../components/Sheet';
import { toast } from '../components/Toasts';
import { fmt } from '../lib/fmt';
import { WIKI } from '../lib/external';
import { classifyPeerAddr } from '../lib/peerAddr';
import { t, joinPhrases } from '../i18n';
import type { MsgKey } from '../i18n';
import { useTick } from '../lib/hooks';
import { discoverKey, isStale, remainingSecs, visibleResults } from '../lib/discovery';
import {
  maybeAutoScan, notePairViewLeft, pruneResults, scanDeadlineAt, toggleScan,
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

type StepState = 'idle' | 'running' | 'done' | 'failed';
const STEPS: MsgKey[] = ['pair.step.connect', 'pair.step.verifyPin', 'pair.step.exchangeKeys', 'pair.step.done'];

/**
 * 「让对方找到我」——配对窗口。
 *
 * 它是二级菜单而不是一直摊在页面上的一栏，但**开启之后主板块必须看得见**
 * （见 PairView 里那枚按钮的激活态）：一个悄悄开着的、允许局域网内任意主机
 * 发起配对的窗口，不是可以藏在面板背后的东西。
 *
 * ⚠ 这里**没有**到期熄灯的 effect。它搬去了 state/discovery.ts——留在这个组件里的话，
 * 关掉面板就等于关掉了那个判定，store 里会留一个早已过期的 pairing。
 */
function BeDiscoveredSheet({ onClose }: { onClose: () => void }) {
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
      footer={footer}
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

export function PairView() {
  const running = useStore((s) => s.discover.running);
  const results = useStore((s) => s.discover.results);
  const pairing = useStore((s) => s.pairing);
  const [armedOpen, setArmedOpen] = useState(false);
  const [addr, setAddr] = useState('');
  const [pin, setPin] = useState('');
  const [steps, setSteps] = useState<StepState>('idle');
  const [cliHint, setCliHint] = useState('');
  const [busy, setBusy] = useState(false);
  const pinRef = useRef<HTMLInputElement>(null);
  const addrRef = useRef<HTMLInputElement>(null);
  const [, bump] = useReducer((x: number) => x + 1, 0);

  // 剩余秒数（扫描窗口 / 配对窗口）自己在走，各需要一个节拍。
  useTick(1000, running || !!pairing);

  // 进页面自动起扫。**返回的不是「停止扫描」**——那正是本轮要治的病；只记一笔
  // 「什么时候离开的」，供冷却期判断用（见 state/discovery.ts）。
  useEffect(() => {
    maybeAutoScan();
    return notePairViewLeft;
  }, []);

  // daemon 还没连上就进到这一页时，上面那次自动起扫会被「conn !== 'online'」挡掉，
  // 而挡掉之后没有任何东西会再试一次——用户看到的是一页永远不扫的配对向导。
  // 订阅只活到离开本页为止，所以它不会在别的页面上悄悄开扫。
  useEffect(() => useStore.subscribe((s, prev) => {
    if (s.conn === 'online' && prev.conn !== 'online') maybeAutoScan();
  }), []);

  // 过期清理 + 陈旧态重算。不扫描时也要跑：一台刚才还在的主机不会因为我们停了扫描
  // 就一直显示成「刚刚见过」。
  useEffect(() => {
    const id = setInterval(() => { pruneResults(); bump(); }, STALE_TICK_MS);
    return () => clearInterval(id);
  }, []);

  // 进度是**诚实的粗粒度**：daemon 的 peers.pair 是一次同步 RPC，中间没有事件，
  // 所以只标「进行中 / 全部完成 / 失败」，绝不假装能看见握手的每一步。
  const stepCls = steps === 'idle' ? '' : steps === 'running' ? ' doing' : steps === 'done' ? ' done' : ' failed';

  async function doPair() {
    if (busy) return;
    const a = addr.trim();
    const p = pin.trim();
    if (!a) { toast(t('pair.right.needAddr'), 'warn'); addrRef.current?.focus(); return; }
    // **配对不走 WebSocket**，所以 URL 在这一格里是被拒的，且拒得早。
    // 让它发出去只会换回一条 daemon 侧的握手报错——用户看到的是「配对失败：
    // unexpected first frame」，而真正该说的话是「这一步请用 IP:端口」。
    const shape = classifyPeerAddr(a);
    if (shape.kind !== 'direct') {
      toast(
        shape.kind === 'badUrl'
          ? t(`addr.badUrl.${shape.reason}`, { addr: a })
          : t('addr.pairNotOverWs'),
        'warn',
      );
      addrRef.current?.focus();
      return;
    }
    if (!p) { toast(t('pair.right.needPin'), 'warn'); pinRef.current?.focus(); return; }
    if (isTauri()) {
      try { await ensureDaemon(); } catch { /* 连接失败会在下一步报出来 */ }
    }
    setBusy(true);
    setCliHint('');
    setSteps('running');
    try {
      // 配对成功后 daemon 会立刻为这台对端分配槽位并下发虚拟设备（模式 B），
      // 所以这里必须刷新对端列表——设备清单与卡片都靠它。
      const peer = await rpc<PeerState>('peers.pair', { addr: a, pin: p });
      setSteps('done');
      toast(t('pair.right.done', { name: (peer && (peer.display_name || peer.name)) || a }), 'ok');
      setPin('');
      await refreshPeers();
      actions.navigate('peers');
    } catch (e) {
      setSteps('failed');
      setCliHint(t('pair.right.failed', { message: String((e as Error)?.message || e), addr: a, pin: p }));
    } finally {
      setBusy(false);
    }
  }

  const now = Date.now();
  const list = visibleResults(results, now);
  const armedRemain = pairing ? Math.max(0, Math.ceil((pairing.expiresAt - now) / 1000)) : 0;

  return (
    <>
      <section className="card block" data-testid="pair-block">
        <div className="title-row">
          <h3 className="block-title">{t('pair.title')}</h3>
          <Help label={t('wiki.discovery')} url={WIKI.discovery} testid="pair-help" />
        </div>

        <div className="scan-row">
          <button
            className={`btn${running ? ' primary' : ''}`} type="button" data-testid="discover-run"
            onClick={toggleScan}
          >
            <Icon name="scan" />{running ? t('pair.right.stopScan') : t('pair.right.scan')}
          </button>
          <span className="spinner" hidden={!running} />
          {/* 窗口会自己到点收工，所以「还剩多久」是状态，不是装饰。 */}
          <span className="scan-remain" data-testid="discover-deadline" hidden={!running}>
            {t('pair.right.remain', { n: fmt.int(remainingSecs(scanDeadlineAt(), now)) })}
          </span>
          <button
            className={`btn scan-armed${pairing ? ' primary' : ''}`} type="button" data-testid="pair-armed-open"
            onClick={() => setArmedOpen(true)}
          >
            <Icon name="pair" />
            {pairing ? t('pair.armed', { n: fmt.int(armedRemain) }) : t('pair.left.open')}
          </button>
        </div>

        <div className="disc-list" data-testid="discover-list">
          {list.map((d) => {
            const key = discoverKey(d);
            const stale = isStale(d, now);
            const a = d.addrs && d.addrs.length
              ? `${d.addrs[0]}:${d.port}`
              : t('pair.right.portOnly', { port: String(d.port ?? '') });
            return (
              <button
                key={key} className={`disc-item card${stale ? ' stale' : ''}`} type="button"
                data-testid={`discover-item-${key}`}
                onClick={() => {
                  if (d.addrs && d.addrs.length) setAddr(`${d.addrs[0]}:${d.port}`);
                  pinRef.current?.focus();
                }}
              >
                <div className="disc-main">
                  <strong>{d.name || d.instance || t('pair.right.unknownHost')}</strong>
                  <span className="disc-tags">
                    {d.paired
                      ? <span className="tag ok">{t('pair.right.paired')}</span>
                      : <span className="tag">{t('pair.right.unpaired')}</span>}
                    {/* 「陈旧」是状态：一条四分钟前见过的记录不许和两秒前刚答复的
                        长成同一个样子。 */}
                    {stale
                      ? <span className="tag" data-testid={`discover-item-stale-${key}`}>{t('pair.right.stale')}</span>
                      : null}
                  </span>
                </div>
                {/* 地址与指纹是两条并列短语，分隔符由语料给（原来这里硬编码着 ` · `）。 */}
                <div className="disc-sub">{joinPhrases([a, d.fingerprint ? fmt.fp(d.fingerprint, 12) : null])}</div>
              </button>
            );
          })}
        </div>
        <p className="muted" data-testid="discover-empty" hidden={list.length > 0}>
          {t('pair.right.empty')}
        </p>

        <div className="divider" />
        <div className="manual-pair">
          <div className="form-row">
            <label className="field grow">
              <span className="field-label">{t('pair.right.addrLabel')}</span>
              <input
                ref={addrRef} className="input" data-testid="manual-pair-addr"
                placeholder={t('pair.right.addrPlaceholder')} autoComplete="off" spellCheck="false"
                value={addr} onChange={(e) => setAddr(e.currentTarget.value)}
              />
            </label>
            <label className="field">
              <span className="field-label">{t('pair.right.pinLabel')}</span>
              <input
                ref={pinRef} className="input pin-input" data-testid="manual-pair-pin"
                placeholder={t('pair.right.pinPlaceholder')} inputMode="numeric" maxLength={8} autoComplete="off"
                value={pin} onChange={(e) => setPin(e.currentTarget.value)}
              />
            </label>
            <span className="field-btn">
              <button
                className="btn primary" type="button" data-testid="manual-pair-btn"
                disabled={busy} onClick={() => void doPair()}
              >
                {busy ? t('pair.right.going') : t('pair.right.go')}
              </button>
            </span>
          </div>
          {/* 只在配对失败时可见 */}
          <p className="cli-hint" data-testid="pair-cli-hint" hidden={!cliHint}>{cliHint}</p>
          <ol className="pair-steps" data-testid="pair-progress">
            {STEPS.map((k) => (
              <li key={k} className={stepCls.trim() || undefined}><span className="step-dot" />{t(k)}</li>
            ))}
          </ol>
        </div>
      </section>

      {armedOpen ? <BeDiscoveredSheet onClose={() => setArmedOpen(false)} /> : null}
    </>
  );
}
