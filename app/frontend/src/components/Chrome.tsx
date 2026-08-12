// 应用外壳：浮动导航胶囊、daemon 徽标、断线覆盖层。
//
// 左侧竖导航已经拆掉（docs/spec-ui.md §2）。导航现在是**浮在内容之上**的一枚居中
// 玻璃胶囊：内容从它下面穿过去滚动，胶囊本身不占布局宽度。testid 全部原样保留，
// `nav-*` 四个只是换了宿主元素。

import { useEffect, useState } from 'react';
import { RawIcon } from './Icon';
import { useStore } from '../state/store';
import type { AppState, ViewName } from '../state/store';
import { isShareMode } from '../state/mode';
import { connectDaemon, IPC_VERSION } from '../state/connection';
import { t } from '../i18n';
import type { MsgKey } from '../i18n';

type NavEntry = { view: ViewName; labelKey: MsgKey; icon: 'peers' | 'cable' | 'stats' | 'settings' };

// 「配对向导」那一格已删（用户 2026-08-11 第 1 条）：配对搬进主面板上的二级菜单。
//
// 「共享协议」**只在共享模式下出现**（用户同一条指令的第 3 段）。它是这排格子里
// 唯一一个条件项，代价是格数会在 3/4 之间变——而 `--nav-count` / `--nav-index`
// 本来就是按当前这张表算的，指示块照样滑得对。
const NAV_BASE: NavEntry[] = [
  { view: 'peers', labelKey: 'nav.peers', icon: 'peers' },
  { view: 'stats', labelKey: 'nav.stats', icon: 'stats' },
  { view: 'settings', labelKey: 'nav.settings', icon: 'settings' },
];
const NAV_SHARE: NavEntry = { view: 'share', labelKey: 'nav.share', icon: 'cable' };

export function navEntries(share: boolean): NavEntry[] {
  if (!share) return NAV_BASE;
  return [NAV_BASE[0]!, NAV_SHARE, ...NAV_BASE.slice(1)];
}

// 详情页高亮主面板
const NAV_OF: Partial<Record<ViewName, ViewName>> = { detail: 'peers' };

export const VIEW_TITLE: Record<ViewName, MsgKey> = {
  peers: 'nav.peers',
  detail: 'nav.detail',
  share: 'nav.share',
  settings: 'nav.settings',
  stats: 'nav.stats',
};

/**
 * 内容滚下去之后胶囊收拢一档。**动画被关掉时直接不收拢**：收拢本身就是一个动作，
 * 全局的 prefers-reduced-motion 规则会把过渡掐掉，留下的就只剩一次生硬的跳变
 * ——那比不收拢更糟。所以这里在 JS 里就判掉，而不是指望 CSS 去兜。
 */
