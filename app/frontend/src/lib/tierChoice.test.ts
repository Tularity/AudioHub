// Guards for the *choice* half of `lib/tier.ts`.
//
// `tier.test.ts` covers the other half -- `effectiveTier()`, i.e. where the
// bytes actually go. This file covers what the user picked and, more
// importantly, the one place those two quantities meet: a stored tunnel URL
// overrides the picked tier for outbound dials, so the selector can be showing
// "direct (UDP)" while the daemon multiplexes. plan §16.4 rule 5 forbids either
// quantity standing in for the other, and the only defence against that here is
// `endpointShadowsTier()` driving a visible warning.
//
// The regression this file exists for: Tier 2 was missing from the selector
// entirely (plan-conformance §二.8) -- the daemon and CLI accepted it, the UI
// offered three of the four tiers, and nothing failed.

import { describe, it, expect } from 'vitest';
import { t } from '../i18n';
import {
  TIER_CHOICES, TIER_PICK_HINT, TIER_PICK_LABEL,
  dialsMultiplexed, endpointShadowsTier, endpointVisible, hasEndpoint, tierPickLabel,
} from './tier';

describe('TIER_CHOICES: every tier the daemon accepts is offered', () => {
  // `peers.set_tier` rejects anything outside this set with
  // "tier 必须是 'auto'、'tier0'、'tier1' 或 'tier2'" (ipcserv.rs), and
  // `TransportTier::parse` is the single writer-side gate. A selector that is a
  // strict subset of that set is a feature the UI simply cannot reach.
  it('offers exactly auto, tier0, tier1, tier2', () => {
    expect([...TIER_CHOICES]).toEqual(['auto', 'tier0', 'tier1', 'tier2']);
  });

  // The §二.8 regression, stated on its own so the failure message names it.
  it('includes tier2 -- it is pinnable on any peer, with or without a URL', () => {
    expect(TIER_CHOICES).toContain('tier2');
  });

  it('gives every choice both a label and a consequence line', () => {
    for (const id of TIER_CHOICES) {
      expect(TIER_PICK_LABEL[id], `no label for ${id}`).toBeTruthy();
      expect(TIER_PICK_HINT[id], `no hint for ${id}`).toBeTruthy();
    }
  });

  // `t()` returns the key itself when the catalogue has no entry, so a missing
  // string renders as `detail.transport.tier2Hint` on screen rather than
  // failing anywhere. That is exactly the class of breakage a text-scraping
  // guard cannot see, so assert the round trip.
  it('resolves every label and hint to real copy, not to the key', () => {
    for (const id of TIER_CHOICES) {
      const label = TIER_PICK_LABEL[id];
      const hint = TIER_PICK_HINT[id];
      expect(t(label), `missing catalogue entry ${label}`).not.toBe(label);
      expect(t(hint), `missing catalogue entry ${hint}`).not.toBe(hint);
    }
  });

  // plan §16.4 rule 2: say the transport, never the internal code name. A label
  // reading "tier1" explains nothing, which is the whole point of the rule.
  it('never shows an internal code name as a label', () => {
    for (const id of TIER_CHOICES) {
      expect(t(TIER_PICK_LABEL[id])).not.toMatch(/tier\s*[0-2]/i);
    }
  });
});

describe('dialsMultiplexed: mirrors the daemon carrier rule', () => {
  // conn.rs: `let tier2 = endpoint.is_some() || tier == TransportTier::Tier2;`
  // Both disjuncts stand alone. Collapsing this to "tier decides" is the bug
  // that makes the UI claim a direct link while bytes ride the mux.
  it('multiplexes when the tier is pinned to tier2, with no URL', () => {
    expect(dialsMultiplexed('tier2', '')).toBe(true);
    expect(dialsMultiplexed('tier2', undefined)).toBe(true);
  });

  it('multiplexes when a URL is stored, whatever the tier says', () => {
    expect(dialsMultiplexed('auto', 'ws://tunnel.example/audio')).toBe(true);
    expect(dialsMultiplexed('tier0', 'ws://tunnel.example/audio')).toBe(true);
    expect(dialsMultiplexed('tier1', 'ws://tunnel.example/audio')).toBe(true);
  });

  it('does not multiplex for the other tiers without a URL', () => {
    expect(dialsMultiplexed('auto', '')).toBe(false);
    expect(dialsMultiplexed('tier0', '')).toBe(false);
    expect(dialsMultiplexed('tier1', undefined)).toBe(false);
    expect(dialsMultiplexed(undefined, undefined)).toBe(false);
  });

  // The daemon treats an empty stored endpoint as absent (`PeerTransport::
  // endpoint` returns None on an empty string). Whitespace has to land on the
  // same side, or a stray space would flip the UI's verdict without flipping
  // the daemon's.
  it('treats blank and whitespace-only endpoints as no endpoint', () => {
    expect(hasEndpoint('')).toBe(false);
    expect(hasEndpoint('   ')).toBe(false);
    expect(hasEndpoint(null)).toBe(false);
    expect(dialsMultiplexed('tier0', '   ')).toBe(false);
  });
});

