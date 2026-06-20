import { describe, it, expect, beforeEach } from 'vitest';
import { StrictMode } from 'react';
import { render, cleanup } from '@testing-library/react';
import { DemoViz } from './DemoViz';
import { demoVizCounters, resetDemoVizCounters } from './demoVizState';

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
