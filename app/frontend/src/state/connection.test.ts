import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T) => void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

const harness = vi.hoisted(() => ({
  state: {
    conn: 'offline',
    mode: 'tauri',
    endpoint: null,
    endpointSource: null,
    connError: null,
    daemon: null,
    sessions: [],
    airplaySessions: [],
    daemonSettings: null,
    permissions: { supported: null },
  } as Record<string, unknown>,
  connectResults: [] as Promise<unknown>[],
  connectCalls: [] as Array<{ port: number; token: string }>,
  closeCalls: 0,
  endpointCalls: 0,
  autoRetryDelay: vi.fn((_kind: string, _attempt: number) => 5_000 as number | null),
}));

vi.mock('../ipc/client', () => {
  class VersionMismatchError extends Error {
    actual: number | string;

    constructor(actual: number | string) {
      super('version mismatch');
      this.actual = actual;
    }
  }

  class IpcClient {
    connected = false;

    on(): () => void {
      return () => {};
    }

    connect(port: number, token: string): Promise<unknown> {
      harness.connectCalls.push({ port, token });
      const result = harness.connectResults.shift();
      if (!result) return Promise.reject(new Error('missing controlled connect result'));
      return result.then((daemon) => {
        this.connected = true;
        return daemon;
      });
    }

    close(): void {
      harness.closeCalls += 1;
      this.connected = false;
    }

    request(method: string): Promise<unknown> {
      if (method === 'daemon.status') return Promise.resolve(harness.state.daemon);
      if (method === 'settings.get') return Promise.resolve({ native_locale: 'en-US' });
      return Promise.resolve([]);
    }
  }

  return { IpcClient, VersionMismatchError, IPC_VERSION: 7 };
});

vi.mock('../ipc/endpoint', () => ({
  isTauri: () => true,
  resolveEndpoint: () => {
    harness.endpointCalls += 1;
    return Promise.resolve({
      port: 47_000 + harness.endpointCalls,
      token: `token-${harness.endpointCalls}`,
      source: 'tauri',
    });
  },
  tauriInvoke: (command: string) => {
    if (command === 'daemon_service_status') {
      return Promise.resolve({
        payload_present: true,
        canonical: true,
        installed: true,
        registration: 'current',
        target: '/Applications/AudioHub.app',
        running: true,
      });
    }
    return Promise.resolve({});
  },
}));

vi.mock('./store', () => ({
  getState: () => harness.state,
  setState: (patch: Record<string, unknown> | ((state: Record<string, unknown>) => Record<string, unknown>)) => {
    Object.assign(harness.state, typeof patch === 'function' ? patch(harness.state) : patch);
  },
  actions: {
    pushStats: vi.fn(),
    setAirPlaySessions: vi.fn(),
    setAirPlaySessionsUnsupported: vi.fn(),
    setDaemonSettings: vi.fn(),
    setPeers: vi.fn(),
    setPermissions: vi.fn(),
    setPermissionsError: vi.fn(),
    setSettingsUnsupported: vi.fn(),
  },
}));

vi.mock('./permissions', () => ({
  normalizeList: () => [],
  normalizeOne: () => null,
  gateNeeded: () => false,
}));
vi.mock('../lib/platform', () => ({ applyChromeDirection: vi.fn() }));
vi.mock('../lib/appearanceHost', () => ({ activeTheme: () => 'light' }));
vi.mock('../lib/trayIcon', () => ({ iconStateFrom: () => 'idle' }));
vi.mock('../lib/airplay', () => ({
  isUnknownMethod: () => false,
  sanitizeDaemonSettings: (value: unknown) => value,
}));
vi.mock('../lib/nativeLocale', () => ({ nativeLocaleNeedsSync: () => false }));
vi.mock('../lib/daemonService', () => ({ nativeServiceGate: () => null }));
vi.mock('../lib/connectionRetry', () => ({
  autoRetryDelay: (kind: string, attempt: number) => harness.autoRetryDelay(kind, attempt),
}));
vi.mock('../components/Toasts', () => ({ toast: vi.fn() }));
vi.mock('../i18n', () => ({
  getLocale: () => 'en-US',
  t: (key: string) => key,
}));

describe('connection attempt generation fence', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    harness.state = {
      conn: 'offline',
      mode: 'tauri',
      endpoint: null,
      endpointSource: null,
      connError: null,
      daemon: null,
      sessions: [],
      airplaySessions: [],
      daemonSettings: null,
      permissions: { supported: null },
    };
    harness.connectResults = [];
    harness.connectCalls = [];
    harness.closeCalls = 0;
    harness.endpointCalls = 0;
    harness.autoRetryDelay.mockClear();
  });

  afterEach(() => {
    vi.clearAllTimers();
    vi.useRealTimers();
  });

  it('does not let a late older connect overwrite or close the newer socket', async () => {
    const old = deferred<Record<string, unknown>>();
    const oldDaemon = { ipc_version: 7, name: 'old daemon' };
    const newDaemon = { ipc_version: 7, name: 'new daemon' };
    harness.connectResults.push(old.promise, Promise.resolve(newDaemon));

    const { connectDaemon } = await import('./connection');
    const first = connectDaemon();
    await vi.waitFor(() => expect(harness.connectCalls).toHaveLength(1));

    // A direct Retry supersedes the generation which is still inside
    // client.connect. It may abandon that old socket once, before opening the
    // replacement, but the old catch must never close the replacement later.
    const second = connectDaemon();
    await second;
    expect(harness.connectCalls).toHaveLength(2);
    expect(harness.closeCalls).toBe(1);
    expect(harness.state.conn).toBe('online');
    expect(harness.state.daemon).toEqual(newDaemon);

    const closesAfterNewConnection = harness.closeCalls;
    old.resolve(oldDaemon);
    await first;

    expect(harness.closeCalls).toBe(closesAfterNewConnection);
    expect(harness.state.conn).toBe('online');
    expect(harness.state.daemon).toEqual(newDaemon);
    expect(harness.autoRetryDelay).not.toHaveBeenCalled();
  });
});
