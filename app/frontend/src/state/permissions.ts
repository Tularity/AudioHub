// 权限模型：把 daemon 的权限回包收敛成 UI 用的固定形状，并给出「首启授权门」的判据。
//
// 契约（唯一事实来源仍是 core/audiohub-ipc/src/lib.rs）：
//   "daemon.permissions"        {}     -> { permissions: [PermissionState] }
//   "daemon.request_permission" {id}   -> PermissionState
//
// 对若干等价写法做兼容（顶层直接是数组、map 形式、布尔状态、驼峰别名），读不懂的一律
// 落 "unknown"。**「不知道」既不算已授权、也不算已拒绝**：前者会让用户以为功能可用，
// 后者会把人挡在门外——两种误判都比如实说「不知道」更糟。

import type { IconName } from '../components/Icon';
import { t } from '../i18n';
import type { MsgKey } from '../i18n';

const IS_MAC = /mac/i.test(navigator.platform || '') || /Macintosh/i.test(navigator.userAgent || '');

// 系统设置深链只是兜底：daemon 给了 settings_url 就用它的。锚点取自本机
// SecurityPrivacyExtension.appex（Privacy_Microphone / Privacy_AudioCapture 确实存在；
// 本地网络那一栏没有独立锚点，只能开到「隐私与安全性」根页）。
const PRIVACY_PANE = 'x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension';

export type PermStatus = 'granted' | 'denied' | 'undetermined' | 'restricted' | 'unknown';

export interface PermissionState {
  id: string;
  name: string;
  icon: IconName;
  why: string;
  status: PermStatus;
  required: boolean;
  queryable: boolean;
  canRequest: boolean;
  knowable: boolean;
  settingsUrl: string | null;
  manual: string | null;
  note: string | null;
  unknownNote: string | null;
  inCatalog: boolean;
}

// 目录里存的是**文案键**，不是文案本身：权限名/说明会随产品打磨反复改写，
// 键不该跟着漂。settingsUrl 是系统深链，不是自然语言，照旧写死。
interface CatalogEntry {
  id: string;
  nameKey: MsgKey;
  icon: IconName;
  required: boolean;
  whyKey: MsgKey;
  settingsUrl: string;
  manualKey: MsgKey;
  queryable?: boolean;
  canRequest?: boolean;
  unknownNoteKey?: MsgKey;
}

// 目录里的每一项都要能在 daemon 一个字段都不给的情况下独立成文——UI 不能因为
// daemon 省了 name/why 就渲染出一行只有 id 的天书。
export const CATALOG: CatalogEntry[] = [
  {
    id: 'microphone',
    nameKey: 'perm.microphone.name',
    icon: 'mic',
    required: true,
    // 与 Info.plist 的 NSMicrophoneUsageDescription 同源同口径，用户在系统弹窗里
    // 看到的那句话必须和这里说的是同一件事。
    whyKey: 'perm.microphone.why',
    settingsUrl: `${PRIVACY_PANE}?Privacy_Microphone`,
    manualKey: 'perm.microphone.manual',
  },
  {
    id: 'local_network',
    nameKey: 'perm.localNetwork.name',
    icon: 'scan',
    required: true,
    // macOS 不提供本地网络授权的查询 API：既没有 authorizationStatus，也没有
    // 请求接口。所以这一项默认「不可查询、不可主动请求」，界面必须如实说明，
    // 绝不能拿一个猜出来的状态糊弄用户。
    queryable: false,
    canRequest: false,
    whyKey: 'perm.localNetwork.why',
    settingsUrl: PRIVACY_PANE,
    manualKey: 'perm.localNetwork.manual',
    unknownNoteKey: 'perm.localNetwork.unknownNote',
  },
  {
    id: 'system_audio',
    nameKey: 'perm.systemAudio.name',
    icon: 'wave',
    // 只有 mac-catap 后端要它：没有它，其余功能一概照常。所以它不该挡住任何人进门。
    required: false,
    whyKey: 'perm.systemAudio.why',
    settingsUrl: `${PRIVACY_PANE}?Privacy_AudioCapture`,
    manualKey: 'perm.systemAudio.manual',
  },
];

