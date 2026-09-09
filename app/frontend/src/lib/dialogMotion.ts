import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { CSSProperties, RefObject } from 'react';
import { consumeActivationOrigin } from './pointerOrigin';
import type { Rect } from './pointerOrigin';

export interface DialogAnchor {
  rect: Rect;
}
export interface DialogPose { x: number; y: number; scale: number }

/** The compact surface begins at the source, not a few pixels from the dialog. */
export function dialogTransform(target: Rect, source: Rect | null): DialogPose {
  if (!source || target.width <= 0 || target.height <= 0) return { x: 0, y: 0, scale: 0.15 };
  return {
    x: source.left + source.width / 2 - (target.left + target.width / 2),
    y: source.top + source.height / 2 - (target.top + target.height / 2),
    scale: 0.15,
  };
}

/** Capture before modal focus moves. Keyboard activation has a source too. */
export function captureDialogAnchor(): DialogAnchor | null {
  const activation = consumeActivationOrigin(Date.now());
  if (!activation) return null;
  const r = activation.element?.isConnected ? activation.element.getBoundingClientRect() : null;
  return {
    rect: r && r.width > 0 && r.height > 0
      ? { left: r.left, top: r.top, width: r.width, height: r.height }
      : activation.rect ?? {
      left: activation.point.x - 16, top: activation.point.y - 16, width: 32, height: 32,
    },
  };
}

/** Freeze the opening offset before the panel exists, as in x-tier's Dialog. */
export function captureDialogPose(): DialogPose {
  return dialogTransform(
    { left: 0, top: 0, width: window.innerWidth, height: window.innerHeight },
    captureDialogAnchor()?.rect ?? null,
  );
}

export function dialogStyle(pose: DialogPose): CSSProperties {
  return { '--dialog-x': `${pose.x}px`, '--dialog-y': `${pose.y}px` } as CSSProperties;
}

/** A separate mounting commit gives @starting-style its real origin on first paint. */
export function useDialogOpening(): DialogPose | null {
  const captured = useRef<DialogPose | null>(null);
  const [pose, setPose] = useState<DialogPose | null>(null);
  useLayoutEffect(() => {
    captured.current ??= captureDialogPose();
    setPose(captured.current);
  }, []);
  return pose;
}

/** Wait for the CSS transitions themselves; content spinners must not hold a modal open. */
export function useExitMotion(
  closing: boolean,
  card: RefObject<HTMLElement | null>,
  mask: RefObject<HTMLElement | null> | null,
  finish: () => void,
): void {
  useEffect(() => {
    if (!closing) return;
    let cancelled = false;
    let complete = false;
    const done = () => {
      if (cancelled || complete) return;
      complete = true;
      clearTimeout(safety);
      finish();
    };
    // Only a stalled browser uses this backstop. Normal lifetime comes from CSS.
    const safety = setTimeout(done, 1200);
    const frame = requestAnimationFrame(() => {
      const animations = [card.current, mask?.current].flatMap(node =>
        typeof node?.getAnimations === 'function' ? node.getAnimations() : []);
      const finite = animations.filter(animation =>
        (animation.playState === 'running' || animation.playState === 'paused')
        && Number.isFinite(animation.effect?.getComputedTiming().endTime));
      void Promise.allSettled(finite.map(animation => animation.finished)).then(done);
    });
    return () => {
      cancelled = true;
      cancelAnimationFrame(frame);
      clearTimeout(safety);
    };
  }, [closing, card, mask, finish]);
}
