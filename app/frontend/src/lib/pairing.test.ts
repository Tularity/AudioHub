import { describe, expect, it } from 'vitest';
import { discoveredAddress } from './pairing';

describe('discovered addresses retain the device endpoint', () => {
  it('combines IPv4 and hostnames with the advertised port', () => {
    expect(discoveredAddress('192.168.1.77', 47810)).toBe('192.168.1.77:47810');
    expect(discoveredAddress('studio.local', 47810)).toBe('studio.local:47810');
  });
  it('brackets IPv6 including a link-local scope', () => {
    expect(discoveredAddress('fe80::1%en0', 47810)).toBe('[fe80::1%en0]:47810');
    expect(discoveredAddress('[::1]', 47810)).toBe('[::1]:47810');
  });
  it('does not offer an incomplete discovered endpoint', () => {
    expect(discoveredAddress(undefined, 47810)).toBeNull();
    expect(discoveredAddress('host', 0)).toBeNull();
    expect(discoveredAddress('host', undefined)).toBeNull();
  });
});
