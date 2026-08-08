// 对端详情页的「传输档位」区块（plan §15）：**每对端 × 每方向**的四个控件。
//
// # 与卡片的分工，一句话
//
// **卡片 = 两个方向的实测值；这里 = 两个方向的目标值。**
// 卡片不放任何控件，这里不重复画色带明细。两者靠「方向行序一致 + 同一套
// `in`/`out` testid 段」对齐——用户才连得起「我设的 300」与「卡上的 304」。
//
// # 为什么必须常驻一句「这是目标」
//
// plan §14 附逐字：「用户看到 300 ms 时必须能分辨**这是自己设定的目标**而非
// 系统能力不足——当前界面对此一个字都没说，是本次误判的直接成因」。
// 于是那句话在这里是一个常驻段落，不是 tooltip：不悬停鼠标的人拿到的仍然
// 只有一个孤零零的毫秒数。
//
// # 共享模式：不隐藏、不置灰成空壳，**显示对端推来的值**
//
// 共享模式的机器**真的有两个执行器**，只是被远程指挥：本机 JB 由消费者的
// `send.latency` 驱动，本机阶梯由消费者的 `recv.quality` 驱动。隐藏 ⇒ 共享侧
// 的人永远看不到自己机器上正在被执行什么，而本次事故里缺的正是这个视图。
// 置灰成空壳 ⇒ 把一个正在生效的真实值画成「没有值」，撞 §14 裁定 2 的红线。

import { useMemo, useState } from 'react';
import { StopSlider } from './StopSlider';
import { t } from '../i18n';
import type { MsgKey } from '../i18n';
import { fmt } from '../lib/fmt';
import { latencyStops, normLatency, qualityStops, stopLabel } from '../lib/transportStops';
import { pickWorst, qualityDepthKey, readLatency, readQuality, splitByDirection } from '../lib/metrics';
import type { Dir } from '../lib/metrics';
import { rpc, refreshPeers } from '../state/connection';
import { useStore } from '../state/store';
import type { PeerState, SessionInfo } from '../ipc/types';

/** UI 的方向段（`in`/`out`）与 daemon 的方向串（`recv`/`send`）之间的唯一翻译点。
 *
 *  两套字母**必须只在这里相遇**。散在各处翻译的话，某一处写反就是一个方向的
 *  设置静默落到另一个方向上——而两个方向的执行器在**不同的机器上**，
 *  那种错误的表现是「我设的没生效」，没有任何报错。 */
const WIRE_DIR: Record<Dir, 'recv' | 'send'> = { in: 'recv', out: 'send' };

/** 行序：**发（out）在上、收（in）在下**。
 *
 *  这个顺序本身没有独立价值，它唯一的约束是**与卡片指标区逐字一致**
 *  （`PeerMetrics` 里 `<DirBlock dir="out">` 在 `<DirBlock dir="in">` 之前）。
 *  卡片给实测、这里给目标，两张表位置对得上，用户才能把「我设的 300」与
 *  「卡上的 304」连起来。序不一致 = 又一次让用户自己去猜对应关系。 */
const ROWS: Dir[] = ['out', 'in'];

/**
 * 一格：滑条 + 其下一行**实测读数**。
 *
 * 读数只念实测值，**绝不复述目标值**——把用户刚选的档复述一遍的读数，正是这个
 * 项目反复栽过的那种「报告成功、其实什么都没发生」。
 */
function Cell({ dir, kind, stops, value, live, disabled, onSelect }: {
  dir: Dir;
  kind: 'latency' | 'quality';
  stops: ReturnType<typeof latencyStops>;
  /** 目标档。`null` = 没有值（共享模式下对端未表态）。 */
  value: string | null;
  /** 实测读数那一行，已经是最终文案。 */
  live: string;
  disabled: boolean;
  onSelect: (v: string) => Promise<unknown>;
}) {
  const testid = `detail-${kind}-${dir}`;
  // `null`（未设定）时**不塞一个 auto 进去**：那会把「对端没表态」画成
  // 「对端明确要求 auto」，而 §14 裁定 2 的红线正是「不许用编造值冒充有数据」。
  // 传空串 ⇒ StopSlider 找不到匹配档，thumb 停在第一档而值标签照实显示空——
  // 所以未设定态由下面的 `.transport-unset` 整个接管，滑条只负责被禁用。
  return (
    <div className="transport-cell" data-testid={testid}>
      {value == null ? (
        <p className="transport-unset" data-testid={`${testid}-unset`}>
          {t('detail.transport.unset')}
        </p>
      ) : (
        <StopSlider
          testid={testid}
          label={t(kind === 'latency'
            ? (dir === 'in' ? 'detail.transport.latencyIn' : 'detail.transport.latencyOut')
            : (dir === 'in' ? 'detail.transport.qualityIn' : 'detail.transport.qualityOut'))}
          stops={stops}
          value={value}
          disabled={disabled}
          onSelect={onSelect}
        />
      )}
      <p className="transport-live" data-testid={`detail-transport-live-${kind}-${dir}`}>{live}</p>
    </div>
  );
}