const BY_ID = new Map(CATALOG.map((c) => [c.id, c]));

// daemon/系统各家的写法都收敛到同一套 id 上；认不出来的 id 原样保留（照样渲染，
// 只是用通用文案），绝不丢弃——丢一项等于让用户永远不知道还差这个权限。
const ID_ALIAS: Record<string, string> = {
  mic: 'microphone', microphone: 'microphone', audioinput: 'microphone', input: 'microphone',
  localnetwork: 'local_network', bonjour: 'local_network', mdns: 'local_network',
  network: 'local_network',
  systemaudio: 'system_audio', audiocapture: 'system_audio', sysaudio: 'system_audio',
  systemaudiorecording: 'system_audio', catap: 'system_audio', maccatap: 'system_audio',
};

const STATUS_ALIAS: Record<string, PermStatus> = {
  granted: 'granted', authorized: 'granted', allowed: 'granted', enabled: 'granted',
  ok: 'granted', yes: 'granted', on: 'granted',
  denied: 'denied', refused: 'denied', blocked: 'denied', rejected: 'denied', no: 'denied',
  restricted: 'restricted',
  undetermined: 'undetermined', notdetermined: 'undetermined', unset: 'undetermined',
  prompt: 'undetermined', ask: 'undetermined', pending: 'undetermined',
  unknown: 'unknown', unavailable: 'unknown', unsupported: 'unknown', na: 'unknown',
};

const STATUS_KEY: Record<PermStatus, MsgKey> = {
  granted: 'perm.status.granted',
  denied: 'perm.status.denied',
  undetermined: 'perm.status.undetermined',
  restricted: 'perm.status.restricted',
  unknown: 'perm.status.unknown',
};

export function statusLabel(status: PermStatus): string {
  return t(STATUS_KEY[status] || 'perm.status.unknown');
}

export const STATUS_TAG: Record<PermStatus, string> = {
  granted: 'tag ok',
  denied: 'tag danger',
  undetermined: 'tag warn',
  restricted: 'tag danger',
  unknown: 'tag',
};

function key(v: unknown): string {
  return String(v == null ? '' : v).toLowerCase().replace(/[^a-z0-9]/g, '');
}

function normId(v: unknown): string {
  const k = key(v);
  return ID_ALIAS[k] || (BY_ID.has(String(v)) ? String(v) : String(v || ''));
}

function normStatus(v: unknown): PermStatus {
  if (v === true) return 'granted';
  // 布尔 false 说不清是「拒绝了」还是「还没问过」。当作「未确定」：这样界面给的是
  // 「授权」而不是「去系统设置」，而真被拒绝时 request 会立刻返回 denied，
  // 下一轮复查自然翻成「未授权」。反过来猜成 denied 则会让还没问过的用户白跑一趟设置。
  if (v === false) return 'undetermined';
  return STATUS_ALIAS[key(v)] || 'unknown';
}

function str(v: unknown): string | null {
  return typeof v === 'string' && v.trim() ? v.trim() : null;
}

function bool(v: unknown): boolean | null {
  return typeof v === 'boolean' ? v : null;
}

function pickStatus(raw: unknown): PermStatus {
  if (raw == null) return 'unknown';
  if (typeof raw !== 'object') return normStatus(raw);
  const r = raw as Record<string, unknown>;
  for (const k of ['status', 'state', 'authorization', 'value']) {
    if (r[k] != null) return normStatus(r[k]);
  }
  if (typeof r.granted === 'boolean') return normStatus(r.granted);
  return 'unknown';
}

