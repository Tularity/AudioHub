import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { VolumeState } from '../ipc/types';
import {
  VOLUME_INTENT_HOLD_MS,
  applyVolumeWriteFailureOwnership,
  consumeQueuedVolumeWrite,
  deviceVolumeEndpointFor,
  deviceVolumeRequest,
  deviceVolumeStatus,
  enqueueVolumeIntent,
  reconcileVolumeIntentPins,
  scheduleDeviceIntentExpiry,
  updateVolumeIntentPins,
  volumeWithIntentPins,
  volumeUsability,
  type VolumeIntentPins,
  type VolumeWriteFailureContext,
} from './VolumeControl';

const adjustable: VolumeState = { scalar: 0.42, muted: false, adjustable: true };

describe('mode-B device volume contract', () => {
  it('maps virtual speaker and microphone rows to real peer endpoints', () => {
    expect(deviceVolumeEndpointFor('out')).toBe('default_output');
    expect(deviceVolumeEndpointFor('in')).toBe('default_input');
  });

  it('builds an endpoint request without a synthetic session id', () => {
    const request = deviceVolumeRequest('peer-fingerprint', 'default_input', {
      scalar: 0.55,
      muted: false,
    });

    expect(request).toEqual({
      peer: 'peer-fingerprint',
      endpoint: 'default_input',
      scalar: 0.55,
      muted: false,
    });
    expect(request).not.toHaveProperty('id');
  });

  it('omits muted when a slider write must preserve the peer mute state', () => {
    expect(deviceVolumeRequest('peer-fingerprint', 'default_output', { scalar: 0.7 }))
      .toEqual({ peer: 'peer-fingerprint', endpoint: 'default_output', scalar: 0.7 });
  });
});

describe('mode-B device volume availability', () => {
  it('disables legacy or absent protocol versions even if stale state exists', () => {
    expect(deviceVolumeStatus(undefined, adjustable, false)).toBe('legacy');
    expect(deviceVolumeStatus(0, adjustable, false)).toBe('legacy');
    expect(deviceVolumeStatus(0, adjustable, true, true)).toBe('legacy');
  });

  it('distinguishes waiting, pending, fixed, and ready endpoint state', () => {
    expect(deviceVolumeStatus(1, null, false)).toBe('waiting');
    expect(deviceVolumeStatus(1, null, true)).toBe('pending');
    expect(deviceVolumeStatus(0, adjustable, true)).toBe('pending');
    expect(deviceVolumeStatus(1, { ...adjustable, adjustable: false }, false)).toBe('fixed');
    expect(deviceVolumeStatus(1, adjustable, false)).toBe('ready');
  });

  it('keeps scalar and mute writability independent with legacy fallback', () => {
    expect(volumeUsability({ ...adjustable, mute_adjustable: false }, false, true))
      .toEqual({ scalar: true, mute: false });
    expect(volumeUsability({ ...adjustable, adjustable: false, mute_adjustable: true }, false, true))
      .toEqual({ scalar: false, mute: true });
    expect(volumeUsability({ ...adjustable, adjustable: false, mute_adjustable: false }, true, true))
      .toEqual({ scalar: true, mute: true });
    expect(volumeUsability(adjustable, false, true))
      .toEqual({ scalar: true, mute: true });
  });
});

