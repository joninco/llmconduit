import { describe, it, expect, beforeEach } from 'vitest';
import { StrictMode } from 'react';
import { render, cleanup } from '@testing-library/react';
import { DemoViz } from './DemoViz';
import { demoVizCounters, resetDemoVizCounters } from './demoVizState';
import { ForceDemoViz } from './ForceDemoViz';
import { forceDemoState, resetForceDemoState } from './forceDemoState';

describe('useImperativeViz — StrictMode-safe cleanup', () => {
  beforeEach(() => {
    resetDemoVizCounters();
    cleanup();
  });

  it('does not duplicate the SVG under StrictMode double-invoke', () => {
    const { container, unmount } = render(
      <StrictMode>
        <DemoViz />
      </StrictMode>,
    );
    // StrictMode mounts → unmounts → remounts in dev. Exactly ONE svg must survive.
    const svgs = container.querySelectorAll('svg[data-demo-viz="true"]');
    expect(svgs).toHaveLength(1);

    // Every setup was matched by a cleanup of the previous mount (balanced ± the live one).
    expect(demoVizCounters.cleanups).toBe(demoVizCounters.setups - 1);

    unmount();
    // After unmount the final cleanup runs — fully balanced, nothing leaked.
    expect(demoVizCounters.cleanups).toBe(demoVizCounters.setups);
  });

  it('mount → unmount → remount leaves a single SVG and no leaked instance', () => {
    const first = render(
      <StrictMode>
        <DemoViz />
      </StrictMode>,
    );
    first.unmount();
    resetDemoVizCounters();

    const second = render(
      <StrictMode>
        <DemoViz />
      </StrictMode>,
    );
    const svgs = second.container.querySelectorAll('svg[data-demo-viz="true"]');
    expect(svgs).toHaveLength(1);
    second.unmount();
    expect(demoVizCounters.cleanups).toBe(demoVizCounters.setups);
  });
});

describe('useImperativeViz — a REAL d3-force simulation is stopped on teardown (findings 7+8)', () => {
  beforeEach(() => {
    resetForceDemoState();
    cleanup();
  });

  it('StrictMode double-invoke CALLS simulation.stop() and clears its tick handler', () => {
    const { unmount } = render(
      <StrictMode>
        <ForceDemoViz />
      </StrictMode>,
    );
    // A real forceSimulation was created and is tracked.
    expect(forceDemoState.simulation).not.toBeNull();
    // StrictMode double-invokes: setup ran twice, and the discarded first mount was torn
    // down (cleanup >= 1). Finding 8: `simulation.stop()` was actually CALLED on teardown
    // (a missing stop() call would leave stopCalls at 0 and fail here), and every
    // torn-down sim had its tick handler cleared.
    expect(forceDemoState.setups).toBeGreaterThanOrEqual(2);
    expect(forceDemoState.cleanups).toBeGreaterThanOrEqual(1);
    expect(forceDemoState.stopCalls).toBe(forceDemoState.cleanups);
    expect(forceDemoState.allTickHandlersCleared).toBe(true);
    expect(forceDemoState.stoppedCleanly).toBe(forceDemoState.cleanups);

    unmount();
    // After unmount the LIVE simulation is also torn down: fully balanced (no leak),
    // stop() called for every setup, tick handler cleared so the alpha-decay timer can
    // never re-fire it.
    expect(forceDemoState.cleanups).toBe(forceDemoState.setups);
    expect(forceDemoState.stopCalls).toBe(forceDemoState.setups);
    expect(forceDemoState.allTickHandlersCleared).toBe(true);
    expect(forceDemoState.simulation?.on('tick')).toBeUndefined();
  });
});
