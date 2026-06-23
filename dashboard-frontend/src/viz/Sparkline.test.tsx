import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { StrictMode } from 'react';
import { act, cleanup, render } from '@testing-library/react';
import { Sparkline } from './Sparkline';
import { sparklineCounters, resetSparklineCounters } from './sparklineState';

/**
 * The 2D-canvas + Path2D + matchMedia jsdom gaps that uPlot needs are polyfilled globally in
 * `vitest.setup.ts` (uPlot draws on a DEFERRED commit, so per-test stubs would race teardown).
 * Here we only fix the per-element `getBoundingClientRect` sizing uPlot reads at construction.
 */
let restoreRect: (() => void) | null = null;

beforeEach(() => {
  resetSparklineCounters();
  const originalRect = HTMLElement.prototype.getBoundingClientRect;
  HTMLElement.prototype.getBoundingClientRect = function () {
    return { x: 0, y: 0, top: 0, left: 0, right: 96, bottom: 28, width: 96, height: 28, toJSON: () => ({}) } as DOMRect;
  };
  restoreRect = () => {
    HTMLElement.prototype.getBoundingClientRect = originalRect;
  };
});
afterEach(() => {
  cleanup();
  restoreRect?.();
  restoreRect = null;
  vi.restoreAllMocks();
});

describe('Sparkline — StrictMode-safe uPlot lifecycle', () => {
  it('renders exactly ONE <canvas> under StrictMode double-invoke (no duplicate)', () => {
    const { container, unmount } = render(
      <StrictMode>
        <Sparkline data={[1, 2, 3, 2, 4]} />
      </StrictMode>,
    );
    // StrictMode mounts → unmounts → remounts in dev. uPlot is recreated, but the discarded
    // instance is disposed (its canvas removed) → exactly one canvas survives.
    const canvases = container.querySelectorAll('canvas');
    expect(canvases.length).toBe(1);
    // Creates exceed destroys by exactly the ONE live instance (balanced minus the survivor).
    expect(sparklineCounters.destroys).toBe(sparklineCounters.creates - 1);

    unmount();
    // After unmount the final instance is destroyed: fully balanced, nothing leaked.
    expect(sparklineCounters.destroys).toBe(sparklineCounters.creates);
  });

  it('updates the live instance via setData WITHOUT recreating it (no churn on new data)', () => {
    const u = render(<Sparkline data={[1, 2, 3]} />); // not StrictMode → one create
    const createsAfterMount = sparklineCounters.creates;
    expect(createsAfterMount).toBe(1);
    const canvasBefore = u.container.querySelector('canvas');

    // New data prop → setData path, NOT a recreate.
    act(() => {
      u.rerender(<Sparkline data={[1, 2, 3, 4, 5]} />);
    });
    expect(sparklineCounters.creates).toBe(1); // unchanged — instance reused
    expect(u.container.querySelectorAll('canvas').length).toBe(1);
    expect(u.container.querySelector('canvas')).toBe(canvasBefore); // same node
  });

  it('accepts a (number|null)[] series with GAPS (null + non-finite) — renders one canvas, no throw', () => {
    // A `null` is an honest uPlot gap; a `NaN`/Infinity is NORMALIZED to `null` at the boundary
    // (uPlot treats ONLY `null` as a gap — a NaN would poison the scale). Both must render cleanly.
    const { container, rerender } = render(<Sparkline data={[1, null, 3, 2]} />);
    expect(container.querySelectorAll('canvas').length).toBe(1);
    // Pushing a non-finite value must not throw (it is coerced to a gap, not plotted as 0).
    act(() => {
      rerender(<Sparkline data={[1, Number.NaN, 3, Number.POSITIVE_INFINITY, 4]} />);
    });
    expect(container.querySelectorAll('canvas').length).toBe(1);
    expect(sparklineCounters.creates).toBe(1); // setData path, not a recreate
  });

  it('exposes an accessible label and fixed size', () => {
    const { getByTestId } = render(<Sparkline data={[1, 2]} label="req/s trend" width={120} height={30} />);
    const el = getByTestId('sparkline');
    expect(el.getAttribute('aria-label')).toBe('req/s trend');
    expect(el).toHaveStyle({ width: '120px', height: '30px' });
  });
});

describe('Sparkline — prefers-reduced-motion', () => {
  it('still renders a single static canvas when reduced motion is set', () => {
    vi.stubGlobal('matchMedia', (q: string) => ({
      matches: q.includes('prefers-reduced-motion'),
      media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false,
    }));
    const { container, unmount } = render(<Sparkline data={[1, 3, 2]} />);
    expect(container.querySelectorAll('canvas').length).toBe(1);
    unmount();
    expect(sparklineCounters.destroys).toBe(sparklineCounters.creates);
    vi.unstubAllGlobals();
  });
});
