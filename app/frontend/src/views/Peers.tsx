// 主面板：全局模式栏 + 对端卡片列表 + 手动添加对端。
//
// 模式栏放在这里而不是设置页，是 plan §7.1 的直接后果：模式决定了下面每张卡片的
// 含义（模式 A 在卡片上选对端，模式 B 在系统声音设置里选设备），把它藏进设置页
// 就等于把「这些开关为什么消失了」的答案藏起来。

import { useCallback, useRef, useState } from 'react';
import { Icon } from '../components/Icon';
import { Segmented, Switch } from '../components/Controls';
import { VolumeControl } from '../components/VolumeControl';
import { BridgeControl } from '../components/BridgeControl';
import { ShareSourceControl } from '../components/ShareSourceControl';
import { PeerMetrics } from '../components/PeerMetrics';
import { splitByDirection } from '../lib/metrics';
import { toast } from '../components/Toasts';
import { bridgeTargets } from '../lib/bridge';
import { backendParam, normalizeSource, SOURCE_SYSAUDIO } from '../lib/sysaudio';
import { fmt } from '../lib/fmt';
import { createBusySet, useTick } from '../lib/hooks';
import { t, joinPhrases } from '../i18n';
import type { MsgKey } from '../i18n';
import { actions, getState, useStore } from '../state/store';
import type { AppState } from '../state/store';
import {
  MODE_SHARE, MODE_A, MODE_B, halState, requestedMode, effectiveMode, isModeB,
  isShareMode, modeDowngraded, peerDeviceRows, halReasonText, peerUnusableText,
} from '../state/mode';
import type { AppMode } from '../state/mode';
import { applySettings, refreshPeers, refreshSessions, rpc } from '../state/connection';
import type { PeerState, SessionInfo } from '../ipc/types';

// 进行中的通路操作（`${fp}:mic` / `${fp}:spk`），也是开关 pending 态的唯一判据。
const busy = createBusySet();

// mic / monitor / bridge 操作的是同一条 mic 通路，必须共用一把锁，否则会互相关掉
// 对方刚开的会话。spk / source / backend 同理，共用 spk 那一把。
function busyKey(fp: string, kind: string): string {
  if (kind === 'monitor' || kind === 'bridge') return `${fp}:mic`;
  if (kind === 'source' || kind === 'backend') return `${fp}:spk`;
  return `${fp}:${kind}`;
}

// 本机取用对端麦克风的开启参数。monitor（本机监听）与 bridge（写入虚拟声卡）是
// 同一条通路上的两个去向，daemon 允许同时开，所以两者都从偏好里读当前值。
function micParams(fp: string): Record<string, unknown> {
  const s = getState();
  const p: Record<string, unknown> = { peer: fp, kind: 'mic', monitor: !!s.monitorPref[fp] };
  const b = s.bridgePref[fp];
  // 只在选了卡、且这张卡此刻确实可写时才带 bridge：OpenSessionParams.bridge 是
  // Option<String>，空串或已消失的设备名都会被当成设备名去找，daemon 找不到就直接
  // 开会话失败（规格明确禁止静默回落）。偏好本身保留：卡装回来就照旧生效。
  if (b) {
    if (bridgeTargets(s.daemon).some((c) => c.name === b)) p.bridge = b;
    else toast(t('peers.bridgeUnavailable', { name: b }), 'warn');
  }
  return p;
}

// 本机送对端扬声器的开启参数（plan §7.1 模式 A 的扬声器方向）。
//
// **默认 source 是 'sysaudio'，不是 'mic'**：plan §7.1 定义模式 A 的这条通路就是
// 「捕获本机系统默认音频送对方默认输出播放」，本机与对端同时发声。此前这里写死了
// 'mic'，送过去的是本机麦克风——与产品语义、与设置页自己的文案都对不上。麦克风保留
// 为可选来源，由卡片上的「共享来源」显式选择。
//
// backend 只在选了具体后端时才带：缺席 = daemon 的 'auto'，由它按优先级挑第一个可用的。
// volume_sync 一律带（spec-m4b §A3-4）：daemon 只对开了它的会话接受 session.set_volume，
// 不带就等于卡片上的音量滑块必然报错。
function spkParams(fp: string): Record<string, unknown> {
  const s = getState();
  const source = normalizeSource(s.spkSourcePref[fp]);
  const p: Record<string, unknown> = { peer: fp, kind: 'spk', source, volume_sync: true };
  if (source === SOURCE_SYSAUDIO) {
    const b = backendParam(s.spkBackendPref[fp]);
    if (b) p.backend = b;
  }
  return p;
}

