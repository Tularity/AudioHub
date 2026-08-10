// The interface does not explain itself any more. This file is what keeps it
// that way.
//
// User ruling, 2026-08-10 (docs/plan.md §3.1): **simplify for simplicity's
// sake**. Every functional description was removed from the UI and moved to the
// public wiki; what a screen may show is a title, a value or status, and a `?`
// that opens the matching wiki section in the system browser.
//
// Nothing in the type system stops the next edit from putting a paragraph back:
// `t('some.key')` type-checks no matter how long the string behind it is. Twice
// already this project has watched short copy grow back into an essay one
// justified sentence at a time — the catalogue peaked at 819 entries with 86 of
// them over 50 characters and a single one at 558, printed in full on the
// settings page. So the ceiling is enforced here rather than remembered.

import { describe, it, expect } from 'vitest';
import { zhCN } from './zh-CN';

const entries = Object.entries(zhCN) as Array<[keyof typeof zhCN, string]>;

/**
 * The hard ceiling, in characters.
 *
 * 80 is not a style preference, it is the length of the longest string that
 * survived the cull — `settings.net.announceNotInForce`, at 78, which spends
 * its whole budget telling you which System Settings pane to open and which
 * switch to flick afterwards. That is the shape of the only copy still allowed
 * to be long: a failure the user has to act on, with the action in it.
 *
 * If a new string does not fit, it is almost certainly a description, and
 * descriptions go in the wiki with a `<Help/>` beside the control. Raising this
 * number is how the essays come back.
 */
const MAX = 80;

describe('the catalogue stays out of the way', () => {
  it('has no message longer than the ceiling', () => {
    const over = entries
      .filter(([, v]) => v.length > MAX)
      .map(([k, v]) => `${k} (${v.length})`);
    expect(over, `move these to the wiki and leave a <Help/> behind:\n${over.join('\n')}`)
      .toEqual([]);
  });

  it('carries no Markdown emphasis', () => {
    // There is no Markdown renderer anywhere in the tree: every message lands
    // in a text node or a `title` attribute. `**必须**` therefore renders as
    // four literal asterisks, which it did — on the settings page, the peer
    // detail page and the transport help panel simultaneously — until the
    // 2026-08-10 sweep. The corpus header has forbidden this since the first
    // commit; a comment is not a check.
    const marked = entries.filter(([, v]) => v.includes('**')).map(([k]) => k);
    expect(marked, 'plain text nodes render ** as asterisks').toEqual([]);
  });

  it('offers a wiki label for every place that lost its explanation', () => {
    // The `?` buttons take their accessible name from these keys. A missing one
    // does not crash — `t()` falls back to the key itself — so the failure mode
    // is a screen reader announcing "wiki.transport, button". Cheap to prevent.
    const labels = entries.filter(([k]) => k.startsWith('wiki.'));
    expect(labels.length).toBeGreaterThan(10);
    for (const [k, v] of labels) {
      expect(v, `${k} should say it leads somewhere English`).toMatch(/（英文）$/);
    }
  });
});
