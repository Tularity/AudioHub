import type { DaemonSettings } from '../ipc/types';

/**
 * settings 回包是外部输入。即使有问题的 daemon 误回了只写密码，也不能让它进入
 * store 或测试快照；复制后删除还避免改写 IPC 客户端交给我们的原对象。
 */
export function sanitizeDaemonSettings(value: unknown): DaemonSettings | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  const clean = { ...(value as Record<string, unknown>) };
  delete clean.airplay_password;
  return clean as DaemonSettings;
}

export function isUnknownMethod(error: unknown): boolean {
  return /unknown method/i.test(String((error as Error)?.message || error));
}
