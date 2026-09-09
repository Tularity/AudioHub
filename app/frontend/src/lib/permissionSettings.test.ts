import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { PermissionState } from '../state/permissions';

const mocks = vi.hoisted(() => ({ native: true, invoke: vi.fn(), toast: vi.fn() }));
vi.mock('../ipc/endpoint', () => ({ isTauri: () => mocks.native, tauriInvoke: mocks.invoke }));
vi.mock('../components/Toasts', () => ({ toast: mocks.toast }));
vi.mock('../state/permissions', () => ({ permissionManual: () => 'Privacy & Security > Microphone' }));
vi.mock('../i18n', () => ({ t: (key: string) => key }));
import { openPermissionSettings } from './permissionSettings';

beforeEach(() => { mocks.native = true; mocks.invoke.mockReset(); mocks.toast.mockReset(); });

describe('permission settings are native actions', () => {
  const permission = { id: 'microphone', settingsUrl: 'file:///untrusted' } as PermissionState;
  it('passes only the known permission id, never an arbitrary URL', async () => {
    mocks.invoke.mockResolvedValue(true);
    await openPermissionSettings(permission);
    expect(mocks.invoke).toHaveBeenCalledWith('open_permission_settings', { id: 'microphone' });
    expect(mocks.toast).not.toHaveBeenCalled();
  });
  it('preserves a manual navigation hint when only the parent pane is supported', async () => {
    mocks.invoke.mockResolvedValue(false);
    await openPermissionSettings(permission);
    expect(mocks.toast).toHaveBeenCalledWith('perm.settingsFallback', 'info');
  });
  it('reports launch errors without replacing the clipboard', async () => {
    mocks.invoke.mockRejectedValue(new Error('launch failed'));
    await openPermissionSettings(permission);
    expect(mocks.toast).toHaveBeenCalledWith('perm.settingsOpenFailed', 'warn');
    expect(mocks.invoke).toHaveBeenCalledTimes(1);
  });
  it('does not open the viewer computer settings from a remote browser or lab', async () => {
    mocks.native = false;
    await openPermissionSettings(permission);
    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(mocks.toast).toHaveBeenCalledWith('perm.openInApp', 'info');
  });
});
