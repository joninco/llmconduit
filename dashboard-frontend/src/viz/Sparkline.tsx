/**
 * A minimal uPlot sparkline — the shared trend renderer for the StatsStrip chips (D11).
 *
 * Discipline (the contract every D10-D12 viz must satisfy):
 *  - The uPlot instance is created inside `useImperativeViz`'s `setup` and ALWAYS disposed in
 *    the returned cleanup, so React 18 StrictMode's mount→unmount→remount can never leak an
 *    instance or duplicate the <canvas> (asserted by Sparkline.test). It is recreated only when
 *    the chart SHAPE changes (size/stroke/motion) — pure data updates go through `setData` on
 *    the live instance (no churn while MetricTicks stream in).
 *  - `prefers-reduced-motion` is honored: uPlot draws no animation regardless, but we also drop
 *    the cursor/focus affordance so the sparkline is a fully static glyph under reduced motion.
 *  - Size is passed explicitly (jsdom reports 0 layout; the strip lays out fixed-size chips).
 *
 * Data is `number[]` (the y series, oldest→newest). uPlot wants an x axis too, so we synthesize
 * a 0..n-1 index axis — sparklines have no meaningful x labels.
 */
import { useEffect, useRef } from 'react';
import uPlot from 'uplot';
import 'uplot/dist/uPlot.min.css'; // `.uplot{position:relative}` etc. — required for canvas layout.
import { useImperativeViz, type VizCleanup } from './useImperativeViz';
import { colors, prefersReducedMotion } from '../design/tokens';
import { sparklineCounters } from './sparklineState';

export interface SparklineProps {
  /** The y series, oldest → newest. Empty/length<2 renders a flat baseline. */
  data: number[];
  width?: number;
  height?: number;
  /** Stroke color (hex). Defaults to the accent token. */
  stroke?: string;
  /** Accessible label for the sparkline region. */
  label?: string;
}

const DEFAULT_W = 96;
const DEFAULT_H = 28;

/** Build the uPlot options for a sparkline of a given size/stroke (no axes, no legend). */
function sparkOpts(width: number, height: number, stroke: string, reducedMotion: boolean): uPlot.Options {
  return {
    width,
    height,
    // No legend/axes/grid — a sparkline is pure trend; the container clips to size.
    legend: { show: false },
    // Reduced motion → no cursor focus affordance (a fully static glyph).
    cursor: { show: !reducedMotion, x: false, y: false },
    scales: { x: { time: false }, y: { auto: true } },
    axes: [{ show: false }, { show: false }],
    series: [
      {},
      { stroke, width: 1.5, points: { show: false }, fill: `${stroke}22` },
    ],
  };
}

/** `number[]` (y, oldest→newest) → uPlot's `[x[], y[]]` aligned data with a synthetic index x. */
function toAlignedData(data: number[]): uPlot.AlignedData {
  const n = data.length;
  const xs = new Array<number>(n);
  for (let i = 0; i < n; i++) xs[i] = i;
  return [xs, data];
}

export function Sparkline({
  data,
  width = DEFAULT_W,
  height = DEFAULT_H,
  stroke = colors.accent,
  label,
}: SparklineProps) {
  const ref = useRef<HTMLDivElement>(null);
  const plotRef = useRef<uPlot | null>(null);
  // Keep the latest data accessible to the (size/stroke-keyed) setup without re-running it.
  const dataRef = useRef(data);
  dataRef.current = data;
  const reduced = prefersReducedMotion();

  // Recreate the uPlot ONLY when the chart shape changes (size/stroke/motion). Data updates
  // below go through `setData` on the live instance, so streaming MetricTicks never churn it.
  useImperativeViz(
    ref,
    (el): VizCleanup => {
      const u = new uPlot(sparkOpts(width, height, stroke, reduced), toAlignedData(dataRef.current), el);
      plotRef.current = u;
      sparklineCounters.creates += 1;
      return () => {
        sparklineCounters.destroys += 1;
        u.destroy();
        plotRef.current = null;
      };
    },
    [width, height, stroke, reduced],
  );

  // Live data push: update the existing instance in place (no recreate) whenever `data` changes.
  // `useImperativeViz` runs in a LAYOUT effect; this ordinary effect runs after it on the SAME
  // commit, so on first mount the instance already exists. On a StrictMode discarded mount the
  // instance is null (already disposed) — guarded.
  useEffect(() => {
    const u = plotRef.current;
    if (!u) return;
    u.setData(toAlignedData(data));
  }, [data]);

  return (
    <div
      ref={ref}
      data-testid="sparkline"
      role="img"
      aria-label={label}
      style={{ width, height }}
    />
  );
}
