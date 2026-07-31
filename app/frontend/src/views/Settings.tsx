// 设置：模式镜像、端口只读、延迟/质量档、虚拟设备开关与设备清单、配置目录展示。
//
// 模式**选择器本身**不在这一页（plan §7.1：它决定整个主面板的含义，藏进设置页
// 等于把「那排开关为什么没了」的答案藏起来）。这里只放一面只读的镜子 + 去主面板
// 的入口，避免同一个全局状态出现两个可点的控件、两处 pending 态。

import { useEffect, useState } from 'react';
import type { ReactNode } from 'react';
import { Icon } from '../components/Icon';
import { ExtLink, Segmented, Switch } from '../components/Controls';
import { PermissionRow } from '../components/PermissionRow';
import { toast } from '../components/Toasts';
import { openExternal } from '../lib/external';
import { bridgeCatalog, vendors } from '../lib/bridge';
import { fmt, IS_MAC } from '../lib/fmt';
import {
  getWebUiStatus, inferredStatus, setWebUiSettings, webPortValid, webUiSupported,
  WEB_PORT_MAX, WEB_PORT_MIN,
} from '../lib/webui';
import type { WebUiPatch, WebUiStatus } from '../lib/webui';
import { t, joinPhrases } from '../i18n';
import type { MsgKey } from '../i18n';
import { actions, useStore } from '../state/store';
import type { AppState } from '../state/store';
import { actionOf } from '../state/permissions';
import type { PermissionState } from '../state/permissions';
import {
  halState, requestedMode, effectiveMode, isModeB, modeDowngraded, deviceStateLabel,
} from '../state/mode';
import { applySettings, refreshPermissions, rpc } from '../state/connection';

const MODE_LABEL_KEY: Record<string, MsgKey> = { a: 'mode.a.label', b: 'mode.b.label' };

function SettingRow({ title, desc, control, badge }: {
  title: string; desc: string; control: ReactNode; badge?: string;
}) {
  return (
    <div className="setting-row">
      <div className="setting-text">
        <div className="setting-title">
          {title}
          {badge ? <span className="tag warn">{badge}</span> : null}
        </div>
        <p className="setting-desc">{desc}</p>
      </div>
      <div className="setting-ctl">{control}</div>
    </div>
  );
}

// 授权门只在「必需权限没齐」时出现，可选的系统音频录制因此可能永远等不到那扇门。
// 这张卡就是它进门之后唯一的入口——也是跳过授权的用户回头补授权的地方。
function PermissionsCard() {
  const perms = useStore((s) => s.permissions);
  const conn = useStore((s) => s.conn);

  async function onAction(p: PermissionState) {
    if (actionOf(p) === 'request') {
      actions.setPermissionBusy(p.id);
      try {
        const res = await rpc('daemon.request_permission', { id: p.id });
        await refreshPermissions({ force: true, seed: res });
      } catch { /* rpc 已 toast */ } finally {
        actions.setPermissionBusy(null);
      }
      return;
    }
    if (p.settingsUrl) {
      void openExternal(p.settingsUrl);
      if (p.manual) toast(t('perm.settingsFallback', { manual: p.manual }), 'info');
    } else {
      toast(p.manual ? t('perm.openManual', { manual: p.manual }) : t('perm.noSettingsUrl'), 'warn');
    }
  }

  const note = perms.list.length ? ''
    : perms.supported === false ? t('settings.perm.unsupported')
      : perms.error ? t('settings.perm.error', { message: perms.error })
        : conn === 'online' ? t('settings.perm.probing') : t('settings.perm.offline');

  return (
    <section className="card block" data-testid="settings-permissions">
      <h3 className="block-title">{t('settings.perm.title')}</h3>
      <p className="muted">{t('settings.perm.desc')}</p>
      <div className="perm-list" data-testid="settings-perm-list" hidden={perms.list.length === 0}>
        {perms.list.map((p) => (
          <PermissionRow key={p.id} perm={p} prefix="settings-perm" busy={perms.busy} onAction={onAction} />
        ))}
      </div>
      <p className="muted small" data-testid="settings-perm-note" hidden={!note}>{note}</p>
      <button
        className="btn small" type="button" data-testid="settings-perm-recheck"
        onClick={() => void refreshPermissions({ force: true })}
      >
        {t('settings.perm.recheck')}
      </button>
    </section>
  );
}