// 重连倒计时：peers.list 10s 一轮，retry_in_s 只在那一刻是准的，中间自己往下走。
// 每当 daemon 报来一个新值就重新对时，绝不在本地凭空续命。
const retryAnchor = new Map<string, { value: number | undefined; at: number }>();

function reconnectLabel(fp: string, reported: number | undefined): string {
  const cur = retryAnchor.get(fp);
  if (!cur || cur.value !== reported) retryAnchor.set(fp, { value: reported, at: Date.now() });
  const a = retryAnchor.get(fp)!;
  if (typeof a.value !== 'number' || !isFinite(a.value)) return t('peers.card.reconnecting');
  const remain = a.value - (Date.now() - a.at) / 1000;
  // 走到 0 说明这一拨已经在飞，而下一次的间隔还没报回来：显示「0s 后重试」会像卡死。
  return remain >= 1 ? t('peers.card.reconnectingIn', { s: Math.ceil(remain) }) : t('peers.card.reconnecting');
}

// 本机发起的会话：mic = 取对方麦克风（媒体 对方→我，dir recv）；spk = 送对方扬声器（dir send）。
// 必须 (kind,dir) 联合过滤：daemon 存的 kind 是发起方视角，对端发起的 mic 会话 dir 是 send
// （= 对方在取用本机麦克风），只按 kind 匹配会把它误当成本机的通路。
function matching(state: AppState, fp: string, kind: string): SessionInfo[] {
  const dir = kind === 'mic' ? 'recv' : 'send';
  return state.sessions.filter((x) => x.peer_fingerprint === fp && x.kind === kind && x.dir === dir);
}

// ---------------------------------------------------------------- 通路操作

async function settle(key: string): Promise<void> {
  busy.delete(key);
  try { await refreshSessions(); } catch { /* ignore */ }
}

// session.open 在 daemon 侧最坏要 30s：期间开关停在可见的 pending 态、busy 键一直
// 握着，结束后一律用 daemon 的会话列表对账，绝不乐观翻转——否则超时报错时会话其实
// 已经建立，开关就跟真实状态脱节了。
async function toggleSession(fp: string, kind: 'mic' | 'spk', want: boolean): Promise<void> {
  const key = busyKey(fp, kind);
  if (busy.has(key)) return;
  busy.add(key);
  try {
    if (want) {
      const params = kind === 'mic' ? micParams(fp) : spkParams(fp);
      actions.upsertSession(await rpc<SessionInfo>('session.open', params));
      if (kind === 'spk') actions.setSpkFault(fp, null);
    } else {
      for (const sess of matching(getState(), fp, kind)) {
        await rpc('session.close', { id: sess.id });
        actions.removeSession(sess.id);
      }
      if (kind === 'spk') actions.setSpkFault(fp, null);
    }
  } catch (e) {
    // rpc 已 toast，但 toast 会消失。系统音频捕获不可用是**必须留在界面上**的那一类
    // 失败：否则用户看到的只是一个自己弹回去的开关，没有任何地方说得出为什么。
    if (kind === 'spk' && want) actions.setSpkFault(fp, faultText(e));
  } finally {
    await settle(key);
  }
}

function faultText(e: unknown): string {
  const msg = String((e as Error)?.message || e || '').trim();
  return msg || t('share.fault.unknown');
}

// 换共享来源 / 换捕获后端 = 换一条 spk 会话。与 reopenMic 同样先开新的、成功后才关
// 旧的：反过来一旦重开失败，对端就直接没声音了，而界面上一条会话都不剩。
// 代价是切换的一瞬间对端可能同时听到两路（daemon 不拒绝重复 spk 流），比断音可接受。
async function reopenSpk(fp: string, rollback: () => void): Promise<void> {
  const key = busyKey(fp, 'spk');
  if (busy.has(key)) return; // spk 通路正忙，偏好也先别动
  const active = matching(getState(), fp, 'spk');
  if (!active.length) {
    actions.setSpkFault(fp, null); // 只记偏好，下次打开时生效；旧的失败原因作废
    return;
  }
  busy.add(key);
  try {
    const info = await rpc<SessionInfo>('session.open', spkParams(fp));
    actions.upsertSession(info);
    actions.setSpkFault(fp, null);
    for (const sess of active) {
      if (sess.id === info.id) continue;
      try {
        await rpc('session.close', { id: sess.id }, { silent: true });
        actions.removeSession(sess.id);
      } catch {
        toast(t('peers.reopenFailed', { id: sess.id }), 'warn');
      }
    }
  } catch (e) {
    actions.setSpkFault(fp, faultText(e));
    rollback(); // 新会话没开起来，偏好回滚：界面上显示的仍是此刻真正在送的那一路
  } finally {
    await settle(key);
  }
}