/**
 * 实测读数。**只有实测值**：目标在滑条上，这里再念一遍就等于自证。
 *
 * 分支顺序即优先级——没有会话时连「测量中」都不该说，那是一句永远不会兑现的
 * 承诺（同 `metrics.ts` 里那条「缺席 ≠ 测量中」）。
 */
function liveLatency(list: SessionInfo[]): string {
  const s = pickWorst(list);
  if (!s) return t('detail.transport.noStream');
  const r = readLatency(s);
  const ms = r && typeof r.totalMs === 'number' ? r.totalMs : null;
  if (ms == null) return t('detail.transport.measuring');
  // 贴边的两句只在 daemon 说得出口时才说：它们是关于**物理**的断言，
  // 开环下地板是假设的 0，拿它宣布「已达下限」等于凭空造一个没测过的结论。
  if (s.stats?.at_floor) return t('detail.transport.liveAtFloor', { n: fmt.int(ms) });
  if (s.stats?.at_ceiling) return t('detail.transport.liveAtCeiling', { n: fmt.int(ms) });
  return t('detail.transport.liveMs', { n: fmt.int(ms) });
}

/**
 * ## 全应用里单位混淆最刺眼的一处（2026-08-04 用户实测）
 *
 * 这一行紧贴在音质滑条**正下方**，而滑条的档位标签逐字写着「PCM 48 kHz」。
 * 它此前读 `bandwidthKhz`，于是屏幕上相邻两行是：
 *
 *     [滑条]  PCM 48 kHz          ← 用户刚选的
 *     线上 24 kHz                 ← 这一行
 *
 * 两个数都对（24 kHz 是 48 kHz 采样率的奈奎斯特带宽），但**没有任何一个字**说明
 * 它们是两个量。任何人读到这两行的第一结论都是「我设了 48，实测只有 24，没生效」。
 *
 * 所以这里改读线上**采样率**：与正上方的滑条同量纲、同数字。这不是把读数换成
 * 「复述目标值」——它取自会话的实测线上速率，AUTO 掉档时它会与滑条上的档不一致，
 * 而那正是这一行存在的理由（见下面 `Cell` 的注释）。
 */
function liveQuality(list: SessionInfo[]): string {
  const s = pickWorst(list);
  if (!s) return t('detail.transport.noStream');
  const q = readQuality(s);
  const khz = q && typeof q.wireRateKhz === 'number' ? q.wireRateKhz : null;
  if (khz == null) return t('detail.transport.measuring');
  // 与滑条档位标签同量纲、同两个维度：滑条上写「PCM 48 kHz · 24 bit」，
  // 这一行就得写「线上格式 48 kHz · 24 bit」。只写采样率会让相邻两行看起来
  // 像是在说两件不同的事，而那正是这一行当初被改掉的理由。
  // 位深 → 文案键的映射表在 `lib/metrics`，**全应用只此一份**（理由见那里）。
  // 认不出的拼写没有条目 ⇒ 退回只写采样率那一行，不猜一个 16 bit 填上。
  const depth = qualityDepthKey(q?.wireDepth);
  if (!depth) return t('detail.transport.liveKhz', { n: fmt.int(khz) });
  return t('detail.transport.liveFormat', { khz: fmt.int(khz), depth: t(depth) });
}

