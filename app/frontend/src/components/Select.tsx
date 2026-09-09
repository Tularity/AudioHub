// The app's own single-choice dropdown, replacing the two native `<select>`
// elements that survived on the peer card (capture backend, bridge card).
//
// # Why not `<select>`
//
// A native select paints its popup in the operating system's own layer: the
// system font, the system corner radius, the system highlight colour, and on
// Windows a completely different frame from the one macOS draws. Everything
// else in this window is drawn by us, so the one control that was not looked
// borrowed — and the peer card is exactly where it stood out, because it sits
// two rows below a `Segmented` that *is* ours.
//
// The reference is the language menu in the top strip (`TopControls.tsx`):
// a button, a floating panel, `.pop-item` rows. This component reuses that
// panel's classes verbatim rather than restyling a copy, so the two surfaces
// cannot drift apart.
//
// # What it deliberately keeps from the native control
//
//   - `role="listbox"` / `role="option"` with `aria-selected`, so screen
//     readers still hear a single-choice list rather than a pile of buttons.
//   - A real `disabled` option state. Both call sites need it: a backend the
//     daemon reported as unavailable, and a bridge card that was unplugged.
//     Those rows must stay *visible* — they carry the only explanation the
//     user gets — while being genuinely unclickable.
//   - Type-ahead is NOT reimplemented. It is the one native behaviour worth
//     missing, and a half-working version (no timeout tuning, no locale
//     collation) is worse than none.
//
// # Click containment
//
// The whole peer card is a click target (it navigates to the detail view), so
// every interactive descendant has to stop propagation. The two call sites
// already stop it at the box level, but the popup renders *inside* the card,
// so its own rows stop it too — a card that navigated away mid-selection was
// the original bug that put `onClick={(e) => e.stopPropagation()}` on the box.

import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState } from 'react';
import { Icon } from './Icon';
import { useDismiss } from '../lib/dismiss';

export interface SelectOption<T extends string> {
  value: T;
  label: string;
  /** Re-evaluated every render: availability changes with daemon status. */
  disabled?: boolean;
  /** Secondary line, right-aligned, same slot the locale menu uses. */
  sub?: string;
}

/** Index of the next enabled option in `dir`, or -1 when there is none. */
function step<T extends string>(opts: SelectOption<T>[], from: number, dir: 1 | -1): number {
  for (let i = from + dir; i >= 0 && i < opts.length; i += dir) {
    if (!opts[i].disabled) return i;
  }
  return -1;
}

function edge<T extends string>(opts: SelectOption<T>[], dir: 1 | -1): number {
  return dir === 1 ? step(opts, -1, 1) : step(opts, opts.length, -1);
}

