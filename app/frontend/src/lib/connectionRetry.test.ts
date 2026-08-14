import { describe, expect, it } from 'vitest';
import {
  AUTO_RETRY_DELAYS_MS, autoRetryDelay, isStableConnectionFailure,
} from './connectionRetry';

describe('daemon background reconnect policy', () => {
  it.each([
    'no-binary', 'not-installed', 'stopped', 'user-cancelled',
    'install-failed', 'installed-unavailable', 'service-conflict',
    'payload-missing', 'payload-invalid', 'unsupported', 'spawn-failed',
    'stop-failed', 'port-busy', 'start-failed', 'timeout', 'internal',
    'version', 'no-endpoint',
  ])('keeps structural state %s stable', (kind) => {
    expect(isStableConnectionFailure(kind)).toBe(true);
    expect(autoRetryDelay(kind, 0)).toBeNull();
  });

  it('uses a finite increasing window for a running but unreachable daemon', () => {
    expect(AUTO_RETRY_DELAYS_MS).toEqual([5_000, 15_000, 30_000]);
    expect(autoRetryDelay('running-unreachable', 0)).toBe(5_000);
    expect(autoRetryDelay('running-unreachable', 1)).toBe(15_000);
    expect(autoRetryDelay('running-unreachable', 2)).toBe(30_000);
    expect(autoRetryDelay('running-unreachable', 3)).toBeNull();
    expect(autoRetryDelay('auth-timeout', 3)).toBeNull();
  });

  it('rejects invalid retry counters instead of creating an unbounded timer', () => {
    expect(autoRetryDelay('running-unreachable', -1)).toBeNull();
    expect(autoRetryDelay('running-unreachable', 0.5)).toBeNull();
  });
});