export function PeerTransportCard({ peer }: { peer: PeerState }) {
  const fp = peer.fingerprint;
  const ds = useStore((s) => s.daemonSettings);
  const sessions = useStore((s) => s.sessions);
  const [busy, setBusy] = useState(false);
  const [help, setHelp] = useState(false);

  const lStops = useMemo(() => latencyStops(ds), [ds]);
  const qStops = useMemo(() => qualityStops(ds), [ds]);

  // 共享模式：本机不发起，故本机存的四个档对任何链路都不生效。
  // 判据取 `effective_mode`（真的在跑的那个），不取 `mode`（用户请求的）——
  // 请求了模式 B 但驱动没起来的机器实际跑在别的模式上。
  const shared = (ds?.effective_mode || ds?.mode) === 'share';
  const tr = peer.transport || {};

  // 卡片指标区按 `dir`（本机视角）分栏，这里用**同一个函数**——
  // 两处各写一份判据，就会出现「详情页的收对着卡片的发」这种谁也查不出来的错位。
  const { send: sendList, recv: recvList } = splitByDirection(sessions, fp);
  const listOf = (d: Dir): SessionInfo[] => (d === 'out' ? sendList : recvList);

  /**
   * 这一格显示什么值。
   *
   * 共享模式下显示的是**对端推来的**那一份，不是本机存的：本机那份照存不误
   * （切回 A/B 时它是这台对端的既有设置），但此刻它对任何链路都不生效，
   * 显示它就是撒谎。对端只推「执行器在本机」的那两个：
   *   - 本机 rx 的延迟目标（= 对端的 `send.latency`）→ 本机的**收**行延迟
   *   - 本机 tx 的音质目标（= 对端的 `recv.quality`）→ 本机的**发**行音质
   * 另外两格对端根本没有资格表态（执行器在它自己那边），恒为「未设定」。
   */
  function valueOf(dir: Dir, kind: 'latency' | 'quality'): string | null {
    if (shared) {
      if (dir === 'in' && kind === 'latency') {
        const v = tr.peer_rx_latency;
        return typeof v === 'string' ? normLatency(v) : null;
      }
      if (dir === 'out' && kind === 'quality') {
        const v = tr.peer_tx_quality;
        return typeof v === 'string' ? v : null;
      }
      return null;
    }
    const slot = dir === 'in' ? tr.recv : tr.send;
    const raw = slot ? slot[kind] : undefined;
    // 使用端这一侧**不许**出现「未设定」：daemon 对每台配对过的对端都给得出
    // 四个值（没设过就是 auto）。读不到只可能是旧服务 —— 回落到 auto 而不是
    // 空着，否则滑条会整个消失，用户连改都改不了。
    // 质量档**不做任何规范化**：daemon 装载时已经把认不出来的串重置为默认，
    // 这里再翻一次就是在前端复刻一份档表（那正是被删掉的那层兼容代码）。
    return typeof raw === 'string'
      ? (kind === 'latency' ? normLatency(raw) : raw)
      : 'auto';
  }

  /** 装载时被重置掉的档位格（daemon 报的原值）。空数组 = 一切正常。 */
  const resets = ROWS.flatMap((dir) => {
    const slot = dir === 'in' ? tr.recv : tr.send;
    const out: { dir: Dir; kind: 'latency' | 'quality'; old: string }[] = [];
    if (typeof slot?.latency_reset_from === 'string') {
      out.push({ dir, kind: 'latency', old: slot.latency_reset_from });
    }
    if (typeof slot?.quality_reset_from === 'string') {
      out.push({ dir, kind: 'quality', old: slot.quality_reset_from });
    }
    return out;
  });

  async function set(dir: Dir, kind: 'latency' | 'quality', v: string): Promise<void> {
    if (busy) return;
    setBusy(true);
    try {
      await rpc('peers.set_transport', { peer: fp, dir: WIRE_DIR[dir], [kind]: v });
      await refreshPeers();
    } catch { /* rpc 已 toast */ } finally {
      setBusy(false);
    }
  }

  /** 连通性档位（plan §16.2）。**另一个动词**，不是 `peers.set_transport`
   *  的第三个字段——那个方法的 `dir` 是必填的，而 tier 不分方向。 */
  async function setTier(v: string): Promise<void> {
    if (busy || v === tier) return;
    setBusy(true);
    try {
      await rpc('peers.set_tier', { peer: fp, tier: v });
      await refreshPeers();
    } catch { /* rpc 已 toast */ } finally {
      setBusy(false);
    }
  }

  // 读不到就是 `auto`，与四个档位串同一条理由：daemon 对每台配对过的对端都
  // 给得出一个值，读不到只可能是旧服务——空着会让整组按钮消失。
  const tier = typeof tr.tier === 'string' ? tr.tier : 'auto';
  const TIERS: { id: string; label: MsgKey; hint: MsgKey }[] = [
    { id: 'auto', label: 'detail.transport.tierAuto', hint: 'detail.transport.tierAutoHint' },
    { id: 'tier0', label: 'detail.transport.tier0', hint: 'detail.transport.tier0Hint' },
    { id: 'tier1', label: 'detail.transport.tier1', hint: 'detail.transport.tier1Hint' },
  ];

  return (
    <section className="card block" data-testid="detail-transport">
      <h3 className="block-title">{t('detail.transport.title')}</h3>
      {/* §14 裁定 4：**常驻**，不是 tooltip。不悬停鼠标的人拿到的仍然只有一个
          孤零零的毫秒数，而这句话正是本次误判缺的那一句。 */}
      <p className="muted small" data-testid="detail-transport-note">{t('detail.transport.note')}</p>
      {shared ? (
        <p className="transport-provenance" data-testid="detail-transport-shared">
          {t('detail.transport.sharedBy', { name: peer.display_name || peer.name || fp.slice(0, 8) })}
        </p>
      ) : null}
      {/* 盘上存着一个本 build 不认识的档位串时，daemon 已经把它重置为默认，
          这里**必须把这件事说出来**。静默重置与被删掉的那层静默翻译是同一个病：
          用户的选择消失了，而界面上处处自洽。 */}
      {resets.length ? (
        <p className="transport-reset" data-testid="detail-transport-reset">
          {resets.map((r) => t('detail.transport.stopReset', {
            dir: t(r.dir === 'out' ? 'peers.card.streamOut' : 'peers.card.streamIn'),
            kind: t(r.kind === 'latency' ? 'settings.transport.latency' : 'settings.transport.quality'),
            old: r.old,
          })).join(' ')}
        </p>
      ) : null}
      <div className="transport-grid" data-testid="detail-transport-grid">
        <span className="transport-corner" aria-hidden="true" />
        <span className="transport-col">{t('detail.transport.colLatency')}</span>
        <span className="transport-col">{t('detail.transport.colQuality')}</span>
        {ROWS.map((dir) => (
          <div className="transport-row" key={dir} data-dir={dir} data-testid={`detail-transport-row-${dir}`}>
            <span className="transport-rowname">
              <span className="dir-arrow" aria-hidden="true">{dir === 'out' ? '↑' : '↓'}</span>
              {t(dir === 'out' ? 'peers.card.streamOut' : 'peers.card.streamIn')}
            </span>
            <Cell
              dir={dir} kind="latency" stops={lStops}
              value={valueOf(dir, 'latency')}
              live={liveLatency(listOf(dir))}
              disabled={shared || busy}
              onSelect={(v) => set(dir, 'latency', v)}
            />
            <Cell
              dir={dir} kind="quality" stops={qStops}
              value={valueOf(dir, 'quality')}
              live={liveQuality(listOf(dir))}
              disabled={shared || busy}
              onSelect={(v) => set(dir, 'quality', v)}
            />
          </div>
        ))}
      </div>
      {/* 交叉的那半边**必须说出来**，否则「我改的是发送音质，为什么对端的采样率
          没动」这个问题在界面上无解。措辞按用户视角，不按执行器：用户不需要知道
          值被推到了哪里（plan §15 裁定 3），但需要知道「一个方向的两个旋钮不在
          同一台机器上执行」。 */}
      <p className="muted small" data-testid="detail-transport-where">{t('detail.transport.where')}</p>

      {/* ---- 连通方式（plan §16.2 的「手动覆盖恒可用」）--------------------
          放在四个档位**之后**：那四个是日常旋钮，这一个是「网络不让我直连」
          时才动的。三个互斥选项而不是一个开关——`auto` 与 `tier0` 不是同一件事
          （前者是「你决定」，后者是「钉住直连，别自己改」），做成开关就必须
          把其中一个藏起来。

          ⚠ 这里显示的是**用户的选择**，不是链路现在实际跑在哪一档。后者是
          §16.4 要求的一级信息（贴着延迟数字显示「经 TCP 中转」），它需要
          `transport_tier` 这个尚不存在的字段。**不得用这一组按钮冒充它**：
          选「自动」的对端此刻可能正跑在 tier 1 上，而这里仍然显示「自动」。 */}
      <div className="transport-tier" data-testid="detail-transport-tier">
        <h4 className="block-subtitle">{t('detail.transport.tierTitle')}</h4>
        <p className="muted small" data-testid="detail-transport-tier-note">
          {t('detail.transport.tierNote')}
        </p>
        {typeof tr.tier_reset_from === 'string' ? (
          <p className="transport-reset" data-testid="detail-transport-tier-reset">
            {t('detail.transport.tierReset', { old: tr.tier_reset_from })}
          </p>
        ) : null}
        <div className="transport-tier-row" role="radiogroup" aria-label={t('detail.transport.tierTitle')}>
          {TIERS.map((o) => (
            <button
              key={o.id}
              type="button"
              role="radio"
              aria-checked={tier === o.id}
              className={`transport-tier-opt${tier === o.id ? ' is-on' : ''}`}
              data-testid={`detail-transport-tier-${o.id}`}
              disabled={busy}
              onClick={() => void setTier(o.id)}
            >
              <span className="transport-tier-label">{t(o.label)}</span>
              <span className="transport-tier-hint">{t(o.hint)}</span>
            </button>
          ))}
        </div>
      </div>

      {/* ---- 两个档位的权威解释 --------------------------------------------
          `settings.transport.latencyDesc` / `qualityDesc` 是这两个旋钮的**权威
          解释**（延迟是端到端目标而非缓冲深度；音质一档定下采样率与位深两件事）。
          §15 把档位从设置页搬到这里时，那两条语料的渲染点留在了原地 ⇒ 它们成了
          **死键**：全仓无任何组件引用，界面上没有一处说得出位深是什么、为什么
          带宽翻倍、AUTO 为什么不会自己上去。

          所以接在这里。**收起态**是因为这两段很长，而这张卡的主角是四个控件；
          常驻会把控件挤下屏。留一段没人读的「权威解释」在语料里，下一个人会
          以为它在线上——那比没有更坏。 */}
      <button
        type="button"
        className="transport-help-toggle"
        data-testid="detail-transport-help-toggle"
        aria-expanded={help}
        aria-controls="detail-transport-help"
        onClick={() => setHelp((v) => !v)}
      >
        {t(help ? 'detail.transport.helpHide' : 'detail.transport.helpShow')}
      </button>
      <div
        className="transport-help"
        id="detail-transport-help"
        data-testid="detail-transport-help"
        hidden={!help}
      >
        <h4>{t('settings.transport.latency')}</h4>
        <p>{t('settings.transport.latencyDesc')}</p>
        <h4>{t('settings.transport.quality')}</h4>
        <p>{t('settings.transport.qualityDesc')}</p>
      </div>
    </section>
  );
}

