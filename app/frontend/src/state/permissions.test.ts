import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { setLocale } from '../i18n';
import {
  normalizeOne, permissionName, permissionUnknownNote, permissionWhy,
} from './permissions';

beforeAll(() => vi.stubGlobal('document', { documentElement: { lang: '' } }));
afterEach(() => setLocale('zh-CN'));
afterAll(() => vi.unstubAllGlobals());

describe('permission copy follows the UI locale', () => {
  it('does not render daemon-authored prose for known permission ids', () => {
    setLocale('en-US');
    const permission = normalizeOne({
      kind: 'microphone',
      name: '服务端权限名',
      why: '服务端中文说明',
      note: '已授权。',
      status: 'granted',
    }, null);

    expect(permissionName(permission)).toBe('Microphone');
    expect(permissionWhy(permission)).toContain("this device's mic");
    expect(permissionWhy(permission)).not.toContain('服务端');
    // The status tag already says Granted; a server-localized duplicate note
    // must not leak into an otherwise English sheet.
    expect(permission.note).toBeNull();
  });

  it('resolves known copy again after a live locale switch', () => {
    setLocale('en-US');
    const permission = normalizeOne({ kind: 'local_network', status: 'unknown' }, null);
    expect(permissionName(permission)).toBe('Local Network');
    expect(permissionUnknownNote(permission)).toContain('first use');

    setLocale('zh-CN');
    expect(permissionName(permission)).toBe('本地网络');
    expect(permissionUnknownNote(permission)).toContain('首次');
  });

  it('keeps service text for unknown future permissions', () => {
    setLocale('en-US');
    const permission = normalizeOne({
      id: 'future_permission',
      name: 'Future permission',
      why: 'Needed by a future feature',
      note: 'Service-specific detail',
      status: 'unknown',
    }, null);

    expect(permission.inCatalog).toBe(false);
    expect(permissionName(permission)).toBe('Future permission');
    expect(permissionWhy(permission)).toBe('Needed by a future feature');
    expect(permission.note).toBe('Service-specific detail');
  });
});
