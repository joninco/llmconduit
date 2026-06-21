/**
 * Test-observable counters for `RadialTopology` (D12). Kept out of the .tsx so the component file
 * exports only the component (react-refresh) — the same split `forceDemoState`/`sparklineState` use.
 *
 * The StrictMode test asserts `setups`/`cleanups` balance (no leaked sim, no duplicate SVG) and that
 * every torn-down `forceSimulation` had `stop()` CALLED and its tick handler cleared — the d3-force
 * teardown contract (`useImperativeViz`). These are pure COUNTERS: the simulation itself is NOT
 * retained here (finding 8) — a module-global handle would outlive the unmount and keep the stopped
 * graph alive; the component's cleanup closure owns the only reference and releases it.
 */
export const radialTopologyState: {
  setups: number;
  cleanups: number;
  /** Times a simulation's `stop()` was actually invoked on teardown (spied in setup). */
  stopCalls: number;
  /** Whether EVERY torn-down simulation had its tick handler cleared. */
  allTickHandlersCleared: boolean;
} = {
  setups: 0,
  cleanups: 0,
  stopCalls: 0,
  allTickHandlersCleared: true,
};

export function resetRadialTopologyState(): void {
  radialTopologyState.setups = 0;
  radialTopologyState.cleanups = 0;
  radialTopologyState.stopCalls = 0;
  radialTopologyState.allTickHandlersCleared = true;
}