/**
 * 设置页那张只读总览用得到：一台对端四个档的文本形态。
 *
 * # 这里为什么不再有「规范化」这一步
 *
 * 曾经有。质量档串有三条读路径（详情页滑条的 `valueOf`、本函数、共享模式的
 * 回显），每条都得记得调一次 `normQuality()` —— 而**本函数漏掉了**：同一个
 * 存盘值在详情页显示「PCM 32 kHz · 16 bit」、在这张总览里显示裸的 `pcm32k`，
 * 两处各说各话且没有任何一处会报错。那层兼容代码自己制造了这个回归。
 *
 * 现在 daemon 在**装载时一次性**把认不出来的串重置为默认（`StoredDir::sanitize`），
 * 于是这里拿到的永远是档表里有的 id，三条读路径不可能再分岔。
 * 重置这件事本身由 `quality_reset_from` / `latency_reset_from` 带到 UI 说明。
 */
export function transportCells(
  ds: import('../ipc/types').DaemonSettings | null,
  peer: PeerState,
): { dir: Dir; latency: string; quality: string }[] {
  const l = latencyStops(ds);
  const q = qualityStops(ds);
  const tr = peer.transport || {};
  return ROWS.map((dir) => {
    const slot = dir === 'in' ? tr.recv : tr.send;
    return {
      dir,
      // 读不到就是 `—`（`stopLabel` 返回 null）：**绝不填一个 auto 冒充**。
      latency: stopLabel(l, slot?.latency ? normLatency(slot.latency) : slot?.latency)
        ?? t('common.dash'),
      quality: stopLabel(q, slot?.quality) ?? t('common.dash'),
    };
  });
}
