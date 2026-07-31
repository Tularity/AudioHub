// daemon IPC 的载荷形状。唯一事实来源仍是 core/audiohub-ipc/src/lib.rs；
// 这里只是它在 TS 侧的镜像。
//
// 刻意把几乎所有字段标成可选：这些对象是**外部输入**（另一个进程、可能是另一个
// 版本的 daemon）。把它们声明成必填等于用类型系统假装运行时保证——旧版本少一个
// 字段就会在某个 `.x.y` 上炸掉，而 TS 一句也不会警告。可选 + 严格空检查会逼着
// 每个读取点自己兜底，那正是我们要的。

export interface HalDeviceInfo {
  fingerprint?: string;
  slot?: number;
  generation?: number;
  state?: 'bound' | 'pending' | 'delisted' | 'free' | string;
  observed?: boolean;
  peer_connected?: boolean;
  out_name?: string;
  out_uid?: string;
  in_name?: string;
  in_uid?: string;
  io_in?: boolean;
  io_out?: boolean;
  mic_frames?: number;
  mic_dropped?: number;
  spk_frames?: number;
}

export interface HalStatus {
  registered?: boolean;
  driver_connected?: boolean;
  status_reason?: string | null;
  protocol_version?: number;
  driver_protocol_version?: number;
  devices?: HalDeviceInfo[];
  mic_frames?: number;
  mic_dropped?: number;
  spk_frames?: number;
  last_driver_msg_secs?: number;
}

export interface VirtualCard {
  id?: string;
  name?: string;
  kind?: string;
  present?: boolean;
}

export interface DaemonInfo {
  ipc_version?: number;
  fingerprint: string;
  name?: string;
  control_port?: number;
  uptime_s?: number;
  hal?: HalStatus | null;
  output_devices?: string[];
  virtual_cards?: VirtualCard[];
}

/** PeerState.hal_device —— 模式 A 下为 null。 */
export interface PeerHalDevice {
  out_name?: string;
  out_uid?: string;
  in_name?: string;
  in_uid?: string;
  state?: string;
  observed?: boolean;
}

export interface PeerState {
  fingerprint: string;
  name?: string;
  alias?: string | null;
  display_name?: string;
  online?: boolean;
  reconnecting?: boolean;
  retry_in_s?: number;
  last_addr?: string;
  port?: number;
  added_unix?: number;
  public_key_b64?: string;
  hal_device?: PeerHalDevice | null;
  hal_reason?: string | null;
}

export interface VolumeState {
  scalar: number;
  muted: boolean;
  adjustable?: boolean;
}

export interface Verdict {
  detected?: boolean;
  snr_db?: number;
}

export interface SessionStats {
  loss_pct?: number;
  jitter_ms?: number;
  bitrate_kbps?: number;
  rung?: number;
  received?: number;
  lost?: number;
  sent_packets?: number;
  jb_depth_frames?: number;
  rung_changes?: number;
  volume?: VolumeState | null;
  verdict?: Verdict | null;
  mix_verdicts?: unknown[];
}

export type SessionKind = 'mic' | 'spk' | string;
export type SessionDir = 'send' | 'recv' | string;

export interface SessionInfo {
  id: number;
  peer_fingerprint: string;
  peer_name?: string;
  kind: SessionKind;
  dir: SessionDir;
  origin?: 'hal' | 'peer' | string | null;
  hal_device?: string | null;
  sample_rate?: number;
  channels?: number;
  stats?: SessionStats | null;
}

/** settings.get / settings.set 的回包（daemon 拥有的全局设置）。 */
export interface DaemonSettings {
  consumer_mode?: 'a' | 'b' | string;
  effective_mode?: 'a' | 'b' | string;
  latency?: string;
  quality?: string;
  remove_virtual_on_disconnect?: boolean;
  mark_offline_devices?: boolean;
  hal_capacity?: number;
  hal_used?: number;
}

export interface DiscoverResult {
  fingerprint?: string;
  instance?: string;
  name?: string;
  port?: number;
  addrs?: string[];
  paired?: boolean;
  lastSeen?: number;
}

export interface IpcEndpoint {
  port: number;
  token: string;
}
