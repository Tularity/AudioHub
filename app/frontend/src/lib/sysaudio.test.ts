// Guards for `lib/sysaudio.ts` -- the module that turns
// `daemon.status.sysaudio_backends` into the rows of the mode-A backend picker.
//
// The regression this file exists for: the daemon did not report that field at
// all (plan-conformance §二.12c). The consumer side here was written first and
// sat unexercised, so the branch that matters most -- what the UI shows when
// the daemon says NOTHING -- was never pinned down. Now that the daemon does
// report it, both branches are live at once (an older daemon still omits it),
// and the difference between them is a product rule, not a formatting detail:
//
//   reported + available:false  =>  "this host cannot run it, here is why"
//   absent                      =>  "unknown; you will find out on start"
//
// Collapsing the second into the first greys out a working feature on a
// perfectly capable machine, which is why `available` is a tri-state and why
// `noBackendAvailable()` refuses to answer without a report.

import { describe, it, expect } from 'vitest';
import type { DaemonInfo, SysAudioBackend } from '../ipc/types';
import {
  BACKEND_AUTO, DEFAULT_SPK_SOURCE, SOURCE_MIC, SOURCE_SYSAUDIO,
  backendKnown, backendOptions, backendParam, backendsReported,
  isShareSource, noBackendAvailable, normalizeSource,
} from './sysaudio';

/** A DaemonInfo carrying exactly the backend list under test. */
function daemon(sysaudio_backends?: SysAudioBackend[]): DaemonInfo {
  return { fingerprint: 'self', ...(sysaudio_backends ? { sysaudio_backends } : {}) } as DaemonInfo;
}

/** The shape core's `BackendInfo` serialises to (sysaudio.rs). */
function backend(over: Partial<SysAudioBackend> = {}): SysAudioBackend {
  return {
    id: 'mac-catap',
    name: 'macOS Core Audio process tap',
    available: true,
    excludes_self: true,
    declined: false,
    note: 'Core Audio process tap, excluding this process',
    ...over,
  };
}

describe('backendsReported: absent and empty are different answers', () => {
  it('is false when the daemon omits the field', () => {
    expect(backendsReported(daemon())).toBe(false);
  });

  // An empty array is a daemon that looked and found none -- a real verdict.
  // Absent is a daemon that cannot answer. Reading `.length` instead of
  // `Array.isArray` would merge the two.
  it('is true for an empty array -- that is a verdict, not a silence', () => {
    expect(backendsReported(daemon([]))).toBe(true);
  });

  it('is false for a missing daemon', () => {
    expect(backendsReported(null)).toBe(false);
    expect(backendsReported(undefined)).toBe(false);
  });
});

describe('backendOptions: the daemon is the authority when it speaks', () => {
  it('reports available/unavailable exactly as the daemon said', () => {
    const opts = backendOptions(daemon([
      backend({ id: 'mac-catap', available: true }),
      backend({ id: 'win-proc-exclude', available: false, note: 'Windows only' }),
    ]));
    expect(opts.map((o) => [o.id, o.available])).toEqual([
      ['mac-catap', true],
      ['win-proc-exclude', false],
    ]);
  });

  // `available` / `declined` remain daemon-owned facts, but its prose may use
  // another locale. Known product vocabulary must come from the UI catalogue.
  it('localizes a known backend note instead of exposing daemon prose', () => {
    const [opt] = backendOptions(daemon([backend({ note: 'the last attempt was refused' })]));
    expect(opt.note).not.toBe('the last attempt was refused');
    expect(opt.note).not.toBe('');
  });

  it('uses the catalogue note when the daemon sends an empty one', () => {
    const [opt] = backendOptions(daemon([backend({ note: '' })]));
    expect(opt.note).not.toBe('');
  });

  it('keeps daemon prose only as a compatibility fallback for an unknown backend', () => {
    const [opt] = backendOptions(daemon([backend({ id: 'future-backend', note: 'future note' })]));
    expect(opt.note).toBe('future note');
  });

  // core holds `declined => !available`. A malformed report claiming both must
  // not produce a selectable row that can only ever error out on open.
  it('forces available:false on a declined backend even if the report disagrees', () => {
    const [opt] = backendOptions(daemon([backend({ id: 'mac-sck', declined: true, available: true })]));
    expect(opt.declined).toBe(true);
    expect(opt.available).toBe(false);
  });

  it('honours a daemon that un-declines a backend -- it runs the real core', () => {
    const [opt] = backendOptions(daemon([backend({ id: 'mac-sck', declined: false, available: true })]));
    expect(opt.declined).toBe(false);
    expect(opt.available).toBe(true);
  });

  it('drops entries with no id rather than rendering a blank row', () => {
    expect(backendOptions(daemon([backend({ id: '' }), backend({ id: 'mac-catap' })])))
      .toHaveLength(1);
  });
});

