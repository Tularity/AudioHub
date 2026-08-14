import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { isUnknownMethod, sanitizeDaemonSettings } from './airplay';

const SHARE_PROTOCOLS_SRC = readFileSync(
  fileURLToPath(new URL('../views/ShareProtocols.tsx', import.meta.url)),
  'utf8',
);

describe('AirPlay IPC 边界', () => {
  it('只写密码永不进入 DaemonSettings', () => {
    const raw = {
      airplay_enabled: true,
      airplay_password_set: true,
      airplay_password: 'secret',
    };
    const clean = sanitizeDaemonSettings(raw) as Record<string, unknown>;
    expect(clean).toEqual({
      airplay_enabled: true,
      airplay_password_set: true,
    });
    expect(raw.airplay_password).toBe('secret');
  });

  it('拒绝非对象设置回包', () => {
    expect(sanitizeDaemonSettings(null)).toBeNull();
    expect(sanitizeDaemonSettings([])).toBeNull();
    expect(sanitizeDaemonSettings('bad')).toBeNull();
  });

  it('只把旧服务的 unknown method 当作能力缺席', () => {
    expect(isUnknownMethod(new Error("unknown method 'airplay.sessions.list'")))
      .toBe(true);
    expect(isUnknownMethod(new Error('request timeout'))).toBe(false);
  });
});

describe('AirPlay 设置界面边界', () => {
  it('关闭时不挂载接收设置、状态或二级菜单', () => {
    expect(SHARE_PROTOCOLS_SRC).toContain('{enabled ? (');
    expect(SHARE_PROTOCOLS_SRC)
      .toContain('{enabled && settingsOpen && settings ? (');
  });

  it('广播名称和密码只存在于接收设置 Sheet 中', () => {
    const sheetStart = SHARE_PROTOCOLS_SRC.indexOf('function AirPlaySettingsSheet');
    const cardStart = SHARE_PROTOCOLS_SRC.indexOf('function AirPlayCard');
    expect(sheetStart).toBeGreaterThan(0);
    expect(cardStart).toBeGreaterThan(sheetStart);
    const sheet = SHARE_PROTOCOLS_SRC.slice(sheetStart, cardStart);
    const card = SHARE_PROTOCOLS_SRC.slice(cardStart);
    expect(sheet).toContain('share-proto-airplay-name');
    expect(sheet).toContain('share-proto-airplay-password');
    expect(card).not.toContain('data-testid="share-proto-airplay-name"');
    expect(card).not.toContain('data-testid="share-proto-airplay-password"');
  });

  it('名称与密码共用 Sheet 底部的一次保存', () => {
    const sheetStart = SHARE_PROTOCOLS_SRC.indexOf('function AirPlaySettingsSheet');
    const cardStart = SHARE_PROTOCOLS_SRC.indexOf('function AirPlayCard');
    const sheet = SHARE_PROTOCOLS_SRC.slice(sheetStart, cardStart);
    expect(sheet).toContain('data-testid="share-proto-airplay-save"');
    expect(sheet).toContain('primaryAction={(');
    expect(sheet).toContain("dismissLabel={t('common.cancel')}");
    expect(sheet).not.toContain('share-proto-airplay-name-save');
    expect(sheet).not.toContain('share-proto-airplay-password-save');
  });

  it('不再提供 AirPlay 音频去向选择', () => {
    expect(SHARE_PROTOCOLS_SRC).not.toContain('airplay_route');
    expect(SHARE_PROTOCOLS_SRC).not.toContain('share.proto.airplay.route');
  });
});
