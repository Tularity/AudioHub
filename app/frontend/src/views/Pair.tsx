// 配对向导：左「我要被发现」（PIN + 倒计时环），右「我要连别人」（发现列表 + 手动发起）。

import { useCallback, useEffect, useRef, useState } from 'react';
import { Icon } from '../components/Icon';
import { toast } from '../components/Toasts';
import { fmt, sleep } from '../lib/fmt';
import { classifyPeerAddr } from '../lib/peerAddr';
import { t, joinPhrases } from '../i18n';
import type { MsgKey } from '../i18n';
import { useTick } from '../lib/hooks';
import { actions, getState, useStore } from '../state/store';
import { ensureDaemon, refreshPeers, rpc } from '../state/connection';
import { isTauri } from '../ipc/endpoint';
import type { DiscoverResult, PeerState } from '../ipc/types';

const PAIR_TTL_S = 120;
const RING_R = 30;
const RING_C = 2 * Math.PI * RING_R;
// 单次短扫描之间的间隔：给共享的那一条 IPC 连接留出处理其它请求的空隙。
const SCAN_GAP_MS = 400;

function discKey(d: DiscoverResult): string {
  return d.fingerprint || `${d.instance || 'unknown'}-${d.port}`;
}

type StepState = 'idle' | 'running' | 'done' | 'failed';
const STEPS: MsgKey[] = ['pair.step.connect', 'pair.step.verifyPin', 'pair.step.exchangeKeys', 'pair.step.done'];

function BeDiscovered() {
  const pairing = useStore((s) => s.pairing);
  const [busy, setBusy] = useState(false);
  useTick(250, !!pairing);

  // 到期即熄灯。放在 effect 里而不是渲染中：渲染期间改 store 会引发级联更新。
  useEffect(() => {
    if (!pairing) return;
    if (pairing.expiresAt - Date.now() > 0) return;
    actions.setPairing(null);
    toast(t('pair.left.expired'), 'info');
  });

  const remain = pairing ? Math.max(0, (pairing.expiresAt - Date.now()) / 1000) : 0;
  const offset = pairing ? RING_C * (1 - remain / pairing.ttlS) : 0;

  return (
    <section className="card block pair-col">
      <h3 className="block-title">{t('pair.left.title')}</h3>
      <div className="pair-idle" hidden={!!pairing}>
        <p className="muted">{t('pair.left.desc')}</p>
        <button
          className="btn primary big" type="button" data-testid="pairing-enable" disabled={busy}
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
      </div>
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
        <div className="pin-display" data-testid="pin-display">
          {pairing ? [...pairing.pin].map((ch, i) => (
            // CSSOM 变量而不是内联 style 属性字符串（CSP）
            <span className="pin-digit" key={i} style={{ ['--i' as string]: String(i) } as React.CSSProperties}>{ch}</span>
          )) : null}
        </div>
        <p className="pair-tip">{t('pair.left.tip')}</p>
        <button
          className="btn" type="button" data-testid="pairing-disable"
          onClick={async () => {
            try { await rpc('pairing.disable', {}); } catch { /* ignore */ }
            actions.setPairing(null);
          }}
        >
          {t('pair.left.disable')}
        </button>
      </div>
    </section>
  );
}

function ConnectOthers() {
  const running = useStore((s) => s.discover.running);
  const results = useStore((s) => s.discover.results);
  const [addr, setAddr] = useState('');
  const [pin, setPin] = useState('');
  const [steps, setSteps] = useState<StepState>('idle');
  const [cliHint, setCliHint] = useState('');
  const [pairing, setPairing] = useState(false);
  const pinRef = useRef<HTMLInputElement>(null);
  const addrRef = useRef<HTMLInputElement>(null);

  // 循环短扫描（discover.run {secs:2}）合并结果，近似实时刷进。
  // scanGen 是唯一的「谁还有效」判据：反复点按钮只会作废旧循环，绝不叠加并发循环——
  // 多个 discover.run 会在同一条 IPC 连接上串成队头阻塞，把其它请求全拖到超时。
  const scanGen = useRef(0);
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      scanGen.current++;
      actions.setDiscoverRunning(false);
    };
  }, []);

  const scanLoop = useCallback(async (gen: number) => {
    while (alive.current && gen === scanGen.current && getState().discover.running) {
      try {
        const res = await rpc('discover.run', { secs: 2 }, { silent: true, timeoutMs: 15000 });
        if (!alive.current || gen !== scanGen.current) break;
        actions.mergeDiscover(res);
      } catch {
        if (!alive.current || gen !== scanGen.current) break;
        await sleep(1000);
        continue;
      }
      await sleep(SCAN_GAP_MS);
    }
  }, []);

  const toggleScan = useCallback(() => {
    if (getState().discover.running) {
      scanGen.current++;
      actions.setDiscoverRunning(false);
      return;
    }
    actions.setDiscoverRunning(true);
    const gen = ++scanGen.current;
    void scanLoop(gen);
  }, [scanLoop]);

  // 进度是**诚实的粗粒度**：daemon 的 peers.pair 是一次同步 RPC，中间没有事件，
  // 所以只标「进行中 / 全部完成 / 失败」，绝不假装能看见握手的每一步。
  const stepCls = steps === 'idle' ? '' : steps === 'running' ? ' doing' : steps === 'done' ? ' done' : ' failed';

  async function doPair() {
    if (pairing) return;
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
    setPairing(true);
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
      setPairing(false);
    }
  }

  const sorted = results.slice().sort((a, b) => (b.lastSeen || 0) - (a.lastSeen || 0));

  return (
    <section className="card block pair-col">
      <h3 className="block-title">{t('pair.right.title')}</h3>
      <div className="scan-row">
        <button
          className={`btn${running ? ' primary' : ''}`} type="button" data-testid="discover-run"
          onClick={toggleScan}
        >
          <Icon name="scan" />{running ? t('pair.right.stopScan') : t('pair.right.scan')}
        </button>
        <span className="spinner" hidden={!running} />
      </div>
      <div className="disc-list" data-testid="discover-list">
        {sorted.map((d) => {
          const key = discKey(d);
          const a = d.addrs && d.addrs.length
            ? `${d.addrs[0]}:${d.port}`
            : t('pair.right.portOnly', { port: String(d.port ?? '') });
          return (
            <button
              key={key} className="disc-item card" type="button" data-testid={`discover-item-${key}`}
              onClick={() => {
                if (d.addrs && d.addrs.length) setAddr(`${d.addrs[0]}:${d.port}`);
                pinRef.current?.focus();
              }}
            >
              <div className="disc-main">
                <strong>{d.name || d.instance || t('pair.right.unknownHost')}</strong>
                {d.paired
                  ? <span className="tag ok">{t('pair.right.paired')}</span>
                  : <span className="tag">{t('pair.right.unpaired')}</span>}
              </div>
              {/* 地址与指纹是两条并列短语，分隔符由语料给（原来这里硬编码着 ` · `）。 */}
              <div className="disc-sub">{joinPhrases([a, d.fingerprint ? fmt.fp(d.fingerprint, 12) : null])}</div>
            </button>
          );
        })}
      </div>
      <p className="muted" data-testid="discover-empty" hidden={results.length > 0}>
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
              disabled={pairing} onClick={() => void doPair()}
            >
              {pairing ? t('pair.right.going') : t('pair.right.go')}
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
        <p className="muted small">{t('pair.right.note')}</p>
      </div>
    </section>
  );
}

export function PairView() {
  return (
    <div className="pair-grid">
      <BeDiscovered />
      <ConnectOthers />
    </div>
  );
}