describe('backendOptions: with no report, availability is unknown -- never false', () => {
  // The load-bearing assertion. `null` renders as "unknown"; `false` would grey
  // the row out and tell the user their machine cannot do something it can.
  it('reports null availability for every non-declined backend', () => {
    for (const opt of backendOptions(daemon())) {
      if (opt.declined) continue;
      expect(opt.available, `${opt.id} must be unknown, not unavailable`).toBeNull();
    }
  });

  // Declined is the one thing the frontend knows without asking: the code path
  // is absent from this build, so no OS, grant or setting can turn it on.
  it('still reports false for a declined backend', () => {
    const declined = backendOptions(daemon()).filter((o) => o.declined);
    for (const opt of declined) expect(opt.available).toBe(false);
  });

  it('gives every fallback row a label and a note', () => {
    for (const opt of backendOptions(daemon())) {
      expect(opt.label, `no label for ${opt.id}`).toBeTruthy();
      expect(opt.note, `no note for ${opt.id}`).toBeTruthy();
    }
  });
});

describe('noBackendAvailable: silence is not a "no"', () => {
  it('is false when the daemon never reported -- unknown must not disable the feature', () => {
    expect(noBackendAvailable(daemon())).toBe(false);
  });

  it('is true only when every reported backend is explicitly unavailable', () => {
    expect(noBackendAvailable(daemon([backend({ available: false })]))).toBe(true);
    expect(noBackendAvailable(daemon([
      backend({ id: 'mac-catap', available: false }),
      backend({ id: 'win-proc-exclude', available: true }),
    ]))).toBe(false);
  });
});

describe('the session.open contract', () => {
  it('defaults the spk direction to system audio, not the microphone', () => {
    expect(DEFAULT_SPK_SOURCE).toBe(SOURCE_SYSAUDIO);
    expect(normalizeSource(undefined)).toBe(SOURCE_SYSAUDIO);
    expect(normalizeSource('halspk')).toBe(SOURCE_SYSAUDIO);
    expect(normalizeSource(SOURCE_MIC)).toBe(SOURCE_MIC);
    expect(isShareSource(SOURCE_MIC)).toBe(true);
    expect(isShareSource('tone')).toBe(false);
  });

  // Switching backends is `session.open.backend`; 'auto' means "send no field
  // and let resolve_backend pick". Sending the literal string would have the
  // daemon look up a backend named "auto" and fail the open.
  it('omits the backend field for auto/blank', () => {
    expect(backendParam(BACKEND_AUTO)).toBeNull();
    expect(backendParam('')).toBeNull();
    expect(backendParam(undefined)).toBeNull();
    expect(backendParam(' mac-catap ')).toBe('mac-catap');
  });

  it('treats auto as known so a fresh install is never flagged as stale', () => {
    expect(backendKnown(daemon([backend()]), BACKEND_AUTO)).toBe(true);
    expect(backendKnown(daemon([backend({ id: 'mac-catap' })]), 'mac-catap')).toBe(true);
    expect(backendKnown(daemon([backend({ id: 'mac-catap' })]), 'win-proc-exclude')).toBe(false);
  });
});
