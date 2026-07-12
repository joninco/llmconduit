/** Timestamped instantaneous metrics history shared by every StatsStrip horizon. */
import type { EngineThroughputSample, HistoryPoint, InstantMetricSample, MetricsResponse } from '../../api/types';

export type WindowKey = 'm1' | 'm5' | 'h1';
export const WINDOW_KEYS: readonly WindowKey[] = ['m1', 'm5', 'h1'];
export const WINDOW_LABELS: Record<WindowKey, string> = { m1: '1m', m5: '5m', h1: '1h' };
const HORIZON_MS: Record<WindowKey, number> = { m1: 60_000, m5: 300_000, h1: 3_600_000 };
const MAX_HISTORY_MS = 3_600_000;

export type MetricKey =
  | 'accepted_per_sec'
  | 'terminal_per_sec'
  | 'active_streams_now'
  | 'failure_pct'
  | 'cancellation_pct'
  | 'p50_ms'
  | 'p95_ms'
  | 'p99_ms'
  | 'reported_tokens_per_sec'
  | 'cost_per_min';

/** Which telemetry seam supplies a chip and its sparkline. */
export type MetricSource = 'gateway' | 'reported' | 'engine';

export interface MetricPoint {
  seq: number;
  t: number;
  instant: InstantMetricSample;
  engineThroughput: EngineThroughputSample | null;
}

export type MetricHistory = MetricPoint[];
export function emptyHistory(): MetricHistory { return []; }

function normalized(points: MetricPoint[]): MetricHistory {
  const byIdentity = new Map<string, MetricPoint>();
  for (const point of points) {
    if (!Number.isFinite(point.t)) continue;
    byIdentity.set(`${point.seq}:${point.t}`, point);
  }
  const sorted = [...byIdentity.values()].sort((a, b) => a.t - b.t || a.seq - b.seq);
  const newest = sorted.at(-1)?.t;
  return newest === undefined ? [] : sorted.filter((point) => point.t >= newest - MAX_HISTORY_MS);
}

export function appendTick(history: MetricHistory, tick: MetricsResponse): MetricHistory {
  return normalized([...history, {
    seq: tick.metrics_seq,
    t: tick.generated_at_ms,
    instant: tick.instant,
    engineThroughput: tick.engine_throughput ?? null,
  }]);
}

export function mergeRetained(history: MetricHistory, points: HistoryPoint[]): MetricHistory {
  return normalized([
    ...points.map((point) => ({
      seq: point.cursors.metrics_seq,
      t: point.at_ms,
      instant: point.instant,
      engineThroughput: point.engine_throughput ?? null,
    })),
    ...history,
  ]);
}

export function horizon(history: MetricHistory, window: WindowKey, endAt?: number | null): MetricHistory {
  const end = endAt ?? history.at(-1)?.t;
  if (end === undefined) return [];
  const start = end - HORIZON_MS[window];
  return history.filter((point) => point.t >= start && point.t <= end);
}

export function metricUnavailable(sample: InstantMetricSample | null, metric: MetricKey): boolean {
  if (!sample) return true;
  if (metric === 'active_streams_now') return false;
  if (!sample.ready) return true;
  if ((metric === 'failure_pct' || metric === 'cancellation_pct') && sample.terminal_requests === 0) return true;
  if (metric === 'p50_ms' && sample.latency_samples === 0) return true;
  // A single observation is a real median, but it is not an operationally useful tail percentile.
  // Keep p95/p99 unavailable until the window contains at least two latency observations.
  if ((metric === 'p95_ms' || metric === 'p99_ms') && sample.latency_samples < 2) return true;
  if (metric === 'reported_tokens_per_sec' && sample.usage_samples === 0) return true;
  if (metric === 'cost_per_min' && sample.priced_samples === 0) return true;
  const value = sample[metric];
  return value === null || !Number.isFinite(value);
}

export function seriesFor(
  history: MetricHistory,
  metric: MetricKey,
  source: MetricSource = 'gateway',
): { times: number[]; values: number[] } {
  return {
    times: history.map((point) => point.t / 1_000),
    values: history.map(({ instant, engineThroughput }) => {
      if (metric === 'reported_tokens_per_sec' && source === 'engine') {
        return engineThroughput?.generated_tokens_per_sec ?? NaN;
      }
      const value = instant[metric];
      return metricUnavailable(instant, metric) || value === null ? NaN : value;
    }),
  };
}

export function latest(history: MetricHistory): InstantMetricSample | null {
  return history.at(-1)?.instant ?? null;
}

/** Newest backend-counter sample, optionally strictly older than a scrape timestamp. */
export function latestEngineThroughput(
  history: MetricHistory,
  beforeSampledAt?: number | null,
): EngineThroughputSample | null {
  for (let index = history.length - 1; index >= 0; index -= 1) {
    const sample = history[index]?.engineThroughput;
    if (sample && (beforeSampledAt == null || sample.sampled_at_ms < beforeSampledAt)) return sample;
  }
  return null;
}

/** Same request-activity predicate as the publisher's retained-sample seam. */
export function hasRequestActivity(sample: InstantMetricSample): boolean {
  return sample.active_streams_now > 0 || sample.accepted_requests > 0 || sample.terminal_requests > 0;
}

/** Newest request-bearing point, optionally strictly before a retained sample timestamp. */
export function latestActivity(history: MetricHistory, beforeAt?: number | null): MetricPoint | null {
  for (let index = history.length - 1; index >= 0; index -= 1) {
    const point = history[index]!;
    if ((beforeAt == null || point.t < beforeAt) && hasRequestActivity(point.instant)) return point;
  }
  return null;
}

export function previous(history: MetricHistory, beforeAt?: number | null): InstantMetricSample | null {
  const candidates = beforeAt == null ? history.slice(0, -1) : history.filter((point) => point.t < beforeAt);
  return candidates.at(-1)?.instant ?? null;
}
