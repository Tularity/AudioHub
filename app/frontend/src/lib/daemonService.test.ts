import { describe, expect, it } from 'vitest';
import type { DaemonServiceStatus } from '../ipc/types';
import { nativeServiceGate } from './daemonService';

function status(patch: Partial<DaemonServiceStatus> = {}): DaemonServiceStatus {
  return {
    payload_present: true,
    canonical: true,
    installed: true,
    registration: 'current',
    target: '/Applications/AudioHub.app',
    running: true,
    ...patch,
  };
}

describe('native daemon ownership gate', () => {
  it('refuses a stopped or non-current process before endpoint connection', () => {
    expect(nativeServiceGate(status({ running: false }))).toBe('stopped');
  });

  it('keeps payload and installation repair ahead of Start', () => {
    expect(nativeServiceGate(status({ payload_present: false, running: false }))).toBe('no-binary');
    expect(nativeServiceGate(status({ installed: false, running: false }))).toBe('not-installed');
    expect(nativeServiceGate(status({ registration: 'stale', running: false }))).toBe('not-installed');
  });

  it('allows endpoint resolution only for a verified current daemon', () => {
    expect(nativeServiceGate(status())).toBeNull();
  });
});
