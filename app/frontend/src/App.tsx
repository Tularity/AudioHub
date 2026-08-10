import { useCallback, useEffect, useState, useSyncExternalStore } from 'react';
import { Watermark, NavPill, DaemonBadge, Overlay, VIEW_TITLE } from './components/Chrome';
import { CaptionButtons } from './components/CaptionButtons';
import { chromeContextMenu, chromeMouseDown } from './lib/drag';
import { Toasts } from './components/Toasts';
import { ConfirmHost } from './components/ConfirmDialog';
import { PeersView } from './views/Peers';
import { DetailView } from './views/Detail';
import { PairView } from './views/Pair';
import { SettingsView } from './views/Settings';
import { StatsView } from './views/Stats';
import { OnboardingGate } from './views/Onboarding';
import { ShortcutSheet } from './components/ShortcutSheet';
import { currentBindings, installShortcuts, subscribeShortcuts } from './lib/shortcutHost';
import type { ShortcutActionId } from './lib/shortcuts';
import { actions, getState, useStore } from './state/store';
import { boot, gateVisible, syncTray } from './state/connection';
import { t } from './i18n';

const VIEWS = {
  peers: PeersView,
  detail: DetailView,
  pair: PairView,
  settings: SettingsView,
  stats: StatsView,
} as const;

/**
 * 授权门开不开是个**粘滞**判断（一旦挡上就只能由用户自己让开），所以判据留在
 * connection.ts 的模块状态里。这里只订阅 store 的变更去重新问它一次。
 */
function useGateVisible(): boolean {
  return useSyncExternalStore(useStore.subscribe, gateVisible);
}

/**
 * 快捷键动作 → 真正做的事。**只有这一处**把动作 id 兑现成行为：`shortcuts.ts` 只
 * 认识 id 与组合键，不知道有路由这回事，所以那一层能当纯函数测。
 */
function useShortcutDispatch(setSheet: (fn: (v: boolean) => boolean) => void) {
  return useCallback((action: ShortcutActionId) => {
    switch (action) {
      case 'view.peers': actions.navigate('peers'); break;
      case 'view.pair': actions.navigate('pair'); break;
      case 'view.stats': actions.navigate('stats'); break;
      case 'view.settings': actions.navigate('settings'); break;
      // 「返回」只在详情页成立。在别处让它也跳主面板会跟 ⌘1 重复，而一个在多数页面
      // 上什么都不做的键，比一个含义随页面漂移的键好解释。
      case 'nav.back':
        if (getState().route.view === 'detail') actions.navigate('peers');
        break;
      case 'help.shortcuts': setSheet((v) => !v); break;
    }
  }, [setSheet]);
}

export function App() {
  const view = useStore((s) => s.route.view);
  const gate = useGateVisible();
  const View = VIEWS[view] || PeersView;
  const [sheet, setSheet] = useState(false);
  const dispatch = useShortcutDispatch(setSheet);
  // 速查表读的是**当前生效**的绑定，所以设置页改完立刻反映，不用重开。
  const bindings = useSyncExternalStore(subscribeShortcuts, currentBindings);

  useEffect(() => { boot(); }, []);
  // 托盘状态跟着连接走；syncTray 自带去重，重复调用无副作用。
  useEffect(() => useStore.subscribe(syncTray), []);
  // 授权门挡着的时候不派发：那时候导航到别的页面只会得到一屏查不出任何东西的空视图。
  useEffect(() => (gate ? undefined : installShortcuts(dispatch)), [gate, dispatch]);

  return (
    <>
      <div id="app">
        {/* 背景水印。排在最前只是为了读起来像「背景」——它靠 z-index:-1 定位到
            负层，画在画布底色之上、所有内容之下，DOM 顺序不参与这个决定。 */}
        <Watermark />

        <main id="view-root">
          {/* key 让视图切换重新挂载，从而复现那段淡入 + 上移的动画 */}
          <section className="view" data-testid={`view-${view}`} key={view}>
            <View />
          </section>
        </main>

        {/* 浮动控件层，**排在内容之后**：它盖在内容上，靠 position:fixed 脱离布局，
            所以内容能从它下面穿过去滚动。

            两个平台在这条带子里的分工不同，但**只差一个方向变量**（lib/platform.ts）：
            macOS 的红绿灯（titleBarStyle=Overlay）由系统画在前缘、在 webview 之上，
            永远先拿到点击，不会被这里挡掉；Windows 关掉了系统边框（decorations:false），
            后缘那三颗由 CaptionButtons 自己画。徽标待在系统没占的那一端。
            onMouseDown 是窗口拖拽：控件与可选文本由 lib/drag.ts 自己排除。 */}
        <header id="topbar" onMouseDown={chromeMouseDown} onContextMenu={chromeContextMenu}>
          <h1 id="view-title">{t(VIEW_TITLE[view])}</h1>
          <NavPill onNavigate={(v) => actions.navigate(v)} />
          <DaemonBadge />
          <CaptionButtons />
        </header>
      </div>

      {/* 首启授权门：盖住整个应用，但排在覆盖层之下——服务都没连上时，
          先说服务的事，权限页此刻也查不出任何东西。 */}
      <div id="gate" data-testid="gate" hidden={!gate}>
        {gate ? <OnboardingGate /> : null}
      </div>

      {sheet ? <ShortcutSheet bindings={bindings} onClose={() => setSheet(false)} /> : null}

      <Overlay />
      <ConfirmHost />
      <Toasts />
    </>
  );
}
