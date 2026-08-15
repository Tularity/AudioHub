// Click-away + Escape for popovers that open on click.
//
// Extracted from `components/TopControls.tsx` when the peer-card selects grew a
// second copy of the same behaviour. Two hand-written copies of a listener that
// has to get the *down* edge right is how the two surfaces drift apart.
//
// `pointerdown` rather than `click`: a `click` listener fires after the button
// has already re-rendered, and on the press that closes a menu the browser
// delivers the close *and* the button's own toggle, so the menu reopens
// immediately. Listening on the down edge and checking containment is the
// version that does not fight itself.

import { useEffect, useRef } from 'react';

export function useDismiss<T extends HTMLElement = HTMLDivElement>(
  open: boolean,
  close: () => void,
) {
  const host = useRef<T>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      const el = e.target;
      if (el instanceof Node && host.current?.contains(el)) return;
      close();
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') close(); };
    document.addEventListener('pointerdown', onDown, true);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('pointerdown', onDown, true);
      document.removeEventListener('keydown', onKey);
    };
  }, [open, close]);
  return host;
}