function ModeMirrorCard() {
  const s = useStore();
  const eff = effectiveMode(s);
  const hs = halState(s.daemon);
  const note = modeDowngraded(s)
    ? t('settings.mode.downgraded', { mode: t(MODE_LABEL_KEY[requestedMode(s)]), hint: hs.text })
    : hs.text;

  return (
    <section className="card block" data-testid="settings-mode">
      <h3 className="block-title">{t('settings.mode.title')}</h3>
      <SettingRow
        title={t('settings.mode.rowTitle')}
        desc={t('settings.mode.rowDesc')}
        control={(
          <div className="field-btn">
            <span className={`mode-mirror mode-${eff}`} data-testid="settings-mode-current">
              {t(MODE_LABEL_KEY[eff])}
            </span>
            <button
              className="btn small" type="button" data-testid="settings-mode-goto"
              onClick={() => actions.navigate('peers')}
            >
              {t('settings.mode.goto')}
            </button>
          </div>
        )}
      />
      <p className={`muted small tone-${hs.tone}`} data-testid="settings-mode-note">{note}</p>
    </section>
  );
}

function DeviceInventory() {
  const s = useStore();
  const ds = s.daemonSettings;
  const hal = s.daemon ? s.daemon.hal : null;
  const list = hal && Array.isArray(hal.devices) ? hal.devices : [];
  const cap = ds ? ds.hal_capacity : (hal ? 16 : 0);
  const used = ds ? ds.hal_used : list.length;

  const note = list.length
    ? t('settings.devices.noteHas')
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

// 虚拟声卡桥接（spec-m4c §B / plan §7.1）：这里只报「检测到了什么」并给官网链接，
// 真正的选择在主面板的对端卡片上——桥接目标是**按对端**决定的。
// 冻结的口径：不代装、不主动引导安装，所以这里没有任何安装按钮或催促文案。
function BridgeCard() {
  const daemon = useStore((s) => s.daemon);
  const hidden = useStore(isModeB);
  const catalog = bridgeCatalog(daemon);

  return (
    <section className="card block" data-testid="settings-bridge" hidden={hidden}>
      <h3 className="block-title">{t('settings.bridge.title')}</h3>
      <p className="muted">{t('settings.bridge.desc')}</p>
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
      <p className="muted small">{t('settings.bridge.foot')}</p>
      <div className="bridge-links" data-testid="settings-bridge-links">
        {vendors().map((v) => (
          <ExtLink key={v.id} text={v.label} url={v.url} testid={`settings-bridge-link-${v.id}`} />
        ))}
      </div>
    </section>
  );
}

// 网页访问（plan §7.5）。三个选项落在 **App 自己的** <config>/webui.json：daemon
// 是音频与网络引擎，不该为「App 要不要开个网页端口」长一个字段。
//
// 这一块的重点是那条警告。「仅允许本机」关掉之后发生的事不是抽象的「安全风险」，
// 而是：整个局域网都能打开这套界面并完整操作本机音频，且 /ipc-endpoint 会把 IPC
// 令牌明文交出去——所以文案逐条说出来，而不是写一句「请注意安全」。
function WebAccessCard() {
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
    <section className="card block" data-testid="settings-web">
      <h3 className="block-title">{t('settings.web.title')}</h3>
      <p className="muted">{t('settings.web.desc')}</p>

      <SettingRow
        title={t('settings.web.enabledTitle')}
        desc={t('settings.web.enabledDesc')}
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
        desc={t('settings.web.portDesc')}
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
        desc={t('settings.web.localOnlyDesc')}
        badge={locked ? t('settings.web.localOnlyBadge') : undefined}
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
      <p className="muted small" data-testid="settings-web-local-only-note" hidden={!locked}>
        {t('settings.web.localOnlyLocked')}
      </p>

      {/* 关掉「仅允许本机」= 把一个无鉴权的控制界面连同 IPC 令牌一起交给局域网。
          plan §7.5 要求这条警告存在，且该选项永不为默认值。 */}
      <div className="web-warn" data-testid="settings-web-warning" hidden={localOnly}>
        <strong className="web-warn-title">{t('settings.web.warnTitle')}</strong>
        <p className="web-warn-body">{t('settings.web.warnBody')}</p>
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
                {/* 实测（本机 ↔ 30-win）：页面与令牌都能过局域网，但 daemon 的 IPC
                    只监听回环，所以远端页面连不上服务。不写出来，用户只会看到一个
                    永远停在「连接中」的界面，而怀疑的是自己的网络。 */}
                <p className="muted small" data-testid="settings-web-lan-note">{t('settings.web.lanIpcNote')}</p>
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
          t('settings.web.quitNote'),
        ])}
      </p>
    </section>
  );
}

