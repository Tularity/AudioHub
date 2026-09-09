// Root-level modal background isolation shared by the daemon recovery overlay
// and ConfirmDialog.
//
// A plain "remember the old attributes, then restore them" effect is not
// composable: the recovery overlay can appear while a confirmation is open.
// If either layer closes first, it would remove `inert` that the other layer
// still owns. This tiny reference-counted registry restores the original DOM
// state only after the last modal releases a node.

interface InertTarget {
  hasAttribute(name: string): boolean;
  getAttribute(name: string): string | null;
  setAttribute(name: string, value: string): void;
  removeAttribute(name: string): void;
}

interface LockState {
  count: number;
  inert: boolean;
  ariaHidden: string | null;
}

/**
 * Attribute lock registry extracted so overlap/restore semantics remain
 * testable without adding a browser DOM implementation to the unit-test tree.
 */
export function createInertRegistry<T extends InertTarget>() {
  const states = new WeakMap<T, LockState>();

  return {
    acquire(node: T): void {
      const existing = states.get(node);
      if (existing) {
        existing.count += 1;
        return;
      }
      states.set(node, {
        count: 1,
        inert: node.hasAttribute('inert'),
        ariaHidden: node.getAttribute('aria-hidden'),
      });
      node.setAttribute('inert', '');
      node.setAttribute('aria-hidden', 'true');
    },

    release(node: T): void {
      const state = states.get(node);
      if (!state) return;
      state.count -= 1;
      if (state.count > 0) return;
      states.delete(node);

      if (state.inert) node.setAttribute('inert', '');
      else node.removeAttribute('inert');
      if (state.ariaHidden === null) node.removeAttribute('aria-hidden');
      else node.setAttribute('aria-hidden', state.ariaHidden);
    },
  };
}

const registry = createInertRegistry<HTMLElement>();

/**
 * Make every sibling of `modal` inaccessible until the returned cleanup runs.
 * Newly mounted siblings are covered as well (for example, a ConfirmDialog
 * opened while the recovery overlay is already visible).
 *
 * `except` is used by ConfirmDialog for the always-mounted recovery overlay:
 * it is hidden normally, but must remain able to supersede the confirmation
 * if the daemon disconnects.
 */
export function inertSiblings(
  modal: HTMLElement,
  except: readonly HTMLElement[] = [],
  onlyEarlier = false,
): () => void {
  const host = modal.parentElement;
  if (!host) return () => undefined;

  const excluded = new Set<HTMLElement>([modal, ...except]);
  const held = new Set<HTMLElement>();
  const acquire = (node: Element) => {
    if (!(node instanceof HTMLElement) || excluded.has(node) || held.has(node)) return;
    // A Sheet owns the layers below it. A later nested Sheet must stay usable.
    if (onlyEarlier && !(node.compareDocumentPosition(modal) & Node.DOCUMENT_POSITION_FOLLOWING)) return;
    held.add(node);
    registry.acquire(node);
  };

  for (const node of host.children) acquire(node);
  const observer = new MutationObserver(() => {
    for (const node of host.children) acquire(node);
  });
  observer.observe(host, { childList: true });

  return () => {
    observer.disconnect();
    for (const node of held) registry.release(node);
    held.clear();
  };
}
