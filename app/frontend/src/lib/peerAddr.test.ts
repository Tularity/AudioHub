// Guards for peer-address shape classification.
//
// Two callers with two different rules share one classifier:
//   - the pair / add-peer fields, where `IP:port` is the normal input and a URL
//     is the special case (`classifyPeerAddr`);
//   - the tunnel-address field on the peer detail page, where a `ws://` URL is
//     the *only* accepted input and "direct" is expressed by leaving it empty
//     (`checkEndpoint`).
//
// The failure this protects against is the silent one: a string the UI accepts
// and stores but the daemon cannot dial. `peers.set_tier` refuses such a value,
// but only after the round trip, and in English with Rust context attached --
// so every refusal has to be reachable here, before the button is pressed.

import { describe, it, expect } from 'vitest';
import { checkEndpoint, classifyPeerAddr, looksLikeUrl } from './peerAddr';

describe('classifyPeerAddr', () => {
  it('reads a bare host or IP, with or without a port, as direct', () => {
    expect(classifyPeerAddr('192.168.1.9').kind).toBe('direct');
    expect(classifyPeerAddr('192.168.1.9:47810').kind).toBe('direct');
    expect(classifyPeerAddr('desk.local:47810').kind).toBe('direct');
    expect(classifyPeerAddr('').kind).toBe('direct');
  });

  it('reads ws:// and wss:// as their own shapes, never as a hostname', () => {
    expect(classifyPeerAddr('ws://h/').kind).toBe('ws');
    expect(classifyPeerAddr('WS://h/').kind).toBe('ws');
    expect(classifyPeerAddr('wss://h/').kind).toBe('wss');
  });

  // `wsx://host/p` must not be parsed as a machine called "wsx": the user would
  // get a DNS failure, which is the least useful report possible for a typo.
  it('does not treat an unknown scheme as a URL', () => {
    expect(looksLikeUrl('wsx://h/')).toBe(false);
    expect(classifyPeerAddr('wsx://h/').kind).toBe('direct');
  });

  it('names why a URL is unusable rather than rejecting it flatly', () => {
    expect(classifyPeerAddr('ws://')).toEqual({ kind: 'badUrl', reason: 'noHost' });
    expect(classifyPeerAddr('ws://:8080')).toEqual({ kind: 'badUrl', reason: 'noHost' });
    expect(classifyPeerAddr('ws://h:99999')).toEqual({ kind: 'badUrl', reason: 'badPort' });
    expect(classifyPeerAddr('ws://h:0')).toEqual({ kind: 'badUrl', reason: 'badPort' });
    expect(classifyPeerAddr('ws://h:80x')).toEqual({ kind: 'badUrl', reason: 'badPort' });
    expect(classifyPeerAddr('ws://[::1')).toEqual({ kind: 'badUrl', reason: 'badIpv6' });
  });

  // Bracketed IPv6 has its own colon rule -- `rsplit_once(':')` on `[::1]`
  // would otherwise read `1]` as the port. Matches `WsUrl::parse`.
  it('accepts bracketed IPv6 with and without a port', () => {
    expect(classifyPeerAddr('ws://[::1]/').kind).toBe('ws');
    expect(classifyPeerAddr('ws://[::1]:8080/x').kind).toBe('ws');
  });
});

describe('checkEndpoint: what the tunnel-address field will store', () => {
  it('accepts an empty value as "clear it, go back to direct"', () => {
    expect(checkEndpoint('')).toEqual({ ok: true, value: '' });
    expect(checkEndpoint('   ')).toEqual({ ok: true, value: '' });
  });

  it('accepts a ws:// URL and stores it trimmed', () => {
    expect(checkEndpoint('ws://tunnel.example:8080/audio'))
      .toEqual({ ok: true, value: 'ws://tunnel.example:8080/audio' });
    expect(checkEndpoint('  ws://h/  ')).toEqual({ ok: true, value: 'ws://h/' });
  });

  // This build has no TLS client (`WsUrl::require_plaintext`). The refusal has
  // to name that, because "wss:// is unsupported" and "your address is
  // malformed" ask the user to do completely different things.
  it('refuses wss:// as its own case, not as a malformed URL', () => {
    expect(checkEndpoint('wss://tunnel.example/audio')).toEqual({ ok: false, why: 'wss' });
  });

  // The rule that differs from the add-peer field: an `IP:port` here is not a
  // valid tunnel address. Stored, it would read back as no endpoint at all --
  // the field would look filled and do nothing.
  it('refuses a direct address instead of silently prefixing ws://', () => {
    expect(checkEndpoint('192.168.1.9:47810')).toEqual({ ok: false, why: 'notUrl' });
    expect(checkEndpoint('tunnel.example')).toEqual({ ok: false, why: 'notUrl' });
  });

  it('passes through the specific reason a URL is malformed', () => {
    expect(checkEndpoint('ws://')).toEqual({ ok: false, why: 'noHost' });
    expect(checkEndpoint('ws://h:99999')).toEqual({ ok: false, why: 'badPort' });
    expect(checkEndpoint('ws://[::1')).toEqual({ ok: false, why: 'badIpv6' });
  });
});
