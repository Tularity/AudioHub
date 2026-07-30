// 权限模型：把 daemon 的权限回包收敛成 UI 用的固定形状，并给出「首启授权门」的判据。
//
// 契约（唯一事实来源仍是 core/audiohub-ipc/src/lib.rs；daemon 侧定稿后以那边为准）：
//   "daemon.permissions"        {}     -> { permissions: [PermissionState] }
//   "daemon.request_permission" {id}   -> PermissionState
//   PermissionState {
//     id:           "microphone" | "local_network" | "system_audio" | …,
//     status:       "granted" | "denied" | "undetermined" | "restricted" | "unknown",
//     required?:    bool,     省略时用下面 CATALOG 的默认值
//     queryable?:   bool,     false = 系统根本不提供查询接口（macOS 的本地网络就是）
//     can_request?: bool,     false = 程序无法再触发弹窗，只能去系统设置
//     settings_url?: string,  「打开系统设置」深链
//     name?/why?/note?: string，daemon 给的文案优先于 CATALOG
//   }
//
// 对若干等价写法做兼容（顶层直接是数组、map 形式、布尔状态、驼峰别名），读不懂的一律
// 落 "unknown"。**「不知道」既不算已授权、也不算已拒绝**：前者会让用户以为功能可用，
// 后者会把人挡在门外——两种误判都比如实说「不知道」更糟。

const IS_MAC = /mac/i.test(navigator.platform || '') || /Macintosh/i.test(navigator.userAgent || '');

// 系统设置深链只是兜底：daemon 给了 settings_url 就用它的。锚点取自本机
// SecurityPrivacyExtension.appex（Privacy_Microphone / Privacy_AudioCapture 确实存在；
// 本地网络那一栏没有独立锚点，只能开到「隐私与安全性」根页）。
const PRIVACY_PANE = 'x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension';

// 目录里的每一项都要能在 daemon 一个字段都不给的情况下独立成文——UI 不能因为
// daemon 省了 name/why 就渲染出一行只有 id 的天书。
export const CATALOG = [
  {
    id: 'microphone',
    name: '麦克风',
    icon: 'mic',
    required: true,
    // 与 Info.plist 的 NSMicrophoneUsageDescription 同源同口径，用户在系统弹窗里
    // 看到的那句话必须和这里说的是同一件事。
    why: '把本机麦克风的声音共享给已配对的设备（例如让 Windows 电脑使用这台 Mac 的麦克风）。仅在你主动开启共享时采集。',
    settingsUrl: `${PRIVACY_PANE}?Privacy_Microphone`,
    manual: '系统设置 → 隐私与安全性 → 麦克风 → 打开 AudioHub',
  },
  {
    id: 'local_network',
    name: '本地网络',
    icon: 'scan',
    required: true,
    // macOS 不提供本地网络授权的查询 API：既没有 authorizationStatus，也没有
    // 请求接口。所以这一项默认「不可查询、不可主动请求」，界面必须如实说明，
    // 绝不能拿一个猜出来的状态糊弄用户。
    queryable: false,
    canRequest: false,
    why: '在同一局域网内发现其他 AudioHub 设备，并与已配对的设备直接传输音频。音频不会上传到互联网。',
    settingsUrl: PRIVACY_PANE,
    manual: '系统设置 → 隐私与安全性 → 本地网络 → 打开 AudioHub',
    unknownNote: '系统不提供查询接口，首次使用时会询问。若此前拒绝过，需要到系统设置里重新允许。',
  },
  {
    id: 'system_audio',
    name: '系统音频录制',
    icon: 'wave',
    // 只有 mac-catap 后端要它：没有它，其余功能一概照常。所以它不该挡住任何人进门。
    required: false,
    why: '把这台 Mac 正在播放的声音共享给对方；只有把共享来源选为「系统音频」时才需要。',
    settingsUrl: `${PRIVACY_PANE}?Privacy_AudioCapture`,
    manual: '系统设置 → 隐私与安全性 → 系统音频录制 → 打开 AudioHub',
  },
];

const BY_ID = new Map(CATALOG.map((c) => [c.id, c]));

// daemon/系统各家的写法都收敛到同一套 id 上；认不出来的 id 原样保留（照样渲染，
// 只是用通用文案），绝不丢弃——丢一项等于让用户永远不知道还差这个权限。
// 键都是 key() 归一化后的形式（小写、去掉非字母数字）。
const ID_ALIAS = {
  mic: 'microphone', microphone: 'microphone', audioinput: 'microphone', input: 'microphone',
  localnetwork: 'local_network', bonjour: 'local_network', mdns: 'local_network',
  network: 'local_network',
  systemaudio: 'system_audio', audiocapture: 'system_audio', sysaudio: 'system_audio',
  systemaudiorecording: 'system_audio', catap: 'system_audio', maccatap: 'system_audio',
};

const STATUS_ALIAS = {
  granted: 'granted', authorized: 'granted', allowed: 'granted', enabled: 'granted',
  ok: 'granted', yes: 'granted', on: 'granted',
  denied: 'denied', refused: 'denied', blocked: 'denied', rejected: 'denied', no: 'denied',
  restricted: 'restricted',
  undetermined: 'undetermined', notdetermined: 'undetermined', unset: 'undetermined',
  prompt: 'undetermined', ask: 'undetermined', pending: 'undetermined',
  unknown: 'unknown', unavailable: 'unknown', unsupported: 'unknown', na: 'unknown',
};