describe('mode-B field-scoped volume intent', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.clearAllTimers();
    vi.useRealTimers();
  });

  it('pins only the field the user changed and merges the other field from readback', () => {
    const empty: VolumeIntentPins = { scalar: null, muted: null };
    const scalarPins = updateVolumeIntentPins(empty, 0.73, null, 100);
    expect(scalarPins.scalar).toEqual({ value: 0.73, at: 100 });
    expect(scalarPins.muted).toBeNull();
    expect(volumeWithIntentPins(
      { scalar: 0.42, muted: true, adjustable: true },
      scalarPins,
    )).toMatchObject({ scalar: 0.73, muted: true });

    const mutePins = updateVolumeIntentPins(empty, 0.42, true, 200);
    expect(mutePins.scalar).toBeNull();
    expect(mutePins.muted).toEqual({ value: true, at: 200 });
    expect(volumeWithIntentPins(
      { scalar: 0.64, muted: false, adjustable: true },
      mutePins,
    )).toMatchObject({ scalar: 0.64, muted: true });
  });

  it('converges and expires scalar and mute independently', () => {
    const pins: VolumeIntentPins = {
      scalar: { value: 0.73, at: 1_000 },
      muted: { value: true, at: 500 },
    };
    const scalarConverged = reconcileVolumeIntentPins(
      pins,
      { scalar: 0.73, muted: false, adjustable: true },
      1_200,
    );
    expect(scalarConverged.scalar).toBeNull();
    expect(scalarConverged.muted).toBe(pins.muted);

    const muteConverged = reconcileVolumeIntentPins(
      pins,
      { scalar: 0.1, muted: true, adjustable: true },
      1_200,
    );
    expect(muteConverged.scalar).toBe(pins.scalar);
    expect(muteConverged.muted).toBeNull();

    const muteExpired = reconcileVolumeIntentPins(pins, null, 2_500);
    expect(muteExpired.muted).toBeNull();
    expect(muteExpired.scalar).toBe(pins.scalar);
    const bothExpired = reconcileVolumeIntentPins(pins, null, 3_000);
    expect(bothExpired).toEqual({ scalar: null, muted: null });
  });

  it('does not extend one field deadline when the other field changes', () => {
    const empty: VolumeIntentPins = { scalar: null, muted: null };
    const muteFirst = updateVolumeIntentPins(empty, 0.4, true, 100);
    const thenScalar = updateVolumeIntentPins(muteFirst, 0.8, null, 1_000);
    expect(thenScalar.muted).toBe(muteFirst.muted);
    expect(thenScalar.muted?.at).toBe(100);
    expect(thenScalar.scalar?.at).toBe(1_000);

    const scalarFirst = updateVolumeIntentPins(empty, 0.8, null, 200);
    const thenMute = updateVolumeIntentPins(scalarFirst, 0.8, true, 1_100);
    expect(thenMute.scalar).toBe(scalarFirst.scalar);
    expect(thenMute.scalar?.at).toBe(200);
    expect(thenMute.muted?.at).toBe(1_100);
  });

  it('hands one refused field back at its deadline and refreshes', () => {
    const oldPin = { value: 0.73, at: 0 };
    let pin: typeof oldPin | null = oldPin;
    let displayed = oldPin.value;
    const reported = 0.68;
    const rerender = vi.fn(() => { displayed = pin?.value ?? reported; });
    const refresh = vi.fn();

    scheduleDeviceIntentExpiry({
      isCurrent: () => pin === oldPin,
      clearPin: () => { pin = null; },
      rerender,
      refresh,
    });

    vi.advanceTimersByTime(1250);
    rerender();
    expect(displayed).toBe(0.73);
    rerender.mockClear();

    vi.advanceTimersByTime(VOLUME_INTENT_HOLD_MS - 1251);
    expect(pin).toBe(oldPin);
    expect(displayed).toBe(0.73);
    expect(refresh).not.toHaveBeenCalled();
    expect(rerender).not.toHaveBeenCalled();

    vi.advanceTimersByTime(1);
    expect(pin).toBeNull();
    expect(displayed).toBe(reported);
    expect(rerender).toHaveBeenCalledTimes(1);
    expect(refresh).toHaveBeenCalledTimes(1);
  });

  it('does not let an old field timer clear a replacement pin', () => {
    const oldPin = { value: 0.2, at: 0 };
    const replacement = { value: 0.9, at: 1_000 };
    let generation = 4;
    let pin: typeof oldPin | null = oldPin;
    const clearPin = vi.fn(() => { pin = null; });

    scheduleDeviceIntentExpiry({
      isCurrent: () => generation === 4 && pin === oldPin,
      clearPin,
      rerender: vi.fn(),
      refresh: vi.fn(),
    });
    vi.advanceTimersByTime(1_000);
    pin = replacement;
    vi.advanceTimersByTime(1_000);
    expect(pin).toBe(replacement);
    expect(clearPin).not.toHaveBeenCalled();

    // A target reset invalidates the same closure even if an object were reused.
    pin = oldPin;
    generation += 1;
    scheduleDeviceIntentExpiry({
      isCurrent: () => generation === 4 && pin === oldPin,
      clearPin,
      rerender: vi.fn(),
      refresh: vi.fn(),
    });
    vi.advanceTimersByTime(VOLUME_INTENT_HOLD_MS);
    expect(pin).toBe(oldPin);
    expect(clearPin).not.toHaveBeenCalled();
  });
});

