// 应用外壳：侧栏导航、顶栏徽标、断线覆盖层。原本这些是 index.html 里的静态 DOM +
// app.js 里一大段 renderChrome()（带手写的 chromeKey 缓存）；React 下它们就是普通
// 组件，缓存键随之消失。

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

export function Sidebar({ onNavigate }: { onNavigate: (v: ViewName) => void }) {
  const view = useStore((s) => s.route.view);
  const mode = useStore((s) => s.mode);
  const conn = useStore((s) => s.conn);
  const port = useStore((s) => s.endpoint?.port ?? null);
  const active = NAV_OF[view] || view;

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

  return (
    <aside id="sidebar">
      {/* macOS titleBarStyle=Overlay：红绿灯浮在内容上，这块空区既是安全区也是拖拽把手 */}
      <div className="titlebar-space" data-tauri-drag-region />
      <div className="brand" data-tauri-drag-region>
        <span className="brand-logo"><RawIcon name="wave" /></span>
        <div className="brand-text">
          <strong>{t('app.name')}</strong>
          <span>{t('app.tagline')}</span>
        </div>
      </div>
      <nav id="nav">
        {NAV.map((n) => (
          <button
            key={n.view}
            className={`nav-item${active === n.view ? ' active' : ''}`}
            type="button"
            data-view={n.view}
            data-testid={`nav-${n.view}`}
            onClick={() => onNavigate(n.view)}
          >
            <RawIcon name={n.icon} />
            {t(n.labelKey)}
          </button>
        ))}
      </nav>
      <div className="sidebar-foot" id="conn-hint" data-testid="conn-mode">{foot}</div>
    </aside>
  );
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