function setSpkSource(fp: string, want: string): void {
  if (busy.has(busyKey(fp, 'source'))) return;
  const prev = normalizeSource(getState().spkSourcePref[fp]);
  if (want === prev) return;
  actions.setSpkSourcePref(fp, want);
  void reopenSpk(fp, () => actions.setSpkSourcePref(fp, prev));
}

function setSpkBackend(fp: string, want: string): void {
  if (busy.has(busyKey(fp, 'backend'))) return;
  const prev = getState().spkBackendPref[fp] || '';
  if (want === prev) return;
  actions.setSpkBackendPref(fp, want);
  void reopenSpk(fp, () => actions.setSpkBackendPref(fp, prev));
}

// 切换监听 / 换桥接目标 = 换一条 mic 会话。先开新的、成功后才关旧的：反过来
// 一旦重开失败，音频就断了而且界面上一条会话都不剩。
async function reopenMic(fp: string, kind: string, rollback: () => void): Promise<void> {
  const key = busyKey(fp, kind);
  if (busy.has(key)) return; // mic 通路正忙，偏好也先别动
  const active = matching(getState(), fp, 'mic');
  if (!active.length) return; // 只记偏好，下次打开 mic 通路时生效
  busy.add(key);
  try {
    const info = await rpc<SessionInfo>('session.open', micParams(fp));
    actions.upsertSession(info);
    for (const sess of active) {
      if (sess.id === info.id) continue;
      try {
        await rpc('session.close', { id: sess.id }, { silent: true });
        actions.removeSession(sess.id);
      } catch {
        toast(t('peers.reopenFailed', { id: sess.id }), 'warn');
      }
    }
  } catch {
    rollback(); // 新会话没开起来，偏好回滚
  } finally {
    await settle(key);
  }
}

function toggleMonitor(fp: string, want: boolean): void {
  if (busy.has(busyKey(fp, 'monitor'))) return;
  const prev = !!getState().monitorPref[fp];
  actions.setMonitorPref(fp, want);
  void reopenMic(fp, 'monitor', () => actions.setMonitorPref(fp, prev));
}

// 桥接目标：'' = 不桥接。未检测到虚拟声卡时控件本身是禁用的，走不到这里。
function setBridge(fp: string, want: string): void {
  if (busy.has(busyKey(fp, 'bridge'))) return;
  const prev = getState().bridgePref[fp] || '';
  if (want === prev) return;
  actions.setBridgePref(fp, want);
  void reopenMic(fp, 'bridge', () => actions.setBridgePref(fp, prev));
}

// ---------------------------------------------------------------- 模式栏

// 切换后的提示语。三档各一条：模式切换会**真的关掉正在跑的会话**（plan §13
// 推论 2），用户必须从这句话里读到发生了什么，而不是从「对端怎么突然没声了」。
const SWITCHED_KEY: Record<AppMode, MsgKey> = {
  share: 'mode.switched.toShare',
  a: 'mode.switched.toA',
  b: 'mode.switched.toB',
};

const RESULT_KEY: Record<AppMode, MsgKey> = {
  share: 'mode.share.result',
  a: 'mode.a.result',
  b: 'mode.b.result',
};

