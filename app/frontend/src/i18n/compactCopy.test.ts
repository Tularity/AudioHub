import { describe, expect, it } from 'vitest';
import { enUS } from './en-US';
import { LOCALE_ENDONYM } from './index';

// These strings sit in fixed-width menus, segmented controls, metric cards,
// and table headers. Give each real constraint its own budget instead of
// imposing an arbitrary ceiling on explanatory copy elsewhere in the
// catalogue.
const budgets = {
  'chrome.locale.system': 8,
  'nav.peers': 9,
  'nav.share': 9,
  'nav.stats': 9,
  'nav.settings': 9,
  'mode.share.label': 8,
  'mode.a.label': 15,
  'mode.b.label': 15,
  'pair.left.open': 14,
  'pair.left.enable': 14,
  'pair.left.disable': 12,
  'metric.latency.grade.imperceptible': 7,
  'metric.latency.grade.conversational': 7,
  'metric.latency.grade.noticeable': 7,
  'metric.latency.grade.unusable': 7,
  'metric.quality.grade.excellent': 8,
  'metric.quality.grade.good': 8,
  'metric.quality.grade.fair': 8,
  'metric.quality.grade.poor': 8,
  'detail.sessions.colSession': 3,
  'detail.sessions.colFlow': 4,
  'detail.sessions.colDir': 4,
  'detail.sessions.colBitrate': 4,
  'detail.sessions.colRung': 4,
  'detail.sessions.colLoss': 4,
  'detail.sessions.colJitter': 6,
  'detail.sessions.colVolume': 4,
  'detail.sessions.colVerdict': 5,
  'detail.sessions.colAction': 6,
} as const satisfies Partial<Record<keyof typeof enUS, number>>;

type BudgetedKey = keyof typeof budgets;
const budgetedKeys = Object.keys(budgets) as BudgetedKey[];

// These notes are not inside a button, but remain visible throughout their
// corresponding settings/detail section. Long English prose here makes the
// card or sheet much taller than zh-CN, so keep a deliberate per-sentence cap.
const persistentCopyBudgets = {
  'share.proto.airplay.consequence': 125,
  'detail.transport.endpointShadow': 85,
  'settings.startup.orphaned': 100,
} as const satisfies Partial<Record<keyof typeof enUS, number>>;

type PersistentCopyKey = keyof typeof persistentCopyBudgets;
const persistentCopyKeys = Object.keys(persistentCopyBudgets) as PersistentCopyKey[];

describe('en-US copy in width-constrained controls', () => {
  it('keeps the language selector compact', () => {
    expect(LOCALE_ENDONYM['en-US'].length, 'en-US endonym').toBeLessThanOrEqual(12);
    expect(enUS['chrome.locale.system'].length, 'system-locale option').toBeLessThanOrEqual(8);
  });

  it('stays within explicit per-control character budgets', () => {
    const over = budgetedKeys
      .filter((key) => enUS[key].length > budgets[key])
      .map((key) => `${key}: ${JSON.stringify(enUS[key])} (${enUS[key].length} > ${budgets[key]})`);

    expect(over, `shorten copy used by narrow controls:\n${over.join('\n')}`).toEqual([]);
  });

  it('keeps persistent explanatory copy within explicit length budgets', () => {
    const over = persistentCopyKeys
      .filter((key) => enUS[key].length > persistentCopyBudgets[key])
      .map((key) => (
        `${key}: ${JSON.stringify(enUS[key])} `
        + `(${enUS[key].length} > ${persistentCopyBudgets[key]})`
      ));

    expect(over, `shorten persistent copy:\n${over.join('\n')}`).toEqual([]);
  });

  it('does not restore the verbose labels that previously broke the layout', () => {
    const constrainedCopy = [
      LOCALE_ENDONYM['en-US'],
      ...budgetedKeys.map((key) => enUS[key]),
    ].join('\n');
    const retiredPhrases = [
      'English (United States)',
      'Use system setting',
      'Make This Device Discoverable',
      'Good for conversation',
      'Audio quality',
      'Measured by the peer',
      'Packet loss',
      'Verification',
    ];

    for (const phrase of retiredPhrases) {
      expect(constrainedCopy, `replace the retired compact-control label ${JSON.stringify(phrase)}`)
        .not.toContain(phrase);
    }
  });
});