export const STATUS_LABEL = {
  granted: '已授权',
  denied: '未授权',
  undetermined: '未确定',
  restricted: '受限',
  unknown: '未知',
};

export const STATUS_TAG = {
  granted: 'tag ok',
  denied: 'tag danger',
  undetermined: 'tag warn',
  restricted: 'tag danger',
  unknown: 'tag',
};

function key(v) {
  return String(v == null ? '' : v).toLowerCase().replace(/[^a-z0-9]/g, '');
}

function normId(v) {
  const k = key(v);
  return ID_ALIAS[k] || (BY_ID.has(String(v)) ? String(v) : String(v || ''));
}

function normStatus(v) {
  if (v === true) return 'granted';
  // 布尔 false 说不清是「拒绝了」还是「还没问过」。当作「未确定」：这样界面给的是
  // 「授权」而不是「去系统设置」，而真被拒绝时 request 会立刻返回 denied，
  // 下一轮复查自然翻成「未授权」。反过来猜成 denied 则会让还没问过的用户白跑一趟设置。
  if (v === false) return 'undetermined';
  return STATUS_ALIAS[key(v)] || 'unknown';
}

function str(v) {
  return typeof v === 'string' && v.trim() ? v.trim() : null;
}

function bool(v) {
  return typeof v === 'boolean' ? v : null;
}

function pickStatus(raw) {
  if (raw == null) return 'unknown';
  if (typeof raw !== 'object') return normStatus(raw);
  for (const k of ['status', 'state', 'authorization', 'value']) {
    if (raw[k] != null) return normStatus(raw[k]);
  }
  if (typeof raw.granted === 'boolean') return normStatus(raw.granted);
  return 'unknown';
}

/** 单条回包 → UI 形状。id 缺席时用 fallbackId（map 形式的键）。 */
export function normalizeOne(raw, fallbackId) {
  const src = raw && typeof raw === 'object' ? raw : {};
  // name 只有在它本身就是一个认得出的 id 时才拿来当 id 用（有的实现把 id 塞在 name 里）。
  const named = src.name && BY_ID.has(normId(src.name)) ? src.name : null;
  const id = normId(str(src.id) || str(named) || fallbackId || '');
  const meta = BY_ID.get(id) || {};
  const status = pickStatus(raw);
  const queryable = bool(src.queryable) ?? meta.queryable ?? true;
  const canRequest = bool(src.can_request) ?? bool(src.canRequest) ?? meta.canRequest ?? true;
  return {
    id,
    name: str(src.name) || meta.name || id || '未知权限',
    icon: meta.icon || 'plug',
    why: str(src.why) || str(src.description) || meta.why
      || '该权限由本机服务上报，界面暂无对应说明。',
    status,
    required: bool(src.required) ?? meta.required ?? false,
    queryable,
    canRequest,
    // 状态可知 = 系统给得出答案。不可知的项永远不参与「挡不挡人」的判断。
    knowable: queryable && status !== 'unknown',
    settingsUrl: str(src.settings_url) || str(src.settingsUrl) || meta.settingsUrl || null,
    manual: IS_MAC ? (meta.manual || null) : null,
    note: str(src.note) || null,
    unknownNote: meta.unknownNote || null,
    inCatalog: BY_ID.has(id),
  };
}

/** 整份回包 → PermissionState[]，按目录顺序排列，目录外的项排在后面。 */
export function normalizeList(raw) {
  let items = [];
  if (Array.isArray(raw)) items = raw.map((x) => normalizeOne(x, null));
  else if (raw && typeof raw === 'object') {
    const arr = raw.permissions || raw.list || raw.items;
    if (Array.isArray(arr)) items = arr.map((x) => normalizeOne(x, null));
    else {
      // map 形式：{ microphone: {...} } 或 { microphone: "granted" }
      items = Object.entries(raw).map(([k, v]) => normalizeOne(v, k));
    }
  }
  const seen = new Map();
  for (const p of items) {
    if (!p.id) continue;
    if (!seen.has(p.id)) seen.set(p.id, p);
  }
  const order = (p) => {
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
export function isBlocking(p) {
  return !!p && p.required && p.knowable && p.status !== 'granted';
}

export function gateNeeded(list) {
  return Array.isArray(list) && list.some(isBlocking);
}

/** 这一行该给什么动作：none（已授权）/ request（能弹窗）/ settings（只能去设置）。 */
export function actionOf(p) {
  if (!p || p.status === 'granted') return 'none';
  if (p.status === 'denied' || p.status === 'restricted') return 'settings';
  return p.canRequest ? 'request' : 'settings';
}

export function actionLabel(p) {
  const a = actionOf(p);
  if (a === 'request') return '授权';
  // 状态不可知时说「检查」而不是「打开」——我们并不知道那里现在是什么样子。
  return p.knowable ? '打开系统设置' : '在系统设置中检查';
}

/** 「全部授权」要走的队列：必需、还没到位、且真能弹窗的那些。 */
export function requestQueue(list) {
  return (list || []).filter((p) => p.required && actionOf(p) === 'request');
}
