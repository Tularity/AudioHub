// 使用端模式（plan §7.1，冻结）——**全局**设置，不是每个对端各自的开关。
//
// 这个模块是整套 UI 里关于「现在是哪种模式」「模式 B 能不能用」的唯一判据来源：
// 三个视图各自判一次的话，迟早会出现主面板按 A 渲染、设置页按 B 渲染的分裂。
//
// 两条冻结语义直接决定了各视图的形状：
//   · 模式 B 下「用哪个对端」由用户在系统声音设置里选设备决定，**UI 不提供对端
//     选择器**——这正是模式 B 存在的理由（以系统最原生的体验为核心目标）；
//   · 只有模式 A 才在 UI 里决定音频送往哪个对端。

import { store } from './store.js';

export const MODE_A = 'a';
export const MODE_B = 'b';

/**
 * 模式 B 的可用性 + 该说什么。
 *
 * 判据是「这台 daemon 有没有 HAL 桥」（`hal != null` / `registered`），**不是**
 * 「此刻有没有驱动连着」。理由不是偏好而是一致性：daemon 的 `effective_mode`
 * 就是这么算的（haldev.rs:604 `wants_mode_b() && inner.hal().is_some()`）。
 * coreaudiod 会重启，桥几秒内自己接回来、绑定全程保留（plan §7.3）；若 UI 改用
 * `driver_connected` 置灰，重启那几秒里 daemon 明明还在模式 B（设备还在系统里、
 * 会话照跑），界面却把 B 变灰——用户看到的是一个「已选中但点不动」的选项，
 * 而且此时切走会真的把设备全删一遍。所以：
 *   · 桥不存在   → 真禁用（daemon 也会把模式钳回 A，界面与它一致）
 *   · 桥在、驱动没连上 → **仍可选**，但要说清楚是哪一种没连上
 */
export function halState(daemon) {
  const hal = daemon && daemon.hal ? daemon.hal : null;
  if (!daemon) {
    return { kind: 'unknown', available: false, tone: 'dim', text: '服务未连接，暂时无法判断驱动是否可用。' };
  }
  if (!hal || !hal.registered) {
    return {
      kind: 'absent',
      available: false,
      tone: 'warn',
      text: '未检测到 AudioHub 驱动，模式 B 不可用；安装驱动后重启本应用即可选择。',
      why: '未检测到 AudioHub 驱动，无法使用模式 B',
    };
  }
  if (hal.status_reason === 'driver_protocol_mismatch') {
    // 唯一一种「重启也没用」的驱动故障：装着的驱动和这一版服务说的不是同一套协议。
    // 表现是系统里一台 AudioHub 设备都没有，而 daemon 一切正常——不点破就是无解。
    const mine = hal.protocol_version;
    const theirs = hal.driver_protocol_version;
    const ver = (mine != null && theirs != null) ? `（服务 v${mine} / 驱动 v${theirs}）` : '';
    return {
      kind: 'mismatch',
      available: true,
      tone: 'danger',
      text: `已安装的 AudioHub 驱动版本与本机服务不匹配${ver}：不会有任何虚拟设备出现在系统里。`
        + '请重新安装与当前版本配套的驱动——重启应用或等待都不会修好它。',
    };
  }
  if (!hal.driver_connected) {
    return {
      kind: 'detached',
      available: true,
      tone: 'warn',
      text: '驱动已注册但桥接尚未连上（通常是 coreaudiod 正在重启）：已发布的虚拟设备保留在系统中，'
        + '此刻不处理声音。请稍候，或重启 AudioHub 服务后重试。',
    };
  }
  return { kind: 'ready', available: true, tone: 'ok', text: '已连接 AudioHub 驱动，模式 B 可用。' };
}

/** 用户请求的模式：daemon 说了算，没拿到回包前用本地缓存兜住首帧。 */
export function requestedMode(s = store.state) {
  const d = s.daemonSettings;
  if (d && (d.consumer_mode === MODE_A || d.consumer_mode === MODE_B)) return d.consumer_mode;
  return s.settings.consumerMode === MODE_B ? MODE_B : MODE_A;
}

/**
 * 真正生效的模式。daemon 的 `effective_mode` 优先——它才知道自己有没有 HAL 桥。
 * 拿不到时（旧服务、还没回包）自己按 halState 回落一次，绝不把存过的 'b' 当成
 * 「现在能用 B」。
 */
export function effectiveMode(s = store.state) {
  const d = s.daemonSettings;
  if (d && (d.effective_mode === MODE_A || d.effective_mode === MODE_B)) return d.effective_mode;
  return requestedMode(s) === MODE_B && halState(s.daemon).available ? MODE_B : MODE_A;
}

export function isModeB(s = store.state) {
  return effectiveMode(s) === MODE_B;
}

/** 用户选了 B、daemon 却只能给 A：必须说出来，否则界面在无声地降级。 */
export function modeDowngraded(s = store.state) {
  return requestedMode(s) === MODE_B && effectiveMode(s) === MODE_A;
}

// ---------------------------------------------------------------- 设备

export const DEVICE_STATE_LABEL = {
  bound: '已发布',
  pending: '等待驱动确认',
  delisted: '正在移除',
  free: '未发布',
};

/** `hal.devices` 里属于某个对端的那一条（诊断字段齐全，PeerState 上的那份没有）。 */
export function halDeviceOf(daemon, fp) {
  const list = daemon && daemon.hal && Array.isArray(daemon.hal.devices) ? daemon.hal.devices : [];
  return list.find((d) => d && d.fingerprint === fp) || null;
}

/**
 * 一台对端的两台设备，按「输出 / 输入」摊平成行，供卡片与设置页共用。
 * `peer.hal_device` 是权威（模式 A 下它就是 null），`hal.devices` 只补诊断字段。
 */
export function peerDeviceRows(peer, daemon) {
  const d = peer && peer.hal_device;
  if (!d) return [];
  const info = halDeviceOf(daemon, peer.fingerprint);
  return [
    {
      dir: 'out',
      icon: 'spk',
      name: d.out_name,
      uid: d.out_uid,
      role: '扬声器',
      io: !!(info && info.io_out),
      frames: info ? info.spk_frames : null,
      dropped: null,
    },
    {
      dir: 'in',
      icon: 'mic',
      name: d.in_name,
      uid: d.in_uid,
      role: '麦克风',
      io: !!(info && info.io_in),
      frames: info ? info.mic_frames : null,
      dropped: info ? info.mic_dropped : null,
    },
  ];
}

/**
 * 某台对端「为什么没有虚拟设备」。hal_reason 是 daemon 给的机器可读原因，
 * 每一种下一步动作都不同，不能合并成一句「暂不可用」。
 */
export function halReasonText(reason) {
  switch (reason) {
    case 'capacity':
      return '虚拟设备已达上限（16 台），该对端暂无对应设备。解除其它配对后可用。';
    case 'no_driver':
      return '本机未安装 AudioHub 驱动，无法为该对端创建虚拟设备。';
    case 'removed_while_offline':
      return '已按「断开后移除虚拟设备」把该对端的设备从系统中移除；对端重新连上后会以相同 UID 恢复。';
    case 'mode_a':
      return '当前是模式 A：虚拟设备只在模式 B 下存在。';
    default:
      return reason ? `暂无虚拟设备（${reason}）。` : '暂无虚拟设备。';
  }
}
