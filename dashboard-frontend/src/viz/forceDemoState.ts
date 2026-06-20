import type { Simulation, SimulationNodeDatum } from 'd3-force';

/**
 * Test-observable handle on the real d3-force simulation the ForceDemoViz mounts. The
 * StrictMode test inspects this to assert the simulation is STOPPED and its tick handler
 * cleared after teardown (finding 7). Kept out of the .tsx so the component file exports
 * only the component (react-refresh).
 */
interface ForceDemoNode extends SimulationNodeDatum {
  id: string;
}

export const forceDemoState: {
  /** The most-recently-created simulation (the live one while mounted). */
  simulation: Simulation<ForceDemoNode, undefined> | null;
  setups: number;
  cleanups: number;
  /** Count of cleanups that observed both `sim.stop()` AND a cleared tick handler. */
  stoppedCleanly: number;
  /** Whether EVERY torn-down simulation had its tick handler cleared. */
  allTickHandlersCleared: boolean;
} = {
  simulation: null,
  setups: 0,
  cleanups: 0,
  stoppedCleanly: 0,
  allTickHandlersCleared: true,
};

export function resetForceDemoState(): void {
  forceDemoState.simulation = null;
  forceDemoState.setups = 0;
  forceDemoState.cleanups = 0;
  forceDemoState.stoppedCleanly = 0;
  forceDemoState.allTickHandlersCleared = true;
}

export type { ForceDemoNode };
