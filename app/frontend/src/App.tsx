import { useCallback, useEffect, useSyncExternalStore } from 'react';
import { Watermark, NavPill, Overlay, VIEW_TITLE } from './components/Chrome';
import { ChromeControls } from './components/TopControls';
import { CaptionButtons } from './components/CaptionButtons';
import { chromeContextMenu, chromeMouseDown } from './lib/drag';
import { Toasts } from './components/Toasts';
import { ConfirmHost } from './components/ConfirmDialog';
import { PeersView } from './views/Peers';
import { DetailView } from './views/Detail';
import { ShareProtocolsView } from './views/ShareProtocols';
import { SettingsView } from './views/Settings';
import { StatsView } from './views/Stats';
import { OnboardingGate } from './views/Onboarding';
import { ShortcutSheetHost, toggleShortcutSheet } from './components/ShortcutSheet';
import { PermissionsSheetHost, isPermissionsSheetOpen, openPermissionsSheet } from './components/PermissionsSheet';
import { installShortcuts } from './lib/shortcutHost';
import { pendingSignature, readPermSeen, shouldAutoOpenPermissions } from './lib/permIntro';
import { installNativeSettingsMenu } from './lib/nativeMenu';
import type { ShortcutActionId } from './lib/shortcuts';
import { actions, getState, useStore } from './state/store';
import { isShareMode } from './state/mode';
import { boot, gateVisible, syncNativeAppearance, syncTray } from './state/connection';
import { subscribeLocale, subscribeTheme } from './lib/appearanceHost';
import { getLocale, t } from './i18n';

const VIEWS = {
  peers: PeersView,
  detail: DetailView,
  share: ShareProtocolsView,
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
 * 首次运行时自动弹一次系统权限面板（用户 2026-08-10 第 22 条）。
 *
 * 挂在**根层**而不是设置页：用户首次启动落在主面板，那时设置页根本没挂载，而这块
 * 面板要能在任何页面上弹。
 *
 * 判据是 `lib/permIntro.ts` 的纯函数（有单测），这里只负责把 store 的当下喂给它。
 * ⚠ 它读写一份 localStorage 签名，那份东西**不参与 `gateVisible()`**——授权门仍然
 * 每次启动重新探测、不落任何「已看过」标记。两者的边界写在 permIntro.ts 顶部，
 * 改这里之前先读那一段。
 */
function usePermissionIntro(gate: boolean): void {
  const perms = useStore((s) => s.permissions);
  const conn = useStore((s) => s.conn);
  const mode = useStore((s) => s.mode);

  useEffect(() => {
    if (isPermissionsSheetOpen()) return;
    const ok = shouldAutoOpenPermissions({
      online: conn === 'online',
      probed: perms.probed,
      gateVisible: gate,
      tauri: mode === 'tauri',
      signature: pendingSignature(perms.list),
      seen: readPermSeen(),
    });
    if (ok) openPermissionsSheet(true);
  }, [conn, gate, mode, perms]);
}

/**
 * 快捷键动作 → 真正做的事。**只有这一处**把动作 id 兑现成行为：`shortcuts.ts` 只
 * 认识 id 与组合键，不知道有路由这回事，所以那一层能当纯函数测。
 */
function useShortcutDispatch() {
  return useCallback((action: ShortcutActionId) => {
    switch (action) {
      case 'view.peers': actions.navigate('peers'); break;
      case 'view.stats': actions.navigate('stats'); break;
      case 'view.settings': actions.navigate('settings'); break;
      // 「返回」只在详情页成立。在别处让它也跳主面板会跟 ⌘1 重复，而一个在多数页面
      // 上什么都不做的键，比一个含义随页面漂移的键好解释。
      case 'nav.back':
        if (getState().route.view === 'detail') actions.navigate('peers');
        break;
      case 'help.shortcuts': toggleShortcutSheet(); break;
    }
  }, []);
}

/**
 * 「共享协议」只属于共享模式。模式在设置页被切走时，停在这一页的用户必须被带回
 * 主面板——导航胶囊里那一格同时消失，留在原地等于停在一页当前模式下并不存在的界面
 * 上，而且没有任何一格是高亮的。
 */
function useViewModeGuard(view: string, share: boolean): void {
  useEffect(() => {
    if (view === 'share' && !share) actions.navigate('peers');
  }, [view, share]);
}

export function App() {
  const view = useStore((s) => s.route.view);
  const share = useStore(isShareMode);
  const gate = useGateVisible();
  useViewModeGuard(view, share);
  // 语种订阅在**根组件**上，而且刻意不用它的值。
  //
  // `t()` 是纯函数调用，读的是 i18n 模块里那个模块级的 `current`——换语种改了它，
  // 但没有任何组件因此重渲，界面会原地不动。语言选择器现在有多个实际语种，
  // 所以这条根订阅是实际切换整棵组件树的必需接线。
  //
  // 挂在根上并且不 memo 任何子树，一次订阅就让整棵树跟着重渲，够了。
  useSyncExternalStore(subscribeLocale, getLocale);
  const View = VIEWS[view] || PeersView;
  const dispatch = useShortcutDispatch();
  usePermissionIntro(gate);

  useEffect(() => { boot(); }, []);
  // macOS consumes Command-comma in its application menu before WebKit sees a
  // keydown. The native item emits this event, restoring/focusing the window
  // on the Rust side first. Browser mode can keep the inert listener: it has no
  // native emitter, and avoiding a Tauri-global timing check keeps early menu
  // clicks reliable while the shell is still booting.
  useEffect(() => installNativeSettingsMenu(
    window,
    () => actions.navigate('settings'),
  ), []);
  // 托盘状态跟着连接走；syncTray 自带去重，重复调用无副作用。
  useEffect(() => useStore.subscribe(syncTray), []);
  // 主题不在 store 里（它是 localStorage + matchMedia，见 lib/appearanceHost），
  // 所以上面那条订阅看不见它变。Dock 图标要跟着深浅走，就得单独订一份。
  useEffect(() => subscribeTheme(syncTray), []);
  // Unlike ordinary browser copy, native device/tray strings are machine-wide.
  // connection.ts enforces the Tauri-only boundary before writing the resolved
  // locale to the daemon; this subscription makes an in-app switch immediate.
  useEffect(() => subscribeLocale(syncNativeAppearance), []);
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
          <ChromeControls />
          <CaptionButtons />
        </header>
      </div>

      {/* 首启授权门：盖住整个应用，但排在覆盖层之下——服务都没连上时，
          先说服务的事，权限页此刻也查不出任何东西。 */}
      <div id="gate" data-testid="gate" hidden={!gate}>
        {gate ? <OnboardingGate /> : null}
      </div>

      {/* 二级菜单层。两块都自带模块级开合状态：它们各有两个入口（⌘/ 与设置页；
          设置页与首启 effect），而那些入口不在同一棵子树上。 */}
      <ShortcutSheetHost />
      <PermissionsSheetHost />

      <Overlay />
      <ConfirmHost />
      <Toasts />
    </>
  );
}
