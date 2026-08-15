// 共享协议：外部 AirPlay 发送端把音频送进本机直接播放。
// 页面只渲染 settings.* 与 airplay.sessions.list 的权威回包，不做乐观翻转。

import { useEffect, useState } from 'react';
import { BlockTitle, Help, SettingRow, Switch } from '../components/Controls';
import { Icon } from '../components/Icon';
import { Sheet } from '../components/Sheet';
import { WIKI } from '../lib/external';
import { fmt } from '../lib/fmt';
import { joinPhrases, t } from '../i18n';
import type { MsgKey } from '../i18n';
import type {
  AirPlayArtwork, AirPlaySessionInfo, DaemonSettings, DaemonSettingsPatch,
} from '../ipc/types';
import { applySettings, rpc } from '../state/connection';
import { useShallow, useStore } from '../state/store';

function stateText(
  settings: DaemonSettings | null,
  supported: boolean | null,
): string {
  if (supported == null) return t('share.proto.airplay.stateLoading');
  if (!supported) return t('share.proto.airplay.stateUnsupported');
  const ports = [
    settings?.airplay_airplay2_port
      ? t('share.proto.airplay.airplay2Port', {
        port: settings.airplay_airplay2_port,
      })
      : null,
  ];
  if (settings?.airplay_error) {
    return joinPhrases([
      t('share.proto.airplay.stateError'),
      ...ports,
    ]);
  }
  if (settings?.airplay_listening) {
    return joinPhrases([
      t('share.proto.airplay.stateListening'),
      ...ports,
      settings.airplay_warning
        ? t('share.proto.airplay.stateWarning')
        : null,
    ]);
  }
  return t('share.proto.airplay.stateStarting');
}

function AirPlaySettingsSheet({
  settings, effectiveName, writing, onPush, onClose,
}: {
  settings: DaemonSettings;
  effectiveName: string;
  writing: number;
  onPush: (patch: DaemonSettingsPatch) => Promise<boolean>;
  onClose: () => void;
}) {
  // 这扇 Sheet 是一个设置事务：两项输入先留在本地，底部「保存」一次提交。
  // 关闭 / Esc / 遮罩都只是取消，不再让字段旁各自发一次 settings.set。
  const savedName = settings.airplay_name || '';
  const [nameDraft, setNameDraft] = useState(savedName);
  // 密码只活在组件里。daemon 只回 password_set，store 与快照都没有密码槽位。
  const [passwordDraft, setPasswordDraft] = useState('');
  const [clearPassword, setClearPassword] = useState(false);

  const normalizedName = nameDraft.trim();
  const nameDirty = normalizedName !== savedName;
  const passwordDirty = passwordDraft.length > 0 || clearPassword;
  const dirty = nameDirty || passwordDirty;
  const disabled = writing > 0;

  async function save(): Promise<void> {
    if (disabled || !dirty) return;
    const patch: DaemonSettingsPatch = {};
    if (nameDirty) patch.airplay_name = normalizedName;
    if (passwordDraft) patch.airplay_password = passwordDraft;
    else if (clearPassword) patch.airplay_password = '';
    if (await onPush(patch)) onClose();
  }

  return (
    <Sheet
      testid="share-proto-airplay-settings-sheet"
      title={t('share.proto.airplay.settingsTitle')}
      help={(
        <Help
          label={t('wiki.shareProtocols')}
          url={WIKI.airplay}
          testid="share-proto-airplay-settings-help"
        />
      )}
      dismissLabel={t('common.cancel')}
      dismissDisabled={disabled}
      onClose={onClose}
      primaryAction={(
        <button
          className="btn primary" type="button"
          data-testid="share-proto-airplay-save"
          disabled={disabled || !dirty}
          onClick={() => void save()}
        >
          {t('common.save')}
        </button>
      )}
      wide
    >
      <SettingRow
        title={t('share.proto.airplay.name')}
        note={effectiveName
          ? t('share.proto.airplay.effectiveName', { name: effectiveName })
          : undefined}
        control={(
          <div className="proto-field sheet-field-control">
            <input
              className="input proto-input"
              data-testid="share-proto-airplay-name"
              disabled={disabled}
              value={nameDraft}
              maxLength={48}
              placeholder={effectiveName || t('share.proto.airplay.namePlaceholder')}
              onChange={(event) => setNameDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter' && dirty) void save();
              }}
            />
            <button
              className="btn small" type="button"
              data-testid="share-proto-airplay-name-default"
              disabled={disabled || normalizedName === ''}
              onClick={() => setNameDraft('')}
            >
              {t('share.proto.airplay.nameDefault')}
            </button>
          </div>
        )}
      />

      <SettingRow
        title={t('share.proto.airplay.password')}
        note={clearPassword ? t('share.proto.airplay.passwordWillClear') : undefined}
        control={(
          <div className="proto-field sheet-field-control">
            <input
              className="input proto-input"
              type="password"
              autoComplete="new-password"
              data-testid="share-proto-airplay-password"
              disabled={disabled}
              value={passwordDraft}
              maxLength={128}
              placeholder={settings.airplay_password_set
                ? t('share.proto.airplay.passwordPlaceholderSet')
                : t('share.proto.airplay.passwordPlaceholderUnset')}
              onChange={(event) => {
                setPasswordDraft(event.target.value);
                setClearPassword(false);
              }}
              onKeyDown={(event) => {
                if (event.key === 'Enter' && dirty) void save();
              }}
            />
            <button
              className={`btn small${clearPassword ? ' is-staged' : ''}`} type="button"
              data-testid="share-proto-airplay-password-clear"
              aria-pressed={clearPassword}
              disabled={disabled || (!settings.airplay_password_set && !passwordDraft)}
              onClick={() => {
                setPasswordDraft('');
                setClearPassword((value) => (settings.airplay_password_set ? !value : false));
              }}
            >
              {t('common.clear')}
            </button>
          </div>
        )}
      />

    </Sheet>
  );
}

