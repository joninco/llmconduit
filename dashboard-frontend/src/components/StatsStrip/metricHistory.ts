/**
 * Per-window metric history — the ring buffers the StatsStrip sparklines read.
 *
 * Each `MetricTick` (and the seed `/metrics`) carries `windows.{m1,m5,h1}`, every one a full
 * `MetricWindow` (the 8 chip metrics). A sparkline for a chosen window+metric is the recent
 * history of that `windows[w][metric]` scalar. So we keep, PER window, a capped ring of the last
 * N `MetricWindow` samples; a chip extracts one field into a `number[]` for its Sparkline.
 *
 * Pure + DOM-free (a plain reducer over an immutable state object) so it is trivially testable
 * and can live in `useMemo`/`useRef` without React entanglement.
 */
import type { MetricWindow, MetricsResponse, MetricTickPayload } from '../../api/types';

/** The three sliding windows the selector switches between. */
export type WindowKey = 'm1' | 'm5' | 'h1';
export const WINDOW_KEYS: readonly WindowKey[] = ['m1', 'm5', 'h1'];
export const WINDOW_LABELS: Record<WindowKey, string> = { m1: '1m', m5: '5m', h1: '1h' };

/** The chip metrics, in strip display order. Each maps to a `MetricWindow` field. */
export type MetricKey =
  | 'reqs_per_sec'
  | 'active_streams'
  | 'error_pct'
  | 'p50'
  | 'p95'
  | 'p99'
  | 'tokens_per_sec'
  | 'cost_per_min';

/** Sparkline depth (samples retained per window). Spec: 60-sample sparklines. */
export const HISTORY_DEPTH = 60;

/** Immutable history state: a capped ring of `MetricWindow` samples per window. */
export interface MetricHistory {
  m1: MetricWindow[];
  m5: MetricWindow[];
  h1: MetricWindow[];
}

export function emptyHistory(): MetricHistory {
  return { m1: [], m5: [], h1: [] };
}

/** Append `sample` to `ring`, dropping the oldest beyond `cap` (returns a NEW array). */
function pushCapped(ring: MetricWindow[], sample: MetricWindow, cap = HISTORY_DEPTH): MetricWindow[] {
  const next = ring.length >= cap ? ring.slice(ring.length - cap + 1) : ring.slice();
  next.push(sample);
  return next;
}

/**
 * Fold one tick's `windows` into the history (one sample per window). Accepts the `MetricTick`
 * WS payload OR the `/metrics` REST response — both expose the same `windows` shape — so the
 * seed read and the live stream share one accumulator. Returns a NEW `MetricHistory`.
 */
export function appendTick(
  history: MetricHistory,
  tick: Pick<MetricTickPayload | MetricsResponse, 'windows'>,
): MetricHistory {
  return {
    m1: pushCapped(history.m1, tick.windows.m1),
    m5: pushCapped(history.m5, tick.windows.m5),
    h1: pushCapped(history.h1, tick.windows.h1),
  };
}

/** Extract the `metric` field across a window's ring into the Sparkline's `number[]`. */
export function seriesFor(history: MetricHistory, window: WindowKey, metric: MetricKey): number[] {
  return history[window].map((w) => w[metric]);
}

/** The newest sample for a window (the live chip VALUE), or null before any tick. */
export function latest(history: MetricHistory, window: WindowKey): MetricWindow | null {
  const ring = history[window];
  return ring.length ? ring[ring.length - 1]! : null;
}

/** The previous sample for a window (drives the chip's delta arrow), or null. */
export function previous(history: MetricHistory, window: WindowKey): MetricWindow | null {
  const ring = history[window];
  return ring.length >= 2 ? ring[ring.length - 2]! : null;
}
