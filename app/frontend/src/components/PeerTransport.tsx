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

import { useEffect, useMemo, useRef, useState } from 'react';
import { Help } from './Controls';
import { Icon } from './Icon';
import {
  DeviceVolumeControl, deviceVolumeEndpointFor, deviceVolumeRequest,
} from './VolumeControl';
import { StopSlider } from './StopSlider';
import { toast } from './Toasts';
import { t, joinPhrases } from '../i18n';
import { WIKI } from '../lib/external';
import { fmt } from '../lib/fmt';
import { checkEndpoint } from '../lib/peerAddr';
import { latencyStops, normLatency, qualityStops } from '../lib/transportStops';
import { pickWorst, qualityDepthKey, readLatency, readQuality, splitByDirection } from '../lib/metrics';
import type { Dir } from '../lib/metrics';
import {
  TIER_CHOICES, TIER_LABEL, TIER_PICK_HINT, TIER_PICK_LABEL, TIER_WHY,
  effectiveTier, endpointShadowsTier, endpointVisible, isDegradedTier, muxLink, tcpMediaLink,
  tierPickLabel, tierUnknownWhy,
} from '../lib/tier';
import { rpc, refreshPeers } from '../state/connection';
import {
  MODE_B, halDeviceOf, peerAudioDirections, peerDeviceRows, peerDevicesNote,
  isModeB, peerHasNoAudioDirections, requestedMode, selectIsShareMode,
} from '../state/mode';
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

/**
 * 二级页面上的**现状**行 + 证据。plan §16.4 第 4 条：「提级不等于搬家」——
 * 一级只回答「为什么慢」，二级回答「凭什么这么判的、我能不能改」，二级的内容
 * 一条不减。
 *
 * ## 三态在这里必须各有各的样子（§16.4 第 5 条那条红线的落点）
 *
 * | 现状 | 这一行长什么样 |
 * |---|---|
 * | Tier 0（已判定为直连） | 正文色，写「直连（UDP）」 |
 * | Tier 1 / 2（已判定为降级） | warn 色，写传输形态 + 后果 |
 * | 未判定 | **灰色的「—」** + 一句说清是哪一种不知道 |
 *
 * 卡片上 Tier 0 不挂徽标（第 3 条），但那是「不画」而不是「等同于未判定」——
 * 分辨这两者的地方就是这一行。
 *
 * ## 关于「原因串」与「判定时间」
 *
 * §16.4 第 4 条要求二级页面给出这两项。daemon 目前**不上报**它们
 * （`transport_reason` / `transport_since` 在契约里都还不存在），所以这里
 * 明写「本版服务未上报」而不是留空：留空会被读成「没有原因」，而事实是
 * 「这一版说不出来」。能给的证据（链路地址、存活、两个降级计数）照给。
 */
function TierNow({ peer }: { peer: PeerState }) {
  const fp = peer.fingerprint;
  const daemon = useStore((s) => s.daemon);
  const tier = effectiveTier(daemon, peer);
  const degraded = isDegradedTier(tier);
  const link = degraded ? tcpMediaLink(daemon, fp) : undefined;
  const mux = tier === 'tier2' ? muxLink(daemon, fp) : undefined;

  return (
    <div className="transport-now" data-testid="detail-transport-now" data-tier={tier || 'unknown'}>
      <div className="transport-now-head">
        <span className="transport-now-cap">{t('tier.now.cap')}</span>
        {/* `.unknown` 的暗色**只**表示读不到，与全应用同一条规矩（`metric-val.unknown`）。
            Tier 0 走正文色：它是一个真结论，不该长得像没读到。 */}
        <span
          className={`transport-now-val${tier ? (degraded ? ' warn' : '') : ' unknown'}`}
          data-testid="detail-transport-now-value"
        >
          {tier ? t(TIER_LABEL[tier]) : t('common.dash')}
        </span>
        <span className="transport-now-why" data-testid="detail-transport-now-why">
          {tier ? t(TIER_WHY[tier]) : t(tierUnknownWhy(daemon) === 'unsupported'
            ? 'tier.now.unknownUnsupported'
            : 'tier.now.unknownOffline')}
        </span>
      </div>
      {/* 证据。**只在降级时出现**：Tier 0 上这条链路根本不存在，画一行全是「—」的
          计数器就是拿「不适用」冒充「零」。 */}
      {degraded ? (
        <div className="transport-now-facts" data-testid="detail-transport-now-facts">
          <span data-testid="detail-transport-now-addr">
            {t('tier.now.linkAddr', { addr: link?.peer || t('common.dash') })}
          </span>
          <span data-testid="detail-transport-now-alive">
            {t(link?.alive === true ? 'tier.now.linkAlive'
              : link?.alive === false ? 'tier.now.linkDead'
                : 'tier.now.linkAliveUnknown')}
          </span>
          {/* 这两个数是解释「降级链路为什么难听」的**仅有两个**（design §5.2 第 4 条）。
              读不到就写「—」，**不折成 0**：0 在这两个量上是一句强断言（队列没积压、
              闸门没丢过帧），而它正是本项目反复栽过的那种编造。 */}
          <span data-testid="detail-transport-now-writeq">
            {t('tier.now.writeq', { ms: fmt.decimal1(link?.writeq_ms) })}
          </span>
          <span data-testid="detail-transport-now-stale">
            {/* `fmt.int` 而**不是** `fmt.count`：后者把缺席折成 0，而 0 在这个量上
                是一句强断言（闸门一帧都没丢过）。缺席要写「—」。 */}
            {t('tier.now.stale', { n: fmt.int(link?.stale_dropped) })}
          </span>
          {mux ? (
            <span data-testid="detail-transport-now-mux">
              {t('tier.now.muxFrames', {
                w: fmt.int(mux.control_frames_written),
                r: fmt.int(mux.control_frames_read),
              })}
            </span>
          ) : null}
        </div>
      ) : null}
      {/* §16.4 第 4 条点名的两项，daemon 还给不出来。**明写出来**而不是留白：
          留白会被读成「没有原因」。 */}
      {degraded ? (
        <p className="muted small" data-testid="detail-transport-now-gap">{t('tier.now.reasonGap')}</p>
      ) : null}
    </div>
  );
}