export function Select<T extends string>({
  testid, label, value, options, disabled = false, onChange,
}: {
  testid: string;
  /** Accessible name. The trigger has no visible label of its own. */
  label: string;
  value: T;
  options: SelectOption<T>[];
  disabled?: boolean;
  onChange: (v: T) => void;
}) {
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  const host = useDismiss<HTMLDivElement>(open, close);
  const btn = useRef<HTMLButtonElement>(null);
  const pop = useRef<HTMLDivElement>(null);
  const listId = useId();

  const selected = options.findIndex((o) => o.value === value);
  // `active` is the keyboard cursor. It is separate from `selected`: moving
  // through the list must not commit until Enter, or an arrow key would write
  // to the daemon once per row travelled.
  const [active, setActive] = useState(-1);

  // Opening from the keyboard starts on the current value; opening from the
  // pointer starts nowhere, so no row is highlighted under a mouse that has
  // not moved yet.
  const openWith = useCallback((cursor: number) => {
    if (disabled) return;
    setActive(cursor);
    setOpen(true);
  }, [disabled]);

  // Flip above the trigger when the panel would land past the viewport bottom.
  // `#view-root` scrolls, so an unflipped panel is not clipped — it just grows
  // the scroll height and the user has to scroll to see what they opened.
  const [up, setUp] = useState(false);
  useLayoutEffect(() => {
    if (!open) return;
    const el = pop.current;
    const anchor = btn.current;
    if (!el || !anchor) return;
    const r = anchor.getBoundingClientRect();
    setUp(r.bottom + el.offsetHeight + 12 > window.innerHeight && r.top > el.offsetHeight + 12);
  }, [open]);

  // Move focus into the panel so the arrow keys have somewhere to land, and
  // back to the trigger on close — losing focus to `<body>` after Escape is
  // how a keyboard user gets stranded.
  useEffect(() => {
    if (!open || active < 0) return;
    const row = pop.current?.querySelector<HTMLElement>(`[data-idx="${active}"]`);
    row?.focus();
  }, [open, active]);

  const commit = useCallback((i: number) => {
    const o = options[i];
    if (!o || o.disabled) return;
    close();
    btn.current?.focus();
    if (o.value !== value) onChange(o.value);
  }, [close, onChange, options, value]);

  const onKey = useCallback((e: React.KeyboardEvent) => {
    if (e.key === 'Escape') {
      if (!open) return;
      e.stopPropagation();
      close();
      btn.current?.focus();
      return;
    }
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      e.stopPropagation();
      const dir = e.key === 'ArrowDown' ? 1 : -1;
      if (!open) { openWith(selected >= 0 ? selected : edge(options, dir)); return; }
      const next = step(options, active < 0 ? (dir === 1 ? -1 : options.length) : active, dir);
      if (next >= 0) setActive(next);
      return;
    }
    if (e.key === 'Home' || e.key === 'End') {
      if (!open) return;
      e.preventDefault();
      e.stopPropagation();
      const i = edge(options, e.key === 'Home' ? 1 : -1);
      if (i >= 0) setActive(i);
      return;
    }
    if ((e.key === 'Enter' || e.key === ' ') && open && active >= 0) {
      e.preventDefault();
      e.stopPropagation();
      commit(active);
    }
  }, [active, close, commit, open, openWith, options, selected]);

  const current = selected >= 0 ? options[selected] : null;

  return (
    <div
      className={`sel${open ? ' open' : ''}`}
      ref={host}
      data-testid={`${testid}-ctl`}
      onKeyDown={onKey}
      onClick={(e) => e.stopPropagation()}
    >
      <button
        ref={btn}
        type="button"
        className="sel-btn"
        data-testid={testid}
        data-value={value}
        disabled={disabled}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={listId}
        aria-label={label}
        onClick={(e) => {
          e.stopPropagation();
          if (open) { close(); return; }
          openWith(-1);
        }}
      >
        <span className="sel-value" data-testid={`${testid}-value`}>
          {current ? current.label : ''}
        </span>
        <Icon name="chev" cls="sel-chev" />
      </button>
      <div
        ref={pop}
        className={`chrome-pop menu-pop sel-pop${open ? ' open' : ''}${up ? ' up' : ''}`}
        id={listId}
        role="listbox"
        aria-label={label}
        aria-activedescendant={open && active >= 0 ? `${listId}-${active}` : undefined}
        data-no-drag=""
      >
        {options.map((o, i) => (
          <button
            key={o.value}
            id={`${listId}-${i}`}
            type="button"
            role="option"
            data-idx={i}
            data-testid={`${testid}-opt-${o.value}`}
            aria-selected={i === selected}
            // Disabled rows stay in the list on purpose: for the capture
            // backend and the bridge card, the row text *is* the explanation
            // of why the thing cannot be picked.
            aria-disabled={o.disabled || undefined}
            className={`pop-item${i === selected ? ' on' : ''}${o.disabled ? ' off' : ''}`}
            // Feeds the cascade: item N fades in N steps late (styles.css,
            // `.chrome-pop.open .pop-item`).
            style={{ '--i': i } as React.CSSProperties}
            // `tabIndex={-1}`: the panel is reached from the trigger, and the
            // rows must not become eleven extra Tab stops on the card.
            tabIndex={-1}
            onClick={(e) => { e.stopPropagation(); commit(i); }}
            onPointerEnter={() => { if (!o.disabled) setActive(i); }}
          >
            <span className="pop-item-label">{o.label}</span>
            {o.sub ? <span className="pop-item-sub">{o.sub}</span> : null}
          </button>
        ))}
      </div>
    </div>
  );
}