describe('endpointShadowsTier: the selector must not be left lying', () => {
  it('warns when a URL is stored and the pick is anything but tier2', () => {
    expect(endpointShadowsTier('auto', 'ws://h/')).toBe(true);
    expect(endpointShadowsTier('tier0', 'ws://h/')).toBe(true);
    expect(endpointShadowsTier('tier1', 'ws://h/')).toBe(true);
  });

  // tier2 + a URL agree with each other, so there is nothing to warn about.
  // Warning anyway would train the user to ignore the line.
  it('stays quiet when the pick already is tier2', () => {
    expect(endpointShadowsTier('tier2', 'ws://h/')).toBe(false);
  });

  it('stays quiet with no endpoint at all', () => {
    expect(endpointShadowsTier('tier0', '')).toBe(false);
    expect(endpointShadowsTier('auto', undefined)).toBe(false);
  });
});

// C.19 (user instruction 19, 2026-08-10): the tunnel-address field is only
// shown when it is relevant. Taken literally -- `tier === 'tier2'` -- that
// instruction produces a setting that can be stored, cannot be seen, and cannot
// be cleared: the daemon picks the carrier with an OR, so a peer pinned to
// "direct (UDP)" that has a `ws://` on disk is *already* multiplexing, and the
// only warning about that (plus the only Clear button) lives inside the field
// being hidden.
//
// The visibility rule therefore reuses `dialsMultiplexed()` rather than
// restating it. These tests exist to keep it reusing it.
describe('endpointVisible: nothing in force may be off-screen', () => {
  it('shows the field whenever tier2 is the pick, URL or not', () => {
    expect(endpointVisible('tier2', '', undefined)).toBe(true);
    expect(endpointVisible('tier2', 'ws://h/', undefined)).toBe(true);
  });

  // The case the literal reading would have broken.
  it('shows the field when a URL is stored under a non-tier2 pick', () => {
    expect(endpointVisible('tier0', 'ws://h/', undefined)).toBe(true);
    expect(endpointVisible('auto', 'ws://h/', undefined)).toBe(true);
    expect(endpointVisible('tier1', 'ws://h/', undefined)).toBe(true);
  });

  // `endpoint_reset_from` is the daemon saying "the URL you stored was
  // unreadable, I dropped it". At that moment `endpoint` is the empty string,
  // so the first two criteria are both false and the sentence would have no
  // surface to appear on.
  it('shows the field for a reset notice even with an empty endpoint', () => {
    expect(endpointVisible('auto', '', 'ws://old/')).toBe(true);
    expect(endpointVisible('tier0', '', '')).toBe(true);
  });

  it('hides the field only when no tier2, no URL and no reset notice', () => {
    expect(endpointVisible('auto', '', undefined)).toBe(false);
    expect(endpointVisible('tier0', undefined, null)).toBe(false);
    expect(endpointVisible('tier1', '   ', undefined)).toBe(false);
    expect(endpointVisible(undefined, undefined, undefined)).toBe(false);
  });

  // The invariant, stated as an implication over the whole truth table rather
  // than as a spot check: if the next dial multiplexes, the field that causes
  // it must be reachable. Any future edit that stops deriving visibility from
  // `dialsMultiplexed` has to keep this true or fail here.
  it('is implied by dialsMultiplexed for every tier/endpoint pair', () => {
    const tiers = ['auto', 'tier0', 'tier1', 'tier2', 'tier9', undefined, ''];
    const endpoints = ['', '   ', 'ws://h/', undefined, null];
    for (const tier of tiers) {
      for (const endpoint of endpoints) {
        if (!dialsMultiplexed(tier, endpoint)) continue;
        expect(
          endpointVisible(tier, endpoint, undefined),
          `multiplexing but hidden: tier=${String(tier)} endpoint=${String(endpoint)}`,
        ).toBe(true);
      }
    }
  });

  // The mirror of the shadow warning: whenever `endpointShadowsTier` wants to
  // print a line, there has to be a field for it to print into.
  it('is implied by endpointShadowsTier for every tier/endpoint pair', () => {
    const tiers = ['auto', 'tier0', 'tier1', 'tier2', undefined];
    const endpoints = ['', 'ws://h/', undefined];
    for (const tier of tiers) {
      for (const endpoint of endpoints) {
        if (!endpointShadowsTier(tier, endpoint)) continue;
        expect(
          endpointVisible(tier, endpoint, undefined),
          `shadow warning with nowhere to show: tier=${String(tier)}`,
        ).toBe(true);
      }
    }
  });
});

describe('tierPickLabel', () => {
  it('maps each known tier string to its own label', () => {
    for (const id of TIER_CHOICES) {
      expect(tierPickLabel(id)).toBe(TIER_PICK_LABEL[id]);
    }
  });

  // A daemon string this build does not know has already been reset to `auto`
  // on the daemon side (`StoredDir::sanitize` + `tier_reset_from`). Saying
  // "auto" here keeps the two ends telling the same story; inventing a label
  // would make them diverge.
  it('falls back to auto for an unknown or absent tier string', () => {
    expect(tierPickLabel('tier9')).toBe(TIER_PICK_LABEL.auto);
    expect(tierPickLabel(undefined)).toBe(TIER_PICK_LABEL.auto);
    expect(tierPickLabel('')).toBe(TIER_PICK_LABEL.auto);
  });

  // `'toString'` and friends live on Object.prototype; a bare `tier in
  // TIER_PICK_LABEL` would say yes and then index to undefined, which `t()`
  // would render as the literal string "undefined".
  it('is not fooled by inherited Object keys', () => {
    expect(tierPickLabel('toString')).toBe(TIER_PICK_LABEL.auto);
    expect(tierPickLabel('constructor')).toBe(TIER_PICK_LABEL.auto);
  });
});
