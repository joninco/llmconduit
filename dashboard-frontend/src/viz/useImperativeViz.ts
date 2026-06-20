/**
 * `useImperativeViz` — the reusable wrapper for ALL imperative visualizations
 * (d3-force, d3-sankey, uPlot). It runs `setup` inside `useLayoutEffect` and ALWAYS
 * runs the returned `cleanup` (destroy sim / dispose uPlot / remove SVG) on unmount or
 * before a re-run. This makes the viz StrictMode-idempotent: React 18 StrictMode mounts,
 * unmounts, and remounts in dev, so a setup that leaks (a running d3 simulation, a
 * duplicated SVG/canvas node) would double up. The cleanup contract prevents that.
 *
 * The concrete d3/uPlot instances are D10-D12; this file provides ONLY the wrapper +
 * the cleanup discipline (covered by a StrictMode double-invoke test).
 */
import { useLayoutEffect, useRef, type RefObject, type DependencyList } from 'react';

/** A setup returns its teardown. Returning `void` means "nothing to clean up". */
export type VizCleanup = (() => void) | void;

/**
 * @param container ref to the DOM node the viz mounts into.
 * @param setup imperative init; receives the (non-null) container element, returns cleanup.
 * @param deps re-run dependencies (like useEffect). Defaults to `[]` (mount-only).
 */
export function useImperativeViz<E extends HTMLElement>(
  container: RefObject<E | null>,
  setup: (el: E) => VizCleanup,
  deps: DependencyList = [],
): void {
  // Keep the latest setup without making it a dep (so callers can pass inline closures).
  const setupRef = useRef(setup);
  setupRef.current = setup;

  useLayoutEffect(() => {
    const el = container.current;
    if (!el) return;
    const cleanup = setupRef.current(el);
    return () => {
      // FULL teardown on every unmount/re-run — StrictMode safety.
      if (typeof cleanup === 'function') cleanup();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
}