/**
 * 隧道地址那一格（plan §16.2 的「地址即传输选择」）。
 *
 * # 为什么它必须在**详情页**，而不是只在「添加对端」表单里
 *
 * 「添加对端」那一格接受 `ws://…`，但它走的是 `peers.connect`，而 daemon 在
 * `conn.rs` 上明写那个 URL **不落盘**（「调用方要的是一次连接，不是一个设置」）。
 * 于是隧道地址**重连即失忆**，而唯一能存住它的入口在命令行（`--endpoint`）。
 * 更糟的是语料里已经有一句 `addr.pairNotOverWs` 逐字告诉用户「先用 IP:端口
 * 完成配对，**再到该对端的详情里把地址改成隧道 URL**」——而详情里根本没有这一格。
 * 一句写在界面上的、做不到的指路，比不写更坏。
 *
 * # 为什么存地址**不顺手把上面那一档改成 tier 2**
 *
 * 因为那是**替用户改他的选择**。daemon 的判据是「或」（见 `dialsMultiplexed`），
 * 地址存下之后复用**已经生效**，不需要再动那一档；而动了之后，用户哪天清掉
 * 地址，留在选择器上的就是一个他从没点过的 tier 2 钉子。
 * 取而代之的是把「这一格盖过了那一组」直接写在屏幕上。
 */
