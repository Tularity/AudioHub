// 应用外壳：浮动导航胶囊、daemon 徽标、断线覆盖层。
//
// 左侧竖导航已经拆掉（docs/spec-ui.md §2）。导航现在是**浮在内容之上**的一枚居中
// 玻璃胶囊：内容从它下面穿过去滚动，胶囊本身不占布局宽度。testid 全部原样保留，
// `nav-*` 四个只是换了宿主元素。

import { useEffect, useState } from 'react';
import { RawIcon } from './Icon';
import { useStore } from '../state/store';
import type { AppState, ViewName } from '../state/store';
import { connectDaemon, IPC_VERSION } from '../state/connection';
import { t } from '../i18n';
import type { MsgKey } from '../i18n';

const NAV: { view: ViewName; labelKey: MsgKey; icon: 'peers' | 'pair' | 'stats' | 'settings' }[] = [
  { view: 'peers', labelKey: 'nav.peers', icon: 'peers' },
  { view: 'pair', labelKey: 'nav.pair', icon: 'pair' },
  { view: 'stats', labelKey: 'nav.stats', icon: 'stats' },
  { view: 'settings', labelKey: 'nav.settings', icon: 'settings' },
];

// 详情页高亮主面板
const NAV_OF: Partial<Record<ViewName, ViewName>> = { detail: 'peers' };

export const VIEW_TITLE: Record<ViewName, MsgKey> = {
  peers: 'nav.peers',
  detail: 'nav.detail',
  pair: 'nav.pair',
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

export function Brand() {
  return (
    <div className="brand">
      <span className="brand-logo"><RawIcon name="wave" /></span>
      <div className="brand-text">
        <strong>{t('app.name')}</strong>
        <span>{t('app.tagline')}</span>
      </div>
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
  const active = NAV_OF[view] || view;
  const index = Math.max(0, NAV.findIndex((n) => n.view === active));
  const contracted = useContracted();

  return (
    <nav
      id="nav"
      className={contracted ? 'contracted' : undefined}
      style={{ '--nav-count': NAV.length, '--nav-index': index } as React.CSSProperties}
    >
      <span className="nav-marker" aria-hidden="true" />
      {NAV.map((n) => (
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

/** 运行形态注脚。原来在侧栏底部，侧栏没了就落到窗口左下角当一条安静的说明。 */
export function ConnFoot() {
  const mode = useStore((s) => s.mode);
  const conn = useStore((s) => s.conn);
  const port = useStore((s) => s.endpoint?.port ?? null);

  // 「浏览器模式」是测试挂钩的自我说明，只在浏览器里出现，绝不进入 App 的文案。
  let foot: string;
  if (mode === 'tauri') {
    if (conn === 'online') foot = port ? t('foot.tauri.online', { port }) : t('foot.tauri.onlineNoPort');
    else if (conn === 'starting') foot = t('foot.tauri.starting');
    else if (conn === 'connecting') foot = t('foot.tauri.connecting');
    else foot = t('foot.tauri.offline');
  } else {
    foot = port ? t('foot.browser', { port }) : t('foot.browserNoPort');
  }

  return <div id="conn-hint" data-testid="conn-mode">{foot}</div>;
}

export function DaemonBadge() {
  const conn = useStore((s) => s.conn);
  const fp = useStore((s) => s.daemon?.fingerprint ?? null);
  const ctlPort = useStore((s) => s.daemon?.control_port ?? null);
  const cls = conn === 'online' ? 'online'
    : (conn === 'connecting' || conn === 'starting') ? 'connecting' : 'offline';
  const label = conn === 'online' ? t('badge.online')
    : conn === 'starting' ? t('badge.starting')
      : conn === 'connecting' ? t('badge.connecting') : t('badge.offline');

  return (
    <div id="daemon-badge" className={`daemon-badge ${cls}`} data-testid="daemon-badge">
      <span className={`dot ${cls}`} />
      <span className="badge-status">{label}</span>
      {fp ? (
        <>
          <span className="badge-sep">·</span>
          <code className="badge-fp" title={fp}>{fp.slice(0, 8)}</code>
          <span className="badge-port">{`:${ctlPort ?? t('common.dash')}`}</span>
        </>
      ) : null}
    </div>
  );
}

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
