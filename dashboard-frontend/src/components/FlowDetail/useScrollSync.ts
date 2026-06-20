/**
 * Scroll-syncs N pane containers (D10: the three JSON panes scroll together). We sync the
 * scrollable CONTAINER elements directly (NOT through react-virtual — the panes are not
 * virtualized; the constraint is that sync must not fight virtualization, so the inspector panes
 * are plain overflow:auto). When one pane scrolls, its `scrollTop`/`scrollLeft` are mirrored onto
 * the siblings, guarded by a re-entrancy flag so the programmatic sets don't loop.
 *
 * Returns one `onScroll` handler factory: `bind(i)` for pane `i`. Each pane registers its element
 * via the returned `refFor(i)` ref callback (a plain ref object).
 */
import { createRef, useCallback, useRef } from 'react';

export interface ScrollSync {
  /** A stable ref OBJECT to attach to pane `i`'s scroll container. */
  refFor: (i: number) => React.RefObject<HTMLDivElement>;
  /** The `onScroll` handler for pane `i`. */
  bind: (i: number) => React.UIEventHandler<HTMLDivElement>;
}

export function useScrollSync(count: number): ScrollSync {
  // One ref object per pane, created once and kept stable across renders. `createRef` types
  // the object as `RefObject<HTMLDivElement>` (the shape a `ref=` prop expects).
  const refsRef = useRef<React.RefObject<HTMLDivElement>[] | null>(null);
  if (!refsRef.current || refsRef.current.length !== count) {
    refsRef.current = Array.from({ length: count }, () => createRef<HTMLDivElement>());
  }
  const refs = refsRef.current;

  // Re-entrancy guard: while we programmatically set siblings, their onScroll must no-op.
  const syncing = useRef(false);

  const refFor = useCallback((i: number) => refs[i]!, [refs]);

  const bind = useCallback(
    (i: number): React.UIEventHandler<HTMLDivElement> =>
      (ev) => {
        if (syncing.current) return;
        const src = ev.currentTarget;
        // Setting `scrollTop`/`scrollLeft` does NOT fire `onScroll` synchronously — the
        // browser dispatches those scroll events asynchronously. So we hold the guard up
        // until the next animation frame (a microtask is too early in some engines), by which
        // point the mirrored scroll events have fired and been ignored. Without this, the
        // siblings' echo would ping-pong.
        syncing.current = true;
        for (let j = 0; j < refs.length; j++) {
          if (j === i) continue;
          const el = refs[j]?.current;
          if (!el) continue;
          if (el.scrollTop !== src.scrollTop) el.scrollTop = src.scrollTop;
          if (el.scrollLeft !== src.scrollLeft) el.scrollLeft = src.scrollLeft;
        }
        const release = () => {
          syncing.current = false;
        };
        if (typeof requestAnimationFrame === 'function') requestAnimationFrame(release);
        else setTimeout(release, 0);
      },
    [refs],
  );

  return { refFor, bind };
}
