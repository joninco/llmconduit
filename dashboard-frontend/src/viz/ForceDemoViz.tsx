/**
 * REAL d3-force demo for the StrictMode teardown test (findings 7+8). It starts an actual
 * `forceSimulation` and, in the REQUIRED cleanup, calls `sim.stop()` and clears its tick
 * handler — exactly the discipline every D12 force/sankey viz must follow. `sim.stop` is
 * wrapped with a counter so the test can assert it was ACTUALLY invoked (deleting the
 * cleanup's `sim.stop()` would drop the count to 0 and fail the test).
 */
import { useRef } from 'react';
import { forceSimulation, forceManyBody, forceCenter } from 'd3-force';
import { useImperativeViz } from './useImperativeViz';
import { forceDemoState, type ForceDemoNode } from './forceDemoState';

export function ForceDemoViz() {
  const ref = useRef<HTMLDivElement>(null);

  useImperativeViz(ref, (el) => {
    forceDemoState.setups += 1;
    const nodes: ForceDemoNode[] = [{ id: 'a' }, { id: 'b' }, { id: 'c' }];
    const sim = forceSimulation(nodes)
      .force('charge', forceManyBody())
      .force('center', forceCenter(0, 0));
    let ticks = 0;
    sim.on('tick', () => {
      ticks += 1;
    });
    // Spy on stop(): wrap it so an actual call increments the observable counter, while
    // still delegating to the real d3 stop (finding 8).
    const realStop = sim.stop.bind(sim);
    sim.stop = () => {
      forceDemoState.stopCalls += 1;
      return realStop();
    };
    forceDemoState.simulation = sim;
    void ticks;
    void el;

    return () => {
      forceDemoState.cleanups += 1;
      // REQUIRED teardown: halt the simulation timer and drop the tick handler so no
      // animation frame survives the unmount (StrictMode-safe). Observed on THIS sim
      // (not just the live one) so every torn-down sim is verified stopped.
      sim.stop();
      sim.on('tick', null);
      const tickCleared = sim.on('tick') == null;
      if (tickCleared) forceDemoState.stoppedCleanly += 1;
      else forceDemoState.allTickHandlersCleared = false;
    };
  }, []);

  return <div ref={ref} data-testid="force-demo-container" />;
}
