// 设置：运行模式（真选择器）→ 模式选项 → 网络（含网页访问）→ 杂项 → 关于。
//
// # 模式选择器为什么在这一页
//
// 它原本在主面板，理由写在 plan §7.1：模式决定了主面板每张卡片的含义，把它藏进
// 设置页等于把「那排开关为什么消失了」的答案藏起来。**用户 2026-08-10 的第 1 条
// 指令推翻了这条裁定**（见 docs/plan.md §17）：模式是这台机器的一项全局配置，
// 全局配置的落点就是设置页，而主面板失去入口的代价由导航胶囊 + 切模式时的 toast
// 兜住。改回去之前请先读 §17，不要照 §7.1 的原文把它搬回主面板。
//
// # 这一页的分块口径
//
// 「杂项」是一个筐，而筐一旦存在，后续每一个没想清楚归属的开关都会掉进去。
// 冻结的约束：**杂项只收「与音频链路无关的本机事务」**——身份、启动、路径、
// 权限入口、快捷键入口。任何与模式 / 网络 / 设备 / 传输相关的东西都不许进。

import { useEffect, useState, useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';
import { Icon, RawIcon } from '../components/Icon';
import { ExtLink, Help, Segmented, Switch } from '../components/Controls';
import { confirmDialog } from '../components/ConfirmDialog';
import { openPermissionsSheet } from '../components/PermissionsSheet';
import { openShortcutSheet } from '../components/ShortcutSheet';
import { Sheet } from '../components/Sheet';
import { toast } from '../components/Toasts';
import { appVersion } from '../lib/appInfo';
import { WIKI } from '../lib/external';
import { autostartView } from '../lib/autostart';
import { bridgeCatalog, vendors } from '../lib/bridge';
import { fmt, IS_MAC } from '../lib/fmt';
import { customizedCount, PLATFORM, SHORTCUT_ACTIONS } from '../lib/shortcuts';
import { getOverrides, subscribeShortcuts } from '../lib/shortcutHost';
import {
  NAME_MAX, canRestoreDefault, nameDirty, nameEditable, normalizeLocalName, parseNameSource,
} from '../lib/identityName';
import {
  getWebUiStatus, inferredStatus, setWebUiSettings, webPortValid, webUiSupported,
  WEB_PORT_MAX, WEB_PORT_MIN,
} from '../lib/webui';
import type { WebUiPatch, WebUiStatus } from '../lib/webui';
import { t, joinPhrases } from '../i18n';
import type { MsgKey } from '../i18n';
import { getState, useStore } from '../state/store';
import type { AppState } from '../state/store';
import type { DaemonSettings } from '../ipc/types';
import {
  MODE_SHARE, MODE_A, MODE_B,
  halState, requestedMode, isModeB, modeDowngraded, deviceStateLabel,
} from '../state/mode';
import type { AppMode } from '../state/mode';
import { applySettings, rpc } from '../state/connection';

// plan §13：三档。`Record<AppMode, MsgKey>` 而不是 `Record<string, MsgKey>`——
// 后者会让漏掉一档在编译期毫无声响，运行期变成一次 `t(undefined)`。
export const MODE_LABEL_KEY: Record<AppMode, MsgKey> = {
  share: 'mode.share.label',
  a: 'mode.a.label',
  b: 'mode.b.label',
};

// 切换后的提示语。三档各一条：模式切换会**真的关掉正在跑的会话**（plan §13
// 推论 2），用户必须从这句话里读到发生了什么，而不是从「对端怎么突然没声了」。
//
// 主面板不再有模式入口之后，这三条 toast 是「刚才那一下都动了什么」的唯一出口。
const SWITCHED_KEY: Record<AppMode, MsgKey> = {
  share: 'mode.switched.toShare',
  a: 'mode.switched.toA',
  b: 'mode.switched.toB',
};

/**
 * 一行设置：**标题 + 值/状态 + 一枚 `?`**，没有第四样东西。
 *
 * `desc` 已经没有了。用户 2026-08-10 裁定「为了简化而简化」（docs/plan.md §3.1）：
 * 每一行原本挂着的两三行说明全部搬进 wiki，界面上只留一个入口。这里因此**刻意
 * 不留 desc 形参**——留着它，下一个人会顺手再写一段进来，而这一整轮改动就是为了
 * 把那些段落清出去。
 *
 * `note` 是给**状态**用的（「广播没建立」这类随运行时变化的事实），不是描述的
 * 后门：它渲染在标题下方，写成一句短话就行。
 */
function SettingRow({ title, control, badge, help, note }: {
  title: string; control: ReactNode; badge?: string; help?: ReactNode; note?: string;
}) {
  return (
    <div className="setting-row">
      <div className="setting-text">
        <div className="setting-title">
          {title}
          {badge ? <span className="tag warn">{badge}</span> : null}
          {help}
        </div>
        {note ? <p className="setting-desc">{note}</p> : null}
      </div>
      <div className="setting-ctl">{control}</div>
    </div>
  );
}

/** 区块标题 + `?`。同一个形状出现十几次，抽出来省得每处各写各的间距。 */
function BlockTitle({ text, url, label, testid, danger = false }: {
  text: string; url: string; label: string; testid: string; danger?: boolean;
}) {
  return (
    <div className="title-row">
      <h3 className={`block-title${danger ? ' danger-title' : ''}`}>{text}</h3>
      <Help label={label} url={url} testid={testid} />
    </div>
  );
}

// ---------------------------------------------------------------- ① 运行模式

// 整块从主面板搬来（用户 2026-08-10 第 1 条）。搬的是**真选择器**，不是镜子：
// 设置页原先只有一面只读镜子 + 一枚「前往主面板切换」的按钮，那两样东西随这次
// 搬迁一并消失——同一个全局状态不该有两个可点的控件、两处 pending 态。
function ModeCard() {
  const daemon = useStore((s) => s.daemon);
  const mode = useStore(requestedMode);
  const downgraded = useStore(modeDowngraded);
  const st = halState(daemon);

  async function setMode(v: AppMode): Promise<void> {
    if (v === requestedMode(getState())) return;
    try {
      await applySettings({ mode: v });
      toast(t(SWITCHED_KEY[v]), 'ok');
    } catch { /* rpc 已 toast */ }
  }

  return (
    <section className="card block mode-bar" data-testid="settings-mode">
      <div className="mode-head">
        {/* 标题 + `?`，没有第三样东西。三种模式的说明、互斥的成因、每一档选完
            之后去哪里操作——原先全都印在这张卡上，现在整段在 wiki。 */}
        <div className="mode-title-wrap title-row">
          <h3 className="block-title">{t('settings.mode.title')}</h3>
          <Help label={t('wiki.modes')} url={WIKI.modes} testid="settings-mode-help" />
        </div>
        <Segmented<AppMode>
          testid="settings-consumer-mode"
          value={mode}
          onSelect={setMode}
          options={[
            // 共享模式排第一：它是默认值，也是「本机被别人使用」这条唯一的路。
            { value: MODE_SHARE, label: t('mode.share.label') },
            { value: MODE_A, label: t('mode.a.label') },
            {
              value: MODE_B,
              label: t('mode.b.label'),
              // 置灰必须是**真禁用**（点击不改变任何状态），判据见 state/mode.ts halState()。
              disabled: !st.available,
              why: st.why || '',
            },
          ]}
        />
      </div>
      {/* 「驱动就绪」是常态，不是消息：只有出问题时这行才值得占一行。 */}
      <p
        className={`mode-note tone-${st.tone}`}
        data-testid="settings-mode-note"
        hidden={st.tone === 'ok'}
      >
        {st.text}
      </p>
      {/* 用户存的是 B、daemon 只能给 A：这是**降级**，不是「他选了 A」。 */}
      <p className="mode-warn" data-testid="consumer-mode-downgraded" hidden={!downgraded}>
        {t('mode.downgraded')}
      </p>
    </section>
  );
}

// ---------------------------------------------------------------- ② 模式选项

// 用户 2026-08-10 第 2 条：「模式 A · 音量」与「虚拟设备」合并成一块，**按选中的
// 模式出现各自的配置项**。
//
// ⚠ 判据取 `requestedMode` 而不是 `effectiveMode`：模式 B 被降级时（驱动没起来）
// 用户请求的是 B，他要配的就是 B 那一组。「降级了」这件事第一块已经用
// `mode.downgraded` 说过了，在这里再按 effective 换一组内容，等于同一页对同一个
// 问题给出两个答案。`DeviceInventory` 读的是真实 HAL 状态，降级时它自己会说
// 「没有设备」——那是事实，允许。
//
// ⚠ 这块推翻了一条现存裁定：「不禁用模式 A 的两个开关，因为用户可能**先配好再切
// 模式**」。按模式显示之后那条路没了。可以接受的理由是模式选择器现在就在同一页的
// 第一块，「改一下模式 → 那一组出现」只有一次点击；而旧设计里它在另一个页面。
function ModeOptionsCard({ writing, noSettings, onPush }: {
  writing: number;
  noSettings: boolean;
  onPush: (patch: Partial<DaemonSettings>) => Promise<boolean>;
}) {
  const mode = useStore(requestedMode);

  return (
    <section className="card block" data-testid="settings-mode-options">
      <BlockTitle
        text={t('settings.modeOptions.title')}
        url={mode === MODE_B ? WIKI.deviceOptions : WIKI.volumeModes}
        label={mode === MODE_B ? t('wiki.devices') : t('wiki.volume')}
        testid="settings-mode-options-help"
      />
      {mode === MODE_A ? <ModeAOptions writing={writing} noSettings={noSettings} onPush={onPush} /> : null}
      {mode === MODE_B ? <ModeBOptions writing={writing} noSettings={noSettings} onPush={onPush} /> : null}
      {mode === MODE_SHARE ? (
        <p className="muted small" data-testid="settings-mode-options-none">
          {t('settings.modeOptions.shareNone')}
        </p>
      ) : null}
    </section>
  );
}

// plan §7.1 模式 A 的两个**独立**开关 + 虚拟声卡桥接。
//
// 三样东西都放在设置页而不是对端卡片上，与 daemon 把它们存进 settings.json 是同一
// 条理由：模式是全局设置（§7.1 冻结），这些是那个模式的选项；而且它们动的是**本机
// 那一个**默认输出设备，不是每台对端各一份。
function ModeAOptions({ writing, noSettings, onPush }: {
  writing: number;
  noSettings: boolean;
  onPush: (patch: Partial<DaemonSettings>) => Promise<boolean>;
}) {
  const ds = useStore((s) => s.daemonSettings);
  const daemon = useStore((s) => s.daemon);
  const catalog = bridgeCatalog(daemon);

  return (
    <>
      <SettingRow
        title={t('settings.modeAVolume.syncTitle')}
        control={(
          <Switch
            testid="settings-mode-a-volume-sync"
            label={t('settings.modeAVolume.syncTitle')}
            checked={!!(ds && ds.mode_a_volume_sync)}
            pending={writing > 0}
            disabled={noSettings}
            onToggle={(want) => void onPush({ mode_a_volume_sync: want })}
          />
        )}
      />
      <SettingRow
        title={t('settings.modeAVolume.muteTitle')}
        control={(
          <Switch
            testid="settings-mode-a-mute-local"
            label={t('settings.modeAVolume.muteTitle')}
            checked={!!(ds && ds.mode_a_mute_local)}
            pending={writing > 0}
            disabled={noSettings}
            onToggle={(want) => void onPush({ mode_a_mute_local: want })}
          />
        )}
      />

      {/* 虚拟声卡桥接（spec-m4c §B / plan §7.1）：这里只报「检测到了什么」并给官网
          链接，真正的选择在主面板的对端卡片上——桥接目标是**按对端**决定的。
          冻结的口径：不代装、不主动引导安装，所以没有任何安装按钮或催促文案。

          它原先是一张独立的卡，且在共享模式下也显示。收进模式 A 这一组是**修正**
          不是回归：plan §7.1 把桥接定义为模式 A 麦克风方向的去向，共享模式下它
          没有作用对象。 */}
      <div className="divider" />
      <div className="bridge-status" data-testid="settings-bridge-status">
        {catalog == null ? (
          <p className="muted small" data-testid="settings-bridge-none">
            {daemon ? t('settings.bridge.noneReported') : t('settings.bridge.noneOffline')}
          </p>
        ) : !catalog.length ? (
          <p className="muted small" data-testid="settings-bridge-none">{t('settings.bridge.noneFound')}</p>
        ) : catalog.map((c) => (
          // present 但不在 output_devices 里：装是装了，daemon 却打不开它，
          // 说「已检测到」就成了骗人。
          <div
            key={c.id}
            className={`bridge-status-row${c.usable ? ' on' : ''}`}
            data-testid={`settings-bridge-card-${c.id}`}
          >
            <Icon name="cable" />
            <span className="bridge-status-name">{c.name}</span>
            {c.usable
              ? <span className="tag ok">{t('settings.bridge.detected')}</span>
              : c.present
                ? <span className="tag warn">{t('settings.bridge.notInOutputs')}</span>
                : <span className="tag">{t('settings.bridge.notDetected')}</span>}
          </div>
        ))}
      </div>
      <div className="bridge-links" data-testid="settings-bridge-links">
        {vendors().map((v) => (
          <ExtLink key={v.id} text={v.label} url={v.url} testid={`settings-bridge-link-${v.id}`} />
        ))}
      </div>
    </>
  );
}

function ModeBOptions({ writing, noSettings, onPush }: {
  writing: number;
  noSettings: boolean;
  onPush: (patch: Partial<DaemonSettings>) => Promise<boolean>;
}) {
  const s = useStore();
  const ds = s.daemonSettings;

  return (
    <>
      <SettingRow
        title={t('settings.devices.removeTitle')}
        control={(
          <Switch
            testid="settings-remove-virtual"
            label={t('settings.devices.removeTitle')}
            checked={ds ? !!ds.remove_virtual_on_disconnect : s.settings.removeVirtual}
            pending={writing > 0}
            disabled={noSettings}
            onToggle={(want) => void onPush({ remove_virtual_on_disconnect: want })}
          />
        )}
      />
      <SettingRow
        title={t('settings.devices.markOfflineTitle')}
        control={(
          <Switch
            testid="settings-mark-offline"
            label={t('settings.devices.markOfflineTitle')}
            checked={ds ? !!ds.mark_offline_devices : true}
            pending={writing > 0}
            disabled={noSettings}
            onToggle={(want) => void onPush({ mark_offline_devices: want })}
          />
        )}
      />
      <div className="divider" />
      <DeviceInventory />
    </>
  );
}

function DeviceInventory() {
  const s = useStore();
  const ds = s.daemonSettings;
  const hal = s.daemon ? s.daemon.hal : null;
  const list = hal && Array.isArray(hal.devices) ? hal.devices : [];
  const cap = ds ? ds.hal_capacity : (hal ? 16 : 0);
  const used = ds ? ds.hal_used : list.length;

  // 只在**没有设备**时说话。有设备时那一行清单自己就是答案，再补一句
  // 「已发布 = …」是把定义写在结论旁边——正是这一轮要清掉的东西。
  const note = list.length
    ? ''
    : !hal ? t('settings.devices.noteNoDriver')
      : isModeB(s) ? t('settings.devices.noteModeB') : t('settings.devices.noteModeA');

  return (
    <>
      <div className="dev-inventory-head">
        <span className="dev-inventory-title">{t('settings.devices.inventory')}</span>
        <span className="dev-count" data-testid="settings-hal-count">
          {cap
            ? t('settings.devices.count', { used: fmt.count(used), cap: fmt.count(cap) })
            : t('settings.devices.countNa')}
        </span>
      </div>
      <div className="dev-inventory" data-testid="settings-hal-devices" hidden={list.length === 0}>
        {list.map((d) => {
          const fp = d.fingerprint || '';
          const peer = s.peers.find((p) => p.fingerprint === fp);
          const owner = (peer && (peer.display_name || peer.name)) || fp.slice(0, 12);
          // state 与 observed 是两件事：前者是驱动应答了我们，后者是系统真的列出了它。
          // 只报前者，就会把「发过 Bind 但设备没出现」显示成一切正常。
          const published = d.state === 'bound' && d.observed;
          const rows = [
            { dir: 'out' as const, ico: 'spk' as const, name: d.out_name, uid: d.out_uid, io: d.io_out, frames: d.spk_frames, drop: null as number | null },
            { dir: 'in' as const, ico: 'mic' as const, name: d.in_name, uid: d.in_uid, io: d.io_in, frames: d.mic_frames, drop: d.mic_dropped ?? null },
          ];
          return (
            <div key={fp} className="dev-inv-card" data-testid={`settings-hal-device-${fp}`}>
              <div className="dev-inv-head">
                <strong>{owner}</strong>
                <code className="mono dim">
                  {t('device.slotGen', { slot: String(d.slot ?? ''), gen: String(d.generation ?? '') })}
                </code>
                {d.peer_connected
                  ? <span className="tag ok">{t('common.online')}</span>
                  : <span className="tag">{t('common.offline')}</span>}
                {published
                  ? <span className="tag ok">{t('settings.devices.tagPublished')}</span>
                  : d.state === 'bound'
                    ? <span className="tag warn">{t('settings.devices.tagMissing')}</span>
                    : <span className="tag">{deviceStateLabel(d.state)}</span>}
              </div>
              {rows.map((r) => (
                <div key={r.dir} className="dev-inv-row" data-testid={`settings-hal-${r.dir}-${fp}`}>
                  <Icon name={r.ico} cls="ico dev-ico" />
                  <div className="dev-text">
                    <span className="dev-name">{r.name || t('common.dash')}</span>
                    <code className="dev-uid mono">{r.uid || ''}</code>
                  </div>
                  <span className="dev-frames mono">
                    {joinPhrases([
                      t('device.frames', { n: fmt.count(r.frames) }),
                      r.drop ? t('device.dropped', { n: fmt.count(r.drop) }) : null,
                    ])}
                  </span>
                  <span className={`dev-state ${r.io ? 'live' : 'idle'}`}>
                    {r.io ? t('device.inUse') : t('device.idle')}
                  </span>
                </div>
              ))}
            </div>
          );
        })}
      </div>
      <p className="muted small" data-testid="settings-hal-note">{note}</p>
    </>
  );
}

// ---------------------------------------------------------------- ③ 网络

// 网页访问（plan §7.5）。三个选项落在 **App 自己的** <config>/webui.json：daemon
// 是音频与网络引擎，不该为「App 要不要开个网页端口」长一个字段。
//
// 这一段的重点是那条警告。「仅允许本机」关掉之后发生的事不是抽象的「安全风险」，
// 而是：整个局域网都能打开这套界面并完整操作本机音频，且 /ipc-endpoint 会把 IPC
// 令牌明文交出去——所以文案逐条说出来，而不是写一句「请注意安全」。
//
// 用户 2026-08-10 第 6 条把它并进了「网络」块，所以它现在是一个 fragment 而不是
// 一张卡；自己的 useState/useEffect 原样保留。它上面那条 `.divider` **不是装饰**：
// 分隔线以上是 daemon 拥有的网络属性，以下是 App 自己的 webui.json（plan §7.5
// 冻结的分层），合并之后界面上再没有第二个地方能看出这条分层。
function WebAccessRows() {
  // 浏览器态没有 Tauri 桥，也就没有调用面：三个选项只读。这不是退让——否则局域网
  // 上任何访客都能顺手把 local_only 关掉。
  const editable = webUiSupported();
  const [status, setStatus] = useState<WebUiStatus | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [busy, setBusy] = useState(false);
  const [portDraft, setPortDraft] = useState('');

  useEffect(() => {
    if (!editable) {
      setStatus(inferredStatus());
      setPortDraft(String(inferredStatus().port));
      setLoaded(true);
      return;
    }
    let alive = true;
    getWebUiStatus()
      .then((s) => {
        if (!alive) return;
        setStatus(s);
        setPortDraft(String(s.port));
      })
      .catch((e) => { if (alive) toast(String(e), 'warn'); })
      .finally(() => { if (alive) setLoaded(true); });
    return () => { alive = false; };
  }, [editable]);

  // 不做乐观翻转：回包才是权威。端口占用时开关必须停在「没开起来」，而不是显示成
  // 已启用——后者会让用户对着一个根本连不上的地址找问题。
  async function push(patch: WebUiPatch): Promise<void> {
    if (!editable || busy) return;
    setBusy(true);
    try {
      const next = await setWebUiSettings(patch);
      setStatus(next);
      setPortDraft(String(next.port));
      if (next.enabled && !next.running && next.error) {
        toast(t('settings.web.error', { message: next.error }), 'warn');
      }
    } catch (e) {
      toast(String(e), 'warn');
    } finally {
      setBusy(false);
    }
  }

  function commitPort(): void {
    const n = Number(portDraft.trim());
    if (!webPortValid(n)) {
      toast(t('settings.web.portInvalid'), 'warn');
      return;
    }
    if (status && n === status.port) return;
    void push({ port: n });
  }

  const st = status;
  const running = !!st && st.running;
  const localOnly = st ? st.local_only : true;
  // 拿不到状态时按「锁死」呈现：还没问出结果就先把开关画成能点的，是最坏的一种默认。
  const locked = st ? st.local_only_locked : true;
  const disabled = !editable || !loaded || busy;

  return (
    <>
      <SettingRow
        title={t('settings.web.enabledTitle')}
        help={<Help label={t('wiki.web')} url={WIKI.web} testid="settings-web-help" />}
        control={(
          <Switch
            testid="settings-web-enabled"
            label={t('settings.web.enabledTitle')}
            checked={!!st && st.enabled}
            pending={busy}
            disabled={disabled}
            onToggle={(want) => void push({ enabled: want })}
          />
        )}
      />

      <SettingRow
        title={t('settings.web.portTitle')}
        control={(
          <div className="field-btn">
            <input
              className="input web-port"
              data-testid="settings-web-port"
              inputMode="numeric"
              size={6}
              value={portDraft}
              disabled={disabled}
              min={WEB_PORT_MIN}
              max={WEB_PORT_MAX}
              onChange={(e) => setPortDraft(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter') commitPort(); }}
            />
            <button
              className="btn small"
              type="button"
              data-testid="settings-web-port-apply"
              disabled={disabled}
              onClick={commitPort}
            >
              {t('settings.web.portApply')}
            </button>
          </div>
        )}
      />

      {/* 「仅允许本机」当前锁死（plan §7.5 用户裁定）：判据 local_only_locked 来自
          服务端，前端不自己写死——解锁那天只改 webui.rs 一处。 */}
      <SettingRow
        title={t('settings.web.localOnlyTitle')}
        badge={locked ? t('settings.web.localOnlyBadge') : undefined}
        help={(
          <Help
            label={t('wiki.webLocalOnly')} url={WIKI.webLocalOnly}
            testid="settings-web-local-only-help"
          />
        )}
        control={(
          <Switch
            testid="settings-web-local-only"
            label={t('settings.web.localOnlyTitle')}
            checked={localOnly}
            pending={busy}
            disabled={disabled || locked}
            onToggle={(want) => void push({ local_only: want })}
          />
        )}
      />
      {/* 关掉「仅允许本机」= 把一个无鉴权的控制界面连同 IPC 令牌一起交给局域网。
          plan §7.5 要求这条警告存在，且该选项永不为默认值。
          ⚠ 合并进「网络」块之后它落在一个更长的块的中段，视觉权重会下降——所以它
          必须仍以 `.web-warn` 的强调样式出现在那个开关的**正下方**，位置不许再动。 */}
      <div className="web-warn" data-testid="settings-web-warning" hidden={localOnly}>
        <strong className="web-warn-title">{t('settings.web.warnTitle')}</strong>
        <Help
          label={t('wiki.webLocalOnly')} url={WIKI.webLocalOnly}
          testid="settings-web-warning-help"
        />
      </div>

      <div className="web-urls" data-testid="settings-web-url">
        {!loaded ? (
          <p className="muted small">{t('settings.web.starting')}</p>
        ) : !running ? (
          <p className="muted small">{t('settings.web.off')}</p>
        ) : (
          <>
            <p className="muted small mono">{t('settings.web.urlLocal', { url: st?.url || '' })}</p>
            {!localOnly ? (
              <>
                {st?.lan_url
                  ? <p className="muted small mono">{t('settings.web.urlLan', { url: st.lan_url })}</p>
                  : <p className="muted small">{t('settings.web.urlLanUnknown')}</p>}
              </>
            ) : null}
            {editable && st?.url ? <ExtLink text={st.url} url={st.url} testid="settings-web-open" /> : null}
          </>
        )}
      </div>

      <p
        className="muted small tone-danger"
        data-testid="settings-web-error"
        hidden={!st || !st.error}
      >
        {st && st.error
          ? joinPhrases([t('settings.web.error', { message: st.error }), t('settings.web.errorHint')])
          : ''}
      </p>

      <p className="muted small" data-testid="settings-web-note">
        {joinPhrases([
          editable ? null : t('settings.web.browserOnly'),
          running && st?.source === 'disk' ? t('settings.web.sourceDisk', { root: st.root || '' }) : null,
          running && st?.source === 'embedded' ? t('settings.web.sourceEmbedded') : null,
        ])}
      </p>
    </>
  );
}

function NetworkCard({ writing, noSettings, onPush }: {
  writing: number;
  noSettings: boolean;
  onPush: (patch: Partial<DaemonSettings>) => Promise<boolean>;
}) {
  const s = useStore();
  const ds = s.daemonSettings;

  return (
    <section className="card block" data-testid="settings-net">
      <BlockTitle
        text={t('settings.net.title')} url={WIKI.discovery}
        label={t('wiki.discovery')} testid="settings-net-help"
      />
      {/*
        plan M3「同网段互见」的开关，以及它的隐私那一半。
        放在网络这一格而不是配对页：它是这台机器的一个持续属性（关掉之后
        永远不广播），不是配对流程里的一个步骤。
      */}
      <SettingRow
        title={t('settings.net.announceTitle')}
        help={(
          <Help
            label={t('wiki.discovery')} url={WIKI.announce}
            testid="settings-net-announce-help"
          />
        )}
        control={(
          <Switch
            testid="settings-discovery-announce"
            label={t('settings.net.announceTitle')}
            // 缺席时按 false 画，而不是按默认值 true：一个还没答复、或者太旧
            // 而没有这个字段的 daemon，画成「正在广播」就是在替它撒谎。
            checked={!!(ds && ds.discovery_announce)}
            pending={writing > 0}
            disabled={noSettings}
            onToggle={(want) => void onPush({ discovery_announce: want })}
          />
        )}
      />
      {/*
        只在**想广播却没广播成**时出现。两个字段一致时说任何话都是噪音，
        而它们不一致时，开关自己看上去是「开着的」——这一句是界面上唯一
        能说出「别人其实看不见这台机器」的地方。
      */}
      <p
        className="muted small"
        data-testid="settings-announce-warn"
        hidden={!(ds && ds.discovery_announce && !ds.discovery_announcing)}
      >
        {t('settings.net.announceNotInForce')}
      </p>
      {/* 控制端口是 plan §7.6 那三个「必须常驻可读」的值之一（右上徽标改成悬停
          显示之后，触摸屏没有 hover，截图排障也拿不到 hover 态）。另外两个在
          「杂项」的本机身份两行里。 */}
      <SettingRow
        title={t('settings.net.controlPort')}
        help={<Help label={t('wiki.discovery')} url={WIKI.ports} testid="settings-net-port-help" />}
        badge={t('settings.net.controlPortBadge')}
        control={(
          <code className="mono" data-testid="settings-port">
            {s.daemon?.control_port != null ? String(s.daemon.control_port) : t('common.dash')}
          </code>
        )}
      />
      <SettingRow
        title={t('settings.net.ipcPort')}
        control={(
          <code className="mono" data-testid="settings-ipc-port">
            {s.endpoint ? String(s.endpoint.port) : t('common.dash')}
          </code>
        )}
      />

      {/* 分层线：以上 daemon，以下 App 自己的 webui.json。 */}
      <div className="divider" />
      <WebAccessRows />
    </section>
  );
}

// ---------------------------------------------------------------- ④ 杂项

// 本机名称。空 = 跟随本机名称（daemon 的 `local_hostname()`）。
//
// 输入框只在用户**没有在编辑**时跟随 daemon：settings 每秒刷新一帧，无条件回灌会
// 把正在敲的字冲掉（与对端别名框、隧道地址框同一条纪律）。
function IdentityRows({ onPush }: { onPush: (patch: Partial<DaemonSettings>) => Promise<boolean> }) {
  const daemon = useStore((s) => s.daemon);
  const ds = useStore((s) => s.daemonSettings);
  const fp = daemon?.fingerprint || '';
  const liveName = daemon?.name || '';
  const source = parseNameSource(ds?.name_source);
  const editable = nameEditable(source);

  const [draft, setDraft] = useState<string | null>(null);
  const shown = draft ?? liveName;
  const dirty = draft != null && nameDirty(draft, liveName);

  // 失败时**草稿留着**：连同错误一起把用户刚敲的字清掉，是这个项目已经付过代价的
  // 形状（回包才是权威，但回包没来之前不能先把界面改成好像成功了）。
  async function save(next: string): Promise<void> {
    if (await onPush({ name: normalizeLocalName(next) })) setDraft(null);
  }

  return (
    <>
      <SettingRow
        title={t('settings.identity.name')}
        help={<Help label={t('wiki.discovery')} url={WIKI.fingerprint} testid="settings-identity-help" />}
        // 后果句（plan §3.1 第 4 类，获准留在界面上）：改名会改掉**每一台对端**
        // 系统里那两台虚拟设备的名字，且对端要等下一次连接才看得到。
        note={editable ? t('settings.identity.renameEffect') : t('settings.identity.nameEnv')}
        control={(
          <div className="name-field">
            <input
              className="input"
              data-testid="settings-identity-name-input"
              value={shown}
              maxLength={NAME_MAX}
              disabled={!editable}
              placeholder={t('settings.identity.namePlaceholder')}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter' && dirty) void save(shown); }}
            />
            <button
              className="btn small primary" type="button"
              data-testid="settings-identity-name-save"
              disabled={!editable || !dirty}
              onClick={() => void save(shown)}
            >
              {t('common.save')}
            </button>
            <button
              className="btn small" type="button"
              data-testid="settings-identity-name-default"
              // 只有确实存在一份用户覆盖时才有东西可恢复。
              disabled={!editable || !canRestoreDefault(source)}
              onClick={() => void save('')}
            >
              {t('settings.identity.nameDefault')}
            </button>
          </div>
        )}
      />
      <SettingRow
        title={t('settings.identity.fingerprint')}
        control={(
          <div className="field-btn">
            {/* ⚠ 无障碍红线：整框可点 = **真 `<button>`**，不是 `<code>` 上挂
                onClick。本机身份这一块正是 §7.6 的键盘/触摸屏兜底，在一个 `<code>`
                上挂 onClick 等于对键盘与读屏用户删掉了复制入口。 */}
            <button
              type="button"
              className="fp-full fp-copy"
              data-testid="settings-identity-fp"
              aria-label={t('settings.identity.copyHint')}
              title={t('settings.identity.copyHint')}
              disabled={!fp}
              onClick={async () => {
                try {
                  await navigator.clipboard.writeText(fp);
                  toast(t('settings.identity.copied'), 'ok');
                } catch {
                  toast(t('common.copyFailed'), 'warn');
                }
              }}
            >
              {fp || t('common.dash')}
            </button>
            <IdentityResetButton fp={fp} />
          </div>
        )}
      />
    </>
  );
}

// 重置本机指纹 = 换一对 ed25519 密钥。**全应用破坏力最大的按钮**，而它待在一个叫
// 「杂项」的板块里，所以语气与门槛都要撑住：danger 色 + 两道确认（Sheet 是第一道，
// confirmDialog 是第二道），且 Sheet 里必须显示「已配对 N 台」这个数——数字比形容词
// 有说服力。
function IdentityResetButton({ fp }: { fp: string }) {
  const peers = useStore((s) => s.peers);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);

  async function doReset(): Promise<void> {
    const ok = await confirmDialog({
      title: t('settings.identity.resetTitle'),
      body: [t('settings.identity.resetConsequence', { n: fmt.count(peers.length) })],
      confirmText: t('settings.identity.reset'),
      danger: true,
      testid: 'confirm-reset-identity',
    });
    if (!ok) return;
    setBusy(true);
    try {
      const res = await rpc<{ fingerprint?: string; restart_required?: boolean }>(
        'daemon.reset_identity', {},
      );
      setOpen(false);
      // daemon 说要重启就照实说，不许假装已经生效——`LocalIdentity` 被广播、监听、
      // 控制通道多处持有，热替换不一定做得到。
      toast(
        res && res.restart_required
          ? t('settings.identity.resetRestart')
          : t('settings.identity.resetDone'),
        res && res.restart_required ? 'warn' : 'ok',
      );
    } catch { /* rpc 已 toast */ } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <button
        className="btn small danger" type="button"
        data-testid="settings-identity-reset-open"
        disabled={!fp}
        onClick={() => setOpen(true)}
      >
        {t('settings.identity.reset')}
      </button>
      {open ? (
        <Sheet
          testid="settings-identity-reset-sheet"
          title={t('settings.identity.resetTitle')}
          help={<Help label={t('wiki.discovery')} url={WIKI.fingerprint} testid="settings-identity-reset-help" />}
          onClose={() => setOpen(false)}
          footer={(
            <span className="danger-slot">
              <button
                className="btn small danger" type="button"
                data-testid="settings-identity-reset-confirm"
                disabled={busy}
                onClick={() => void doReset()}
              >
                {t('settings.identity.reset')}
              </button>
            </span>
          )}
        >
          <div className="kv">
            <div className="kv-row">
              <span className="kv-k">{t('settings.identity.fingerprint')}</span>
              <code className="mono">{fp || t('common.dash')}</code>
            </div>
            <div className="kv-row">
              <span className="kv-k">{t('settings.identity.pairedCount')}</span>
              <span data-testid="settings-identity-reset-peers">{fmt.count(peers.length)}</span>
            </div>
          </div>
          <p className="muted small">
            {t('settings.identity.resetConsequence', { n: fmt.count(peers.length) })}
          </p>
        </Sheet>
      ) : null}
    </>
  );
}

// plan M9「开机自启」。**一个开关管两个平台**——daemon 那边也只有一个键
// （`autostart`），macOS 写 LaunchAgent、Windows 写计划任务。
//
// 三态，不是两态：「开着」「关着但能开」「根本开不了」。第三种必须带理由一起
// 显示——一个点不动、又说不出为什么的开关，是这个项目已经反复付过代价的形状。
//
// 不做乐观翻转：daemon 的回包才是权威。这个开关的「事实」是 daemon 探测出来的
// （登录项在不在），不是某个存盘值的回显——先把开关翻过去、注册再失败，界面上就会
// 出现一条系统里根本不存在的登录项。
function StartupRows({ writing, noSettings, onPush }: {
  writing: number;
  noSettings: boolean;
  onPush: (patch: Partial<DaemonSettings>) => Promise<boolean>;
}) {
  const ds = useStore((s) => s.daemonSettings);
  // 三态判定整个搬进 `lib/autostart.ts`（那里有单测）。这里只负责把结论翻成
  // 语料。尤其 `v.enabled` 不等于 `v.supported`：一条已注册的登录项无论当前
  // 形态配不配注册都必须关得掉，否则用户会得到一个点不动的「开着」。
  const v = autostartView(ds);

  return (
    <>
      <SettingRow
        title={t('settings.startup.autostartTitle')}
        help={<Help label={t('wiki.startup')} url={WIKI.startup} testid="settings-startup-help" />}
        control={(
          <Switch
            testid="settings-autostart"
            label={t('settings.startup.autostartTitle')}
            checked={v.on}
            pending={writing > 0}
            disabled={noSettings || !v.enabled}
            onToggle={(want) => void onPush({ autostart: want })}
          />
        )}
      />
      {/* 装好的登录项指向哪里。它与当前程序不一致，就是「登录项还指着一个已经
          被移走的旧版本」——界面上唯一能看见这件事的地方，所以只在开着时显示。 */}
      <div hidden={!v.on || !v.target}>
        <SettingRow
          title={t('settings.startup.target')}
          control={<code className="mono" data-testid="settings-autostart-target">{v.target}</code>}
        />
      </div>
      <p className="muted small" data-testid="settings-autostart-note">
        {v.note === 'unknown'
          ? t('settings.startup.unknown')
          : v.note === 'unsupported'
            ? t('settings.startup.unsupported', { reason: v.reason || t('common.dash') })
            : v.note === 'orphaned'
              ? t('settings.startup.orphaned', { reason: v.reason || t('common.dash') })
              : ''}
      </p>
    </>
  );
}

function MiscCard({ writing, noSettings, onPush }: {
  writing: number;
  noSettings: boolean;
  onPush: (patch: Partial<DaemonSettings>) => Promise<boolean>;
}) {
  const perms = useStore((s) => s.permissions);
  const cfgDir = IS_MAC ? '~/Library/Application Support/AudioHub' : '%APPDATA%\\AudioHub';
  const pending = perms.list.filter((p) => p.status !== 'granted').length;
  // 快捷键的 override 存在模块级 + localStorage，**不进 AppState**（理由见
  // lib/shortcutHost.ts）。订阅它是为了在用户于快捷键面板里改完键位后，这一行的
  // 「几个已自定义」当场跟着变——否则它会一直显示打开面板之前的那个数。
  const overrides = useSyncExternalStore(subscribeShortcuts, getOverrides);

  return (
    <section className="card block" data-testid="settings-misc">
      <BlockTitle
        text={t('settings.misc.title')} url={WIKI.paths}
        label={t('wiki.paths')} testid="settings-misc-help"
      />

      <IdentityRows onPush={onPush} />
      <StartupRows writing={writing} noSettings={noSettings} onPush={onPush} />

      <SettingRow
        title={t('settings.paths.configDir')}
        control={<code className="mono" data-testid="settings-config-dir">{cfgDir}</code>}
      />

      {/* 两枚 `▸`。左边那一列是**值**（几项、几项待授权），不是描述——这一块的
          每一行都遵守 §3.1 的「标签 + 值 + `?`」。 */}
      <SettingRow
        title={t('settings.perm.title')}
        help={<Help label={t('wiki.permissions')} url={WIKI.permissions} testid="settings-misc-perm-help" />}
        control={(
          <div className="field-btn">
            <span className="muted small" data-testid="settings-perm-summary">
              {perms.list.length
                ? t('settings.misc.permsSummary', { n: perms.list.length, pending })
                : t('common.dash')}
            </span>
            <button
              className="btn small" type="button" data-testid="settings-perm-open"
              onClick={() => openPermissionsSheet()}
            >
              {t('common.open')}
            </button>
          </div>
        )}
      />
      <SettingRow
        title={t('shortcuts.sheet.title')}
        help={<Help label={t('wiki.shortcuts')} url={WIKI.shortcuts} testid="settings-misc-shortcuts-help" />}
        control={(
          <div className="field-btn">
            <span className="muted small" data-testid="settings-shortcuts-summary">
              {t('settings.misc.shortcutsSummary', {
                n: SHORTCUT_ACTIONS.length,
                custom: customizedCount(overrides, PLATFORM),
              })}
            </span>
            <button
              className="btn small" type="button" data-testid="settings-shortcuts-open"
              onClick={() => openShortcutSheet()}
            >
              {t('common.open')}
            </button>
          </div>
        )}
      />
    </section>
  );
}

// ---------------------------------------------------------------- 页面

export function SettingsView() {
  const [writing, setWriting] = useState(0);

  // 全部写操作走同一条路：回包就是新的权威值，不做乐观翻转——开关先翻过去、
  // 请求再失败的话，界面显示的是一个 daemon 从没接受过的设置。
  //
  // 返回**成功与否**而不是让异常逃出去：开关类调用点是 `void onPush(...)`，异常逃
  // 出去就成了一次 unhandled rejection；而名称保存要知道成不成才决定清不清草稿，
  // 失败时草稿必须留着（否则用户刚敲的字连同错误一起消失）。rpc 层已经 toast 过。
  const pushSetting = async (patch: Partial<DaemonSettings>): Promise<boolean> => {
    setWriting((n) => n + 1);
    try {
      await applySettings(patch);
      return true;
    } catch {
      return false;
    } finally {
      setWriting((n) => n - 1);
    }
  };

  // 没有 settings.* 的旧服务：开关点了也不会有任何效果，禁用比假装能用诚实。
  const noSettings = useStore((s) => s.settingsSupported) === false;

  return (
    <>
      <ModeCard />
      <ModeOptionsCard writing={writing} noSettings={noSettings} onPush={pushSetting} />
      <NetworkCard writing={writing} noSettings={noSettings} onPush={pushSetting} />
      <MiscCard writing={writing} noSettings={noSettings} onPush={pushSetting} />
      <AboutCard />
    </>
  );
}

/**
 * 关于。
 *
 * 为什么需要它：左上角的品牌区（logo + 字标 + tagline）已经删掉，标识改由背景水印
 * 承担——而水印**读不出名字**，这是它作为水印的全部意义。于是「这个 App 叫什么、
 * 是哪个版本」在界面里没有了任何落点。系统那一层并不能兜住：macOS 还有菜单栏与
 * Dock，Windows 一旦按 docs/design-ui-chrome.md §3 去掉系统顶栏，连窗口标题都不剩，
 * 只剩任务栏悬停提示。这一块就是那个常驻落点，也是用户报 bug 时能被要求截图的地方。
 *
 * 为什么放在整页**最后**：它是唯一一块不改变任何行为的区块，而「关于」在页尾是
 * macOS 系统设置、VS Code、iOS 一致的位置——用户会往那儿翻。前面每一块都是可操作
 * 的设置，把一块只读的品牌信息插进它们中间只会打断阅读节奏。
 * 「杂项」因此排在它**上面**（用户 2026-08-10 第 3 条「放在关于旁边」），而不是下面。
 *
 * 它与「本机身份」不重合：那几行说的是**这台机器**（名称、指纹），配对时要核对的
 * 东西；这一块说的是**这个程序**。两者恰好都叫「身份」，但没有一个字段是共用的。
 */
function AboutCard() {
  // 拿不到版本时显示破折号，不显示 '0.0.0' 之类编出来的号——理由见 lib/appInfo.ts。
  const version = appVersion();

  return (
    <section className="card block" data-testid="settings-about">
      <h3 className="block-title">{t('settings.about.title')}</h3>
      <div className="about-row">
        <span className="about-mark" aria-hidden="true"><RawIcon name="wave" /></span>
        <div className="about-text">
          <strong data-testid="settings-about-name">{t('app.name')}</strong>
          <span>{t('app.tagline')}</span>
        </div>
        <code className="about-version" data-testid="settings-about-version">
          {version != null ? t('settings.about.version', { version }) : t('common.dash')}
        </code>
      </div>
      {/* 文档的常驻入口。散落在各区块旁的深链解释的是**那一块**；这一条是目录
          本身——用户想「从头读一遍」时，不该只能靠碰巧点开某个深链再往回爬。 */}
      <p className="muted small">
        <ExtLink text={t('wiki.open')} url={WIKI.home} testid="settings-about-wiki" />
      </p>
    </section>
  );
}

export type { AppState };