describe('mode-B throttled volume write ownership', () => {
  const emptyPins = (): VolumeIntentPins => ({ scalar: null, muted: null });
  const failureContext = (
    overrides: Partial<VolumeWriteFailureContext> = {},
  ): VolumeWriteFailureContext => ({
    currentGeneration: 1,
    consumedGeneration: 1,
    currentSequence: 1,
    consumedSequence: 1,
    targetMatches: true,
    ...overrides,
  });

  it('preserves a successful scalar pin when a later mute-only RPC fails', () => {
    const scalarAction = enqueueVolumeIntent(emptyPins(), null, 0.4, null, 100);
    const scalarOwner = scalarAction.pins.scalar;
    const scalarFire = consumeQueuedVolumeWrite(scalarAction.queued);
    expect(scalarFire.write?.scalarOwner).toBe(scalarOwner);
    expect(scalarFire.remaining).toBeNull();

    // The scalar RPC succeeded, but its authoritative readback is still pending.
    const muteAction = enqueueVolumeIntent(
      scalarAction.pins,
      scalarFire.remaining,
      0.4,
      true,
      200,
    );
    const muteOwner = muteAction.pins.muted;
    expect(muteAction.queued.scalarOwner).toBeNull();
    expect(muteAction.queued.muteOwner).toBe(muteOwner);

    const muteFire = consumeQueuedVolumeWrite(muteAction.queued);
    expect(muteFire.write).not.toBeNull();
    const failed = applyVolumeWriteFailureOwnership(
      muteAction.pins,
      muteFire.write!,
      failureContext({ currentGeneration: 7, consumedGeneration: 7 }),
    );
    expect(failed.applied).toBe(true);
    expect(failed.scalarCleared).toBe(false);
    expect(failed.muteCleared).toBe(true);
    expect(failed.pins.scalar).toBe(scalarOwner);
    expect(failed.pins.muted).toBeNull();
  });

  it('keeps both owners in one unsent scalar-plus-mute batch and clears both on failure', () => {
    const scalarAction = enqueueVolumeIntent(emptyPins(), null, 0.3, null, 100);
    const scalarOwner = scalarAction.pins.scalar;
    const muteAction = enqueueVolumeIntent(
      scalarAction.pins,
      scalarAction.queued,
      0.3,
      true,
      200,
    );
    const muteOwner = muteAction.pins.muted;
    expect(muteAction.queued.scalarOwner).toBe(scalarOwner);
    expect(muteAction.queued.muteOwner).toBe(muteOwner);
    expect(muteAction.queued.muted).toBe(true);

    const consumed = consumeQueuedVolumeWrite(muteAction.queued);
    const failed = applyVolumeWriteFailureOwnership(
      muteAction.pins,
      consumed.write!,
      failureContext({ currentGeneration: 11, consumedGeneration: 11 }),
    );
    expect(failed).toMatchObject({
      applied: true,
      scalarCleared: true,
      muteCleared: true,
      pins: { scalar: null, muted: null },
    });
  });

  it('keeps a queued mute owner through a scalar tail and lets the latest mute owner win', () => {
    const muteOn = enqueueVolumeIntent(emptyPins(), null, 0.4, true, 100);
    const muteOnOwner = muteOn.pins.muted;
    const scalarTail = enqueueVolumeIntent(
      muteOn.pins,
      muteOn.queued,
      0.8,
      null,
      200,
    );
    expect(scalarTail.queued.scalar).toBe(0.8);
    expect(scalarTail.queued.muted).toBe(true);
    expect(scalarTail.queued.muteOwner).toBe(muteOnOwner);
    expect(scalarTail.queued.scalarOwner).toBe(scalarTail.pins.scalar);

    const muteOff = enqueueVolumeIntent(
      scalarTail.pins,
      scalarTail.queued,
      0.8,
      false,
      300,
    );
    expect(muteOff.queued.muted).toBe(false);
    expect(muteOff.queued.muteOwner).toBe(muteOff.pins.muted);
    expect(muteOff.queued.muteOwner).not.toBe(muteOnOwner);
    expect(muteOff.queued.scalarOwner).toBe(scalarTail.pins.scalar);
  });

  it('does not let an old failed owner clear its replacement', () => {
    const oldAction = enqueueVolumeIntent(emptyPins(), null, 0.2, null, 100);
    const oldFire = consumeQueuedVolumeWrite(oldAction.queued);
    const replacement = enqueueVolumeIntent(oldAction.pins, null, 0.9, null, 200);

    const failed = applyVolumeWriteFailureOwnership(
      replacement.pins,
      oldFire.write!,
      failureContext({
        currentGeneration: 5,
        consumedGeneration: 5,
        currentSequence: 2,
        consumedSequence: 1,
      }),
    );
    expect(failed.applied).toBe(true);
    expect(failed.scalarCleared).toBe(false);
    expect(failed.pins.scalar).toBe(replacement.pins.scalar);
    expect(failed.pins.scalar).not.toBe(oldAction.pins.scalar);
  });

  it('does not inherit mute value or ownership after that batch is consumed', () => {
    const muteAction = enqueueVolumeIntent(emptyPins(), null, 0.4, true, 100);
    const muteFire = consumeQueuedVolumeWrite(muteAction.queued);
    const sliderAction = enqueueVolumeIntent(
      muteAction.pins,
      muteFire.remaining,
      0.9,
      null,
      200,
    );

    expect(muteFire.write?.muted).toBe(true);
    expect(sliderAction.pins.muted).toBe(muteAction.pins.muted);
    expect(sliderAction.queued).toMatchObject({
      scalar: 0.9,
      muted: null,
      muteOwner: null,
    });
    expect(sliderAction.queued.scalarOwner).toBe(sliderAction.pins.scalar);
  });

  it('does not apply failure ownership from a stale target generation', () => {
    const scalarAction = enqueueVolumeIntent(emptyPins(), null, 0.6, null, 100);
    const muteAction = enqueueVolumeIntent(
      scalarAction.pins,
      scalarAction.queued,
      0.6,
      true,
      200,
    );
    const consumed = consumeQueuedVolumeWrite(muteAction.queued);
    const failed = applyVolumeWriteFailureOwnership(
      muteAction.pins,
      consumed.write!,
      failureContext({ currentGeneration: 9, consumedGeneration: 8 }),
    );

    expect(failed.applied).toBe(false);
    expect(failed.scalarCleared).toBe(false);
    expect(failed.muteCleared).toBe(false);
    expect(failed.pins).toBe(muteAction.pins);
  });

  it('applies an older scalar failure only to its owner and preserves later mute refresh', () => {
    const scalarAction = enqueueVolumeIntent(emptyPins(), null, 0.6, null, 100);
    const scalarFire = consumeQueuedVolumeWrite(scalarAction.queued);
    const muteAction = enqueueVolumeIntent(
      scalarAction.pins,
      scalarFire.remaining,
      0.6,
      true,
      200,
    );
    const muteFire = consumeQueuedVolumeWrite(muteAction.queued);

    const earlierFailure = applyVolumeWriteFailureOwnership(
      muteAction.pins,
      scalarFire.write!,
      failureContext({ currentSequence: 2, consumedSequence: 1 }),
    );
    expect(earlierFailure).toMatchObject({
      applied: true,
      scalarCleared: true,
      muteCleared: false,
      clearGlobalRefresh: false,
    });
    expect(earlierFailure.pins.scalar).toBeNull();
    expect(earlierFailure.pins.muted).toBe(muteAction.pins.muted);

    const laterFailure = applyVolumeWriteFailureOwnership(
      earlierFailure.pins,
      muteFire.write!,
      failureContext({ currentSequence: 2, consumedSequence: 2 }),
    );
    expect(laterFailure).toMatchObject({
      scalarCleared: false,
      muteCleared: true,
      clearGlobalRefresh: true,
      pins: { scalar: null, muted: null },
    });
  });

  it('applies an older mute failure only to its owner and preserves later scalar refresh', () => {
    const muteAction = enqueueVolumeIntent(emptyPins(), null, 0.4, true, 100);
    const muteFire = consumeQueuedVolumeWrite(muteAction.queued);
    const scalarAction = enqueueVolumeIntent(
      muteAction.pins,
      muteFire.remaining,
      0.8,
      null,
      200,
    );
    const scalarFire = consumeQueuedVolumeWrite(scalarAction.queued);

    const earlierFailure = applyVolumeWriteFailureOwnership(
      scalarAction.pins,
      muteFire.write!,
      failureContext({ currentSequence: 2, consumedSequence: 1 }),
    );
    expect(earlierFailure).toMatchObject({
      applied: true,
      scalarCleared: false,
      muteCleared: true,
      clearGlobalRefresh: false,
    });
    expect(earlierFailure.pins.scalar).toBe(scalarAction.pins.scalar);
    expect(earlierFailure.pins.muted).toBeNull();

    const laterFailure = applyVolumeWriteFailureOwnership(
      earlierFailure.pins,
      scalarFire.write!,
      failureContext({ currentSequence: 2, consumedSequence: 2 }),
    );
    expect(laterFailure).toMatchObject({
      scalarCleared: true,
      muteCleared: false,
      clearGlobalRefresh: true,
      pins: { scalar: null, muted: null },
    });
  });

  it('rejects failure ownership when the target no longer matches', () => {
    const action = enqueueVolumeIntent(emptyPins(), null, 0.7, null, 100);
    const consumed = consumeQueuedVolumeWrite(action.queued);
    const failed = applyVolumeWriteFailureOwnership(
      action.pins,
      consumed.write!,
      failureContext({ targetMatches: false }),
    );

    expect(failed).toMatchObject({
      applied: false,
      scalarCleared: false,
      muteCleared: false,
      clearGlobalRefresh: false,
    });
    expect(failed.pins).toBe(action.pins);
  });
});