function AirPlayCard() {
  const daemonName = useStore((s) => s.daemon?.name || '');
  const settings = useStore((s) => s.daemonSettings);
  const settingsSupported = useStore((s) => s.settingsSupported);
  const supported = settings == null && settingsSupported !== false
    ? null
    : settingsSupported === true
      && typeof settings?.airplay_enabled === 'boolean';
  const [writing, setWriting] = useState(0);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const enabled = supported === true && !!settings?.airplay_enabled;

  // 开关关闭后 Sheet 必须真正卸载：除了避免再次开启时自动弹回，
  // 也让密码与未保存名称随子组件一起销毁，不在内存里跨开关存留。
  useEffect(() => {
    if (!enabled) setSettingsOpen(false);
  }, [enabled]);

  async function push(patch: DaemonSettingsPatch): Promise<boolean> {
    setWriting((count) => count + 1);
    try {
      await applySettings(patch);
      return true;
    } catch {
      return false;
    } finally {
      setWriting((count) => count - 1);
    }
  }

  const effectiveName = settings?.airplay_effective_name || daemonName;
  const status = stateText(settings, supported);
  const stateTone = settings?.airplay_error || settings?.airplay_warning
    ? ' error'
    : settings?.airplay_listening ? ' listening' : '';
  const summary = joinPhrases([
    effectiveName || t('common.dash'),
    settings?.airplay_password_set
      ? t('share.proto.airplay.passwordSet')
      : t('share.proto.airplay.passwordUnset'),
  ]);

  return (
    <section className="card block" data-testid="share-proto-airplay">
      <BlockTitle
        text={t('share.proto.airplay.title')}
        url={WIKI.airplay}
        label={t('wiki.shareProtocols')}
        testid="share-proto-airplay-help"
      />

      <SettingRow
        title={t('share.proto.airplay.enable')}
        note={t('share.proto.airplay.consequence')}
        control={(
          <Switch
            testid="share-proto-airplay-toggle"
            label={t('share.proto.airplay.enable')}
            checked={!!settings?.airplay_enabled}
            pending={writing > 0}
            disabled={supported !== true}
            onToggle={(want) => {
              if (!want) setSettingsOpen(false);
              void push({ airplay_enabled: want });
            }}
          />
        )}
      />

      {enabled ? (
        <>
          <SettingRow
            title={t('share.proto.airplay.settings')}
            control={(
              <div className="field-btn">
                <span
                  className="muted small proto-settings-summary"
                  data-testid="share-proto-airplay-settings-summary"
                  title={summary}
                >
                  {summary}
                </span>
                <button
                  className="btn small"
                  type="button"
                  data-testid="share-proto-airplay-settings-open"
                  disabled={writing > 0}
                  onClick={() => setSettingsOpen(true)}
                >
                  {t('common.open')}
                </button>
              </div>
            )}
          />

          <SettingRow
            title={t('share.proto.airplay.state')}
            control={(
              <span
                className={`proto-state${stateTone}`}
                data-testid="share-proto-airplay-state"
              >
                {status}
              </span>
            )}
          />
        </>
      ) : null}

      {enabled && settingsOpen && settings ? (
        <AirPlaySettingsSheet
          settings={settings}
          effectiveName={effectiveName}
          writing={writing}
          onPush={push}
          onClose={() => setSettingsOpen(false)}
        />
      ) : null}
    </section>
  );
}

function sessionFormat(session: AirPlaySessionInfo): string {
  return joinPhrases([
    session.sample_rate && session.sample_rate > 0
      ? t('share.proto.active.rate', { rate: fmt.int(session.sample_rate) })
      : null,
    session.channels && session.channels > 0
      ? t('share.proto.active.channels', { channels: fmt.int(session.channels) })
      : null,
  ]);
}