/** 单条回包 → UI 形状。id 缺席时用 fallbackId（map 形式的键）。 */
export function normalizeOne(raw: unknown, fallbackId: string | null): PermissionState {
  const src = (raw && typeof raw === 'object' ? raw : {}) as Record<string, unknown>;
  // name 只有在它本身就是一个认得出的 id 时才拿来当 id 用（有的实现把 id 塞在 name 里）。
  const named = src.name && BY_ID.has(normId(src.name)) ? src.name : null;
  const id = normId(str(src.id) || str(named) || fallbackId || '');
  const meta = (BY_ID.get(id) || {}) as Partial<CatalogEntry>;
  const status = pickStatus(raw);
  const queryable = bool(src.queryable) ?? meta.queryable ?? true;
  const canRequest = bool(src.can_request) ?? bool(src.canRequest) ?? meta.canRequest ?? true;
  return {
    id,
    // daemon 给的 name/why/note 原样渲染：那是**服务端产出的中文**，前端无从翻译。
    // 已在报告中列出，待 daemon 侧改用稳定的代码位再本地化。
    name: str(src.name) || (meta.nameKey ? t(meta.nameKey) : null) || id || t('perm.unknown.name'),
    icon: meta.icon || 'plug',
    why: str(src.why) || str(src.description) || (meta.whyKey ? t(meta.whyKey) : null)
      || t('perm.unknown.why'),
    status,
    required: bool(src.required) ?? meta.required ?? false,
    queryable,
    canRequest,
    // 状态可知 = 系统给得出答案。不可知的项永远不参与「挡不挡人」的判断。
    knowable: queryable && status !== 'unknown',
    settingsUrl: str(src.settings_url) || str(src.settingsUrl) || meta.settingsUrl || null,
    manual: IS_MAC && meta.manualKey ? t(meta.manualKey) : null,
    note: str(src.note),
    unknownNote: meta.unknownNoteKey ? t(meta.unknownNoteKey) : null,
    inCatalog: BY_ID.has(id),
  };
}

/** 整份回包 → PermissionState[]，按目录顺序排列，目录外的项排在后面。 */
export function normalizeList(raw: unknown): PermissionState[] {
  let items: PermissionState[] = [];
  if (Array.isArray(raw)) items = raw.map((x) => normalizeOne(x, null));
  else if (raw && typeof raw === 'object') {
    const r = raw as Record<string, unknown>;
    const arr = r.permissions || r.list || r.items;
    if (Array.isArray(arr)) items = arr.map((x) => normalizeOne(x, null));
    else {
      // map 形式：{ microphone: {...} } 或 { microphone: "granted" }
      items = Object.entries(r).map(([k, v]) => normalizeOne(v, k));
    }
  }
  const seen = new Map<string, PermissionState>();
  for (const p of items) {
    if (!p.id) continue;
    if (!seen.has(p.id)) seen.set(p.id, p);
  }
  const order = (p: PermissionState) => {
    const i = CATALOG.findIndex((c) => c.id === p.id);
    return i < 0 ? CATALOG.length : i;
  };
  return [...seen.values()].sort((a, b) => order(a) - order(b));
}

/**
 * 拦路判据：只有「必需 + 状态可知 + 尚未授权」才拦人。
 *
 * 状态不可知（macOS 的本地网络）绝不能拦：那会把每一个用户永久锁在门外，
 * 因为它永远不会变成 granted。可选项同理——mac-catap 之外的功能不该被它绑架。
 */
export function isBlocking(p: PermissionState | null | undefined): boolean {
  return !!p && p.required && p.knowable && p.status !== 'granted';
}

export function gateNeeded(list: PermissionState[]): boolean {
  return Array.isArray(list) && list.some(isBlocking);
}

/** 这一行该给什么动作：none（已授权）/ request（能弹窗）/ settings（只能去设置）。 */
export function actionOf(p: PermissionState | null | undefined): 'none' | 'request' | 'settings' {
  if (!p || p.status === 'granted') return 'none';
  if (p.status === 'denied' || p.status === 'restricted') return 'settings';
  return p.canRequest ? 'request' : 'settings';
}

export function actionLabel(p: PermissionState): string {
  const a = actionOf(p);
  if (a === 'request') return t('perm.action.request');
  // 状态不可知时说「检查」而不是「打开」——我们并不知道那里现在是什么样子。
  return p.knowable ? t('perm.action.openSettings') : t('perm.action.checkSettings');
}

/** 「全部授权」要走的队列：必需、还没到位、且真能弹窗的那些。 */
export function requestQueue(list: PermissionState[]): PermissionState[] {
  return (list || []).filter((p) => p.required && actionOf(p) === 'request');
}
