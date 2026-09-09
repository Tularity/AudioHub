import { toast } from '../components/Toasts';
import { isTauri, tauriInvoke } from '../ipc/endpoint';
import { t } from '../i18n';
import { permissionManual } from '../state/permissions';
import type { PermissionState } from '../state/permissions';

/** System settings are a native action, never a clipboard fallback. */
export async function openPermissionSettings(permission: PermissionState): Promise<void> {
  const manual = permissionManual(permission);
  if (!isTauri()) {
    toast(t('perm.openInApp'), 'info');
    return;
  }
  try {
    const direct = await tauriInvoke<boolean>('open_permission_settings', { id: permission.id });
    if (!direct && manual) toast(t('perm.settingsFallback', { manual }), 'info');
  } catch {
    toast(manual ? t('perm.settingsOpenFailed', { manual }) : t('perm.noSettingsUrl'), 'warn');
  }
}
