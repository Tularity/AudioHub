/**
 * Background reconnect policy for the App <-> daemon control socket.
 *
 * Native lifecycle failures are decisions, not network weather. Retrying
 * them from a timer makes the service gate flash between `connecting` and its
 * actionable error forever, and can repeatedly enter a daemon whose auth path
 * is already unhealthy. Only transient reachability failures get a small,
 * finite recovery window. A user-initiated Retry starts a fresh window.
 */

export const AUTO_RETRY_DELAYS_MS = [5_000, 15_000, 30_000] as const;

const STABLE_FAILURES = new Set([
  'no-binary',
  'not-installed',
  'stopped',
  'user-cancelled',
  'install-failed',
  'installed-unavailable',
  'payload-missing',
  'payload-invalid',
  'unsupported',
  'service-conflict',
  'spawn-failed',
  'stop-failed',
  'port-busy',
  'start-failed',
  'timeout',
  'internal',
  'version',
  'no-endpoint',
]);

/**
 * Return the delay for the next silent retry, or null when the current error
 * must remain stable until the user (or a native lifecycle action) intervenes.
 */
export function autoRetryDelay(kind: string, attempt: number): number | null {
  if (STABLE_FAILURES.has(kind)) return null;
  if (!Number.isInteger(attempt) || attempt < 0) return null;
  return AUTO_RETRY_DELAYS_MS[attempt] ?? null;
}

export function isStableConnectionFailure(kind: string): boolean {
  return STABLE_FAILURES.has(kind);
}