function EndpointField({ fp, tier, endpoint, reset }: {
  fp: string;
  tier: string;
  endpoint: string;
  reset?: string | null;
}) {
  const [busy, setBusy] = useState(false);
  const ref = useRef<HTMLInputElement>(null);

  // 与改名那一格同一条：输入框只在用户没在编辑时跟随 daemon，否则每秒一帧的
  // 刷新会把正在敲的字冲掉。
  useEffect(() => {
    const node = ref.current;
    if (node && document.activeElement !== node) node.value = endpoint;
  }, [endpoint]);

  async function save(raw: string): Promise<void> {
    if (busy) return;
    // 拒在按下按钮之前。daemon 也会拒（`WsUrl::parse` + `require_plaintext`），
    // 但那条报错是英文的、带 Rust 上下文的，而这里四种写错方式各有一句能照做
    // 的中文——尤其 `wss://`，它的问题不是「写错了」而是「这一版没有 TLS 客户端」。
    const chk = checkEndpoint(raw);
    if (!chk.ok) {
      toast(
        chk.why === 'notUrl' ? t('addr.endpointNeedsUrl')
          : chk.why === 'wss' ? t('addr.wssUnsupported')
            : t(`addr.badUrl.${chk.why}`, { addr: raw.trim() }),
        'warn',
      );
      ref.current?.focus();
      return;
    }
    setBusy(true);
    try {
      // `tier` 原样带上：`peers.set_tier` 的 `tier` 是必填的，而这一次操作
      // 要改的只有地址。带上当前值 ⇒ daemon 侧 `changed` 只由地址决定，
      // 地址没变就不会白拆一条健康的连接。
      await rpc('peers.set_tier', { peer: fp, tier, endpoint: chk.value });
      toast(chk.value
        ? t('detail.transport.endpointSaved', { addr: chk.value })
        : t('detail.transport.endpointCleared'), 'ok');
      await refreshPeers();
    } catch { /* rpc 已 toast */ } finally {
      setBusy(false);
    }
  }

  return (
    <div className="transport-endpoint" data-testid="detail-transport-endpoint">
      <div className="title-row">
        <h4 className="block-subtitle">{t('detail.transport.endpointTitle')}</h4>
        <Help label={t('wiki.tunnel')} url={WIKI.tunnel} testid="detail-transport-endpoint-help" />
      </div>
      {/* 盘上那串读不懂、已被 daemon 清空 ⇒ 说出来。与档位重置同一条纪律：
          静默清除等于用户的设置消失了，而界面处处自洽。 */}
      {typeof reset === 'string' ? (
        <p className="transport-reset" data-testid="detail-transport-endpoint-reset">
          {t('detail.transport.endpointReset', { old: reset })}
        </p>
      ) : null}
      <div className="form-row">
        <label className="field grow">
          <span className="field-label">{t('detail.transport.endpointField')}</span>
          <input
            ref={ref}
            className="input"
            data-testid="detail-transport-endpoint-input"
            placeholder={t('detail.transport.endpointPlaceholder')}
            autoComplete="off"
            spellCheck="false"
            defaultValue={endpoint}
            onKeyDown={(e) => {
              if (e.key !== 'Enter') return;
              e.preventDefault();
              void save(e.currentTarget.value);
            }}
          />
        </label>
        <span className="field-btn">
          <button
            className="btn primary small" type="button"
            data-testid="detail-transport-endpoint-save" disabled={busy}
            onClick={() => void save(ref.current?.value || '')}
          >
            {t('common.save')}
          </button>
          <button
            className="btn ghost small" type="button"
            data-testid="detail-transport-endpoint-clear" disabled={busy || !endpoint}
            onClick={() => { if (ref.current) ref.current.value = ''; void save(''); }}
          >
            {t('common.clear')}
          </button>
        </span>
      </div>
      {/* **这一格盖过上面那一组**时必须说出来，否则一个选着「直连（UDP）」又填了
          `ws://` 的用户会一直以为自己在直连——而 daemon 说他不是。 */}
      {endpointShadowsTier(tier, endpoint) ? (
        <p className="transport-reset" data-testid="detail-transport-endpoint-shadow">
          {t('detail.transport.endpointShadow', { tier: t(tierPickLabel(tier)) })}
        </p>
      ) : null}
    </div>
  );
}

/**
 * 这台对端实际请求的虚拟设备（用户 2026-08-10 第 18 条：并入「连通方式」下面）。
 *
 * 它此前是详情页上一张独立的卡（`Detail.tsx` 的 `DevicesCard`）。搬进来是因为
 * 「这台对端怎么连的」与「它在我系统里长成哪些设备」是同一个问题的两半，
 * 而分成两张卡时中间隔着别的板块。
 *
 * # 为什么渲染判据取 `requestedMode` 而不是 `effectiveMode`
 *
 * 请求了模式 B、驱动却没起来的机器，这一块**照样要出现**：用户此刻要的正是
 * 「我的设备呢」那个答案，而它由下面 `halReasonText()` 那一行给出（`hal_reason`
 * 的每个取值在那里都有一句能照做的话，缺席也有 `halReason.none` 兜底，不会
 * 渲染成空串）。判据若取 `effectiveMode`，降级的那一刻整块消失——最需要解释的
 * 时候界面上一个字都没有。
 *
 * 模式 A / 共享模式下整块**不渲染**：那两个模式下它唯一的内容是一句
 * 「当前为模式 A，没有虚拟设备」，而 plan §3.1 恰好禁止这种纯描述句。
 * 调用点负责判据，本组件只管画——两处都判会分岔。
 */