function ActiveSession({ session, index }: {
  session: AirPlaySessionInfo; index: number;
}) {
  const [artworkUrl, setArtworkUrl] = useState<string | null>(null);
  const title = session.title || t('share.proto.active.unknownTrack');
  const peer = session.peer || t('share.proto.active.unknownPeer');
  const protocol = session.protocol || t('share.proto.active.protocolUnknown');
  const format = sessionFormat(session);
  const media = joinPhrases([session.artist || null, session.album || null]);
  const connected = typeof session.connected_ms === 'number'
    ? fmt.uptime(session.connected_ms / 1000)
    : null;

  useEffect(() => {
    let cancelled = false;
    const sessionId = session.id;
    const revision = session.artwork_revision;
    const contentType = session.artwork_content_type?.toLowerCase() || null;
    if (
      typeof sessionId !== 'number'
      || !Number.isSafeInteger(sessionId)
      || typeof revision !== 'number'
      || !Number.isSafeInteger(revision)
      || !['image/jpeg', 'image/png'].includes(contentType || '')
    ) {
      setArtworkUrl(null);
      return () => { cancelled = true; };
    }

    setArtworkUrl(null);
    void rpc<AirPlayArtwork | null>(
      'airplay.artwork.get',
      { session_id: sessionId, revision },
      { silent: true, timeoutMs: 5000 },
    ).then((artwork) => {
      if (
        cancelled
        || !artwork
        || artwork.session_id !== sessionId
        || artwork.revision !== revision
        || artwork.content_type.toLowerCase() !== contentType
        || !/^[A-Za-z0-9+/]*={0,2}$/.test(artwork.data_base64)
      ) return;
      setArtworkUrl(`data:${contentType};base64,${artwork.data_base64}`);
    }).catch(() => {});
    return () => { cancelled = true; };
  }, [session.id, session.artwork_revision, session.artwork_content_type]);

  return (
    <li
      className="proto-session"
      data-testid={`share-proto-session-${session.id ?? index}`}
      // Drives the 70ms entrance stagger in `.proto-session`.
      style={{ ['--i' as string]: String(index) } as React.CSSProperties}
    >
      {artworkUrl ? (
        <img
          className="proto-session-artwork"
          src={artworkUrl}
          alt=""
          data-testid={`share-proto-artwork-${session.id ?? index}`}
        />
      ) : null}
      <div className="proto-session-body">
        <div className="proto-session-head">
          <strong>{title}</strong>
          <span className="tag">{protocol}</span>
          <span className={`tag ${session.paused ? 'warn' : 'ok'}`}>
            {session.paused
              ? t('share.proto.active.paused')
              : t('share.proto.active.playing')}
          </span>
        </div>
        {media ? <div className="proto-session-media">{media}</div> : null}
        <div className="proto-session-meta">
          <span>{t('share.proto.active.peer', { peer })}</span>
          {format ? <span>{format}</span> : null}
          {connected
            ? <span>{t('share.proto.active.connected', { time: connected })}</span>
            : null}
        </div>
      </div>
    </li>
  );
}

function ActiveCard() {
  const sessions = useShallow((s) => s.airplaySessions);

  return (
    <section className="card block" data-testid="share-proto-active">
      <h3 className="block-title">{t('share.proto.active.title')}</h3>
      {sessions.length === 0 ? (
        <div className="proto-empty" data-testid="share-proto-active-empty">
          <Icon name="cable" cls="ico" />
          <span>{t('share.proto.active.empty')}</span>
        </div>
      ) : (
        <ul className="proto-session-list" data-testid="share-proto-active-list">
          {sessions.map((session, index) => (
            <ActiveSession
              key={String(session.id ?? `${session.peer || 'unknown'}-${index}`)}
              session={session}
              index={index}
            />
          ))}
        </ul>
      )}
    </section>
  );
}

const PLANNED: MsgKey[] = [
  'share.proto.planned.cast',
  'share.proto.planned.dlna',
  'share.proto.planned.rtsp',
];

function PlannedCard() {
  return (
    <section className="card block" data-testid="share-proto-planned">
      <div className="title-row">
        <h3 className="block-title">{t('share.proto.planned.title')}</h3>
        <Help
          label={t('wiki.shareProtocols')}
          url={WIKI.shareProtocols}
          testid="share-proto-planned-help"
        />
      </div>
      <ul className="proto-list">
        {PLANNED.map((key) => (
          <li key={key} className="proto-item">
            <span className="proto-name">{t(key)}</span>
            <span className="tag">{t('share.proto.planned.tag')}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}

export function ShareProtocolsView() {
  return (
    <>
      <AirPlayCard />
      <ActiveCard />
      <PlannedCard />
    </>
  );
}
