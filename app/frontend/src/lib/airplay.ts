import type { AirPlaySessionInfo, DaemonSettings } from '../ipc/types';

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

/** 会话落在回退的系统共享 PTP 时钟上。 */
export function usesSharedClock(session: AirPlaySessionInfo): boolean {
  return session.timing === 'ptp_system';
}

/** 同时发送端上限；缺省、非整数或小于 1 时返回 null（界面不显示提示）。 */
export function maxSenders(settings: DaemonSettings | null): number | null {
  const n = settings?.airplay_max_senders;
  return typeof n === 'number' && Number.isInteger(n) && n >= 1 ? n : null;
}

/** 本系统只能同时支持一个 AirPlay 2（PTP）发送端。 */
export function singlePtpSender(settings: DaemonSettings | null): boolean {
  return settings?.airplay_ptp_backend === 'system';
}