function PeerDevices({ peer }: { peer: PeerState }) {
  const daemon = useStore((s) => s.daemon);
  const localModeB = useStore(isModeB);
  const active = localModeB && !!peer.online && peer.peer_mode === 'share' && !peer.peer_unusable;
  const inactiveNote = localModeB
    ? t('volume.devicePeerUnavailable')
    : t('volume.deviceModeUnavailable');
  const fp = peer.fingerprint;
  const rows = peerDeviceRows(peer, daemon);
  const info = halDeviceOf(daemon, fp);
  const device = peer.hal_device;
  // 那一句是四叉分支，住在 `state/mode.ts` 里并有单测——它在这次搬家里换了判据，
  // 而「搬家 + 改判据」正是本仓反复栽跟头的组合。
  const note = peerDevicesNote(peer, daemon);

  // 明确零能力是正常能力集，不是一个「暂无设备」故障卡。
  if (peerHasNoAudioDirections(peer)) return null;

  return (
    <div className="transport-devices" data-testid="detail-hal-devices">
      <div className="dev-inv-head">
        <div className="title-row">
          <h4 className="block-subtitle">{t('detail.devices.title')}</h4>
          <Help label={t('wiki.devices')} url={WIKI.modeB} testid="detail-devices-help" />
        </div>
        <code className="mono dim" data-testid="detail-hal-meta">
          {info ? t('device.slotGen', { slot: String(info.slot ?? ''), gen: String(info.generation ?? '') }) : ''}
        </code>
      </div>
      <div className="dev-list" hidden={rows.length === 0}>
        {rows.map((r) => {
          const output = r.dir === 'out';
          const endpoint = deviceVolumeEndpointFor(r.dir);
          const reported = output ? device?.out_volume : device?.in_volume;
          const volumePending = output ? device?.out_volume_pending : device?.in_volume_pending;
          return (
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
              <DeviceVolumeControl
                peer={fp}
                endpoint={endpoint}
                version={device?.device_volume_version}
                reported={reported}
                pending={volumePending}
                online={!!peer.online}
                active={active}
                inactiveNote={inactiveNote}
                softwareGain={output && !!device?.out_volume_software_gain}
                volumeTestid={`detail-device-volume-${r.dir}`}
                muteTestid={`detail-device-mute-${r.dir}`}
                label={t(output ? 'volume.deviceOutputLabel' : 'volume.deviceInputLabel', {
                  name: r.name || peer.name || fp,
                })}
                onSet={(wanted, params) => rpc(
                  'peer.set_device_volume',
                  deviceVolumeRequest(fp, wanted, params),
                  { silent: true },
                )}
                onRefresh={() => { void refreshPeers(); }}
              />
              <span className={`dev-state ${r.io ? 'live' : r.published && r.observed ? 'idle' : 'pending'}`}>
                {r.io ? t('device.inUse') : r.published && r.observed ? t('device.idle') : t('device.awaiting')}
              </span>
            </div>
          );
        })}
      </div>
      <p className="muted small" data-testid="detail-hal-note">{note}</p>
    </div>
  );
}

