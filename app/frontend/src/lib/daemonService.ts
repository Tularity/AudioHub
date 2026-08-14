import type { DaemonServiceStatus } from '../ipc/types';

export type NativeServiceGate = 'no-binary' | 'not-installed' | 'stopped' | null;

/**
 * Decide the native lifecycle action before reading or connecting ipc.json.
 *
 * In particular, `running: false` means the recorded PID was not proven to be
 * this App's selected daemon image. Connecting the endpoint first would let a
 * reachable older/foreign process bypass the native ownership check.
 */
export function nativeServiceGate(service: DaemonServiceStatus): NativeServiceGate {
  if (!service.payload_present) return 'no-binary';
  if (!service.installed || service.registration === 'stale') return 'not-installed';
  if (!service.running) return 'stopped';
  return null;
}
