// 配对向导里那些**不该随组件卸载而停**的东西：扫描循环，与配对窗口的到期熄灯。
//
// 为什么它们要搬出组件（B.13 / B.15）——
//
// `App.tsx` 给视图挂了 `key={view}`，切页面必然重新挂载。扫描循环原来活在
// `ConnectOthers` 的 ref 里，卸载的 cleanup 顺手 `setDiscoverRunning(false)`，
// 于是「扫着扫着切去看一眼对端列表」= 扫描停了，回来还得重按一次。
// 配对 PIN 的到期判定原来活在 `BeDiscovered` 的渲染 effect 里，同一个毛病：
// 把它放进可关闭的 Sheet 之后，**关掉 Sheet 就不再熄灯**——store 里留着一个早已
// 过期的 `pairing`，而 daemon 那边的窗口其实已经关了。
//
// 搬出来的代价必须一起认下：循环脱离组件生命周期后，泄漏的形态从「多一个循环」
// 变成「永远跑的循环」。所以下面三件事一件都不能少——**gen 作废机制**、
// **60 s 窗口**、**失败即停**。

import { toast } from '../components/Toasts';
import { t } from '../i18n';
import { sleep } from '../lib/fmt';
import {
  SCAN_GAP_MS, SCAN_SECS, SCAN_WINDOW_MS,
  scanVerdict, shouldAutoScan, shouldClearUserStop, shouldStopAfterFailure,
} from '../lib/discovery';
import { actions, getState, useStore } from './store';
import { rpc } from './connection';
import type { PairingState } from './store';

// ---------------------------------------------------------------- 扫描循环

/**
 * `gen` 是唯一的「谁还有效」判据：反复起停只会作废旧循环，绝不叠加并发循环——
 * 多个 discover.run 会在同一条 IPC 连接上串成队头阻塞，把其它请求全拖到超时。
 *
 * （这段话原样搬自 `views/Pair.tsx` 里那两个 ref 上方的注释。它描述的危险没有
 * 因为换了宿主而消失，只是从组件 ref 变成了模块变量。）
 */
let gen = 0;
/** 当前窗口的截止时刻；0 = 没有正在跑的窗口。 */
let deadlineAt = 0;
let deadlineTimer: ReturnType<typeof setTimeout> | null = null;
/** 连续失败计数，成功一次即清零。 */
let failures = 0;
/** 用户按过「停止扫描」。切页面不清，离开配对页超过一个冷却期才清。 */
let userStopped = false;
/** 上一次停止的时刻（任何原因），冷却期从这里算。 */
let lastStopAt = 0;
/** 上一次离开配对页的时刻。 */
let leftPairViewAt = 0;

export type StopReason = 'user' | 'deadline' | 'offline' | 'error';

function clearDeadlineTimer(): void {
  if (deadlineTimer) { clearTimeout(deadlineTimer); deadlineTimer = null; }
}

/** 窗口剩余时间靠它画。0 = 没在扫。 */
export function scanDeadlineAt(): number {
  return deadlineAt;
}

/** 幂等：已经在跑就什么都不做，不叠第二个循环。 */
export function startScan(): void {
  if (getState().discover.running) return;
  userStopped = false;
  failures = 0;
  deadlineAt = Date.now() + SCAN_WINDOW_MS;
  actions.setDiscoverRunning(true);
  clearDeadlineTimer();
  // 定时器只负责**准时**；正确性由循环里那次 deadline 判断保证——系统休眠会让
  // 定时器漂到窗口之后很久才响，那期间循环必须自己知道该收工了。
  deadlineTimer = setTimeout(() => {
    deadlineTimer = null;
    if (getState().discover.running) stopScan('deadline');
  }, SCAN_WINDOW_MS);
  const mine = ++gen;
  void scanLoop(mine);
}

export function stopScan(reason: StopReason): void {
  gen++;
  deadlineAt = 0;
  clearDeadlineTimer();
  lastStopAt = Date.now();
  if (reason === 'user') userStopped = true;
  actions.setDiscoverRunning(false);
}

