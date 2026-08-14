import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('../i18n', () => ({ t: (key: string) => key }));

import { IpcClient } from './client';

class FakeWebSocket extends EventTarget {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static instances: FakeWebSocket[] = [];

  readyState = FakeWebSocket.CONNECTING;
  sent: string[] = [];

  constructor(readonly url: string) {
    super();
    FakeWebSocket.instances.push(this);
  }

  send(frame: string): void {
    this.sent.push(frame);
  }

  close(): void {
    this.readyState = FakeWebSocket.CLOSED;
  }

  open(): void {
    this.readyState = FakeWebSocket.OPEN;
    this.dispatchEvent(new Event('open'));
  }

  message(value: unknown): void {
    const event = new Event('message') as MessageEvent;
    Object.defineProperty(event, 'data', { value: JSON.stringify(value) });
    this.dispatchEvent(event);
  }

  serverClose(): void {
    this.readyState = FakeWebSocket.CLOSED;
    this.dispatchEvent(new Event('close'));
  }
}

describe('IpcClient pending authentication cancellation', () => {
  const originalWebSocket = globalThis.WebSocket;

  beforeEach(() => {
    vi.useFakeTimers();
    FakeWebSocket.instances = [];
    globalThis.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
  });

  afterEach(() => {
    globalThis.WebSocket = originalWebSocket;
    vi.clearAllTimers();
    vi.useRealTimers();
  });

  it('settles immediately on close and ignores the abandoned socket thereafter', async () => {
    const client = new IpcClient();
    const closeEvents = vi.fn();
    client.on('close', closeEvents);

    const first = client.connect(47_000, 'old-token');
    const oldSocket = FakeWebSocket.instances[0];
    expect(oldSocket).toBeDefined();
    expect(vi.getTimerCount()).toBe(1);

    client.close();
    await expect(first).rejects.toThrow('error.connectionClosed');
    expect(vi.getTimerCount()).toBe(0);

    const second = client.connect(47_001, 'new-token');
    const newSocket = FakeWebSocket.instances[1];
    newSocket.open();
    expect(newSocket.sent).toEqual([JSON.stringify({ auth: 'new-token' })]);
    newSocket.message({ ok: true, daemon: { ipc_version: 7, name: 'new daemon' } });
    await expect(second).resolves.toMatchObject({ name: 'new daemon' });
    expect(client.connected).toBe(true);

    // A delayed close from the explicitly abandoned first socket must not clear
    // authentication or notify the connection orchestrator about the new one.
    oldSocket.serverClose();
    expect(client.connected).toBe(true);
    expect(closeEvents).not.toHaveBeenCalled();

    client.close();
  });
});