function useContracted(): boolean {
  const [on, setOn] = useState(false);
  useEffect(() => {
    if (window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;
    const root = document.getElementById('view-root');
    if (!root) return;
    const onScroll = () => setOn(root.scrollTop > 12);
    onScroll();
    root.addEventListener('scroll', onScroll, { passive: true });
    return () => root.removeEventListener('scroll', onScroll);
  }, []);
  return on;
}

/**
 * 背景水印。左上角那枚品牌区（logo + 字标 + tagline）已经删掉，标识改由这一枚
 * **超大、极低对比**的波形承担——它要读作背景纹理，而不是「一枚被放大的 logo」。
 *
 * 下面几条是硬约束，不是审美偏好：
 *
 * · **`position: fixed`，绝不跟着 `#view-root` 滚。** 跟着内容滚的水印在每一次滚动
 *   里都是一层会动的噪声，也是这种做法最先被读出「廉价」的地方。它不动，卡片从它
 *   上面盖过去，于是滚动时只有遮挡关系在变，没有第二个运动的东西。
 *
 * · **`z-index: -1`，退到负层。** 于是它画在画布底色之上、所有内容之下。这条成立的
 *   前提是 `#app` 不建立层叠上下文（它只有 display:flex，没有 z-index/transform/
 *   filter/opacity），负层才退得到画布那一级；`body` 的 background 被传播到画布，
 *   所以负层仍在底色之上，不会整个消失。选它而不是给 `#view-root` 加
 *   `position: relative` + `z-index`：后者会把视图内部**每一个**绝对定位子元素的
 *   包含块从视口换成滚动容器，是一次波及全站的改动，代价完全不成比例。
 *
 * · **右下出血，不放左上。** 左上正是刚删掉的品牌区，把标识画回那个位置等于没删。
 *
 * · **一帧动画都不给。** 会呼吸、会漂移的水印就是用户点名要避免的视觉噪声。
 *
 * · **矢量。** 复用 Icon.tsx 里那条同一份 path（不新增第四份拷贝，也不缩放
 *   icons/icon.png），高 DPI 下自然清晰。
 *
 * `aria-hidden`：它不承载任何信息——App 的名字与版本在设置页「关于」里，
 * 读屏用户要的是那个，不是这里一段读不出来的装饰图形。
 */
export function Watermark() {
  return (
    <div id="watermark" data-testid="app-watermark" aria-hidden="true">
      <RawIcon name="wave" />
    </div>
  );
}

/**
 * 居中浮动的导航胶囊。等宽栅格不是偷懒：活动指示块靠 `translateX(index * 100%)`
 * 滑动，等宽才让这条位移算式成立，也才有 macOS 26 那种「指示块在胶囊里滑过去」
 * 而不是「高亮硬切」的读感。
 */
export function NavPill({ onNavigate }: { onNavigate: (v: ViewName) => void }) {
  const view = useStore((s) => s.route.view);
  const share = useStore(isShareMode);
  const nav = navEntries(share);
  const active = NAV_OF[view] || view;
  const index = Math.max(0, nav.findIndex((n) => n.view === active));
  const contracted = useContracted();

  return (
    <nav
      id="nav"
      className={contracted ? 'contracted' : undefined}
      style={{ '--nav-count': nav.length, '--nav-index': index } as React.CSSProperties}
    >
      <span className="nav-marker" aria-hidden="true" />
      {nav.map((n) => (
        <button
          key={n.view}
          className={`nav-item${active === n.view ? ' active' : ''}`}
          type="button"
          data-view={n.view}
          data-testid={`nav-${n.view}`}
          aria-current={active === n.view ? 'page' : undefined}
          onClick={() => onNavigate(n.view)}
        >
          <RawIcon name={n.icon} />
          {t(n.labelKey)}
        </button>
      ))}
    </nav>
  );
}

// 左下角那条固定注脚（#conn-hint / ConnFoot）已删（规格 §2.4、§5）：它说的四种连接
// 状态与右上徽标逐一重合，IPC 端口在设置页「网络 › IPC 端口」，「你正在用网页端查看」
// 也已经由设置页的 settings.web.browserOnly 常驻说明。留着它的代价是整页下内边距要
// 多留 24px 去避让一枚重复信息的浮层——那恰好压住主面板新加的两条常驻脚注。

// DaemonBadge 已搬到 components/TopControls.tsx 并改了形态：文字标签去掉、只剩圆点，
// 指纹/端口/主机名从「原地展开」改成一枚悬停浮层，并与新增的语言、外观两枚图标按钮
// 组成同一簇（`ChromeControls`）。testid `daemon-badge` / `daemon-badge-ident` 原样保留。

// 每一种失败原因都要给出**不同的**下一步动作；kind 与 src-tauri/src/main.rs
// 的 DaemonError::kind 一一对应，那边加一种这里就要加一条。
const FAILURE_COPY: Record<string, { title: MsgKey; desc: MsgKey; hint?: MsgKey }> = {
  'no-binary': { title: 'overlay.noBinary.title', desc: 'overlay.noBinary.desc', hint: 'overlay.noBinary.hint' },
  'spawn-failed': { title: 'overlay.spawnFailed.title', desc: 'overlay.spawnFailed.desc' },
  'port-busy': { title: 'overlay.portBusy.title', desc: 'overlay.portBusy.desc' },
  timeout: { title: 'overlay.timeout.title', desc: 'overlay.timeout.desc' },
  'start-failed': { title: 'overlay.startFailed.title', desc: 'overlay.startFailed.desc' },
  internal: { title: 'overlay.internal.title', desc: 'overlay.internal.desc' },
};

function overlayCopy(s: AppState): { title: string; desc: string; hint: string } {
  if (s.conn === 'starting') {
    return { title: t('overlay.starting.title'), desc: t('overlay.starting.desc'), hint: '' };
  }
  if (s.conn === 'connecting') {
    return {
      title: t('overlay.connecting.title'),
      desc: s.endpoint
        ? t('overlay.connecting.desc', { port: s.endpoint.port })
        : t('overlay.connecting.descNoPort'),
      hint: '',
    };
  }
  const err = s.connError || { kind: 'other', message: '', detail: null };
  if (err.kind === 'version') {
    // 「服务没起来」与「服务版本不兼容」是两回事：后者重启界面也没用，
    // 必须换一个版本匹配的 daemon。
    return {
      title: t('overlay.version.title'),
      desc: t('overlay.version.desc', { message: err.message, version: IPC_VERSION }),
      hint: t('overlay.version.hint'),
    };
  }
  if (err.kind === 'no-endpoint') {
    return {
      title: t('overlay.noEndpoint.title'),
      desc: t('overlay.noEndpoint.desc'),
      hint: t('overlay.noEndpoint.hint'),
    };
  }
  const copy = FAILURE_COPY[err.kind];
  if (copy) {
    return {
      title: t(copy.title),
      desc: t(copy.desc) + (err.detail ? `\n\n${t('overlay.detail', { detail: String(err.detail).trim() })}` : ''),
      hint: copy.hint ? t(copy.hint) : '',
    };
  }
  return {
    title: t('overlay.disconnected.title'),
    desc: s.mode === 'tauri'
      ? t('overlay.disconnected.descTauri', { reason: err.message || t('overlay.disconnected.reasonUnknown') })
      : t('overlay.disconnected.descBrowser'),
    hint: '',
  };
}

export function Overlay() {
  const s = useStore();
  const online = s.conn === 'online';
  // 启动/连接是**进行态**，不是错误：给动画与进度语，不给错误图标和按钮。
  const busy = s.conn === 'starting' || s.conn === 'connecting';
  const copy = overlayCopy(s);

  return (
    <div id="overlay" data-testid="daemon-overlay" hidden={online}>
      <div className="overlay-card">
        <span className="overlay-ico" id="overlay-ico" hidden={busy}><RawIcon name="plug" /></span>
        <div className="overlay-wave" id="overlay-wave" hidden={!busy} aria-hidden="true">
          <i /><i /><i /><i /><i />
        </div>
        <h2 id="overlay-title">{copy.title}</h2>
        <p id="overlay-desc">{copy.desc}</p>
        <div className="overlay-actions" id="overlay-actions" hidden={busy}>
          <button
            id="overlay-retry"
            className="btn primary"
            type="button"
            data-testid="overlay-retry"
            disabled={busy}
            onClick={() => void connectDaemon()}
          >
            {t('common.retry')}
          </button>
        </div>
        <p className="overlay-hint" id="overlay-hint">{busy ? '' : copy.hint}</p>
      </div>
    </div>
  );
}