/** 按钮的语义：在跑就停（记成用户停的），没跑就起。 */
export function toggleScan(): void {
  if (getState().discover.running) stopScan('user');
  else startScan();
}

/** 过期清理。没有东西要清就不写 store——1/2.4 Hz 地造新数组会白白重渲列表。 */
export function pruneResults(): void {
  actions.pruneDiscover();
}

/**
 * 进入配对页时试着自动起扫（B.11）。判据全在 `lib/discovery.ts` 的纯函数里，
 * 这里只负责把模块状态喂进去。
 */
export function maybeAutoScan(): void {
  const now = Date.now();
  if (userStopped && shouldClearUserStop(leftPairViewAt, now)) userStopped = false;
  pruneResults();
  if (!shouldAutoScan({
    conn: getState().conn,
    running: getState().discover.running,
    userStopped,
    lastStopAt,
    now,
  })) return;
  startScan();
}

/** 配对页卸载时记一笔。**不停扫描**——那正是本轮要治的病。 */
export function notePairViewLeft(): void {
  leftPairViewAt = Date.now();
}

async function scanLoop(mine: number): Promise<void> {
  for (;;) {
    // 停止条件整张表在 lib/discovery.ts 的 `scanVerdict` 里（纯函数，有单测）。
    // 这里只负责问，以及把「窗口到点」这类判断在**每一轮**都问一次——不能只靠
    // startScan 里那个 setTimeout：系统休眠会让定时器漂到窗口之后很久才响。
    const verdict = scanVerdict({
      mine, gen, running: getState().discover.running, conn: getState().conn,
      deadlineAt, now: Date.now(),
    });
    if (verdict === 'superseded') break;
    if (verdict !== 'go') { stopScan(verdict); break; }
    try {
      const res = await rpc('discover.run', { secs: SCAN_SECS }, { silent: true, timeoutMs: 15000 });
      if (mine !== gen) break;
      failures = 0;
      actions.mergeDiscover(res);
    } catch {
      if (mine !== gen) break;
      failures++;
      // 原来这里是 `sleep(1000); continue`——一个 1 Hz 的无限重试，靠用户切走页面
      // 时的组件卸载兜着。现在没人兜了，所以连续失败到上限就收工。
      if (shouldStopAfterFailure(failures)) { stopScan('error'); break; }
      await sleep(1000);
      continue;
    }
    pruneResults();
    await sleep(SCAN_GAP_MS);
  }
}

// ---------------------------------------------------------------- 配对窗口到期

let pinTimer: ReturnType<typeof setTimeout> | null = null;
let watchedPairing: PairingState | null = null;

function schedulePinExpiry(p: PairingState): void {
  // +50ms：定时器早到一点点时不至于立刻再排一轮。
  const delay = Math.max(0, p.expiresAt - Date.now()) + 50;
  pinTimer = setTimeout(() => {
    pinTimer = null;
    const cur = getState().pairing;
    if (!cur) return;
    // 还没到点（定时器早到，或休眠期间被换成了新的一轮）——重排，别误熄灯。
    if (cur.expiresAt - Date.now() > 0) { watchedPairing = cur; schedulePinExpiry(cur); return; }
    actions.setPairing(null);
    toast(t('pair.left.expired'), 'info');
  }, delay);
}

function watchPairing(p: PairingState | null): void {
  if (p === watchedPairing) return;
  watchedPairing = p;
  if (pinTimer) { clearTimeout(pinTimer); pinTimer = null; }
  if (p) schedulePinExpiry(p);
}

// 模块级订阅，随本模块被导入而生效（`views/Pair.tsx` 是静态导入，所以它在应用
// 启动时就装好了）。配对窗口是一个「局域网内任意主机都能来配对」的窗口——它到期
// 与否，不该取决于用户此刻正开着哪一页。
useStore.subscribe((s) => watchPairing(s.pairing));
