/**
 * Test-observable counters + simulation handle for `RadialTopology` (D12). Kept out of the
 * .tsx so the component file exports only the component (react-refresh) — the same split
 * `forceDemoState`/`sparklineState` use.
 *
 * The StrictMode test asserts `setups`/`cleanups` balance (no leaked sim, no duplicate SVG)
 * and that every torn-down `forceSimulation` had `stop()` CALLED and its tick handler cleared
 * — the d3-force teardown contract (`useImperativeViz`, findings 7+8).
 */
import type { Simulation, SimulationNodeDatum } from 'd3-force';

export const radialTopologyState: {
  /** The most-recently-created simulation (the live one while mounted). */
  simulation: Simulation<SimulationNodeDatum, undefined> | null;
  setups: number;
  cleanups: number;
  /** Times a simulation's `stop()` was actually invoked on teardown (spied in setup). */
  stopCalls: number;
  /** Whether EVERY torn-down simulation had its tick handler cleared. */
  allTickHandlersCleared: boolean;
} = {
  simulation: null,
  setups: 0,
  cleanups: 0,
  stopCalls: 0,
  allTickHandlersCleared: true,
};

export function resetRadialTopologyState(): void {
  radialTopologyState.simulation = null;
  radialTopologyState.setups = 0;
  radialTopologyState.cleanups = 0;
  radialTopologyState.stopCalls = 0;
  radialTopologyState.allTickHandlersCleared = true;
}