export function PeerTransportCard({ peer }: { peer: PeerState }) {
  const fp = peer.fingerprint;
  const ds = useStore((s) => s.daemonSettings);
  const sessions = useStore((s) => s.sessions);
  const [busy, setBusy] = useState(false);

  const lStops = useMemo(() => latencyStops(ds), [ds]);
  const qStops = useMemo(() => qualityStops(ds), [ds]);

  // 共享模式：本机不发起，故本机存的四个档对任何链路都不生效。
  // 判据取 `effective_mode`（真的在跑的那个），不取 `mode`（用户请求的）——
  // 请求了模式 B 但驱动没起来的机器实际跑在别的模式上。
  const shared = useStore(selectIsShareMode);
  // 虚拟设备那一块的判据。取**请求**的模式，不取生效的——理由在 `PeerDevices` 上。
  const wantsModeB = useStore(requestedMode) === MODE_B;
  const tr = peer.transport || {};
  // 共享模式展示对端在本机上驱动的双向执行器；使用模式则只显示对端
  // 明确拥有的默认端点。`None` 由 `peerAudioDirections` 保持为可见。
  const directions = shared ? ROWS : peerAudioDirections(peer);

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
  const resets = directions.flatMap((dir) => {
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
  // 缺席与空串在这里含义相同（都没有隧道地址），所以不区分两者。
  const endpoint = typeof tr.endpoint === 'string' ? tr.endpoint : '';

  return (
    <section className="card block" data-testid="detail-transport">
      <h3 className="block-title">{t('detail.transport.title')}</h3>
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
      {directions.length ? (
        <div className="transport-grid" data-testid="detail-transport-grid">
          <span className="transport-corner" aria-hidden="true" />
          {/* 「这是目标不是实测」「延迟由接收端执行、音质由发送端执行」两段说明
              都搬进了 wiki（用户 2026-08-10 裁定，docs/plan.md §3.1）。两枚 `?` 挂在
              列头而不是卡片标题上：这张卡有两个**不同**的旋钮，一个入口指不了两处。 */}
          <span className="transport-col title-row">
            {t('detail.transport.colLatency')}
            <Help label={t('wiki.latency')} url={WIKI.latencyTarget} testid="detail-transport-latency-help" />
          </span>
          <span className="transport-col title-row">
            {t('detail.transport.colQuality')}
            <Help label={t('wiki.quality')} url={WIKI.qualityLadder} testid="detail-transport-quality-help" />
          </span>
          {directions.map((dir) => (
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
      ) : null}
      {/* ---- 连通方式（plan §16.2 的「手动覆盖恒可用」）--------------------
          放在四个档位**之后**：那四个是日常旋钮，这一个是「网络不让我直连」
          时才动的。四个互斥选项而不是一个开关——`auto` 与 `tier0` 不是同一件事
          （前者是「你决定」，后者是「钉住直连，别自己改」），做成开关就必须
          把其中一个藏起来。

          ⚠ **四个，含 tier2**（曾经只有三个，见 `lib/tier.ts` 上 `TIER_CHOICES`
          的那段论证）。tier2 不需要隧道地址就能选：不填地址是裸 TCP 上的复用，
          填了是带 WebSocket 外壳的复用，两者都是 §4.3 的「单连接复用」。

          ⚠ 这一组按钮显示的是**用户的选择**，不是链路现在实际跑在哪一档。
          现状由上面的 `TierNow` 那一行负责（§16.4 的一级信息在卡片上，这里是
          它的二级完整版）。**两者不得互相冒充**：选「自动」的对端此刻可能正跑在
          tier 1 上，而这一组按钮仍然、并且应当显示「自动」。 */}
      <div className="transport-tier" data-testid="detail-transport-tier">
        <div className="title-row">
          <h4 className="block-subtitle">{t('detail.transport.tierTitle')}</h4>
          <Help label={t('wiki.transport')} url={WIKI.transport} testid="detail-transport-tier-help" />
        </div>
        {/* 现状在选择之前：用户点进这一节最常见的问题是「我现在到底走的哪条路」，
            而不是「我上次选了什么」。

            ⚠ 这一行是**状态**，不是描述，所以它留下了（§16.4：「已判定为直连」与
            「未判定」不得渲染成同一个样子）。下面那一组按钮是**选择**——把「以下
            是你的选择」那句提示搬进 wiki 之后，两者靠 `TierNow` 自带的「当前连接
            方式」标签区分，那个标签本身就是状态语。 */}
        <TierNow peer={peer} />
        {typeof tr.tier_reset_from === 'string' ? (
          <p className="transport-reset" data-testid="detail-transport-tier-reset">
            {t('detail.transport.tierReset', { old: tr.tier_reset_from })}
          </p>
        ) : null}
        <div className="transport-tier-row" role="radiogroup" aria-label={t('detail.transport.tierTitle')}>
          {TIER_CHOICES.map((id) => (
            <button
              key={id}
              type="button"
              role="radio"
              aria-checked={tier === id}
              className={`transport-tier-opt${tier === id ? ' is-on' : ''}`}
              data-testid={`detail-transport-tier-${id}`}
              disabled={busy}
              onClick={() => void setTier(id)}
            >
              <span className="transport-tier-label">{t(TIER_PICK_LABEL[id])}</span>
              <span className="transport-tier-hint">{t(TIER_PICK_HINT[id])}</span>
            </button>
          ))}
        </div>
        {/* ---- 虚拟设备（用户第 18 条）----------------------------------
            接在四个档位按钮**之后**：先说这条链路怎么连，再说它在系统里长成
            哪些设备。判据 `requestedMode === 'b'` 见 `PeerDevices` 的注释——
            这里判、组件不判，两处都判会分岔。 */}
        {wantsModeB ? <PeerDevices peer={peer} /> : null}
        {/* ---- 隧道地址（用户第 19 条：属于「单连接复用」，按条件显示）------
            判据**不是**照字面的 `tier === 'tier2'`：那会造出一个存得下、看不见、
            删不掉的设置（daemon 选承载的判据是「或」）。三条判据收在
            `endpointVisible()` 里，理由与真值表都在那个函数上。 */}
        {endpointVisible(tier, endpoint, tr.endpoint_reset_from)
          ? <EndpointField fp={fp} tier={tier} endpoint={endpoint} reset={tr.endpoint_reset_from} />
          : null}
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
    </section>
  );
}
