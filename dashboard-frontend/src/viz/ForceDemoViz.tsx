/**
 * REAL d3-force demo for the StrictMode teardown test (finding 7). It starts an actual
 * `forceSimulation` and, in the REQUIRED cleanup, stops it and clears its tick handler —
 * exactly the discipline every D12 force/sankey viz must follow. The test mounts this
 * under StrictMode (mount→unmount→remount) and asserts the live simulation is stopped and
 * leaves no running timer.
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
    forceDemoState.simulation = sim;
    void ticks;
    void el;

    return () => {
      forceDemoState.cleanups += 1;
      // REQUIRED teardown: halt the simulation timer and drop the tick handler so no
      // animation frame survives the unmount (StrictMode-safe). We observe BOTH on THIS
      // simulation (not just the live one) so every torn-down sim is verified stopped.
      sim.stop();
      sim.on('tick', null);
      const tickCleared = sim.on('tick') == null;
      if (tickCleared) forceDemoState.stoppedCleanly += 1;
      else forceDemoState.allTickHandlersCleared = false;
    };
  }, []);

  return <div ref={ref} data-testid="force-demo-container" />;
}