function ModeBanner() {
  const daemon = useStore((s) => s.daemon);
  const mode = useStore(effectiveMode);
  const downgraded = useStore(modeDowngraded);
  const st = halState(daemon);

  const setMode = useCallback(async (v: AppMode) => {
    if (v === requestedMode(getState())) return;
    try {
      await applySettings({ mode: v });
      toast(t(SWITCHED_KEY[v]), 'ok');
    } catch { /* rpc 已 toast */ }
  }, []);

  return (
    <section className="card block mode-bar" data-testid="consumer-mode">
      <div className="mode-head">
        <div className="mode-title-wrap">
          <h3 className="block-title">{t('mode.title')}</h3>
          <p className="mode-sub">{t('mode.sub')}</p>
        </div>
        {/* testid 沿用旧名 `settings-consumer-mode`（回归已依赖），外层另给别名 */}
        <Segmented<AppMode>
          testid="settings-consumer-mode"
          value={mode}
          onSelect={setMode}
          options={[
            // 共享模式排第一：它是默认值，也是「本机被别人使用」这条唯一的路。
            { value: MODE_SHARE, label: t('mode.share.label') },
            { value: MODE_A, label: t('mode.a.label') },
            {
              value: MODE_B,
              label: t('mode.b.label'),
              // 置灰必须是**真禁用**（点击不改变任何状态），判据见 state/mode.ts halState()。
              disabled: !st.available,
              why: st.why || '',
            },
          ]}
        />
      </div>
      {/* 长文（mode.a.desc / mode.b.desc）下沉到设置页——那里本来就有一份更完整的。
          一级只留一句结果句：读完就知道「现在去哪里选对端」。 */}
      <p className="mode-desc" data-testid="consumer-mode-desc">
        {t(RESULT_KEY[mode])}
        <button
          className="link-btn" type="button" data-testid="consumer-mode-more"
          onClick={() => actions.navigate('settings')}
        >
          {t('mode.learnMore')}
        </button>
      </p>
      {/* 互斥是这次改动的**全部意义**，必须常驻一行：用户在切换前就该知道
          「选了这个，另一件事本机就不做了」。两句互为反面，各自只在对应侧出现。 */}
      <p className="mode-exclusive" data-testid="mode-exclusive">
        {mode === MODE_SHARE ? t('mode.exclusive.share') : t('mode.exclusive.consumer')}
      </p>
      {/* 「驱动就绪」是常态，不是消息：只有出问题时这行才值得占一行。 */}
      <p
        className={`mode-note tone-${st.tone}`}
        data-testid="settings-mode-note"
        hidden={st.tone === 'ok'}
      >
        {st.text}
      </p>
      {/* 用户存的是 B、daemon 只能给 A：这是**降级**，不是「他选了 A」。 */}
      <p className="mode-warn" data-testid="consumer-mode-downgraded" hidden={!downgraded}>
        {t('mode.downgraded')}
      </p>
    </section>
  );
}

// ---------------------------------------------------------------- 对端卡片

// 模式 B 的对端卡片主体：这台对端的两台系统设备 + 它们此刻的状态。
// 这里**没有任何选择器**——选哪台对端 = 在系统里选哪台设备（plan §7.1 冻结）。
function PeerDevices({ peer, hidden }: { peer: PeerState; hidden: boolean }) {
  const daemon = useStore((s) => s.daemon);
  const fp = peer.fingerprint;
  const devs = peerDeviceRows(peer, daemon);
  const has = devs.length > 0;
  const dev = peer.hal_device;
  const published = !!dev && dev.state === 'bound' && !!dev.observed;

  let note = '';
  let noteWarn = false;
  if (!has) {
    note = halReasonText(peer.hal_reason);
    noteWarn = true;
  } else if (!peer.online) {
    // 对端离线时设备仍在系统里可选，只是不处理声音——这是 plan §7.3 的既定语义，
    // 必须在卡片上说出来，否则「没声音」在系统里完全不可观测。
    note = t('peers.devices.offline');
    noteWarn = true;
  } else if (!published) {
    note = t('peers.devices.settling');
  }

  return (
    <div
      className="peer-devices"
      data-testid={`peer-devices-${fp}`}
      hidden={hidden}
      // 卡片整体可点（进入详情）；设备区里的文本要能选中复制 UID。
      onClick={(e) => e.stopPropagation()}
    >
      {/* 「系统设备」标题行已删（规格 §2.3）：下面两行各带 🔊/🎤 图标 + 设备全名，
          已经自明，标题只是又一个不承载信息的方框。 */}
      <div className="dev-list" hidden={!has}>
        {(['out', 'in'] as const).map((dir) => {
          const d = devs.find((x) => x.dir === dir);
          // 三层状态，含义各不相同：正在被应用使用 / 已发布但没人用 / 还没真正出现在系统里。
          const io = !!d && d.io;
          const state = io ? 'live' : published ? 'idle' : 'pending';
          const text = io ? t('device.inUse') : published ? t('device.idle') : t('device.awaiting');
          return (
            <div
              key={dir}
              className={`dev-row${io ? ' active' : ''}`}
              data-testid={`peer-device-${dir}-${fp}`}
            >
              <Icon name={dir === 'out' ? 'spk' : 'mic'} cls="ico dev-ico" />
              {/* 设备 UID（`AudioHub:<fp>:out`）整行下沉到详情页 DevicesCard：
                  用户点名它是「不必要的编号」，而它在一级界面既不可执行也无人核对。 */}
              <div className="dev-text">
                <span className="dev-name" title={d?.name || ''}>{d?.name || t('common.dash')}</span>
              </div>
              <span className={`dev-state ${state}`}>{text}</span>
            </div>
          );
        })}
      </div>
      {/* 长脚注（peers.devices.footOnce）已移到卡片列表底部渲染一次：它对每张卡片
          说的是同一句话，印 N 遍不会更有用。 */}
      <p className={`dev-note${noteWarn ? ' warn' : ''}`} data-testid={`peer-devices-note-${fp}`} hidden={!note}>
        {note}
      </p>
    </div>
  );
}

