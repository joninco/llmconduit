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

/**
 * A viz teardown — REQUIRED (finding 7). Every `setup` MUST return its cleanup
 * (stop the d3 simulation, dispose the uPlot, remove the SVG). Making this mandatory at
 * the type level forces each D10-D12 viz to declare teardown, so a StrictMode double
 * mount→unmount→remount can never leak a running simulation or duplicate a node.
 */
export type VizCleanup = () => void;

/**
 * @param container ref to the DOM node the viz mounts into.
 * @param setup imperative init; receives the (non-null) container element, returns its
 *   REQUIRED cleanup. If a viz genuinely has nothing to tear down it must still return a
 *   no-op `() => {}` — this is intentional friction so teardown is never silently skipped.
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
    // FULL teardown on every unmount/re-run — StrictMode safety. cleanup is required,
    // so this always runs the viz's own disposal.
    return cleanup;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
}
