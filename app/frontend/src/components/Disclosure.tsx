import { useCallback, useLayoutEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { useExitMotion } from '../lib/dialogMotion';

/** Keep the live detail tree only until its closing track has collapsed. */
export function Disclosure({ open, children }: { open: boolean; children: ReactNode }) {
  const [retained, setRetained] = useState(open);
  const ref = useRef<HTMLDivElement>(null);
  const finish = useCallback(() => setRetained(false), []);
  useLayoutEffect(() => { if (open) setRetained(true); }, [open]);
  useExitMotion(!open && retained, ref, null, finish);
  const present = open || retained;
  return (
    <div ref={ref} className="disclosure" data-open={open || undefined} hidden={!present} inert={!open}>
      <div className="disclosure-clip">{present ? children : null}</div>
    </div>
  );
}