function PeerCard({ peer, modeB, share }: { peer: PeerState; modeB: boolean; share: boolean }) {
  const fp = peer.fingerprint;
  const daemon = useStore((s) => s.daemon);
  const sessions = useStore((s) => s.sessions);
  const monitorPref = useStore((s) => !!s.monitorPref[fp]);
  const bridgePref = useStore((s) => s.bridgePref[fp] || '');
  const spkSource = useStore((s) => normalizeSource(s.spkSourcePref[fp]));
  const spkBackend = useStore((s) => s.spkBackendPref[fp] || '');
  const spkFault = useStore((s) => s.spkFault[fp] || '');
  // 系统音频录制授权。取不到（还没探测 / daemon 不报这一项）就传 null，控件不提示——
  // 「不知道」不能渲染成「你还没授权」。
  const sysPerm = useStore((s) => s.permissions.list.find((p) => p.id === 'system_audio') || null);
  busy.use(); // 订阅进行中的操作，pending 态跟着翻

  const reconnecting = !peer.online && !!peer.reconnecting;
  useTick(1000, reconnecting);

  // 两个开关**必须**按 `(kind, dir)` 取：它们各自开的是一条具体的通路
  //（「取对方麦克风」= mic/recv，「送对方扬声器」= spk/send），这是使用端语义。
  const micS = sessions.find((x) => x.peer_fingerprint === fp && x.kind === 'mic' && x.dir === 'recv') || null;
  const spkS = sessions.find((x) => x.peer_fingerprint === fp && x.kind === 'spk' && x.dir === 'send') || null;
  // 指标区按 **`dir`（本机视角）** 分栏，不按 `kind`（开流方视角）。
  //
  // 四种 `(kind, dir)` 组合全部落进这两个数组，包括共享模式的 `mic/send` 与
  // `spk/recv`——此前那两种一个都匹配不上，共享模式下指标区恒显示「未建立通路」，
  // 而隔壁隐私横幅同时亮着，同一张卡上下自相矛盾。
  //
  // 用 `filter` 而不是 `find`：同方向可以有多条会话（`MAX_STREAMS_PER_CONN = 16`，
  // 去重只按 `stream_id`）。`find` 只是把「两个方向里选一个」这个 bug 降级成
  // 「同方向 N 条里静默选第一条」，同一个形状换了个轴。
  // 规则本身住在 `lib/metrics.ts`（纯函数，可被回归断言）：接线层零覆盖正是
  // 这次事故能活下来的机制——有测试的那层没坏，坏的那层没测试。
  const { send: sendList, recv: recvList } = splitByDirection(sessions, fp);
  // 对端发起、正在取用本机麦克风的会话（kind=mic + dir=send）。隐私相关，必须显式可见。
  const inbound = sessions.filter((x) => x.peer_fingerprint === fp && x.kind === 'mic' && x.dir === 'send');

  // 「接收」这条通路已就绪、只是还没有应用在用它。三个条件缺一不可，每一条都对应
  // 一种**不能**这么说的情形：
  //   - 对端在线      —— 离线时虚拟设备仍在系统里可选，但没有任何声音会被处理；
  //   - observed 为真 —— 设备真的出现在系统设备列表里了（daemon 从驱动侧确认过），
  //     `state === 'bound'` 而未被观测到时只是「已下发」，还不能承诺可用；
  //   - 没有 mic 方向的会话 —— 有会话时这一行显示的是实时码率，轮不到状态词。
  // 任一条不满足就退回原样（「空闲」/ 空白）：宣称一个兜不住的就绪状态，比不说更糟。
  const micReady = recvList.length === 0 && !!peer.online && peer.hal_device?.observed === true;

  const micBusy = busy.has(busyKey(fp, 'mic'));
  // 重连中不是「离线」：给和「连接中」同一种呼吸点，别让用户以为已经放弃了。
  const dotCls = peer.online ? 'online' : reconnecting ? 'connecting' : 'offline';
  if (!reconnecting) retryAnchor.delete(fp);

  const displayName = peer.display_name || peer.name || t('peers.card.unnamed');
  const nav = () => actions.navigate('detail', fp);

  // plan §13 推论 1：对端处于使用端模式时本机用不了它。空串 = 不知道（离线 /
  // 还没收到通告），此时**什么都不说**——把「不知道」画成「不可用」会让一台刚
  // 连上的正常主机看起来是坏的。
  //
  // 只在本机是使用端时才显示：共享模式下本机根本不打算用它，这行提示对当下的
  // 操作没有任何意义，只是噪声。
  const unusable = share ? '' : peerUnusableText(peer);

  return (
    <article
      className="card peer-card"
      data-testid={`peer-card-${fp}`}
      tabIndex={0}
      role="button"
      aria-label={t('peers.card.viewDetail', { name: peer.name || fp })}
      onClick={nav}
      onKeyDown={(e) => { if (e.key === 'Enter' && e.target === e.currentTarget) nav(); }}
    >
      {/* 头部只回答「这台主机是谁、在不在」。指纹**保留**（plan §7.6 裁定 2：它在配对
          校验场景有安全意义），但做克制呈现——弱字重、弱色阶，不与主机名争注意力。
          `⟩` 是新增的可点提示：整张卡此前已经 role="button"，却对视觉用户毫无线索。
          别名徽章已删（规格 §2.3 ①）：改名后原主机名走 `.peer-name` 的 title，详情页
          还有一张 AliasCard。留着它等于同一条信息印两遍，而它是 flex:none——一旦对端
          设了别名，被挤掉的恰恰是主机名本身。 */}
      <header className="peer-head">
        <span className={`dot ${dotCls}`} />
        <h3 className="peer-name" title={peer.name || fp}>{displayName}</h3>
        <span className="peer-unusable-badge" data-testid={`peer-unusable-badge-${fp}`} hidden={!unusable}>
          {t('peers.unusable.badge')}
        </span>
        <code className="peer-fp" data-testid={`peer-fp-${fp}`} title={fp}>{fmt.fp(fp, 16)}</code>
        <Icon name="chev" cls="ico peer-chev" />
      </header>

      {/* 徽章只说「不可用」，这一行说「为什么、去哪里改」——两者缺一不可：光有
          徽章会让用户在本机翻遍设置也找不到开关，因为要改的是**对面那台**。 */}
      <p className="peer-unusable" data-testid={`peer-unusable-${fp}`} hidden={!unusable} role="status">
        {unusable}
      </p>

      {/* ②③ 一级指标：这一屏唯一新增的一级信息（规格 §2.1）。
          `peer` 是无会话时的兜底数据源：延迟按流统计，没有会话就整块没有，而控制面
          的网络单程（`PeerState.net_ms`）配对连上就有——它是「连着但闲着」这一态下
          唯一测得到的一段。 */}
      <PeerMetrics fp={fp} peer={peer} sendList={sendList} recvList={recvList} micReady={micReady} />

      {/* ④ 隐私条紧贴指标区：原位夹在重连提示与开关之间，视觉权重低到会被略过，
          而「对方正在取用本机麦克风」是这张卡上唯一不该被略过的一行。 */}
      <div className="peer-inbound" data-testid={`inbound-mic-${fp}`} hidden={inbound.length === 0} role="status">
        <span className="dot live" />
        <Icon name="mic" />
        <span className="inbound-text">
          {inbound.length > 1
            ? t('peers.card.inboundMicN', { n: inbound.length })
            : t('peers.card.inboundMic')}
        </span>
      </div>

      {/* 控制通道断了但没被解除配对：daemon 正在按退避重拨，界面必须说出来，
          否则「离线」看着就像放弃了。 */}
      <div className="peer-reconnect" data-testid={`reconnecting-${fp}`} hidden={!reconnecting} role="status">
        <span className="spinner tiny" />
        <span className="reconnect-text">{reconnecting ? reconnectLabel(fp, peer.retry_in_s) : ''}</span>
      </div>

      {/* 模式 A 专属的一整排通路控件。模式 B 下它们全部**下线**（不是变灰）：
          取谁的麦克风、送谁的扬声器由 App 在系统里选设备决定，这些开关既不反映那个
          选择、也无法表达它；留着只会让用户以为自己在这里做了什么。
          monitorPref / bridgePref 在模式 B 下**不读不写**（数据保留，切回 A 复用）。

          共享模式下同样全部下线，理由更硬（plan §13）：本机在这个模式里**根本不
          使用**别的主机，daemon 会直接拒掉 `session.open`。留着它们等于摆一排必然
          报错的开关。 */}
      <div className="peer-toggles" data-testid={`peer-toggles-${fp}`} hidden={modeB || share}>
        <div className="toggle-row">
          <Icon name="mic" /><span className="toggle-label">{t('peers.card.takeMic')}</span>
          <Switch
            testid={`toggle-mic-${fp}`} label={t('peers.card.takeMic')} checked={!!micS} pending={micBusy}
            onToggle={(w) => void toggleSession(fp, 'mic', w)}
          />
        </div>
        <div className="toggle-row">
          <Icon name="spk" /><span className="toggle-label">{t('peers.card.sendSpk')}</span>
          <Switch
            testid={`toggle-spk-${fp}`} label={t('peers.card.sendSpk')} checked={!!spkS}
            pending={busy.has(busyKey(fp, 'spk'))}
            onToggle={(w) => void toggleSession(fp, 'spk', w)}
          />
        </div>
        {/* 「送对方扬声器」送的是什么（plan §7.1：默认本机系统音频）+ 用哪个捕获后端
            （plan §6）。这是模式 A 的核心特性，此前整个界面上没有任何入口。 */}
        <ShareSourceControl
          testid={`share-source-${fp}`}
          daemon={daemon}
          source={spkSource}
          backend={spkBackend}
          pending={busy.has(busyKey(fp, 'spk'))}
          perm={sysPerm}
          fault={spkFault}
          onSource={(v) => setSpkSource(fp, v)}
          onBackend={(v) => setSpkBackend(fp, v)}
          onGrant={() => actions.navigate('settings')}
        />
        {/* 「送对方扬声器」下方的音量同步控件：会话激活后出现，值来自 stats.volume。 */}
        <VolumeControl
          volumeTestid={`volume-${fp}`}
          muteTestid={`mute-${fp}`}
          label={t('peers.card.volumeLabel', { name: peer.name || fp })}
          sess={modeB ? null : spkS}
          // silent：拖动会连发，失败提示由控件自己在框内给一条，不刷 toast。
          onSet={(id, params) => rpc('session.set_volume', { id, ...params }, { silent: true })}
        />
        <div className="toggle-row">
          <Icon name="monitor" /><span className="toggle-label">{t('peers.card.monitor')}</span>
          <Switch
            testid={`toggle-monitor-${fp}`} label={t('peers.card.monitor')} checked={monitorPref} pending={micBusy}
            onToggle={(w) => toggleMonitor(fp, w)}
          />
        </div>
        {/* 「取对方麦克风」的第二个去向（plan §7.1）：写入第三方虚拟声卡的播放端。 */}
        <BridgeControl
          testid={`bridge-${fp}`}
          daemon={daemon}
          value={bridgePref}
          pending={micBusy}
          onChange={(v) => setBridge(fp, v)}
        />
      </div>

      <PeerDevices peer={peer} hidden={!modeB} />

      {/* ⑤ 电平条与码率**已并入指标区的方向块**（`PeerMetrics` 的 `DirBlock`）。
          此前它们独立在卡底，按 `kind` 分成「接收 / 发送」两列——而上面的指标区
          完全没有方向概念。同一张卡上两套坐标系，其中一套还是对的，正好把指标区
          那个「170 是哪条通路」的问题衬托得更隐蔽：底下写着两行，上面只有一个数。
          并进去之后码率与它自己那条通路的延迟 / 音质在同一块里，方向只讲一次。
          「接收就绪、暂无应用在录音」那一态由 `DirBlock` 的 `ready` 承担，
          `mic-idle-<fp>` 这个 testid 原样保留。 */}
    </article>
  );
}