export function SettingsView() {
  const s = useStore();
  const [writing, setWriting] = useState(0);

  // daemon 的值优先，拿不到才回落到本地缓存——反过来会让界面显示一个 daemon
  // 根本不认的档位。
  function settingValue(key: 'latency' | 'quality', dft: string): string {
    const d = s.daemonSettings;
    const fromDaemon = d && typeof d[key] === 'string' ? d[key] : '';
    if (fromDaemon) return fromDaemon;
    const local = s.settings[key];
    return typeof local === 'string' && local ? local : dft;
  }

  // 全部写操作走同一条路：回包就是新的权威值，不做乐观翻转——开关先翻过去、
  // 请求再失败的话，界面显示的是一个 daemon 从没接受过的设置。
  async function pushSetting(patch: Parameters<typeof applySettings>[0]): Promise<void> {
    setWriting((n) => n + 1);
    try {
      await applySettings(patch);
    } catch { /* rpc 已 toast */ } finally {
      setWriting((n) => n - 1);
    }
  }

  const ds = s.daemonSettings;
  // 没有 settings.* 的旧服务：开关点了也不会有任何效果，禁用比假装能用诚实。
  const noSettings = s.settingsSupported === false;
  const cfgDir = IS_MAC ? '~/Library/Application Support/AudioHub' : '%APPDATA%\\AudioHub';

  return (
    <>
      <PermissionsCard />
      <ModeMirrorCard />

      <section className="card block">
        <h3 className="block-title">{t('settings.net.title')}</h3>
        <SettingRow
          title={t('settings.net.controlPort')}
          desc={t('settings.net.controlPortDesc')}
          badge={t('settings.net.controlPortBadge')}
          control={(
            <code className="mono" data-testid="settings-port">
              {s.daemon?.control_port != null ? String(s.daemon.control_port) : t('common.dash')}
            </code>
          )}
        />
        <SettingRow
          title={t('settings.net.ipcPort')}
          desc={t('settings.net.ipcPortDesc')}
          control={(
            <code className="mono" data-testid="settings-ipc-port">
              {s.endpoint ? String(s.endpoint.port) : t('common.dash')}
            </code>
          )}
        />
      </section>

      <WebAccessCard />

      {/* 延迟/质量：**真的下发并落盘**（settings.json），但媒体面还没读它。角标写
          「已保存 · 暂未生效」而不是隐藏：藏起来会让下一版接上时用户以为是新功能。 */}
      <section className="card block">
        <h3 className="block-title">{t('settings.transport.title')}</h3>
        <SettingRow
          title={t('settings.transport.latency')}
          desc={t('settings.transport.latencyDesc')}
          badge={t('settings.transport.savedBadge')}
          control={(
            <Segmented
              testid="settings-latency"
              value={settingValue('latency', 'min')}
              onSelect={(v) => pushSetting({ latency: v })}
              options={[
                { value: 'min', label: t('settings.transport.latencyMin') },
                { value: 'auto', label: t('settings.transport.auto') },
              ]}
            />
          )}
        />
        <SettingRow
          title={t('settings.transport.quality')}
          desc={t('settings.transport.qualityDesc')}
          badge={t('settings.transport.savedBadge')}
          control={(
            <Segmented
              testid="settings-quality"
              value={settingValue('quality', 'auto')}
              onSelect={(v) => pushSetting({ quality: v })}
              options={[
                { value: 'pcm', label: t('settings.transport.qualityPcm') },
                { value: 'auto', label: t('settings.transport.auto') },
              ]}
            />
          )}
        />
        <p className="muted small">{t('settings.transport.note')}</p>
      </section>

      <BridgeCard />

      <section className="card block" data-testid="settings-devices">
        <h3 className="block-title">{t('settings.devices.title')}</h3>
        <SettingRow
          title={t('settings.devices.removeTitle')}
          desc={t('settings.devices.removeDesc')}
          control={(
            <Switch
              testid="settings-remove-virtual"
              label={t('settings.devices.removeTitle')}
              checked={ds ? !!ds.remove_virtual_on_disconnect : s.settings.removeVirtual}
              pending={writing > 0}
              disabled={noSettings}
              onToggle={(want) => void pushSetting({ remove_virtual_on_disconnect: want })}
            />
          )}
        />
        <SettingRow
          title={t('settings.devices.markOfflineTitle')}
          desc={t('settings.devices.markOfflineDesc')}
          control={(
            <Switch
              testid="settings-mark-offline"
              label={t('settings.devices.markOfflineTitle')}
              checked={ds ? !!ds.mark_offline_devices : true}
              pending={writing > 0}
              disabled={noSettings}
              onToggle={(want) => void pushSetting({ mark_offline_devices: want })}
            />
          )}
        />
        <DeviceInventory />
      </section>

      <section className="card block">
        <h3 className="block-title">{t('settings.paths.title')}</h3>
        <SettingRow
          title={t('settings.paths.configDir')}
          desc={t('settings.paths.configDirDesc')}
          control={<code className="mono" data-testid="settings-config-dir">{cfgDir}</code>}
        />
      </section>
    </>
  );
}

export type { AppState };