// ---------------------------------------------------------------- 手动添加

function AddPeerForm({ open, onClose }: { open: boolean; onClose: () => void }) {
  const peers = useStore((s) => s.peers);
  const peerRef = useRef<HTMLInputElement>(null);
  const [addr, setAddr] = useState('');
  const [peerVal, setPeerVal] = useState('');
  const [pending, setPending] = useState(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    const peer = peerVal.trim();
    const a = addr.trim();
    if (!peer) {
      toast(t('peers.form.needFingerprint'), 'warn');
      peerRef.current?.focus();
      return;
    }
    setPending(true);
    try {
      // 超时由 ipc/client.ts 的方法级表给出（daemon 最坏 TCP 5s + 握手 10s）。
      await rpc('peers.connect', a ? { peer, addr: a } : { peer });
      toast(t('peers.form.done'), 'ok');
      setAddr('');
      onClose();
      void refreshPeers();
    } catch { /* rpc 已 toast */ } finally {
      setPending(false);
    }
  };

  return (
    <form className="card add-peer-form" hidden={!open} data-testid="add-peer-form" onSubmit={submit}>
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
            placeholder={t('peers.form.addrPlaceholder')}
            autoComplete="off"
            spellCheck="false"
            value={addr}
            onChange={(e) => setAddr(e.currentTarget.value)}
          />
        </label>
        <button className="btn primary" type="submit" data-testid="add-peer-connect" disabled={pending}>
          {pending ? t('common.connecting') : t('common.connect')}
        </button>
      </div>
      <p className="form-note">{t('peers.form.note')}</p>
    </form>
  );
}

// ---------------------------------------------------------------- 视图

export function PeersView() {
  const peers = useStore((s) => s.peers);
  const modeB = useStore(isModeB);
  const share = useStore(isShareMode);
  const [formOpen, setFormOpen] = useState(false);

  const retrying = peers.filter((p) => !p.online && p.reconnecting).length;
  const offline = peers.filter((p) => !p.online && !p.reconnecting).length;
  // 汇总条**只在异常时出现**（规格 §2.4）：「已配对 3 台 · 在线 3 台」这类正常态陈述
  // 每张卡片上都自带一颗状态点，重复一遍只是占掉一行。摘要仍是并列短语，不是句子。
  const summary = joinPhrases([
    offline ? t('peers.summary.offline', { n: offline }) : null,
    retrying ? t('peers.summary.retrying', { n: retrying }) : null,
  ]);
  const hasDevices = modeB && peers.some((p) => p.hal_device);

  return (
    <>
      <ModeBanner />
      <div className="toolbar">
        <div className="toolbar-note warn" data-testid="peers-summary" hidden={!summary}>{summary}</div>
        <button
          className="btn primary" type="button" data-testid="add-peer-btn"
          onClick={() => setFormOpen((v) => !v)}
        >
          <Icon name="plus" />{t('peers.addManual')}
        </button>
      </div>
      <AddPeerForm open={formOpen} onClose={() => setFormOpen(false)} />

      <div className={`peer-grid${modeB ? ' mode-b' : ''}`}>
        {peers.map((p) => (
          <PeerCard key={p.fingerprint} peer={p} modeB={modeB} share={share} />
        ))}
      </div>

      {/* 从每张卡片上收拢来的两条常驻脚注：一句怎么用虚拟设备（只在模式 B 有意义），
          一句延迟数字的口径。它们对整份列表说的是同一件事，所以只说一次。 */}
      <div className="peer-list-foot" data-testid="peers-foot" hidden={peers.length === 0}>
        <p className="foot-line" data-testid="peers-devices-foot" hidden={!hasDevices}>
          {t('peers.devices.footOnce')}
        </p>
        <p className="foot-line" data-testid="peers-latency-foot">{t('metric.latency.footnote')}</p>
      </div>

      {/* 首次启动的空态：不能是一片空白，必须把「两台设备先配对」这件事说清楚。 */}
      <div className="empty card" data-testid="peers-empty" hidden={peers.length > 0}>
        <Icon name="pair" cls="empty-ico" />
        <h3>{t('peers.empty.title')}</h3>
        <p>{t('peers.empty.desc')}</p>
        <ol className="empty-steps">
          <li>{t('peers.empty.step1')}</li>
          <li>{t('peers.empty.step2')}</li>
          <li>{t('peers.empty.step3')}</li>
        </ol>
        <p className="empty-mode-b" data-testid="peers-empty-mode-b" hidden={!modeB}>
          {t('peers.empty.modeB')}
        </p>
        <button
          className="btn primary" type="button" data-testid="peers-empty-pair"
          onClick={() => actions.navigate('pair')}
        >
          <Icon name="pair" />{t('peers.empty.openPair')}
        </button>
      </div>
    </>
  );
}
